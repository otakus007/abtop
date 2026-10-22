//! Collector for Cursor Agent CLI (`cursor-agent`) sessions.
//!
//! Discovery strategy:
//! 1. Find running `cursor-agent` processes via shared `ps` data (exclude `worker-server`)
//! 2. Map PID → open `~/.cursor/chats/{hash}/{sid}/store.db` via `/proc` fds (Linux) or `lsof`
//! 3. Read sibling `meta.json` for cwd/title/timestamps
//! 4. Parse `~/.cursor/projects/{encoded-cwd}/agent-transcripts/{sid}/{sid}.jsonl`
//!
//! Fallback when FD mapping misses: scan recent chat `meta.json` files and match
//! process cwd. One chat is claimed per PID.
//!
//! Tokens/context are not available in local Cursor Agent files today — leave zeros.
//! Never parse encrypted `store.db` blobs. Never match a bare `agent` binary.

use super::process;
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp, LaunchSurface,
    SessionStatus, ToolCall, MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const ACTIVITY_FRESH_SECS: u64 = 30;
const MAX_TOOL_CALLS: usize = 40;
const MAX_PROMPT_CHARS: usize = 120;
const MAX_FALLBACK_METAS: usize = 200;

/// Collector for Cursor Agent CLI sessions under `~/.cursor`.
pub struct CursorCollector {
    cursor_home: PathBuf,
}

impl CursorCollector {
    pub fn new() -> Self {
        Self {
            cursor_home: cursor_home(),
        }
    }

    fn collect_sessions(&self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let pids = find_cursor_agent_pids(&shared.process_info);
        if pids.is_empty() {
            return vec![];
        }

        let mut pid_to_chat = map_pid_to_chat_dir(&pids, &self.cursor_home);
        let mut claimed_chats: HashSet<PathBuf> = pid_to_chat.values().cloned().collect();

        // Fallback: match process cwd to meta.json or project agent-transcripts for PIDs without an FD hit.
        // Claim each chat immediately so two same-cwd PIDs cannot share one dir.
        for &pid in &pids {
            if pid_to_chat.contains_key(&pid) {
                continue;
            }
            let chat_dir = get_process_cwd(pid)
                .and_then(|cwd| fallback_chat_dir_for_cwd(&self.cursor_home, &cwd, &claimed_chats))
                .or_else(|| fallback_newest_chat_dir(&self.cursor_home, &claimed_chats));
            if let Some(chat_dir) = chat_dir {
                claimed_chats.insert(chat_dir.clone());
                pid_to_chat.insert(pid, chat_dir);
            }
        }

        let mut sessions = Vec::new();
        let mut used_chats = HashSet::new();
        for &pid in &pids {
            let Some(chat_dir) = pid_to_chat.get(&pid) else {
                continue;
            };
            if !used_chats.insert(chat_dir.clone()) {
                continue;
            }
            let info = shared.process_info.get(&pid);
            let mem_mb = info.map(|p| p.rss_kb / 1024).unwrap_or(0);
            let children = child_processes(shared, pid);
            if let Some(session) =
                session_from_chat_dir(&self.cursor_home, chat_dir, pid, mem_mb, children)
            {
                sessions.push(session);
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }
}

impl Default for CursorCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for CursorCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

fn cursor_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".cursor")
}

fn is_cursor_agent_command(command: &str) -> bool {
    process::cmd_has_binary(command, "cursor-agent")
        && !command.contains("worker-server")
        && !command.contains("grep")
}

