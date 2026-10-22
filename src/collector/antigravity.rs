use super::{context_window_for_model, process};
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp, LaunchSurface,
    SessionStatus, SubAgent, ToolCall, MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES,
};
use chrono::DateTime;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum top-level conversations to fetch per Antigravity database.
const MAX_SESSIONS: u32 = 30;
/// Maximum subagent rows to fetch per Antigravity database.
const MAX_SUBAGENTS: u32 = 100;

/// Collector for Google Antigravity (`agy` CLI, Antigravity 2.0 App, and Antigravity IDE).
///
/// Discovery strategy:
/// 1. Scan running processes for `agy` or `antigravity` binaries.
/// 2. Map running PIDs to exact conversation UUIDs via open file descriptors to
///    `presence/<uuid>.lock` or `conversations/<uuid>.db*` (via `/proc/<pid>/fd` on Linux
///    or `lsof` on macOS), falling back to matching process `cwd` against `workspace_uris`.
/// 3. Query `conversation_summaries.db` via `sqlite3 -readonly -json` on `slow_tick` (~10s)
///    across `~/.gemini/antigravity-cli` (`Cli`), `~/.gemini/antigravity` (`App`), and
///    `~/.gemini/antigravity-ide` (`Ide`), including subagent rows (`parent_conversation_id != ''`).
/// 4. Incrementally parse `<root>/brain/<conv_id>/.system_generated/logs/transcript.jsonl`
///    (tracking byte offset + partial line buffer) for token totals, context window usage,
///    model/effort overrides, tool call timeline, file access audit, and chat tail.
pub struct AntigravityCollector {
    roots: Vec<(PathBuf, LaunchSurface)>,
    sqlite3_available: Option<bool>,
    cached_db_state: HashMap<PathBuf, CachedRootDb>,
    transcript_cache: HashMap<String, TranscriptCacheEntry>,
}

#[derive(Default, Clone)]
struct CachedRootDb {
    sessions: Vec<DbSummaryRow>,
    subagents_by_parent: HashMap<String, Vec<DbSummaryRow>>,
    default_model: String,
    cli_version: String,
}

#[derive(Clone, Debug)]
struct DbSummaryRow {
    conversation_id: String,
    title: String,
    step_count: u32,
    last_modified_ms: u64,
    last_user_input_ms: u64,
    workspace_cwd: String,
    status: String,
    agent_name: String,
    parent_conversation_id: String,
    not_fully_idle: bool,
    killed: bool,
}

#[derive(Clone, Default)]
struct TranscriptCacheEntry {
    offset: u64,
    partial_line: String,
    started_at_ms: u64,
    last_step_ms: u64,
    model: String,
    effort: String,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read: u64,
    last_context_tokens: u64,
    turn_count: u32,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
    compaction_count: u32,
    initial_prompt: String,
    first_assistant_text: String,
    chat_messages: Vec<ChatMessage>,
    tool_calls: Vec<ToolCall>,
    file_accesses: Vec<FileAccess>,
    current_tasks: Vec<String>,
    pending_since_ms: u64,
    thinking_since_ms: u64,
}

