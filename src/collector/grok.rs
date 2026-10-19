use super::process;
use crate::model::{
    AgentSession, ChildProcess, FileAccess, FileOp, LaunchSurface, SessionStatus, SubAgent,
    ToolCall,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const TAIL_BYTES: u64 = 256 * 1024;
const ACTIVITY_FRESH_SECS: u64 = 30;
const MAX_TOOL_CALLS: usize = 30;
const MAX_SUBAGENTS: usize = 16;
const MAX_GROUP_NAME: usize = 255;

/// Collector for Grok Build CLI sessions under `$GROK_HOME` or `~/.grok`.
///
/// Live PIDs come from the shared `ps` snapshot. The session directory is
/// taken from an open file under `sessions/<group>/<id>/` when the platform
/// exposes fds, otherwise from the newest `active_sessions.json` row for
/// that PID. Token totals come from `usage.json`, context from `signals.json`.
pub struct GrokCollector {
    home: PathBuf,
}

impl GrokCollector {
    pub fn new() -> Self {
        Self { home: grok_home() }
    }

    fn collect_sessions(&self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let active = read_active_sessions(&self.home.join("active_sessions.json"));
        let version = read_version(&self.home);
        let mut sessions = Vec::new();
        for (pid, info) in &shared.process_info {
            if !is_grok_command(&info.command) {
                continue;
            }
            let Some(dir) = resolve_session_dir(&self.home, *pid, &open_paths(*pid), &active)
            else {
                continue;
            };
            let mem_mb = info.rss_kb / 1024;
            let children = child_processes(shared, *pid);
            if let Some(session) =
                session_from_dir(&dir, *pid, mem_mb, children, &version, &self.home)
            {
                sessions.push(session);
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }
}

impl Default for GrokCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for GrokCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

fn grok_home() -> PathBuf {
    match std::env::var("GROK_HOME") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::home_dir().unwrap_or_default().join(".grok"),
    }
}

fn is_grok_command(command: &str) -> bool {
    process::cmd_has_binary(command, "grok") && !command.contains("grep")
}

fn open_paths(pid: u32) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        process::scan_proc_fds(pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Vec::new()
    }
}

#[derive(Clone)]
struct ActiveRow {
    session_id: String,
    pid: u32,
    cwd: String,
    opened_at: String,
}

fn read_active_sessions(path: &Path) -> Vec<ActiveRow> {
    let text = fs::read_to_string(path).unwrap_or_default();
    parse_active_sessions(&text)
}

fn parse_active_sessions(text: &str) -> Vec<ActiveRow> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            Some(ActiveRow {
                session_id: row.get("session_id")?.as_str()?.to_string(),
                pid: row.get("pid")?.as_u64()? as u32,
                cwd: row.get("cwd")?.as_str()?.to_string(),
                opened_at: row
                    .get("opened_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

fn latest_row(rows: &[ActiveRow], pid: u32) -> Option<&ActiveRow> {
    rows.iter()
        .filter(|row| row.pid == pid)
        .max_by(|a, b| a.opened_at.cmp(&b.opened_at))
}

/// Percent-encode a cwd the way Grok names session groups.
/// Unreserved ASCII (`A-Z a-z 0-9 - _ . ~`) is left as-is. Hex is uppercase.
fn encode_cwd_group(cwd: &str) -> String {
    let mut out = String::with_capacity(cwd.len());
    for byte in cwd.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn session_dir_from_open_path(path: &Path) -> Option<PathBuf> {
    let components: Vec<_> = path.components().collect();
    let index = components
        .iter()
        .position(|component| component.as_os_str() == "sessions")?;
    if index + 2 >= components.len() {
        return None;
    }
    let mut dir = PathBuf::new();
    for component in &components[..=index + 2] {
        dir.push(component);
    }
    Some(dir)
}

fn is_trusted_session_dir(dir: &Path, home: &Path) -> bool {
    let Some(group) = dir.parent() else {
        return false;
    };
    let Some(sessions) = group.parent() else {
        return false;
    };
    if sessions.file_name().and_then(|name| name.to_str()) != Some("sessions") {
        return false;
    }
    let Some(root) = sessions.parent() else {
        return false;
    };
    root == home
        || matches!(
            root.file_name().and_then(|name| name.to_str()),
            Some(".grok") | Some("grok")
        )
}

fn resolve_session_dir(
    home: &Path,
    pid: u32,
    open: &[PathBuf],
    active: &[ActiveRow],
) -> Option<PathBuf> {
    let mut found = Vec::new();
    for path in open {
        if let Some(dir) = session_dir_from_open_path(path) {
            if is_trusted_session_dir(&dir, home)
                && (dir.join("summary.json").is_file() || dir.join("events.jsonl").is_file())
                && !found.iter().any(|existing: &PathBuf| existing == &dir)
            {
                found.push(dir);
            }
        }
    }
    let preferred = latest_row(active, pid);
    if let Some(dir) = pick_open_session_dir(&found, preferred.map(|row| row.session_id.as_str())) {
        return Some(dir);
    }
    let row = preferred?;
    session_dir_for_row(home, row)
}

/// When a process still has files open from more than one session, the
/// active-sessions id wins. Otherwise the newest summary mtime wins.
fn pick_open_session_dir(found: &[PathBuf], preferred_id: Option<&str>) -> Option<PathBuf> {
    if let Some(id) = preferred_id {
        if let Some(dir) = found
            .iter()
            .find(|dir| dir.file_name().and_then(|name| name.to_str()) == Some(id))
        {
            return Some(dir.clone());
        }
    }
    found.iter().max_by_key(|dir| dir_mtime(dir)).cloned()
}

fn dir_mtime(dir: &Path) -> std::time::SystemTime {
    fs::metadata(dir.join("summary.json"))
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn session_dir_for_row(home: &Path, row: &ActiveRow) -> Option<PathBuf> {
    let sessions = home.join("sessions");
    let group = encode_cwd_group(&row.cwd);
    if group.len() <= MAX_GROUP_NAME {
        let direct = sessions.join(&group).join(&row.session_id);
        if direct.is_dir() {
            return Some(direct);
        }
    }
    let entries = fs::read_dir(&sessions).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(&row.session_id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

fn read_version(home: &Path) -> String {
    let text = fs::read_to_string(home.join("version.json")).unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("version")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn session_from_dir(
    dir: &Path,
    pid: u32,
    mem_mb: u64,
    children: Vec<ChildProcess>,
    version: &str,
    home: &Path,
) -> Option<AgentSession> {
    let summary: Value = read_json(&dir.join("summary.json"))?;
    if summary.get("session_kind").and_then(|v| v.as_str()) == Some("subagent") {
        return None;
    }
    let cwd = summary
        .pointer("/info/cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let project_name = Path::new(&cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "grok".to_string());
    let usage = read_json(&dir.join("usage.json")).unwrap_or(Value::Null);
    let (input, output, cache_read, cache_create, token_history) = token_fields(&usage);
    let signals = read_json(&dir.join("signals.json")).unwrap_or(Value::Null);
    let (context_percent, context_window, signal_turns, compaction_count) =
        context_from_signals(&signals);
    let turn_count = if signal_turns > 0 {
        signal_turns
    } else {
        json_u64(usage.get("session").unwrap_or(&Value::Null), "turnCount") as u32
    };
    let events_path = dir.join("events.jsonl");
    let updates_path = dir.join("updates.jsonl");
    let age = activity_age_secs(&events_path, &updates_path);
    let activity = interpret_activity(&read_tail(&events_path), &read_tail(&updates_path), age);
    let title = summary
        .get("generated_title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_id = summary
        .pointer("/info/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        return None;
    }

    Some(AgentSession {
        agent_cli: "grok",
        launch_surface: LaunchSurface::Cli,
        pid,
        session_id,
        cwd,
        project_name,
        started_at: parse_time_ms(
            summary
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ),
        status: activity.status,
        model: summary
            .get("current_model_id")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string(),
        effort: summary
            .get("reasoning_effort")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        context_percent,
        total_input_tokens: input,
        total_output_tokens: output,
        total_cache_read: cache_read,
        total_cache_create: cache_create,
        turn_count,
        current_tasks: vec![activity.task],
        mem_mb,
        version: version.to_string(),
        git_branch: summary
            .get("head_branch")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        git_added: 0,
        git_modified: 0,
        token_history,
        context_history: Vec::new(),
        compaction_count,
        context_window,
        subagents: load_subagents(dir),
        mem_file_count: 0,
        mem_line_count: 0,
        children,
        initial_prompt: super::redact_secrets(&truncate_chars(&title, 80)),
        first_assistant_text: String::new(),
        chat_messages: Vec::new(),
        tool_calls: activity.tool_calls,
        pending_since_ms: activity.pending_since_ms,
        thinking_since_ms: activity.thinking_since_ms,
        file_accesses: activity.file_accesses,
        config_root: super::abbrev_path(home),
    })
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn token_fields(usage: &Value) -> (u64, u64, u64, u64, Vec<u64>) {
    let session = usage.get("session").unwrap_or(&Value::Null);
    let input = json_u64(session, "inputTokens");
    let cached = json_u64(session, "cachedReadTokens");
    let output = json_u64(session, "outputTokens");
    let created = json_u64(session, "cacheCreationTokens");
    // Grok's inputTokens already includes cached reads. Subtract so
    // AgentSession::total_tokens(), which adds cache on top, matches
    // usage.session.totalTokens (input + output).
    let uncached = input.saturating_sub(cached);
    let mut history = Vec::new();
    if let Some(turns) = usage.get("turns").and_then(|v| v.as_array()) {
        for turn in turns {
            if history.len() >= 200 {
                break;
            }
            history.push(json_u64(turn, "totalTokens"));
        }
    }
    (uncached, output, cached, created, history)
}

fn context_from_signals(signals: &Value) -> (f64, u64, u32, u32) {
    if signals.is_null() {
        return (0.0, 0, 0, 0);
    }
    let used = json_u64(signals, "contextTokensUsed");
    let window = json_u64(signals, "contextWindowTokens");
    let percent = if window > 0 {
        (used as f64 / window as f64) * 100.0
    } else {
        json_f64(signals, "contextWindowUsage")
    };
    (
        percent,
        window,
        json_u64(signals, "turnCount") as u32,
        json_u64(signals, "compactionCount") as u32,
    )
}

struct Activity {
    status: SessionStatus,
    task: String,
    thinking_since_ms: u64,
    pending_since_ms: u64,
    tool_calls: Vec<ToolCall>,
    file_accesses: Vec<FileAccess>,
}

fn interpret_activity(events: &str, updates: &str, age_secs: u64) -> Activity {
    let mut phase = String::new();
    let mut started = 0u32;
    let mut completed = 0u32;
    let mut perm_req = 0u32;
    let mut perm_res = 0u32;
    let mut last_tool = String::new();
    let mut last_ts = 0u64;
    let mut last_tool_ts = 0u64;
    for value in jsonl_values(events) {
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = parse_time_ms(value.get("ts").and_then(|v| v.as_str()).unwrap_or(""));
        if ts > 0 {
            last_ts = ts;
        }
        match kind {
            "phase_changed" => {
                if let Some(name) = value.get("phase").and_then(|v| v.as_str()) {
                    phase = name.to_string();
                }
            }
            "tool_started" => {
                started += 1;
                last_tool_ts = ts;
                if let Some(name) = value.get("tool_name").and_then(|v| v.as_str()) {
                    last_tool = name.to_string();
                }
            }
            "tool_completed" => completed += 1,
            "permission_requested" => perm_req += 1,
            "permission_resolved" => perm_res += 1,
            _ => {}
        }
    }

    let mut tools: HashMap<String, (usize, ToolCall, Option<FileOp>)> = HashMap::new();
    let mut order = 0usize;
    for value in jsonl_values(updates) {
        let update = value
            .pointer("/params/update")
            .cloned()
            .unwrap_or(Value::Null);
        let kind = update
            .get("sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if kind != "tool_call" && kind != "tool_call_update" {
            continue;
        }
        let id = update
            .get("toolCallId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let meta = update.get("_meta").and_then(|value| value.get("x.ai/tool"));
        let name = meta
            .and_then(|m| m.get("label").and_then(|v| v.as_str()))
            .or_else(|| meta.and_then(|m| m.get("name").and_then(|v| v.as_str())))
            .or_else(|| update.get("title").and_then(|v| v.as_str()))
            .unwrap_or("tool")
            .to_string();
        let path = update
            .pointer("/locations/0/path")
            .and_then(|v| v.as_str())
            .or_else(|| {
                meta.and_then(|m| m.pointer("/input/path"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("")
            .to_string();
        let file_op = meta
            .and_then(|m| m.get("kind").and_then(|v| v.as_str()))
            .and_then(file_op_from_kind);
        let entry = tools.entry(id).or_insert_with(|| {
            let slot = order;
            order += 1;
            (
                slot,
                ToolCall {
                    name: name.clone(),
                    arg: String::new(),
                    duration_ms: 0,
                },
                None,
            )
        });
        entry.1.name = name;
        if !path.is_empty() {
            entry.1.arg = super::redact_secrets(&path);
            if file_op.is_some() {
                entry.2 = file_op;
            }
        }
    }
    let mut indexed: Vec<_> = tools.into_values().collect();
    indexed.sort_by_key(|(slot, _, _)| *slot);
    if indexed.len() > MAX_TOOL_CALLS {
        let skip = indexed.len() - MAX_TOOL_CALLS;
        indexed.drain(0..skip);
    }
    let mut file_accesses = Vec::new();
    for (index, (_, call, op)) in indexed.iter().enumerate() {
        if let Some(operation) = op.clone() {
            if !call.arg.is_empty() {
                file_accesses.push(FileAccess {
                    path: call.arg.clone(),
                    operation,
                    turn_index: index as u32,
                });
            }
        }
    }
    let tool_calls: Vec<ToolCall> = indexed.into_iter().map(|(_, call, _)| call).collect();
    let tool_task = tool_calls.last().map(|call| {
        if call.arg.is_empty() {
            call.name.clone()
        } else {
            format!("{} {}", call.name, call.arg)
        }
    });

    let (status, task, thinking_since_ms, pending_since_ms) = if age_secs > ACTIVITY_FRESH_SECS {
        (
            SessionStatus::Waiting,
            "waiting for user input".to_string(),
            0,
            0,
        )
    } else if phase == "permission_prompt" || perm_req > perm_res {
        (
            SessionStatus::Waiting,
            "waiting for permission".to_string(),
            0,
            0,
        )
    } else if phase == "tool_execution" || started > completed {
        (
            SessionStatus::Executing,
            tool_task.unwrap_or(last_tool),
            0,
            last_tool_ts,
        )
    } else if matches!(
        phase.as_str(),
        "waiting_for_model" | "streaming_reasoning" | "streaming_text"
    ) {
        (
            SessionStatus::Thinking,
            "thinking...".to_string(),
            last_ts,
            0,
        )
    } else {
        (
            SessionStatus::Waiting,
            "waiting for user input".to_string(),
            0,
            0,
        )
    };

    Activity {
        status,
        task,
        thinking_since_ms,
        pending_since_ms,
        tool_calls,
        file_accesses,
    }
}

fn file_op_from_kind(kind: &str) -> Option<FileOp> {
    match kind {
        "read" => Some(FileOp::Read),
        "write" => Some(FileOp::Write),
        "edit" => Some(FileOp::Edit),
        _ => None,
    }
}

fn jsonl_values(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect()
}

fn activity_age_secs(events: &Path, updates: &Path) -> u64 {
    let age = [events, updates]
        .into_iter()
        .filter_map(|path| {
            let modified = fs::metadata(path).ok()?.modified().ok()?;
            modified.elapsed().ok().map(|elapsed| elapsed.as_secs())
        })
        .min();
    age.unwrap_or(u64::MAX)
}

fn read_tail(path: &Path) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return String::new();
    }
    if start > 0 {
        if let Some(index) = buf.find('\n') {
            return buf[index + 1..].to_string();
        }
    }
    buf
}

fn load_subagents(dir: &Path) -> Vec<SubAgent> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir.join("subagents")) else {
        return out;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_SUBAGENTS {
            break;
        }
        let text = fs::read_to_string(entry.path().join("meta.json")).unwrap_or_default();
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let name = value
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("subagent")
            .to_string();
        let status = value
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(SubAgent {
            name,
            status,
            tokens: 0,
        });
    }
    out
}

fn child_processes(shared: &super::SharedProcessData, pid: u32) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack = shared.children_map.get(&pid).cloned().unwrap_or_default();
    let mut visited = std::collections::HashSet::new();
    while let Some(child_pid) = stack.pop() {
        if !visited.insert(child_pid) {
            continue;
        }
        if let Some(info) = shared.process_info.get(&child_pid) {
            let port = shared
                .ports
                .get(&child_pid)
                .and_then(|ports| ports.first().copied());
            children.push(ChildProcess {
                pid: child_pid,
                command: info.command.clone(),
                mem_kb: info.rss_kb,
                port,
            });
        }
        if let Some(grandchildren) = shared.children_map.get(&child_pid) {
            stack.extend(grandchildren);
        }
    }
    children
}

fn json_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn json_f64(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn parse_time_ms(text: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .unwrap_or(0)
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionStatus;

    #[test]
    fn encode_cwd_group_matches_grok_directory_names() {
        assert_eq!(
            encode_cwd_group("/home/tuannh/.grok/worktrees/repos-abtop/grok-support"),
            "%2Fhome%2Ftuannh%2F.grok%2Fworktrees%2Frepos-abtop%2Fgrok-support"
        );
    }

    #[test]
    fn session_dir_from_open_path_stops_at_session_id() {
        let path = Path::new(
            "/home/tuannh/.grok/sessions/%2Fhome%2Fproj/01a14fe3-b31f-73a2-a089-88a42fd5e1d0/events.jsonl",
        );
        let dir = session_dir_from_open_path(path).unwrap();
        assert_eq!(
            dir,
            PathBuf::from(
                "/home/tuannh/.grok/sessions/%2Fhome%2Fproj/01a14fe3-b31f-73a2-a089-88a42fd5e1d0"
            )
        );
    }

    #[test]
    fn open_fds_prefer_the_active_session_id() {
        let old = PathBuf::from("/home/u/.grok/sessions/group/old");
        let new = PathBuf::from("/home/u/.grok/sessions/group/new");
        let picked = pick_open_session_dir(&[old.clone(), new.clone()], Some("new")).unwrap();
        assert_eq!(picked, new);
        let fallback = pick_open_session_dir(std::slice::from_ref(&old), Some("missing")).unwrap();
        assert_eq!(fallback, old);
    }

    #[test]
    fn latest_active_row_wins_for_a_reused_pid() {
        let rows = parse_active_sessions(
            r#"[
                {"session_id":"old","pid":7,"cwd":"/old","opened_at":"2026-10-01T00:00:00Z"},
                {"session_id":"new","pid":7,"cwd":"/new","opened_at":"2026-10-18T00:00:00Z"},
                {"session_id":"other","pid":8,"cwd":"/x","opened_at":"2026-10-19T00:00:00Z"}
            ]"#,
        );
        let row = latest_row(&rows, 7).unwrap();
        assert_eq!(row.session_id, "new");
        assert_eq!(row.cwd, "/new");
    }

    #[test]
    fn usage_cache_is_not_added_twice() {
        let usage = serde_json::json!({
            "session": {
                "inputTokens": 100,
                "outputTokens": 10,
                "cachedReadTokens": 40,
                "cacheCreationTokens": 0,
                "totalTokens": 110
            },
            "turns": [{"totalTokens": 110}]
        });
        let (input, output, cache, created, history) = token_fields(&usage);
        let total = input + output + cache + created;
        assert_eq!(total, 110);
        assert_eq!(cache, 40);
        assert_eq!(history, vec![110]);
    }

    #[test]
    fn tool_execution_keeps_path_and_drops_tool_body() {
        let events = r#"{"ts":"2026-10-18T16:42:31Z","type":"phase_changed","phase":"tool_execution"}
{"ts":"2026-10-18T16:42:31Z","type":"tool_started","tool_name":"read_file"}"#;
        let updates = r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","locations":[{"path":"/tmp/pay.rs"}],"_meta":{"x.ai/tool":{"name":"read_file","label":"Read","kind":"read","input":{"path":"/tmp/pay.rs","content":"SECRET"}}}}}}"#;
        let activity = interpret_activity(events, updates, 1);
        assert_eq!(activity.status, SessionStatus::Executing);
        assert_eq!(activity.task, "Read /tmp/pay.rs");
        assert_eq!(activity.tool_calls.len(), 1);
        assert!(!activity.task.contains("SECRET"));
        assert_eq!(activity.file_accesses[0].operation, FileOp::Read);
    }

    #[test]
    fn stale_events_are_waiting() {
        let events =
            r#"{"ts":"2026-10-18T16:42:31Z","type":"phase_changed","phase":"tool_execution"}"#;
        let activity = interpret_activity(events, "", 120);
        assert_eq!(activity.status, SessionStatus::Waiting);
        assert_eq!(activity.task, "waiting for user input");
    }

    #[test]
    fn subagent_sessions_are_not_top_level_rows() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("summary.json"),
            r#"{"session_kind":"subagent","info":{"id":"child","cwd":"/tmp/proj"},"created_at":"2026-10-18T00:00:00Z"}"#,
        )
        .unwrap();
        let session = session_from_dir(dir.path(), 1, 0, vec![], "", Path::new("/tmp/.grok"));
        assert!(session.is_none());
    }

    #[test]
    fn session_from_dir_maps_summary_usage_and_signals() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("summary.json"),
            r#"{"session_kind":"worktree","info":{"id":"sid-1","cwd":"/work/abtop"},"created_at":"2026-10-18T00:00:00Z","current_model_id":"grok-4.7","generated_title":"Add grok collector","reasoning_effort":"medium","head_branch":"main"}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("usage.json"),
            r#"{"session":{"inputTokens":100,"outputTokens":10,"cachedReadTokens":40,"cacheCreationTokens":0,"turnCount":2}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("signals.json"),
            r#"{"contextTokensUsed":25000,"contextWindowTokens":256000,"turnCount":4,"compactionCount":1}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("subagents").join("child")).unwrap();
        fs::write(
            dir.path().join("subagents").join("child").join("meta.json"),
            r#"{"subagent_type":"explore","status":"completed","prompt":"do not show this"}"#,
        )
        .unwrap();
        let session = session_from_dir(
            dir.path(),
            42,
            10,
            vec![],
            "1.0.62",
            Path::new("/home/u/.grok"),
        )
        .unwrap();
        assert_eq!(session.agent_cli, "grok");
        assert_eq!(session.pid, 42);
        assert_eq!(session.session_id, "sid-1");
        assert_eq!(session.project_name, "abtop");
        assert_eq!(session.model, "grok-4.7");
        assert_eq!(session.effort, "medium");
        assert_eq!(session.initial_prompt, "Add grok collector");
        assert_eq!(session.total_tokens(), 110);
        assert_eq!(session.turn_count, 4);
        assert_eq!(session.compaction_count, 1);
        assert_eq!(session.context_window, 256_000);
        assert!((session.context_percent - 9.765625).abs() < 0.001);
        assert_eq!(session.subagents.len(), 1);
        assert_eq!(session.subagents[0].name, "explore");
        assert!(session.first_assistant_text.is_empty());
    }
}