fn find_cursor_agent_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
    let candidates: HashSet<u32> = process_info
        .iter()
        .filter(|(_, info)| is_cursor_agent_command(&info.command))
        .map(|(pid, _)| *pid)
        .collect();

    // Deduplicate parent/child wrapper pairs (e.g. on Windows when `cursor-agent` launcher
    // spawns `node.exe ...\cursor-agent\versions\...\index.js`). Prefer the leaf process.
    let mut pids: Vec<u32> = candidates
        .iter()
        .copied()
        .filter(|&pid| {
            !candidates.iter().any(|&other| {
                other != pid
                    && process_info
                        .get(&other)
                        .is_some_and(|oinfo| oinfo.ppid == pid)
            })
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// Map PIDs to chat directories by finding open `store.db` (+ WAL/SHM) under
/// `~/.cursor/chats/{hash}/{sid}/`.
fn map_pid_to_chat_dir(pids: &[u32], cursor_home: &Path) -> HashMap<u32, PathBuf> {
    let mut map = HashMap::new();
    if pids.is_empty() {
        return map;
    }

    for &pid in pids {
        for target in open_paths(pid) {
            if let Some(chat_dir) = chat_dir_from_open_path(&target, cursor_home) {
                map.insert(pid, chat_dir);
                break;
            }
        }
    }
    map
}

fn open_paths(pid: u32) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        process::scan_proc_fds(pid)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = pid;
        Vec::new()
    }
    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    {
        lsof_open_paths(pid)
    }
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn lsof_open_paths(pid: u32) -> Vec<PathBuf> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-Fn"])
        .output();
    let Ok(output) = output else {
        return vec![];
    };
    if !output.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n').map(PathBuf::from))
        .collect()
}

/// Accept open paths like:
/// `~/.cursor/chats/{hash}/{sid}/store.db` (+ `-wal` / `-shm`).
fn chat_dir_from_open_path(path: &Path, cursor_home: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if !(name == "store.db" || name == "store.db-wal" || name == "store.db-shm") {
        return None;
    }
    let chat_dir = path.parent()?.to_path_buf();
    if !is_trusted_chat_dir(&chat_dir, cursor_home) {
        return None;
    }
    Some(chat_dir)
}

