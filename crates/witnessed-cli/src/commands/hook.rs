use std::io::Read;

use std::path::Path;

use serde_json::Value;
use witnessed_store::{AgentHook, HookUsage, Store, ToolSummary, now_unix_ns};
use witnessed_supervisor::{Layout, client};

const MAX_INPUT: u64 = 1 << 20;
const MAX_TEXT: usize = 1024;
const MAX_PATH: usize = 4096;
const MAX_COMMAND: usize = 500;
const MAX_ERROR: usize = 300;

#[derive(clap::Args)]
pub struct HookArgs {
    #[arg(long, default_value = "claude", help = "Agent that runs this hook: claude, codex, gemini or opencode")]
    pub agent: String,
}

pub fn hook(args: HookArgs) -> i32 {
    if let Err(e) = forward(&args) {
        eprintln!("witnessed hook: {e:#}");
    }
    0
}

fn forward(args: &HookArgs) -> anyhow::Result<()> {
    let Some(id) = std::env::var("WITNESSED_SESSION").ok().filter(|v| is_session_id(v)) else {
        return Ok(());
    };
    let mut input = Vec::new();
    std::io::stdin().take(MAX_INPUT).read_to_end(&mut input)?;
    let mut event = parse(&args.agent, &input)?;
    let layout = Layout::from_env()?;
    if SUMMARIZE_AT.contains(&event.hook_event_name.as_str())
        && let Some(path) = event.transcript_path.as_deref()
    {
        event.usage = summarize(&layout, &id, Path::new(path));
    }
    client::agent_hook(&layout, &id, event)?;
    Ok(())
}

const SUMMARIZE_AT: &[&str] = &["Stop", "SessionEnd"];

fn summarize(layout: &Layout, id: &str, transcript: &Path) -> Option<HookUsage> {
    let from_ns = Store::open(&layout.db_path()).ok()?.session(id).ok()??.created_at_ns;
    let u = crate::transcript::usage(transcript, from_ns, now_unix_ns()).ok()?;
    Some(HookUsage {
        api_calls: u.api_calls,
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_creation: u.cache_creation,
        models: u.models,
        outside_session: u.outside_session,
    })
}

fn is_session_id(v: &str) -> bool {
    v.len() == 6 && v.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn parse(agent: &str, input: &[u8]) -> anyhow::Result<AgentHook> {
    let v: Value = serde_json::from_slice(input)?;
    let text = |k: &str, max: usize| v.get(k).and_then(Value::as_str).map(|s| truncate(s, max));
    let Some(name) = text("hook_event_name", 64) else { anyhow::bail!("hook input has no hook_event_name") };
    Ok(AgentHook {
        agent: truncate(agent, 32),
        hook_event_name: name,
        session_id: text("session_id", 128),
        transcript_path: text("transcript_path", MAX_PATH),
        cwd: text("cwd", MAX_PATH),
        source: text("source", 64),
        reason: text("reason", 64),
        model: text("model", 128),
        permission_mode: text("permission_mode", 64),
        notification_type: text("notification_type", 64),
        message: text("message", MAX_TEXT),
        tool_name: text("tool_name", 128),
        usage: None,
        tool: tool_summary(&v),
    })
}

fn lines(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_str).map(|t| if t.is_empty() { 0 } else { t.lines().count() as u64 })
}

