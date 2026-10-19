use serde_json::Value;
use std::fs;
use std::path::PathBuf;

const STATUSLINE_SCRIPT: &str = r#"#!/bin/bash
# abtop StatusLine hook — writes rate limit data for abtop to read.
# Installed by: abtop --setup
# Reads JSON from stdin with a 5s timeout, pipes it to python via stdin
# to avoid ARG_MAX limits on large payloads.
INPUT=""
while IFS= read -r -t 5 line || [ -n "$line" ]; do
    INPUT="${INPUT}${line}
"
done
[ -z "$INPUT" ] && exit 0
printf '%s' "$INPUT" | python3 -c "
import sys, json, time, os
data = json.load(sys.stdin)
rl = data.get('rate_limits')
if not rl:
    sys.exit(0)
out = {'source': 'claude', 'updated_at': int(time.time())}
fh = rl.get('five_hour')
if fh:
    out['five_hour'] = {'used_percentage': fh.get('used_percentage', 0), 'resets_at': fh.get('resets_at', 0)}
sd = rl.get('seven_day')
if sd:
    out['seven_day'] = {'used_percentage': sd.get('used_percentage', 0), 'resets_at': sd.get('resets_at', 0)}
config_dir = os.environ.get('CLAUDE_CONFIG_DIR', os.path.join(os.path.expanduser('~'), '.claude'))
with open(os.path.join(config_dir, 'abtop-rate-limits.json'), 'w') as f:
    json.dump(out, f)
" 2>/dev/null
"#;

const AGY_STATUSLINE_SCRIPT: &str = r#"#!/bin/bash
# abtop StatusLine hook for Google Antigravity — writes rate limit data for abtop to read.
# Installed by: abtop --setup
# Reads JSON from stdin, writes ~/.gemini/antigravity-cli/abtop-rate-limits.json,
# and chains to any previous user statusline script if present.
INPUT=$(cat 2>/dev/null || true)
if [ -n "$INPUT" ]; then
    printf '%s' "$INPUT" | python3 -c "
import sys, json, time, os
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
q = data.get('quota')
if not isinstance(q, dict):
    sys.exit(0)
now = int(time.time())
out = {'source': 'antigravity', 'updated_at': now}
fh = q.get('gemini-5h') or q.get('3p-5h')
if isinstance(fh, dict) and fh.get('remaining_fraction') is not None:
    rem = float(fh['remaining_fraction'])
    used = max(0.0, min(100.0, round((1.0 - rem) * 100.0, 1)))
    rst = int(fh.get('reset_in_seconds') or 0)
    out['five_hour'] = {'used_percentage': used, 'resets_at': (now + rst) if rst > 0 else 0, 'window_minutes': 300}
wk = q.get('gemini-weekly') or q.get('3p-weekly')
if isinstance(wk, dict) and wk.get('remaining_fraction') is not None:
    rem = float(wk['remaining_fraction'])
    used = max(0.0, min(100.0, round((1.0 - rem) * 100.0, 1)))
    rst = int(wk.get('reset_in_seconds') or 0)
    out['seven_day'] = {'used_percentage': used, 'resets_at': (now + rst) if rst > 0 else 0, 'window_minutes': 10080}
if 'five_hour' in out or 'seven_day' in out:
    agy_dir = os.path.join(os.path.expanduser('~'), '.gemini', 'antigravity-cli')
    tmp = os.path.join(agy_dir, 'abtop-rate-limits.json.tmp')
    dst = os.path.join(agy_dir, 'abtop-rate-limits.json')
    with open(tmp, 'w') as f:
        json.dump(out, f)
    os.replace(tmp, dst)
" 2>/dev/null &
fi
CHAIN_FILE="$HOME/.gemini/antigravity-cli/abtop-chained-statusline.txt"
if [ -f "$CHAIN_FILE" ]; then
    CHAIN_CMD=$(cat "$CHAIN_FILE" 2>/dev/null || true)
    if [ -n "$CHAIN_CMD" ] && [ -x "$CHAIN_CMD" ]; then
        printf '%s' "$INPUT" | "$CHAIN_CMD" "$@"
    fi
fi
"#;

fn claude_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
}

fn script_path() -> PathBuf {
    claude_dir().join("abtop-statusline.sh")
}

fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

fn agy_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("antigravity-cli")
}