fn is_trusted_chat_dir(chat_dir: &Path, cursor_home: &Path) -> bool {
    // Expect: {cursor_home}/chats/{hash}/{sid}
    let Some(hash_dir) = chat_dir.parent() else {
        return false;
    };
    let Some(chats) = hash_dir.parent() else {
        return false;
    };
    if chats.file_name().and_then(|n| n.to_str()) != Some("chats") {
        return false;
    }
    chats.parent() == Some(cursor_home)
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ChatMeta {
    #[serde(rename = "createdAtMs")]
    created_at_ms: u64,
    #[serde(rename = "updatedAtMs")]
    updated_at_ms: u64,
    title: String,
    cwd: String,
}

fn read_meta(chat_dir: &Path) -> Option<ChatMeta> {
    let text = fs::read_to_string(chat_dir.join("meta.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn encode_project_path(cwd: &str) -> String {
    let trimmed = cwd.trim().trim_end_matches(['/', '\\']);
    let without_root = trimmed.trim_start_matches(['/', '\\']);
    let mut out = String::with_capacity(without_root.len());
    let mut prev_dash = false;
    for ch in without_root.chars() {
        if matches!(ch, '/' | '\\' | ':' | '_' | '.') {
            if !prev_dash && !out.is_empty() {
                out.push('-');
                prev_dash = true;
            }
        } else {
            out.push(ch);
            prev_dash = ch == '-';
        }
    }
    out.trim_matches('-').to_string()
}

fn transcript_path(cursor_home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let projects_root = cursor_home.join("projects");
    let direct = projects_root
        .join(encode_project_path(cwd))
        .join("agent-transcripts")
        .join(session_id)
        .join(format!("{session_id}.jsonl"));
    if direct.is_file() {
        return direct;
    }
    if let Ok(entries) = fs::read_dir(&projects_root) {
        for entry in entries.flatten() {
            let candidate = entry
                .path()
                .join("agent-transcripts")
                .join(session_id)
                .join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    direct
}

fn fallback_chat_dir_for_cwd(
    cursor_home: &Path,
    cwd: &str,
    claimed: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let chats = cursor_home.join("chats");
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    let mut by_sid: HashMap<String, PathBuf> = HashMap::new();
    let Ok(hash_entries) = fs::read_dir(&chats) else {
        return None;
    };
    for hash_entry in hash_entries.flatten() {
        let hash_path = hash_entry.path();
        if !hash_path.is_dir() {
            continue;
        }
        let Ok(sid_entries) = fs::read_dir(&hash_path) else {
            continue;
        };
        for sid_entry in sid_entries.flatten() {
            let chat_dir = sid_entry.path();
            if claimed.contains(&chat_dir) {
                continue;
            }
            if let Some(sid) = chat_dir.file_name().and_then(|n| n.to_str()) {
                by_sid.insert(sid.to_string(), chat_dir.clone());
            }
            let Some(meta) = read_meta(&chat_dir) else {
                continue;
            };
            if !paths_equal(&meta.cwd, cwd) {
                continue;
            }
            let updated = if meta.updated_at_ms > 0 {
                meta.updated_at_ms
            } else {
                meta.created_at_ms
            };
            candidates.push((updated, chat_dir));
            if candidates.len() >= MAX_FALLBACK_METAS {
                break;
            }
        }
        if candidates.len() >= MAX_FALLBACK_METAS {
            break;
        }
    }

    // Newer cursor-agent builds often create `chats/{hash}/{sid}/store.db` without `meta.json`
    // while writing transcripts to `projects/{encoded-cwd}/agent-transcripts/{sid}/{sid}.jsonl`.
    if candidates.is_empty() && !cwd.is_empty() {
        let target_proj = encode_project_path(cwd);
        let projects_root = cursor_home.join("projects");
        if let Ok(proj_entries) = fs::read_dir(&projects_root) {
            for proj_entry in proj_entries.flatten() {
                let proj_path = proj_entry.path();
                let Some(proj_name) = proj_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !proj_name.eq_ignore_ascii_case(&target_proj) {
                    continue;
                }
                let tr_root = proj_path.join("agent-transcripts");
                let Ok(tr_entries) = fs::read_dir(&tr_root) else {
                    continue;
                };
                for tr_entry in tr_entries.flatten() {
                    let tr_dir = tr_entry.path();
                    let Some(sid) = tr_dir.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if let Some(chat_dir) = by_sid.get(sid) {
                        if !claimed.contains(chat_dir) {
                            let mtime = file_mtime_ms(&tr_dir.join(format!("{sid}.jsonl")))
                                .max(chat_dir_mtime_ms(chat_dir));
                            candidates.push((mtime, chat_dir.clone()));
                        }
                    }
                }
            }
        }
    }

    candidates
        .into_iter()
        .max_by_key(|(updated, _)| *updated)
        .map(|(_, path)| path)
}

/// Final fallback for an unclaimed live `cursor-agent` process when open-file FDs are
/// unavailable (Windows) and `meta.json`/transcript cwd matching misses.
fn fallback_newest_chat_dir(cursor_home: &Path, claimed: &HashSet<PathBuf>) -> Option<PathBuf> {
    let chats = cursor_home.join("chats");
    let hash_entries = fs::read_dir(&chats).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for hash_entry in hash_entries.flatten() {
        let hash_path = hash_entry.path();
        if !hash_path.is_dir() {
            continue;
        }
        let Ok(sid_entries) = fs::read_dir(&hash_path) else {
            continue;
        };
        for sid_entry in sid_entries.flatten() {
            let chat_dir = sid_entry.path();
            if claimed.contains(&chat_dir) {
                continue;
            }
            let mtime = chat_dir_mtime_ms(&chat_dir);
            if mtime == 0 {
                continue;
            }
            if best.as_ref().is_none_or(|(best_t, _)| mtime > *best_t) {
                best = Some((mtime, chat_dir));
            }
        }
    }
    best.map(|(_, path)| path)
}

fn chat_dir_mtime_ms(chat_dir: &Path) -> u64 {
    file_mtime_ms(&chat_dir.join("store.db-wal"))
        .max(file_mtime_ms(&chat_dir.join("store.db")))
        .max(file_mtime_ms(&chat_dir.join("meta.json")))
}

fn file_mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    a == b
}

#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
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
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix('n').map(|p| p.to_string()))
}

fn child_processes(shared: &super::SharedProcessData, root_pid: u32) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack: Vec<u32> = shared
        .children_map
        .get(&root_pid)
        .cloned()
        .unwrap_or_default();
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

fn session_from_chat_dir(
    cursor_home: &Path,
    chat_dir: &Path,
    pid: u32,
    mem_mb: u64,
    children: Vec<ChildProcess>,
) -> Option<AgentSession> {
    let session_id = chat_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        return None;
    }
    let meta = read_meta(chat_dir).unwrap_or_default();
    let cwd = if meta.cwd.is_empty() {
        get_process_cwd(pid).unwrap_or_default()
    } else {
        meta.cwd.clone()
    };
    let project_name = process::last_path_segment(&cwd)
        .unwrap_or("cursor-agent")
        .to_string();
    let started_at = if meta.created_at_ms > 0 {
        meta.created_at_ms
    } else {
        now_ms()
    };

    let tpath = transcript_path(cursor_home, &cwd, &session_id);
    let parsed = parse_transcript(&tpath);
    let age_secs = transcript_age_secs(&tpath).or_else(|| {
        if meta.updated_at_ms > 0 {
            Some(now_ms().saturating_sub(meta.updated_at_ms) / 1000)
        } else {
            None
        }
    });
    let status = derive_status(&parsed, age_secs);
    let current_tasks = current_tasks_for(&status, &parsed);
    let title = if !meta.title.is_empty() {
        sanitize_prompt(&meta.title)
    } else {
        parsed.initial_prompt.clone()
    };

    Some(AgentSession {
        agent_cli: "cursor-agent",
        launch_surface: LaunchSurface::Cli,
        pid,
        session_id,
        cwd,
        project_name,
        started_at,
        status,
        model: "default".to_string(),
        effort: String::new(),
        context_percent: 0.0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        turn_count: parsed.turn_count,
        current_tasks,
        mem_mb,
        version: String::new(),
        git_branch: String::new(),
        git_added: 0,
        git_modified: 0,
        token_history: vec![],
        context_history: vec![],
        compaction_count: 0,
        context_window: 0,
        subagents: vec![],
        mem_file_count: 0,
        mem_line_count: 0,
        children,
        initial_prompt: title,
        first_assistant_text: parsed.first_assistant_text,
        chat_messages: parsed.chat_messages,
        tool_calls: parsed.tool_calls,
        pending_since_ms: parsed.pending_since_ms,
        thinking_since_ms: parsed.thinking_since_ms,
        file_accesses: parsed.file_accesses,
        config_root: super::abbrev_path(cursor_home),
    })
}

#[derive(Default)]
struct TranscriptParse {
    initial_prompt: String,
    first_assistant_text: String,
    chat_messages: Vec<ChatMessage>,
    tool_calls: Vec<ToolCall>,
    file_accesses: Vec<FileAccess>,
    turn_count: u32,
    last_tool_label: String,
    last_role: Option<ChatRole>,
    has_pending_tools: bool,
    pending_since_ms: u64,
    thinking_since_ms: u64,
}

fn parse_transcript(path: &Path) -> TranscriptParse {
    let Ok(text) = fs::read_to_string(path) else {
        return TranscriptParse::default();
    };
    parse_transcript_text(&text)
}

fn parse_transcript_text(text: &str) -> TranscriptParse {
    let mut out = TranscriptParse::default();
    let mut turn_index = 0u32;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = value
            .get("role")
            .and_then(|r| r.as_str())
            .or_else(|| value.get("type").and_then(|t| t.as_str()));
        let Some(role) = role else {
            continue;
        };
        let content = value
            .pointer("/message/content")
            .or_else(|| value.get("content"));
        match role {
            "user" => {
                out.last_role = Some(ChatRole::User);
                out.has_pending_tools = false;
                out.pending_since_ms = 0;
                out.thinking_since_ms = now_ms();
                if let Some(text) = first_text_block(content) {
                    let cleaned = sanitize_prompt(&text);
                    if out.initial_prompt.is_empty() && !cleaned.is_empty() {
                        out.initial_prompt = cleaned.clone();
                    }
                    push_chat(&mut out.chat_messages, ChatRole::User, cleaned);
                }
            }
            "assistant" => {
                out.turn_count = out.turn_count.saturating_add(1);
                turn_index = turn_index.saturating_add(1);
                out.last_role = Some(ChatRole::Assistant);
                out.thinking_since_ms = 0;
                let mut saw_tool = false;
                if let Some(arr) = content.and_then(|c| c.as_array()) {
                    for block in arr {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if btype == "text" {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                let cleaned = sanitize_prompt(t);
                                if out.first_assistant_text.is_empty() && !cleaned.is_empty() {
                                    out.first_assistant_text = cleaned.clone();
                                }
                                if !cleaned.is_empty() {
                                    push_chat(&mut out.chat_messages, ChatRole::Assistant, cleaned);
                                }
                            }
                        } else if btype == "tool_use" {
                            saw_tool = true;
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let arg = extract_tool_arg(block);
                            let label = if arg.is_empty() {
                                name.clone()
                            } else {
                                format!("{name} {arg}")
                            };
                            out.last_tool_label = truncate_chars(&label, 60);
                            if out.tool_calls.len() < MAX_TOOL_CALLS {
                                out.tool_calls.push(ToolCall {
                                    name: name.clone(),
                                    arg: arg.clone(),
                                    duration_ms: 0,
                                });
                            }
                            if let Some(access) = file_access_from_tool(&name, block, turn_index) {
                                if out.file_accesses.len() < MAX_FILE_ACCESSES {
                                    out.file_accesses.push(access);
                                }
                            }
                        }
                    }
                }
                if saw_tool {
                    out.has_pending_tools = true;
                    out.pending_since_ms = now_ms();
                } else {
                    out.has_pending_tools = false;
                    out.pending_since_ms = 0;
                }
            }
            _ => {}
        }
    }
    out
}