impl AntigravityCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let gemini_dir = home.join(".gemini");
        let roots = vec![
            (gemini_dir.join("antigravity-cli"), LaunchSurface::Cli),
            (gemini_dir.join("antigravity"), LaunchSurface::App),
            (gemini_dir.join("antigravity-ide"), LaunchSurface::Ide),
        ];
        Self {
            roots,
            sqlite3_available: None,
            cached_db_state: HashMap::new(),
            transcript_cache: HashMap::new(),
        }
    }

    fn check_sqlite3(&mut self) -> bool {
        if let Some(available) = self.sqlite3_available {
            return available;
        }
        let available = Command::new("sqlite3").arg("--version").output().is_ok();
        self.sqlite3_available = Some(available);
        available
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let agy_pids = Self::find_antigravity_pids(&shared.process_info);
        if agy_pids.is_empty() {
            return Vec::new();
        }

        // Map running PIDs -> (open conversation IDs, process cwd)
        let mut pid_conv_ids: HashMap<u32, HashSet<String>> = HashMap::new();
        let mut conv_to_pid: HashMap<String, u32> = HashMap::new();
        let mut pid_cwds: HashMap<u32, String> = HashMap::new();

        for &pid in &agy_pids {
            let mut open_convs = discover_pid_conversations(pid);
            if let Some(info) = shared.process_info.get(&pid) {
                for cid in extract_conv_ids_from_command(&info.command) {
                    open_convs.insert(cid);
                }
            }
            for cid in &open_convs {
                conv_to_pid.entry(cid.clone()).or_insert(pid);
            }
            if !open_convs.is_empty() {
                pid_conv_ids.insert(pid, open_convs);
            }
            if let Some(cwd) = get_process_cwd(pid) {
                pid_cwds.insert(pid, cwd);
            }
        }

        let has_sqlite = self.check_sqlite3();
        if shared.slow_tick || self.cached_db_state.is_empty() {
            self.refresh_roots_cache(has_sqlite);
        }

        let now_ms = current_time_ms();
        let mut sessions = Vec::new();
        let mut seen_conv_ids = HashSet::new();
        let mut claimed_pids = HashSet::new();

        // First claim any PID that has an explicit presence lock / open conversation DB
        for &pid in conv_to_pid.values() {
            claimed_pids.insert(pid);
        }

        let roots = self.roots.clone();
        for (root_dir, default_surface) in &roots {
            let Some(root_cache) = self.cached_db_state.get(root_dir).cloned() else {
                continue;
            };

            let mut matched_rows: Vec<(&DbSummaryRow, u32)> = Vec::new();
            let mut matched_row_ids: HashSet<String> = HashSet::new();

            for row in &root_cache.sessions {
                if row.killed || seen_conv_ids.contains(&row.conversation_id) {
                    continue;
                }

                // 1. Exact match via presence/<uuid>.lock, conversations/<uuid>.db FD, or --conversation
                let matched_pid = if let Some(&pid) = conv_to_pid.get(&row.conversation_id) {
                    Some(pid)
                } else {
                    // 2. Fallback match by workspace cwd for unclaimed Antigravity processes
                    Self::match_unclaimed_pid_by_cwd(
                        &agy_pids,
                        &pid_cwds,
                        &shared.process_info,
                        &row.workspace_cwd,
                        &mut claimed_pids,
                    )
                };

                if let Some(pid) = matched_pid {
                    matched_row_ids.insert(row.conversation_id.clone());
                    matched_rows.push((row, pid));
                }
            }

            // 3. Fallback for remaining unclaimed processes belonging to this launch surface
            // (e.g. on Windows when open-file FDs are unavailable and process cwd differs from workspace_cwd).
            for &pid in &agy_pids {
                if claimed_pids.contains(&pid) {
                    continue;
                }
                let pid_surface = shared
                    .process_info
                    .get(&pid)
                    .map(|p| detect_launch_surface(&p.command, LaunchSurface::Cli))
                    .unwrap_or(LaunchSurface::Cli);
                if pid_surface != *default_surface {
                    continue;
                }
                if let Some(row) = root_cache.sessions.iter().find(|r| {
                    !r.killed
                        && !seen_conv_ids.contains(&r.conversation_id)
                        && !matched_row_ids.contains(&r.conversation_id)
                }) {
                    claimed_pids.insert(pid);
                    matched_row_ids.insert(row.conversation_id.clone());
                    matched_rows.push((row, pid));
                }
            }

            for (row, pid) in matched_rows {
                seen_conv_ids.insert(row.conversation_id.clone());

                let proc_info = shared.process_info.get(&pid);
                let mem_mb = proc_info.map(|p| p.rss_kb / 1024).unwrap_or(0);
                let surface = proc_info
                    .map(|p| detect_launch_surface(&p.command, *default_surface))
                    .unwrap_or(*default_surface);

                // Parse transcript.jsonl incrementally
                let transcript_path = root_dir
                    .join("brain")
                    .join(&row.conversation_id)
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                let tr = self.update_transcript_cache(&row.conversation_id, &transcript_path);

                // Parse subagents attached to this conversation
                let mut subagents = Vec::new();
                if let Some(sa_rows) = root_cache.subagents_by_parent.get(&row.conversation_id) {
                    for sa in sa_rows {
                        let sa_tr_path = root_dir
                            .join("brain")
                            .join(&sa.conversation_id)
                            .join(".system_generated")
                            .join("logs")
                            .join("transcript.jsonl");
                        let sa_tr =
                            self.update_transcript_cache(&sa.conversation_id, &sa_tr_path);
                        let sa_tokens = sa_tr.total_input_tokens
                            + sa_tr.total_output_tokens
                            + sa_tr.total_cache_read;
                        let sa_working = sa.not_fully_idle
                            || sa.status.contains("RUNNING")
                            || (now_ms.saturating_sub(sa.last_modified_ms) < 15_000
                                && !sa.status.contains("IDLE"));
                        let name = if !sa.title.is_empty() {
                            sa.title.clone()
                        } else if !sa.agent_name.is_empty() {
                            sa.agent_name.clone()
                        } else {
                            sa.conversation_id.chars().take(8).collect()
                        };
                        subagents.push(SubAgent {
                            name,
                            status: if sa_working {
                                "working".to_string()
                            } else {
                                "done".to_string()
                            },
                            tokens: sa_tokens,
                        });
                    }
                }

                let cwd = if !row.workspace_cwd.is_empty() {
                    row.workspace_cwd.clone()
                } else {
                    pid_cwds.get(&pid).cloned().unwrap_or_default()
                };
                let project_name = process::last_path_segment(&cwd)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("antigravity")
                    .to_string();

                let (raw_model, parsed_effort) = if !tr.model.is_empty() {
                    (tr.model.clone(), tr.effort.clone())
                } else {
                    parse_model_and_effort(&root_cache.default_model)
                };
                let model = if raw_model.is_empty() {
                    "gemini".to_string()
                } else {
                    raw_model
                };
                let effort = if !tr.effort.is_empty() {
                    tr.effort.clone()
                } else {
                    parsed_effort
                };

                let context_window = context_window_for_model(&model, "", tr.last_context_tokens);
                let context_percent = if context_window > 0 && tr.last_context_tokens > 0 {
                    ((tr.last_context_tokens as f64 / context_window as f64) * 100.0).min(100.0)
                } else {
                    0.0
                };

                // Status detection combining DB status, transcript pending state, and descendant CPU
                let has_active_child = process::has_active_descendant(
                    pid,
                    &shared.children_map,
                    &shared.process_info,
                    5.0,
                );
                let last_activity_ms = tr.last_step_ms.max(row.last_modified_ms);
                let recent_activity = now_ms.saturating_sub(last_activity_ms) < 30_000;
                let db_running =
                    row.not_fully_idle || row.status == "CASCADE_RUN_STATUS_RUNNING";

                let status = if tr.pending_since_ms > 0
                    && (recent_activity || db_running || has_active_child)
                {
                    SessionStatus::Executing
                } else if (tr.thinking_since_ms > 0 && recent_activity) || db_running {
                    if !tr.current_tasks.is_empty() && tr.pending_since_ms > 0 {
                        SessionStatus::Executing
                    } else {
                        SessionStatus::Thinking
                    }
                } else if has_active_child {
                    SessionStatus::Executing
                } else {
                    SessionStatus::Waiting
                };

                let current_tasks = match status {
                    SessionStatus::Executing => {
                        if !tr.current_tasks.is_empty() {
                            tr.current_tasks.clone()
                        } else {
                            vec!["running tool...".to_string()]
                        }
                    }
                    SessionStatus::Thinking => {
                        if !tr.current_tasks.is_empty() {
                            tr.current_tasks.clone()
                        } else {
                            vec!["thinking...".to_string()]
                        }
                    }
                    _ => vec!["waiting for user input".to_string()],
                };

                let children = collect_children(pid, shared);

                let started_at = if tr.started_at_ms > 0 {
                    tr.started_at_ms
                } else if row.last_user_input_ms > 0 {
                    row.last_user_input_ms
                } else if row.last_modified_ms > 0 {
                    row.last_modified_ms
                } else {
                    now_ms
                };

                let initial_prompt = if !row.title.is_empty() {
                    row.title.clone()
                } else {
                    tr.initial_prompt.clone()
                };

                let turn_count = if tr.turn_count > 0 {
                    tr.turn_count
                } else {
                    row.step_count
                };

                let pending_since_ms = if matches!(status, SessionStatus::Executing) {
                    tr.pending_since_ms
                } else {
                    0
                };
                let thinking_since_ms = if matches!(status, SessionStatus::Thinking) {
                    tr.thinking_since_ms
                } else {
                    0
                };

                sessions.push(AgentSession {
                    agent_cli: "antigravity",
                    launch_surface: surface,
                    pid,
                    session_id: row.conversation_id.clone(),
                    cwd,
                    project_name,
                    started_at,
                    status,
                    model,
                    effort,
                    context_percent,
                    total_input_tokens: tr.total_input_tokens,
                    total_output_tokens: tr.total_output_tokens,
                    total_cache_read: tr.total_cache_read,
                    total_cache_create: 0,
                    turn_count,
                    current_tasks,
                    mem_mb,
                    version: root_cache.cli_version.clone(),
                    git_branch: String::new(),
                    git_added: 0,
                    git_modified: 0,
                    token_history: tr.token_history.clone(),
                    context_history: tr.context_history.clone(),
                    compaction_count: tr.compaction_count,
                    context_window,
                    subagents,
                    mem_file_count: 0,
                    mem_line_count: 0,
                    children,
                    initial_prompt,
                    first_assistant_text: tr.first_assistant_text.clone(),
                    chat_messages: tr.chat_messages.clone(),
                    tool_calls: tr.tool_calls.clone(),
                    pending_since_ms,
                    thinking_since_ms,
                    file_accesses: tr.file_accesses.clone(),
                    config_root: super::abbrev_path(root_dir),
                });
            }
        }

        // Fallback: if an `agy` process holds a lock for a brand-new conversation not yet
        // flushed to `conversation_summaries.db` (or sqlite3 is unavailable), synthesize a
        // session directly from its presence lock + transcript.jsonl.
        for (&pid, conv_ids) in &pid_conv_ids {
            for cid in conv_ids {
                if seen_conv_ids.contains(cid) {
                    continue;
                }
                for (root_dir, default_surface) in &roots {
                    let lock_path = root_dir.join("presence").join(format!("{cid}.lock"));
                    let brain_dir = root_dir.join("brain").join(cid);
                    if !lock_path.exists() && !brain_dir.exists() {
                        continue;
                    }
                    seen_conv_ids.insert(cid.clone());
                    let proc_info = shared.process_info.get(&pid);
                    let mem_mb = proc_info.map(|p| p.rss_kb / 1024).unwrap_or(0);
                    let surface = proc_info
                        .map(|p| detect_launch_surface(&p.command, *default_surface))
                        .unwrap_or(*default_surface);
                    let transcript_path = brain_dir
                        .join(".system_generated")
                        .join("logs")
                        .join("transcript.jsonl");
                    let tr = self.update_transcript_cache(cid, &transcript_path);
                    let root_cache = self
                        .cached_db_state
                        .get(root_dir)
                        .cloned()
                        .unwrap_or_default();
                    let cwd = pid_cwds.get(&pid).cloned().unwrap_or_default();
                    let project_name = process::last_path_segment(&cwd)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("antigravity")
                        .to_string();
                    let (raw_model, parsed_effort) = if !tr.model.is_empty() {
                        (tr.model.clone(), tr.effort.clone())
                    } else {
                        parse_model_and_effort(&root_cache.default_model)
                    };
                    let model = if raw_model.is_empty() {
                        "gemini".to_string()
                    } else {
                        raw_model
                    };
                    let effort = if !tr.effort.is_empty() {
                        tr.effort.clone()
                    } else {
                        parsed_effort
                    };
                    let context_window =
                        context_window_for_model(&model, "", tr.last_context_tokens);
                    let context_percent = if context_window > 0 && tr.last_context_tokens > 0 {
                        ((tr.last_context_tokens as f64 / context_window as f64) * 100.0).min(100.0)
                    } else {
                        0.0
                    };
                    let status = if tr.pending_since_ms > 0 {
                        SessionStatus::Executing
                    } else if tr.thinking_since_ms > 0 {
                        SessionStatus::Thinking
                    } else {
                        SessionStatus::Waiting
                    };
                    let current_tasks = if !tr.current_tasks.is_empty() {
                        tr.current_tasks.clone()
                    } else {
                        vec!["waiting for user input".to_string()]
                    };
                    sessions.push(AgentSession {
                        agent_cli: "antigravity",
                        launch_surface: surface,
                        pid,
                        session_id: cid.clone(),
                        cwd,
                        project_name,
                        started_at: if tr.started_at_ms > 0 {
                            tr.started_at_ms
                        } else {
                            now_ms
                        },
                        status,
                        model,
                        effort,
                        context_percent,
                        total_input_tokens: tr.total_input_tokens,
                        total_output_tokens: tr.total_output_tokens,
                        total_cache_read: tr.total_cache_read,
                        total_cache_create: 0,
                        turn_count: tr.turn_count,
                        current_tasks,
                        mem_mb,
                        version: root_cache.cli_version,
                        git_branch: String::new(),
                        git_added: 0,
                        git_modified: 0,
                        token_history: tr.token_history,
                        context_history: tr.context_history,
                        compaction_count: tr.compaction_count,
                        context_window,
                        subagents: Vec::new(),
                        mem_file_count: 0,
                        mem_line_count: 0,
                        children: collect_children(pid, shared),
                        initial_prompt: tr.initial_prompt,
                        first_assistant_text: tr.first_assistant_text,
                        chat_messages: tr.chat_messages,
                        tool_calls: tr.tool_calls,
                        pending_since_ms: tr.pending_since_ms,
                        thinking_since_ms: tr.thinking_since_ms,
                        file_accesses: tr.file_accesses,
                        config_root: super::abbrev_path(root_dir),
                    });
                    break;
                }
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn refresh_roots_cache(&mut self, has_sqlite: bool) {
        let roots = self.roots.clone();
        for (root_dir, _) in roots {
            if !root_dir.exists() || is_symlink(&root_dir) {
                self.cached_db_state.remove(&root_dir);
                continue;
            }
            let default_model = read_default_model(&root_dir);
            let cli_version = read_cli_version(&root_dir);
            let db_path = root_dir.join("conversation_summaries.db");
            let (mut sessions, mut subagents_by_parent) =
                if has_sqlite && db_path.exists() && !is_symlink(&db_path) {
                    query_summaries_db(&db_path).unwrap_or_default()
                } else {
                    (Vec::new(), HashMap::new())
                };
            // Supplement or fall back to history.jsonl + brain/ transcripts when sqlite3 is
            // unavailable (common on Windows) or when brand-new conversations are not yet in SQLite.
            let (fb_sessions, fb_subagents) = fallback_summaries_from_files(&root_dir);
            if sessions.is_empty() {
                sessions = fb_sessions;
            } else {
                let existing: HashSet<String> =
                    sessions.iter().map(|r| r.conversation_id.clone()).collect();
                for fb in fb_sessions {
                    if !existing.contains(&fb.conversation_id) {
                        sessions.push(fb);
                    }
                }
                sessions.sort_by_key(|r| std::cmp::Reverse(r.last_modified_ms));
                sessions.truncate(MAX_SESSIONS as usize);
            }
            for (parent_id, fb_subs) in fb_subagents {
                let entry = subagents_by_parent.entry(parent_id).or_default();
                let existing_subs: HashSet<String> =
                    entry.iter().map(|r| r.conversation_id.clone()).collect();
                for sub in fb_subs {
                    if !existing_subs.contains(&sub.conversation_id) {
                        entry.push(sub);
                    }
                }
            }
            self.cached_db_state.insert(
                root_dir,
                CachedRootDb {
                    sessions,
                    subagents_by_parent,
                    default_model,
                    cli_version,
                },
            );
        }
    }

    fn update_transcript_cache(&mut self, conv_id: &str, path: &Path) -> TranscriptCacheEntry {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                return self
                    .transcript_cache
                    .get(conv_id)
                    .cloned()
                    .unwrap_or_default();
            }
        };
        let file_len = meta.len();
        let entry = self
            .transcript_cache
            .entry(conv_id.to_string())
            .or_default();

        // Reset if file rotated/shrunk
        if file_len < entry.offset {
            *entry = TranscriptCacheEntry::default();
        }
        if file_len == entry.offset {
            return entry.clone();
        }

        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return entry.clone(),
        };
        if file.seek(SeekFrom::Start(entry.offset)).is_err() {
            return entry.clone();
        }

        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return entry.clone();
        }
        entry.offset = file_len;

        let combined = if entry.partial_line.is_empty() {
            buf
        } else {
            let mut s = std::mem::take(&mut entry.partial_line);
            s.push_str(&buf);
            s
        };

        let mut lines_iter = combined.split('\n').peekable();
        while let Some(line) = lines_iter.next() {
            // If this is the last segment and combined did not end with '\n',
            // buffer it as a partial line unless it parses as valid JSON.
            if lines_iter.peek().is_none() {
                if line.trim().is_empty() {
                    break;
                }
                if serde_json::from_str::<Value>(line).is_err() {
                    entry.partial_line = line.to_string();
                    break;
                }
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            apply_transcript_line(entry, trimmed);
        }

        entry.clone()
    }

    fn find_antigravity_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        let self_pid = std::process::id();
        let candidates: HashSet<u32> = process_info
            .iter()
            .filter(|(&pid, info)| {
                if pid == self_pid || info.command.contains("grep") {
                    return false;
                }
                // Ignore statusline helper scripts spawned by agy and Electron child helpers
                if info.command.contains("statusline.sh")
                    || info.command.contains("abtop-statusline.sh")
                    || info.command.contains("--type=")
                    || info.command.contains("--crashpad-handler-pid=")
                {
                    return false;
                }
                process::cmd_has_binary(&info.command, "agy")
                    || process::cmd_has_binary(&info.command, "antigravity")
                    || process::cmd_has_binary(&info.command, "antigravity-ide")
            })
            .map(|(&pid, _)| pid)
            .collect();

        // Deduplicate wrapper/child pairs (e.g. on Windows when a launcher stub spawns the real
        // binary) by keeping leaf CLI processes and root App/IDE processes.
        let mut pids: Vec<u32> = candidates
            .iter()
            .copied()
            .filter(|&pid| {
                let Some(info) = process_info.get(&pid) else {
                    return true;
                };
                // If this process has a child that is also an Antigravity CLI candidate, prefer the child.
                let has_candidate_child = candidates.iter().any(|&other| {
                    other != pid
                        && process_info
                            .get(&other)
                            .is_some_and(|oinfo| oinfo.ppid == pid)
                });
                if has_candidate_child && process::cmd_has_binary(&info.command, "agy") {
                    return false;
                }
                true
            })
            .collect();
        pids.sort_unstable();
        pids
    }

    fn match_unclaimed_pid_by_cwd(
        agy_pids: &[u32],
        pid_cwds: &HashMap<u32, String>,
        process_info: &HashMap<u32, process::ProcInfo>,
        workspace_cwd: &str,
        claimed_pids: &mut HashSet<u32>,
    ) -> Option<u32> {
        if workspace_cwd.len() < 2 {
            return None;
        }
        for &pid in agy_pids {
            if claimed_pids.contains(&pid) {
                continue;
            }
            if let Some(cwd) = pid_cwds.get(&pid) {
                if paths_equal(cwd, workspace_cwd) {
                    claimed_pids.insert(pid);
                    return Some(pid);
                }
            }
            if let Some(info) = process_info.get(&pid) {
                if info.command.contains(workspace_cwd) {
                    claimed_pids.insert(pid);
                    return Some(pid);
                }
            }
        }
        None
    }
}

