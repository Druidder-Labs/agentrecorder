use std::io::Read;

use agentrecorder_store::AgentHook;
use agentrecorder_supervisor::{Layout, client};
use serde_json::Value;

const MAX_INPUT: u64 = 1 << 20;
const MAX_TEXT: usize = 1024;
const MAX_PATH: usize = 4096;

#[derive(clap::Args)]
pub struct HookArgs {
    #[arg(long, default_value = "claude", help = "Agent that runs this hook: claude, codex, gemini or opencode")]
    pub agent: String,
}

pub fn hook(args: HookArgs) -> i32 {
    if let Err(e) = forward(&args) {
        eprintln!("agentrecorder hook: {e:#}");
    }
    0
}

fn forward(args: &HookArgs) -> anyhow::Result<()> {
    let Some(id) = std::env::var("AGENTRECORDER_SESSION").ok().filter(|v| is_session_id(v)) else {
        return Ok(());
    };
    let mut input = Vec::new();
    std::io::stdin().take(MAX_INPUT).read_to_end(&mut input)?;
    let event = parse(&args.agent, &input)?;
    let layout = Layout::from_env()?;
    client::agent_hook(&layout, &id, event)?;
    Ok(())
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
    })
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