fn first_text_block(content: Option<&Value>) -> Option<String> {
    let arr = content?.as_array()?;
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                if !t.trim().is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn push_chat(messages: &mut Vec<ChatMessage>, role: ChatRole, text: String) {
    if text.is_empty() {
        return;
    }
    messages.push(ChatMessage { role, text });
    if messages.len() > MAX_CHAT_MESSAGES {
        let drop_n = messages.len() - MAX_CHAT_MESSAGES;
        messages.drain(0..drop_n);
    }
}

fn extract_tool_arg(tool_use: &Value) -> String {
    let Some(input) = tool_use.get("input") else {
        return String::new();
    };
    if let Some(fp) = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("target_directory"))
        .and_then(|f| f.as_str())
    {
        return shorten_path(fp);
    }
    if let Some(cmd) = input
        .get("command")
        .or_else(|| input.get("pattern"))
        .or_else(|| input.get("glob_pattern"))
        .or_else(|| input.get("query"))
        .and_then(|c| c.as_str())
    {
        let short = cmd.lines().next().unwrap_or(cmd);
        return super::redact_secrets(&truncate_chars(short, 40));
    }
    if let Some(ns) = input.get("namespace").and_then(|n| n.as_str()) {
        let tool = input
            .get("toolName")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if tool.is_empty() {
            return truncate_chars(ns, 40);
        }
        return truncate_chars(&format!("{ns}/{tool}"), 40);
    }
    String::new()
}