impl Default for AntigravityCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for AntigravityCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }

    fn discovered_config_dirs(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .filter(|(dir, _)| dir.is_dir())
            .map(|(dir, _)| dir.clone())
            .collect()
    }
}

fn collect_children(pid: u32, shared: &super::SharedProcessData) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack: Vec<u32> = shared.children_map.get(&pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
    while let Some(cpid) = stack.pop() {
        if !visited.insert(cpid) {
            continue;
        }
        if let Some(cproc) = shared.process_info.get(&cpid) {
            let port = shared.ports.get(&cpid).and_then(|v| v.first().copied());
            children.push(ChildProcess {
                pid: cpid,
                command: cproc.command.clone(),
                mem_kb: cproc.rss_kb,
                port,
            });
        }
        if let Some(grandchildren) = shared.children_map.get(&cpid) {
            stack.extend(grandchildren);
        }
    }
    children
}

fn detect_launch_surface(cmd: &str, default_surface: LaunchSurface) -> LaunchSurface {
    if process::cmd_has_binary(cmd, "antigravity-ide") || cmd.contains("antigravity-ide") {
        LaunchSurface::Ide
    } else if process::cmd_has_binary(cmd, "antigravity") && !process::cmd_has_binary(cmd, "agy") {
        LaunchSurface::App
    } else {
        default_surface
    }
}