fn tool_summary(v: &Value) -> Option<ToolSummary> {
    let name = v.get("tool_name")?.as_str()?;
    let input = v.get("tool_input").filter(|i| i.is_object());
    let field = |k: &str| input.and_then(|i| i.get(k));
    let text = |k: &str, max: usize| field(k).and_then(Value::as_str).map(|s| truncate(s, max));
    let command = field("command").and_then(Value::as_str).map(|c| {
        let mut lines = c.lines();
        let first = truncate(lines.next().unwrap_or(""), MAX_COMMAND);
        match lines.count() {
            0 => first,
            n => format!("{first} [+{n} more lines not recorded]"),
        }
    });
    let mut t = ToolSummary {
        command,
        description: text("description", 200),
        path: text("file_path", MAX_PATH)
            .or_else(|| text("notebook_path", MAX_PATH))
            .or_else(|| text("path", MAX_PATH)),
        pattern: text("pattern", 200),
        url: text("url", 300),
        subagent: text("subagent_type", 64),
        error: v.get("error").and_then(Value::as_str).map(|e| truncate(e, MAX_ERROR)),
        interrupted: v["tool_response"]["interrupted"].as_bool().unwrap_or(false),
        ..ToolSummary::default()
    };
    match name {
        "Edit" => {
            t.lines_removed = lines(field("old_string"));
            t.lines_added = lines(field("new_string"));
        }
        "MultiEdit" => {
            let edits = field("edits").and_then(Value::as_array).cloned().unwrap_or_default();
            t.lines_removed = Some(edits.iter().filter_map(|e| lines(e.get("old_string"))).sum());
            t.lines_added = Some(edits.iter().filter_map(|e| lines(e.get("new_string"))).sum());
        }
        "Write" => t.lines_added = lines(field("content")),
        "NotebookEdit" => t.lines_added = lines(field("new_source")),
        _ => {}
    }
    (t != ToolSummary::default()).then_some(t)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_metadata_and_never_the_prompt_tool_input_or_reply() {
        let input = br#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"my secret",
            "tool_input":{"content":"secret"},"last_assistant_message":"secret","permission_mode":"default"}"#;
        let e = parse("claude", input).unwrap();
        assert_eq!(e.hook_event_name, "UserPromptSubmit");
        assert_eq!(e.session_id.as_deref(), Some("s1"));
        assert_eq!(e.permission_mode.as_deref(), Some("default"));
        let stored = serde_json::to_string(&e).unwrap();
        assert!(!stored.contains("secret"), "{stored}");
    }

    #[test]
    fn summarizes_tool_input_without_file_contents() {
        let edit = br#"{"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{"file_path":"/w/a.py",
            "old_string":"old-secret\nx","new_string":"new-secret\ny\nz"}}"#;
        let e = parse("claude", edit).unwrap();
        let t = e.tool.clone().unwrap();
        assert_eq!((t.path.as_deref(), t.lines_removed, t.lines_added), (Some("/w/a.py"), Some(2), Some(3)));
        let stored = serde_json::to_string(&e).unwrap();
        assert!(!stored.contains("secret"), "{stored}");
        let multi = br#"{"hook_event_name":"PostToolUse","tool_name":"MultiEdit","tool_input":{"file_path":"b.rs",
            "edits":[{"old_string":"a","new_string":"b\nc"},{"old_string":"d\ne","new_string":""}]}}"#;
        let t = parse("claude", multi).unwrap().tool.unwrap();
        assert_eq!((t.lines_removed, t.lines_added), (Some(3), Some(2)));
        let write = br#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":"c.md","content":"s1\ns2"}}"#;
        let e = parse("claude", write).unwrap();
        assert_eq!(e.tool.as_ref().unwrap().lines_added, Some(2));
        assert!(!serde_json::to_string(&e).unwrap().contains("s1"));
        let heredoc = br#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"cat > f.py <<'EOF'\nbody-secret\nEOF"}}"#;
        let e = parse("claude", heredoc).unwrap();
        assert_eq!(
            e.tool.as_ref().unwrap().command.as_deref(),
            Some("cat > f.py <<'EOF' [+2 more lines not recorded]")
        );
        assert!(!serde_json::to_string(&e).unwrap().contains("body-secret"));
        let bash = br#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","error":"exit 1",
            "tool_input":{"command":"python3 -m unittest","description":"Run tests"}}"#;
        let t = parse("claude", bash).unwrap().tool.unwrap();
        assert_eq!(
            (t.command.as_deref(), t.description.as_deref(), t.error.as_deref()),
            (Some("python3 -m unittest"), Some("Run tests"), Some("exit 1"))
        );
        let task = br#"{"hook_event_name":"PostToolUse","tool_name":"Task","tool_input":{"subagent_type":"Explore","prompt":"p-secret"},
            "tool_response":{"interrupted":true}}"#;
        let e = parse("claude", task).unwrap();
        assert_eq!(e.tool.as_ref().map(|t| (t.subagent.as_deref(), t.interrupted)), Some((Some("Explore"), true)));
        assert!(!serde_json::to_string(&e).unwrap().contains("p-secret"));
        assert!(parse("claude", br#"{"hook_event_name":"Stop"}"#).unwrap().tool.is_none());
    }

    #[test]
    fn bounds_long_fields_on_a_char_boundary() {
        let msg = "é".repeat(MAX_TEXT);
        let input = serde_json::json!({"hook_event_name": "Notification", "message": msg}).to_string();
        let e = parse("claude", input.as_bytes()).unwrap();
        let m = e.message.unwrap();
        assert!(m.len() <= MAX_TEXT && m.chars().all(|c| c == 'é'));
    }

    #[test]
    fn rejects_input_without_an_event_name_and_bad_session_ids() {
        assert!(parse("claude", b"{}").is_err());
        assert!(parse("claude", b"not json").is_err());
        assert!(is_session_id("3f9a1c"));
        for bad in ["", "3F9A1C", "../../x", "3f9a1c0"] {
            assert!(!is_session_id(bad), "{bad}");
        }
    }
}