fn file_access_from_tool(name: &str, tool_use: &Value, turn_index: u32) -> Option<FileAccess> {
    let op = match name {
        "Read" | "read_file" => FileOp::Read,
        "Write" | "write_file" => FileOp::Write,
        "Edit" | "StrReplace" | "search_replace" => FileOp::Edit,
        _ => return None,
    };
    let path = tool_use
        .pointer("/input/file_path")
        .or_else(|| tool_use.pointer("/input/path"))
        .and_then(|p| p.as_str())?;
    Some(FileAccess {
        path: shorten_path(path),
        operation: op,
        turn_index,
    })
}

fn shorten_path(path: &str) -> String {
    #[cfg(windows)]
    let parts: Vec<&str> = path.rsplit(['/', '\\']).collect();
    #[cfg(not(windows))]
    let parts: Vec<&str> = path.rsplit('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[1], parts[0])
    }
}

fn sanitize_prompt(raw: &str) -> String {
    let stripped = strip_heavy_wrappers(raw);
    let terminal_safe = super::sanitize_terminal_text(stripped.trim());
    let redacted = super::redact_secrets(&terminal_safe);
    truncate_chars(&redacted, MAX_PROMPT_CHARS)
}

fn strip_heavy_wrappers(raw: &str) -> String {
    // Drop skill dumps / system blocks that dominate Cursor user turns.
    let mut out = String::new();
    let mut skipping = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("<manually_attached_skills>")
            || trimmed.starts_with("<agent_skills>")
            || trimmed.starts_with("<system_reminder>")
        {
            skipping = true;
            continue;
        }
        if skipping {
            if trimmed.starts_with("</manually_attached_skills>")
                || trimmed.starts_with("</agent_skills>")
                || trimmed.starts_with("</system_reminder>")
            {
                skipping = false;
            }
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line.trim());
        if out.len() > MAX_PROMPT_CHARS * 2 {
            break;
        }
    }
    if out.is_empty() {
        raw.chars().take(MAX_PROMPT_CHARS).collect()
    } else {
        out
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn transcript_age_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_secs(),
    )
}