/// Discover conversation UUIDs held open by a process via `/proc/<pid>/fd` (Linux)
/// or `lsof` (macOS/BSD). Matches:
/// - `<root>/presence/<uuid>.lock`
/// - `<root>/conversations/<uuid>.db*`
fn discover_pid_conversations(pid: u32) -> HashSet<String> {
    let mut convs = HashSet::new();
    for path in get_process_open_files(pid) {
        if let Some(cid) = extract_conv_id_from_open_path(&path) {
            convs.insert(cid);
        }
    }
    convs
}

#[cfg(target_os = "linux")]
fn get_process_open_files(pid: u32) -> Vec<PathBuf> {
    process::scan_proc_fds(pid)
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn get_process_open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(output) = Command::new("lsof")
        .args(["-p", &pid.to_string(), "-Fn"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
        .collect()
}

#[cfg(target_os = "windows")]
fn get_process_open_files(_pid: u32) -> Vec<PathBuf> {
    Vec::new()
}

pub(crate) fn extract_conv_id_from_open_path(path: &Path) -> Option<String> {
    let parent_name = path.parent()?.file_name()?.to_str()?;
    let file_name = path.file_name()?.to_str()?;
    if parent_name == "presence" {
        let id = file_name.strip_suffix(".lock")?;
        if is_uuid_like(id) {
            return Some(id.to_string());
        }
    } else if parent_name == "conversations" {
        let id = file_name
            .strip_suffix(".db-wal")
            .or_else(|| file_name.strip_suffix(".db-shm"))
            .or_else(|| file_name.strip_suffix(".db"))?;
        if is_uuid_like(id) {
            return Some(id.to_string());
        }
    }
    None
}

fn is_uuid_like(s: &str) -> bool {
    s.len() == 36
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Extract conversation UUIDs passed on the command line (e.g. `agy --conversation <uuid>`
/// or `--resume <uuid>`).
pub(crate) fn extract_conv_ids_from_command(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].trim_matches(|c| c == '"' || c == '\'');
        if matches!(tok, "--conversation" | "--resume" | "-c") {
            if let Some(next) = tokens.get(i + 1) {
                let candidate = next.trim_matches(|c| c == '"' || c == '\'');
                if is_uuid_like(candidate) {
                    out.push(candidate.to_string());
                }
            }
            i += 2;
            continue;
        }
        if let Some(val) = tok
            .strip_prefix("--conversation=")
            .or_else(|| tok.strip_prefix("--resume="))
        {
            let candidate = val.trim_matches(|c| c == '"' || c == '\'');
            if is_uuid_like(candidate) {
                out.push(candidate.to_string());
            }
        }
        i += 1;
    }
    out
}

type SummaryQueryResult = (Vec<DbSummaryRow>, HashMap<String, Vec<DbSummaryRow>>);

/// Pure-Rust fallback for session & subagent discovery when `sqlite3` is unavailable
/// (standard on Windows) or when `conversation_summaries.db` is locked/incomplete.
/// Combines `<root>/history.jsonl` and `<root>/brain/<uuid>/.system_generated/logs/transcript.jsonl`.
fn fallback_summaries_from_files(root_dir: &Path) -> SummaryQueryResult {
    let mut by_id: HashMap<String, DbSummaryRow> = HashMap::new();
    let mut subagents_by_parent: HashMap<String, Vec<DbSummaryRow>> = HashMap::new();

    // 1. Parse history.jsonl (contains top-level CLI prompts with workspace & conversationId)
    let history_path = root_dir.join("history.jsonl");
    if let Ok(text) = fs::read_to_string(&history_path) {
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let Some(cid) = val["conversationId"]
                .as_str()
                .map(str::trim)
                .filter(|s| is_uuid_like(s))
            else {
                continue;
            };
            let ts = val["timestamp"].as_u64().unwrap_or(0);
            let display = sanitize_field(val["display"].as_str().unwrap_or(""), 512, true);
            let raw_ws = val["workspace"].as_str().unwrap_or("").trim();
            let workspace_cwd = if raw_ws.starts_with("file://") {
                decode_file_uri(raw_ws)
            } else {
                raw_ws.to_string()
            };

            let entry = by_id.entry(cid.to_string()).or_insert_with(|| DbSummaryRow {
                conversation_id: cid.to_string(),
                title: display.clone(),
                step_count: 0,
                last_modified_ms: ts,
                last_user_input_ms: ts,
                workspace_cwd: workspace_cwd.clone(),
                status: String::new(),
                agent_name: String::new(),
                parent_conversation_id: String::new(),
                not_fully_idle: false,
                killed: false,
            });
            entry.step_count = entry.step_count.saturating_add(1);
            if entry.title.is_empty() && !display.is_empty() {
                entry.title = display;
            }
            if entry.workspace_cwd.is_empty() && !workspace_cwd.is_empty() {
                entry.workspace_cwd = workspace_cwd;
            }
            if ts > entry.last_user_input_ms {
                entry.last_user_input_ms = ts;
            }
            if ts > entry.last_modified_ms {
                entry.last_modified_ms = ts;
            }
        }
    }

    // 2. Scan brain/<uuid>/.system_generated/logs/transcript.jsonl for mtime, subagents,
    //    and App/IDE conversations not recorded in history.jsonl.
    let brain_root = root_dir.join("brain");
    if let Ok(entries) = fs::read_dir(&brain_root) {
        let mut brain_candidates: Vec<(u64, String, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_uuid_like(name) {
                continue;
            }
            let tr_path = path
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            let mtime_ms = fs::metadata(&tr_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if mtime_ms > 0 {
                brain_candidates.push((mtime_ms, name.to_string(), tr_path));
            }
        }
        brain_candidates.sort_by_key(|(mtime, _, _)| std::cmp::Reverse(*mtime));
        let max_scan = (MAX_SESSIONS + MAX_SUBAGENTS) as usize;
        for (mtime_ms, cid, tr_path) in brain_candidates.into_iter().take(max_scan) {
            if let Some(existing) = by_id.get_mut(&cid) {
                if mtime_ms > existing.last_modified_ms {
                    existing.last_modified_ms = mtime_ms;
                }
                continue;
            }
            let first_line = read_first_line(&tr_path);
            let Ok(val) = serde_json::from_str::<Value>(&first_line) else {
                continue;
            };
            let step_type = val["type"].as_str().unwrap_or("");
            let content = val["content"].as_str().unwrap_or("");
            let created_ms = parse_timestamp_ms(val["created_at"].as_str().unwrap_or(""));
            if step_type == "SYSTEM_MESSAGE" {
                if let Some(parent_id) = extract_subagent_sender(content) {
                    subagents_by_parent
                        .entry(parent_id.clone())
                        .or_default()
                        .push(DbSummaryRow {
                            conversation_id: cid,
                            title: String::new(),
                            step_count: 1,
                            last_modified_ms: mtime_ms.max(created_ms),
                            last_user_input_ms: created_ms,
                            workspace_cwd: String::new(),
                            status: String::new(),
                            agent_name: "subagent".to_string(),
                            parent_conversation_id: parent_id,
                            not_fully_idle: false,
                            killed: false,
                        });
                }
                continue;
            }
            let title = extract_user_request(content)
                .map(|s| sanitize_field(&s, 512, true))
                .unwrap_or_default();
            by_id.insert(
                cid.clone(),
                DbSummaryRow {
                    conversation_id: cid,
                    title,
                    step_count: 1,
                    last_modified_ms: mtime_ms.max(created_ms),
                    last_user_input_ms: created_ms,
                    workspace_cwd: String::new(),
                    status: String::new(),
                    agent_name: String::new(),
                    parent_conversation_id: String::new(),
                    not_fully_idle: false,
                    killed: false,
                },
            );
        }
    }

    let mut sessions: Vec<DbSummaryRow> = by_id.into_values().collect();
    sessions.sort_by_key(|r| std::cmp::Reverse(r.last_modified_ms));
    sessions.truncate(MAX_SESSIONS as usize);
    (sessions, subagents_by_parent)
}

fn read_first_line(path: &Path) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let mut buf = [0u8; 8192];
    let Ok(n) = f.read(&mut buf) else {
        return String::new();
    };
    let s = String::from_utf8_lossy(&buf[..n]);
    s.lines().next().unwrap_or("").trim().to_string()
}

fn extract_subagent_sender(content: &str) -> Option<String> {
    let marker = "sender=";
    let pos = content.find(marker)?;
    let rest = &content[pos + marker.len()..];
    let token = rest.split_whitespace().next()?;
    if is_uuid_like(token) {
        Some(token.to_string())
    } else {
        None
    }
}

fn query_summaries_db(db_path: &Path) -> Option<SummaryQueryResult> {
    let top_sql = format!(
        r#"
SELECT
  conversation_id,
  title,
  step_count,
  last_modified_time,
  last_user_input_time,
  workspace_uris,
  status,
  agent_name,
  parent_conversation_id,
  not_fully_idle,
  killed
FROM conversation_summaries
WHERE parent_conversation_id = ''
ORDER BY last_modified_time DESC
LIMIT {};"#,
        MAX_SESSIONS
    );

    let sub_sql = format!(
        r#"
SELECT
  conversation_id,
  title,
  step_count,
  last_modified_time,
  last_user_input_time,
  workspace_uris,
  status,
  agent_name,
  parent_conversation_id,
  not_fully_idle,
  killed
FROM conversation_summaries
WHERE parent_conversation_id != ''
ORDER BY last_modified_time DESC
LIMIT {};"#,
        MAX_SUBAGENTS
    );

    let top_rows = run_sqlite_json(db_path, &top_sql)?;
    let sub_rows = run_sqlite_json(db_path, &sub_sql).unwrap_or_default();

    let sessions: Vec<DbSummaryRow> = top_rows.iter().filter_map(parse_db_summary_row).collect();
    let mut subagents_by_parent: HashMap<String, Vec<DbSummaryRow>> = HashMap::new();
    for val in &sub_rows {
        if let Some(row) = parse_db_summary_row(val) {
            if !row.parent_conversation_id.is_empty() {
                subagents_by_parent
                    .entry(row.parent_conversation_id.clone())
                    .or_default()
                    .push(row);
            }
        }
    }

    Some((sessions, subagents_by_parent))
}

fn run_sqlite_json(db_path: &Path, sql: &str) -> Option<Vec<Value>> {
    let db = db_path.to_str()?;
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json", db])
        .arg(sql)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    serde_json::from_str(trimmed).ok()
}

fn parse_db_summary_row(row: &Value) -> Option<DbSummaryRow> {
    let conversation_id = row["conversation_id"].as_str()?.trim().to_string();
    if conversation_id.is_empty() {
        return None;
    }
    let title = sanitize_field(row["title"].as_str().unwrap_or(""), 512, true);
    let step_count = row["step_count"].as_u64().unwrap_or(0) as u32;
    let last_modified_ms = parse_timestamp_ms(row["last_modified_time"].as_str().unwrap_or(""));
    let last_user_input_ms = parse_timestamp_ms(row["last_user_input_time"].as_str().unwrap_or(""));
    let workspace_cwd = parse_first_workspace_uri(row["workspace_uris"].as_str().unwrap_or(""));
    let status = sanitize_field(row["status"].as_str().unwrap_or(""), 128, false);
    let agent_name = sanitize_field(row["agent_name"].as_str().unwrap_or(""), 128, false);
    let parent_conversation_id = sanitize_field(
        row["parent_conversation_id"].as_str().unwrap_or(""),
        128,
        false,
    );
    let not_fully_idle = row["not_fully_idle"].as_i64().unwrap_or(0) != 0;
    let killed = row["killed"].as_i64().unwrap_or(0) != 0;

    Some(DbSummaryRow {
        conversation_id,
        title,
        step_count,
        last_modified_ms,
        last_user_input_ms,
        workspace_cwd,
        status,
        agent_name,
        parent_conversation_id,
        not_fully_idle,
        killed,
    })
}

/// Parse `workspace_uris` JSON array (e.g. `["file:///home/user/repo"]`) into a local path.
pub(crate) fn parse_first_workspace_uri(raw: &str) -> String {
    let Ok(arr) = serde_json::from_str::<Vec<String>>(raw) else {
        return String::new();
    };
    let Some(first) = arr.first() else {
        return String::new();
    };
    decode_file_uri(first)
}

pub(crate) fn decode_file_uri(uri: &str) -> String {
    let path_part = uri.strip_prefix("file://").unwrap_or(uri);
    let decoded = percent_decode(path_part);
    // On Windows/WSL, URIs like `file:///d:/repos/foo` have a leading slash before drive letter
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        decoded[1..].to_string()
    } else {
        decoded
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = ( hex_val(bytes[i + 1]), hex_val(bytes[i + 2]) ) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_timestamp_ms(s: &str) -> u64 {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.starts_with("0001-01-01") {
        return 0;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return dt.timestamp_millis().max(0) as u64;
    }
    // SQLite format: "2026-10-18 16:40:58.78945939+00:00" -> replace first space with 'T'
    let iso = trimmed.replacen(' ', "T", 1);
    if let Ok(dt) = DateTime::parse_from_rfc3339(&iso) {
        return dt.timestamp_millis().max(0) as u64;
    }
    0
}

fn read_default_model(root_dir: &Path) -> String {
    let settings_path = root_dir.join("settings.json");
    let Ok(content) = fs::read_to_string(settings_path) else {
        return String::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(&content) else {
        return String::new();
    };
    sanitize_field(val["model"].as_str().unwrap_or(""), 128, false)
}

fn read_cli_version(root_dir: &Path) -> String {
    // Check cli.log symlink or latest log header for `Language server version: X.Y.Z`
    let cli_log = root_dir.join("cli.log");
    let Ok(mut f) = File::open(cli_log) else {
        return String::new();
    };
    let mut buf = [0u8; 1024];
    let Ok(n) = f.read(&mut buf) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&buf[..n]);
    for line in text.lines() {
        if let Some(rest) = line.split("Language server version:").nth(1) {
            return sanitize_field(rest.trim(), 32, false);
        }
    }
    String::new()
}

/// Extract model name and effort from an Antigravity model display string
/// such as `"Gemini 4 Argon (Medium)"` -> `("Gemini 4 Argon", "medium")`.
pub(crate) fn parse_model_and_effort(raw: &str) -> (String, String) {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.rfind('(') {
        if let Some(close) = trimmed[open..].find(')') {
            let inside = trimmed[open + 1..open + close].trim().to_ascii_lowercase();
            if matches!(inside.as_str(), "minimal" | "low" | "medium" | "high" | "max") {
                let model_base = trimmed[..open].trim().to_string();
                return (model_base, inside);
            }
        }
    }
    (trimmed.to_string(), String::new())
}

fn apply_transcript_line(entry: &mut TranscriptCacheEntry, line: &str) {
    let Ok(val) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let step_type = val["type"].as_str().unwrap_or("");
    let created_ms = parse_timestamp_ms(val["created_at"].as_str().unwrap_or(""));
    if created_ms > 0 {
        if entry.started_at_ms == 0 {
            entry.started_at_ms = created_ms;
        }
        entry.last_step_ms = created_ms;
    }

    match step_type {
        "USER_INPUT" => {
            let content = val["content"].as_str().unwrap_or("");
            if let Some(model_setting) = extract_model_setting_change(content) {
                let (m, e) = parse_model_and_effort(&model_setting);
                if !m.is_empty() {
                    entry.model = m;
                }
                if !e.is_empty() {
                    entry.effort = e;
                }
            }
            if let Some(user_req) = extract_user_request(content) {
                let clean = sanitize_field(&user_req, 512, true);
                if !clean.is_empty() {
                    if entry.initial_prompt.is_empty() {
                        entry.initial_prompt = clean.clone();
                    }
                    push_chat_message(&mut entry.chat_messages, ChatRole::User, clean);
                }
            }
            if created_ms > 0 {
                entry.thinking_since_ms = created_ms;
                entry.pending_since_ms = 0;
            }
        }
        "PLANNER_RESPONSE" => {
            entry.turn_count = entry.turn_count.saturating_add(1);
            let input_tok = val["input_tokens"].as_u64().unwrap_or(0);
            let output_tok = val["output_tokens"].as_u64().unwrap_or(0);
            let cache_read = val["cache_read_tokens"].as_u64().unwrap_or(0);

            entry.total_input_tokens = entry.total_input_tokens.saturating_add(input_tok);
            entry.total_output_tokens = entry.total_output_tokens.saturating_add(output_tok);
            entry.total_cache_read = entry.total_cache_read.saturating_add(cache_read);

            let turn_active = input_tok.saturating_add(output_tok);
            if turn_active > 0 {
                entry.token_history.push(turn_active);
            }

            let ctx_tokens = input_tok.saturating_add(cache_read);
            if ctx_tokens > 0 {
                if let Some(&prev_ctx) = entry.context_history.last() {
                    if prev_ctx > 50_000 && (ctx_tokens as f64) < (prev_ctx as f64 * 0.7) {
                        entry.compaction_count = entry.compaction_count.saturating_add(1);
                    }
                }
                entry.context_history.push(ctx_tokens);
                entry.last_context_tokens = ctx_tokens;
            }

            if let Some(text) = val["content"].as_str() {
                let clean = sanitize_field(text.trim(), 512, true);
                if !clean.is_empty() {
                    if entry.first_assistant_text.is_empty() {
                        entry.first_assistant_text = clean.clone();
                    }
                    push_chat_message(&mut entry.chat_messages, ChatRole::Assistant, clean);
                }
            }

            entry.thinking_since_ms = 0;

            if let Some(tool_calls) = val["tool_calls"].as_array() {
                if !tool_calls.is_empty() {
                    entry.current_tasks.clear();
                    for tc in tool_calls {
                        let raw_name = tc["name"].as_str().unwrap_or("tool");
                        let args = &tc["args"];
                        let (norm_name, arg_str, file_access) =
                            normalize_agy_tool(raw_name, args, entry.turn_count);
                        let task_desc = if arg_str.is_empty() {
                            norm_name.to_string()
                        } else {
                            format!("{norm_name} {arg_str}")
                        };
                        entry.current_tasks.push(task_desc);
                        entry.tool_calls.push(ToolCall {
                            name: norm_name.to_string(),
                            arg: arg_str,
                            duration_ms: 0,
                        });
                        if let Some(fa) = file_access {
                            if entry.file_accesses.len() >= MAX_FILE_ACCESSES {
                                entry.file_accesses.remove(0);
                            }
                            entry.file_accesses.push(fa);
                        }
                    }
                    entry.pending_since_ms = created_ms;
                } else {
                    entry.pending_since_ms = 0;
                    entry.current_tasks.clear();
                }
            } else {
                entry.pending_since_ms = 0;
                entry.current_tasks.clear();
            }
        }
        "GENERIC" => {
            // Tool execution completed; compute duration for pending tool calls
            if entry.pending_since_ms > 0 && created_ms >= entry.pending_since_ms {
                let dur = (created_ms - entry.pending_since_ms).max(1);
                for tc in entry.tool_calls.iter_mut().rev() {
                    if tc.duration_ms == 0 {
                        tc.duration_ms = dur;
                    } else {
                        break;
                    }
                }
            }
            entry.pending_since_ms = 0;
            if created_ms > 0 {
                entry.thinking_since_ms = created_ms;
            }
        }
        _ => {}
    }
}

fn push_chat_message(messages: &mut Vec<ChatMessage>, role: ChatRole, text: String) {
    if messages.len() >= MAX_CHAT_MESSAGES {
        messages.remove(0);
    }
    messages.push(ChatMessage { role, text });
}

fn extract_user_request(content: &str) -> Option<String> {
    let start_tag = "<USER_REQUEST>";
    let end_tag = "</USER_REQUEST>";
    if let Some(start) = content.find(start_tag) {
        let after = &content[start + start_tag.len()..];
        let body = if let Some(end) = after.find(end_tag) {
            &after[..end]
        } else {
            after
        };
        let single_line = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !single_line.is_empty() {
            return Some(single_line);
        }
    }
    None
}

fn extract_model_setting_change(content: &str) -> Option<String> {
    let marker = "changed setting `Model Selection` from ";
    let pos = content.find(marker)?;
    let after = &content[pos + marker.len()..];
    let to_pos = after.find(" to ")?;
    let rest = &after[to_pos + 4..];
    let end = rest.find('.').unwrap_or(rest.len());
    let model = rest[..end].trim();
    if model.is_empty() || model.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(model.to_string())
    }
}

/// Normalize Antigravity tool names & stringified JSON args into `abtop` conventions.
fn normalize_agy_tool(
    raw_name: &str,
    args: &Value,
    turn_index: u32,
) -> (&'static str, String, Option<FileAccess>) {
    let clean_arg = |key: &str| -> String {
        let raw = args[key].as_str().unwrap_or("");
        // Antigravity transcript tool_calls args values are often JSON-encoded strings: `"\"/path\""`
        let unquoted = raw.trim().trim_matches('"').replace("\\\"", "\"");
        sanitize_field(&unquoted, 120, true)
    };

    match raw_name {
        "view_file" => {
            let path = clean_arg("AbsolutePath");
            let short = shorten_arg_path(&path);
            let fa = if !path.is_empty() {
                Some(FileAccess {
                    path,
                    operation: FileOp::Read,
                    turn_index,
                })
            } else {
                None
            };
            ("Read", short, fa)
        }
        "replace_file_content" | "multi_replace_file_content" => {
            let path = clean_arg("TargetFile");
            let short = shorten_arg_path(&path);
            let fa = if !path.is_empty() {
                Some(FileAccess {
                    path,
                    operation: FileOp::Edit,
                    turn_index,
                })
            } else {
                None
            };
            ("Edit", short, fa)
        }
        "write_to_file" => {
            let path = clean_arg("TargetFile");
            let short = shorten_arg_path(&path);
            let fa = if !path.is_empty() {
                Some(FileAccess {
                    path,
                    operation: FileOp::Write,
                    turn_index,
                })
            } else {
                None
            };
            ("Write", short, fa)
        }
        "run_command" => {
            let cmd = clean_arg("CommandLine");
            ("Bash", cmd, None)
        }
        "invoke_subagent" | "define_subagent" | "manage_subagents" | "send_message" => {
            let summary = clean_arg("toolSummary");
            ("Agent", summary, None)
        }
        "search_web" => {
            let q = clean_arg("query");
            ("WebSearch", q, None)
        }
        "read_url_content" => {
            let url = clean_arg("Url");
            ("WebFetch", url, None)
        }
        "grep_search" => {
            let q = clean_arg("Query");
            ("Grep", q, None)
        }
        "find_by_name" => {
            let p = clean_arg("Pattern");
            ("Glob", p, None)
        }
        _ => {
            let summary = clean_arg("toolSummary");
            ("Skill", summary, None)
        }
    }
}

fn shorten_arg_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let p = Path::new(path);
    let components: Vec<_> = p.components().collect();
    if components.len() >= 2 {
        let last_two = components[components.len() - 2..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        last_two
    } else {
        path.to_string()
    }
}

fn sanitize_field(raw: &str, max_bytes: usize, redact: bool) -> String {
    let mut s = super::sanitize_terminal_text(raw);
    if redact {
        s = super::redact_secrets(&s);
    }
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

#[cfg(target_os = "windows")]
fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

#[cfg(not(target_os = "windows"))]
fn paths_equal(a: &str, b: &str) -> bool {
    let na = a.trim_end_matches('/');
    let nb = b.trim_end_matches('/');
    if na == nb {
        return true;
    }
    // Resolve symlinks (e.g. `/home/user/repos/abtop` -> `/mnt/sk/repos/abtop`)
    match (fs::canonicalize(na), fs::canonicalize(nb)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(target_os = "windows")]
fn get_process_cwd(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
    );
    sys.process(pid)
        .and_then(|p| p.cwd())
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn get_process_cwd(pid: u32) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with('n') && l.len() > 1)
        .map(|l| l[1..].to_string())
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_conv_id_from_presence_and_db_paths() {
        let lock = Path::new(
            "/home/u/.gemini/antigravity-cli/presence/9f2bddf6-ece6-4f1c-85e6-3104bc077497.lock",
        );
        assert_eq!(
            extract_conv_id_from_open_path(lock),
            Some("9f2bddf6-ece6-4f1c-85e6-3104bc077497".to_string())
        );

        let wal = Path::new(
            "/home/u/.gemini/antigravity-cli/conversations/9f2bddf6-ece6-4f1c-85e6-3104bc077497.db-wal",
        );
        assert_eq!(
            extract_conv_id_from_open_path(wal),
            Some("9f2bddf6-ece6-4f1c-85e6-3104bc077497".to_string())
        );

        let unrelated = Path::new("/home/u/.gemini/antigravity-cli/conversation_summaries.db");
        assert_eq!(extract_conv_id_from_open_path(unrelated), None);
    }

    #[test]
    fn workspace_uri_decodes_unix_and_windows_paths() {
        assert_eq!(
            parse_first_workspace_uri(r#"["file:///home/tuannh/repos/abtop"]"#),
            "/home/tuannh/repos/abtop"
        );
        assert_eq!(
            parse_first_workspace_uri(r#"["file:///d%3A/repos/ToolManhwa"]"#),
            "d:/repos/ToolManhwa"
        );
    }

    #[test]
    fn parse_model_and_effort_splits_parenthetical_effort() {
        assert_eq!(
            parse_model_and_effort("Gemini 4 Argon (Medium)"),
            ("Gemini 4 Argon".to_string(), "medium".to_string())
        );
        assert_eq!(
            parse_model_and_effort("Gemini 3.8 Flash (High)"),
            ("Gemini 3.8 Flash".to_string(), "high".to_string())
        );
        assert_eq!(
            parse_model_and_effort("gemini-2.5-pro"),
            ("gemini-2.5-pro".to_string(), String::new())
        );
    }

    #[test]
    fn transcript_parser_tracks_tokens_tools_and_durations() {
        let mut entry = TranscriptCacheEntry::default();

        let user_line = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-18T16:40:58Z","content":"<USER_REQUEST>\nAdd Antigravity support\n</USER_REQUEST>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 4 Argon (Medium).\n</USER_SETTINGS_CHANGE>"}"#;
        apply_transcript_line(&mut entry, user_line);
        assert_eq!(entry.initial_prompt, "Add Antigravity support");
        assert_eq!(entry.model, "Gemini 4 Argon");
        assert_eq!(entry.effort, "medium");
        assert!(entry.thinking_since_ms > 0);

        let planner_line = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-18T16:41:00Z","input_tokens":30000,"cache_read_tokens":10000,"output_tokens":500,"tool_calls":[{"name":"view_file","args":{"AbsolutePath":"\"/home/u/repo/src/main.rs\"","toolSummary":"\"View main.rs\""}}]}"#;
        apply_transcript_line(&mut entry, planner_line);
        assert_eq!(entry.turn_count, 1);
        assert_eq!(entry.total_input_tokens, 30000);
        assert_eq!(entry.total_output_tokens, 500);
        assert_eq!(entry.total_cache_read, 10000);
        assert_eq!(entry.last_context_tokens, 40000);
        assert_eq!(entry.tool_calls.len(), 1);
        assert_eq!(entry.tool_calls[0].name, "Read");
        assert_eq!(entry.tool_calls[0].arg, "src/main.rs");
        assert_eq!(entry.tool_calls[0].duration_ms, 0);
        assert_eq!(entry.file_accesses.len(), 1);
        assert_eq!(entry.file_accesses[0].operation, FileOp::Read);
        assert!(entry.pending_since_ms > 0);

        let generic_line = r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-10-18T16:41:03Z","content":"file contents"}"#;
        apply_transcript_line(&mut entry, generic_line);
        assert_eq!(entry.tool_calls[0].duration_ms, 3000);
        assert_eq!(entry.pending_since_ms, 0);
    }

    #[test]
    fn extract_conv_ids_from_command_parses_flags() {
        let cmd = r#""C:\Users\u\AppData\Local\Programs\Antigravity\agy.exe" --dangerously-skip-permissions --conversation e6f434ca-cf90-4d7b-8095-2b3392284630"#;
        assert_eq!(
            extract_conv_ids_from_command(cmd),
            vec!["e6f434ca-cf90-4d7b-8095-2b3392284630".to_string()]
        );
    }

    #[test]
    fn fallback_summaries_from_files_works_without_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cid = "f153b0b3-e649-4f52-8559-c8c0443ab3c7";
        let sub_id = "98cbde3f-b300-4412-9180-f4c65d6a2596";

        fs::write(
            root.join("history.jsonl"),
            format!(
                r#"{{"display":"Fix Windows agent detection","timestamp":1792663956094,"workspace":"C:\\Users\\u\\repos\\abtop","conversationId":"{cid}"}}"#
            ),
        )
        .unwrap();

        let tr_dir = root.join("brain").join(cid).join(".system_generated").join("logs");
        fs::create_dir_all(&tr_dir).unwrap();
        fs::write(
            tr_dir.join("transcript.jsonl"),
            r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-10-22T08:27:28Z","content":"<USER_REQUEST>\nFix Windows agent detection\n</USER_REQUEST>"}"#,
        )
        .unwrap();

        let sub_dir = root
            .join("brain")
            .join(sub_id)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(
            sub_dir.join("transcript.jsonl"),
            format!(
                r#"{{"step_index":0,"type":"SYSTEM_MESSAGE","created_at":"2026-10-22T08:35:58Z","content":"[Message] timestamp=2026-10-22T08:35:58Z sender={cid} priority=HIGH content=test"}}"#
            ),
        )
        .unwrap();

        let (sessions, subagents) = fallback_summaries_from_files(root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].conversation_id, cid);
        assert_eq!(sessions[0].workspace_cwd, r"C:\Users\u\repos\abtop");
        assert_eq!(sessions[0].title, "Fix Windows agent detection");
        assert_eq!(subagents.get(cid).map(|v| v.len()), Some(1));
        assert_eq!(subagents[cid][0].conversation_id, sub_id);
    }
}