pub fn run_setup() {
    println!("abtop --setup: configuring StatusLine hooks\n");

    // Ensure ~/.claude directory exists
    let dir = claude_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  ✗ failed to create {}: {}", dir.display(), e);
        std::process::exit(1);
    }

    // Step 1: Write the statusline script
    let script = script_path();
    match fs::write(&script, STATUSLINE_SCRIPT) {
        Ok(_) => {
            // chmod +x
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
            }
            println!("  ✓ wrote {}", script.display());
        }
        Err(e) => {
            eprintln!("  ✗ failed to write {}: {}", script.display(), e);
            std::process::exit(1);
        }
    }

    // Step 2: Update settings.json
    let settings_file = settings_path();
    let mut settings: Value = if settings_file.exists() {
        let content = match fs::read_to_string(&settings_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ cannot read {}: {}", settings_file.display(), e);
                std::process::exit(1);
            }
        };
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "  ✗ {} contains invalid JSON: {}",
                    settings_file.display(),
                    e
                );
                eprintln!("    fix the file manually before running --setup");
                std::process::exit(1);
            }
        }
    } else {
        Value::Object(Default::default())
    };

    let obj = settings.as_object_mut().unwrap();

    // Check if statusLine is already configured
    let expected_cmd = script.display().to_string();
    if let Some(existing) = obj.get("statusLine") {
        if let Some(existing_obj) = existing.as_object() {
            if let Some(cmd) = existing_obj.get("command") {
                let cmd_str = cmd.as_str().unwrap_or("");
                if cmd_str != expected_cmd && !cmd_str.is_empty() {
                    eprintln!("  ⚠ statusLine already configured: {}", cmd_str);
                    eprintln!("    to override, remove the existing statusLine key from:");
                    eprintln!("    {}", settings_file.display());
                    std::process::exit(1);
                }
            }
        }
    }

    // Set statusLine config
    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": script.display().to_string()
        }),
    );

    match fs::write(
        &settings_file,
        serde_json::to_string_pretty(&settings).unwrap_or_default(),
    ) {
        Ok(_) => println!("  ✓ updated {}", settings_file.display()),
        Err(e) => {
            eprintln!("  ✗ failed to update {}: {}", settings_file.display(), e);
            std::process::exit(1);
        }
    }

    // Step 3: Also configure Antigravity CLI if ~/.gemini/antigravity-cli exists
    let agy = agy_dir();
    if agy.is_dir() {
        setup_antigravity(&agy);
    }

    println!("\n  done! rate limit data will appear in abtop after the next agent response.");
    println!("  restart any running Claude Code / Antigravity sessions to activate.");
}

fn setup_antigravity(agy: &std::path::Path) {
    let script = agy.join("abtop-statusline.sh");
    if let Err(e) = fs::write(&script, AGY_STATUSLINE_SCRIPT) {
        eprintln!("  ✗ failed to write {}: {}", script.display(), e);
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
    }
    println!("  ✓ wrote {}", script.display());

    let settings_file = agy.join("settings.json");
    let mut settings: Value = if settings_file.exists() {
        fs::read_to_string(&settings_file)
            .ok()
            .and_then(|c| serde_json::from_str(content_or_empty(&c)).ok())
            .unwrap_or_else(|| Value::Object(Default::default()))
    } else {
        Value::Object(Default::default())
    };

    let Some(obj) = settings.as_object_mut() else {
        return;
    };

    let expected_cmd = script.display().to_string();
    if let Some(existing_cmd) = obj
        .get("statusLine")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("command"))
        .and_then(|c| c.as_str())
    {
        if !existing_cmd.is_empty() && existing_cmd != expected_cmd {
            // Preserve the user's existing statusline script by chaining to it
            let chain_file = agy.join("abtop-chained-statusline.txt");
            let _ = fs::write(&chain_file, existing_cmd);
            println!(
                "  ✓ chained existing Antigravity statusLine ({})",
                existing_cmd
            );
        }
    }

    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": expected_cmd,
            "enabled": true
        }),
    );

    if let Ok(pretty) = serde_json::to_string_pretty(&settings) {
        if fs::write(&settings_file, pretty).is_ok() {
            println!("  ✓ updated {}", settings_file.display());
        }
    }
}

fn content_or_empty(s: &str) -> &str {
    if s.trim().is_empty() {
        "{}"
    } else {
        s
    }
}