fn derive_status(parsed: &TranscriptParse, age_secs: Option<u64>) -> SessionStatus {
    let fresh = age_secs.is_some_and(|s| s < ACTIVITY_FRESH_SECS);
    if parsed.has_pending_tools && fresh {
        return SessionStatus::Executing;
    }
    if matches!(parsed.last_role, Some(ChatRole::User)) && fresh {
        return SessionStatus::Thinking;
    }
    if fresh && matches!(parsed.last_role, Some(ChatRole::Assistant)) {
        return SessionStatus::Thinking;
    }
    SessionStatus::Waiting
}

fn current_tasks_for(status: &SessionStatus, parsed: &TranscriptParse) -> Vec<String> {
    match status {
        SessionStatus::Executing if !parsed.last_tool_label.is_empty() => {
            vec![parsed.last_tool_label.clone()]
        }
        SessionStatus::Thinking => vec!["thinking...".to_string()],
        SessionStatus::Waiting => vec!["waiting for input".to_string()],
        _ => {
            if !parsed.last_tool_label.is_empty() {
                vec![parsed.last_tool_label.clone()]
            } else {
                vec![]
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_project_path_strips_root_and_replaces_slashes() {
        assert_eq!(
            encode_project_path("/mnt/sk/repos/abtop"),
            "mnt-sk-repos-abtop"
        );
        assert_eq!(encode_project_path("/Users/foo/bar/"), "Users-foo-bar");
        assert_eq!(
            encode_project_path(r"C:\Users\foo\my_repo"),
            "C-Users-foo-my-repo"
        );
    }

    #[test]
    fn is_cursor_agent_command_rejects_worker_and_bare_agent() {
        assert!(is_cursor_agent_command(
            "/usr/bin/cursor-agent --use-system-ca /opt/cursor-agent/index.js"
        ));
        assert!(!is_cursor_agent_command(
            "/opt/cursor-agent/node /opt/cursor-agent/index.js worker-server"
        ));
        assert!(!is_cursor_agent_command("/home/u/.grok/bin/agent --help"));
        assert!(!is_cursor_agent_command("grep cursor-agent"));
    }

    #[test]
    fn chat_dir_from_store_db_paths() {
        let home = PathBuf::from("/home/u/.cursor");
        let db = home.join("chats/abc/sid-1/store.db");
        let wal = home.join("chats/abc/sid-1/store.db-wal");
        let expected = home.join("chats/abc/sid-1");
        assert_eq!(chat_dir_from_open_path(&db, &home), Some(expected.clone()));
        assert_eq!(chat_dir_from_open_path(&wal, &home), Some(expected));
        assert_eq!(
            chat_dir_from_open_path(Path::new("/tmp/other/store.db"), &home),
            None
        );
    }

    #[test]
    fn parse_transcript_extracts_tools_and_prompt() {
        let text = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"Add cursor support"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Sure."},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/main.rs"}}]}}"#,
            "\n",
        );
        let parsed = parse_transcript_text(text);
        assert_eq!(parsed.initial_prompt, "Add cursor support");
        assert_eq!(parsed.first_assistant_text, "Sure.");
        assert_eq!(parsed.turn_count, 1);
        assert!(parsed.has_pending_tools);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "Edit");
        assert!(parsed.last_tool_label.contains("Edit"));
        assert_eq!(parsed.file_accesses.len(), 1);
        assert_eq!(parsed.file_accesses[0].operation, FileOp::Edit);
    }

    #[test]
    fn sanitize_prompt_strips_attached_skills_block() {
        let raw = "<manually_attached_skills>\nhuge skill dump\n</manually_attached_skills>\nReal ask here";
        let cleaned = sanitize_prompt(raw);
        assert!(cleaned.contains("Real ask here"));
        assert!(!cleaned.contains("huge skill dump"));
    }

    #[test]
    fn derive_status_waiting_when_stale() {
        let mut parsed = TranscriptParse::default();
        parsed.last_role = Some(ChatRole::Assistant);
        parsed.has_pending_tools = true;
        assert_eq!(derive_status(&parsed, Some(120)), SessionStatus::Waiting);
        assert_eq!(derive_status(&parsed, Some(5)), SessionStatus::Executing);
    }

    #[test]
    fn fallback_picks_newest_matching_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".cursor");
        let older = home.join("chats/h1/old");
        let newer = home.join("chats/h1/new");
        fs::create_dir_all(&older).unwrap();
        fs::create_dir_all(&newer).unwrap();
        fs::write(
            older.join("meta.json"),
            r#"{"cwd":"/proj","updatedAtMs":100,"createdAtMs":100,"title":"old"}"#,
        )
        .unwrap();
        fs::write(
            newer.join("meta.json"),
            r#"{"cwd":"/proj","updatedAtMs":200,"createdAtMs":150,"title":"new"}"#,
        )
        .unwrap();
        let claimed = HashSet::new();
        let picked = fallback_chat_dir_for_cwd(&home, "/proj", &claimed).unwrap();
        assert_eq!(picked, newer);
    }

    #[test]
    fn find_cursor_agent_pids_filters_worker() {
        let mut info = HashMap::new();
        info.insert(
            1,
            process::ProcInfo {
                pid: 1,
                ppid: 0,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "/usr/bin/cursor-agent --yolo".into(),
            },
        );
        info.insert(
            2,
            process::ProcInfo {
                pid: 2,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "/opt/cursor-agent/node index.js worker-server".into(),
            },
        );
        let pids = find_cursor_agent_pids(&info);
        assert_eq!(pids, vec![1]);
    }

    #[test]
    fn fallback_matches_chat_dir_without_meta_via_project_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".cursor");
        let sid = "1471148c-b0ff-47b8-b7a9-e3668f612d86";
        let chat_dir = home.join("chats/71931c983498cfb7d129288aac75537c").join(sid);
        let tr_dir = home
            .join("projects/C-Users-u-my-repo/agent-transcripts")
            .join(sid);
        fs::create_dir_all(&chat_dir).unwrap();
        fs::create_dir_all(&tr_dir).unwrap();
        fs::write(chat_dir.join("store.db"), b"sqlite").unwrap();
        fs::write(tr_dir.join(format!("{sid}.jsonl")), "{}\n").unwrap();

        let claimed = HashSet::new();
        let picked =
            fallback_chat_dir_for_cwd(&home, r"C:\Users\u\my_repo", &claimed).unwrap();
        assert_eq!(picked, chat_dir);
    }
}
