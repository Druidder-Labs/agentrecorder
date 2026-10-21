use std::io::Read;
use std::path::Path;

use anyhow::Result;

pub struct Resume {
    pub agent: &'static str,
    pub argv: Vec<String>,
    pub note: &'static str,
}

pub struct Launch {
    pub argv: Vec<String>,
    pub agent_session_id: Option<String>,
}

const CLAUDE_SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "import",
    "install",
    "logs",
    "mcp",
    "plugin",
    "purge",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "ultrareview",
    "update",
];

pub const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Notification",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
const HOOK_TIMEOUT_S: u64 = 5;
const HOOK_SUFFIX: &str = " hook --agent claude";

const CLAUDE_NO_HOOKS: &[&str] = &["-h", "--help", "-v", "--version", "--settings"];

const CLAUDE_NO_INJECT: &[&str] = &[
    "-c",
    "--continue",
    "-r",
    "--resume",
    "--session-id",
    "--fork-session",
    "--from-pr",
    "--teleport",
    "--no-session-persistence",
    "--bg",
    "--cloud",
    "--desktop",
    "--tmux",
    "-h",
    "--help",
    "-v",
    "--version",
];

pub fn agent_index(argv: &[String]) -> Option<usize> {
    let first = argv.first()?;
    if file_name(first) != Some("env") {
        return Some(0);
    }
    let mut i = 1;
    while let Some(a) = argv.get(i) {
        match a.as_str() {
            "-i" | "--ignore-environment" | "-" => i += 1,
            "-u" | "--unset" => i += 2,
            "--" => return (i + 1 < argv.len()).then_some(i + 1),
            s if s.starts_with("--unset=") || (s.starts_with("-u") && s.len() > 2) => i += 1,
            s if s.starts_with('-') => return None,
            s if s.contains('=') => i += 1,
            _ => return Some(i),
        }
    }
    None
}

pub fn program(argv: &[String]) -> Option<&str> {
    file_name(argv.get(agent_index(argv)?)?)
}

fn file_name(p: &str) -> Option<&str> {
    Path::new(p).file_name()?.to_str()
}

pub fn prepare(argv: &[String], hook_program: Option<&str>) -> Result<Launch> {
    let unchanged = |id| Launch { argv: argv.to_vec(), agent_session_id: id };
    let Some(at) = agent_index(argv) else { return Ok(unchanged(None)) };
    if program(argv) != Some("claude") {
        return Ok(unchanged(None));
    }
    let args = &argv[at + 1..];
    let has = |list: &[&str]| {
        args.iter()
            .any(|a| list.contains(&a.split('=').next().unwrap_or(a)) || CLAUDE_SUBCOMMANDS.contains(&a.as_str()))
    };
    let Some(hook_program) = hook_program else { return Ok(unchanged(claude_session_in(args))) };
    let mut inserted = Vec::new();
    let agent_session_id = match claude_session_in(args) {
        Some(id) => Some(id),
        None if !has(CLAUDE_NO_INJECT) => {
            let id = new_uuid()?;
            inserted.extend(["--session-id".to_string(), id.clone()]);
            Some(id)
        }
        None => None,
    };
    if !has(CLAUDE_NO_HOOKS) {
        inserted.extend(["--settings".to_string(), claude_hook_settings(hook_program)]);
    }
    let mut out = argv[..=at].to_vec();
    out.extend(inserted);
    out.extend_from_slice(args);
    Ok(Launch { argv: out, agent_session_id })
}

pub fn claude_hook_settings(hook_program: &str) -> String {
    let command = format!("{}{HOOK_SUFFIX}", shell_quote(hook_program));
    let entry = serde_json::json!([{"hooks": [{"type": "command", "command": command, "timeout": HOOK_TIMEOUT_S}]}]);
    let hooks: serde_json::Map<String, serde_json::Value> =
        CLAUDE_HOOK_EVENTS.iter().map(|e| (e.to_string(), entry.clone())).collect();
    serde_json::json!({ "hooks": hooks }).to_string()
}

fn is_injected_settings(value: &str) -> bool {
    let Ok(serde_json::Value::Object(top)) = serde_json::from_str::<serde_json::Value>(value) else { return false };
    let Some(serde_json::Value::Object(hooks)) = top.get("hooks").filter(|_| top.len() == 1) else { return false };
    !hooks.is_empty()
        && hooks.values().flat_map(|v| v.as_array().into_iter().flatten()).all(|m| {
            m["hooks"]
                .as_array()
                .is_some_and(|hs| hs.iter().all(|h| h["command"].as_str().is_some_and(|c| c.ends_with(HOOK_SUFFIX))))
        })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn resumed_session(argv: &[String]) -> Option<String> {
    let at = agent_index(argv)?;
    if program(argv) != Some("claude") {
        return None;
    }
    let args = &argv[at + 1..];
    if args.iter().any(|a| a == "--fork-session") {
        return None;
    }
    args.iter().enumerate().find_map(|(i, a)| {
        let value = match a.split_once('=') {
            Some(("--resume", v)) => Some(v),
            _ if matches!(a.as_str(), "--resume" | "-r") => args.get(i + 1).map(String::as_str),
            _ => None,
        };
        value.filter(|v| is_uuid(v)).map(str::to_string)
    })
}

fn claude_session_in(args: &[String]) -> Option<String> {
    if args.iter().any(|a| a == "--fork-session") {
        return None;
    }
    for (i, a) in args.iter().enumerate() {
        let value = match a.split_once('=') {
            Some(("--session-id" | "--resume", v)) => Some(v),
            _ if matches!(a.as_str(), "--session-id" | "--resume" | "-r") => args.get(i + 1).map(String::as_str),
            _ => None,
        };
        if let Some(v) = value.filter(|v| is_uuid(v)) {
            return Some(v.to_string());
        }
    }
    None
}

pub fn without_injected(argv: &[String], id: Option<&str>) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let next = argv.get(i + 1);
        let pair = (argv[i] == "--session-id" && next.map(String::as_str) == id && id.is_some())
            || (argv[i] == "--settings" && next.is_some_and(|v| is_injected_settings(v)));
        if pair {
            i += 2;
            continue;
        }
        if id.is_some() && argv[i].strip_prefix("--session-id=") == id {
            i += 1;
            continue;
        }
        out.push(argv[i].clone());
        i += 1;
    }
    out
}

pub fn resume_for(argv: &[String], agent_session_id: Option<&str>) -> Option<Resume> {
    let at = agent_index(argv)?;
    let program = program(argv)?;
    let (agent, args, note): (_, Vec<String>, _) = match (program, agent_session_id) {
        ("claude", Some(id)) => ("Claude Code", vec!["--resume".into(), id.into()], "resumes the recorded session id"),
        ("claude", None) => (
            "Claude Code",
            vec!["--continue".into()],
            "continues the most recent conversation in the working directory",
        ),
        ("codex", _) => (
            "Codex CLI",
            vec!["resume".into(), "--last".into()],
            "resumes the most recent session in the working directory",
        ),
        ("gemini", _) => ("Gemini CLI", vec!["--resume".into()], "resumes the most recent session for the project"),
        ("opencode", _) => ("OpenCode", vec!["--continue".into()], "continues the last session"),
        _ => return None,
    };
    let mut out = argv[..=at].to_vec();
    out.extend(args);
    Some(Resume { agent, argv: out, note })
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn new_uuid() -> Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOOK: &str = "/usr/local/bin/agentrecorder";

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn maps_known_agents_and_keeps_the_binary_path() {
        let r = resume_for(&v(&["/opt/bin/codex", "--model", "x"]), None).unwrap();
        assert_eq!(r.argv, v(&["/opt/bin/codex", "resume", "--last"]));
        assert_eq!(resume_for(&v(&["claude"]), None).unwrap().argv, v(&["claude", "--continue"]));
        assert!(resume_for(&v(&["bash"]), None).is_none());
        assert!(resume_for(&[], None).is_none());
    }

    #[test]
    fn resume_is_exact_when_the_session_id_is_known_and_keeps_the_env_prefix() {
        let id = "4b41da33-24de-41bb-92ef-77589bb77d13";
        let argv = v(&["env", "-u", "CLAUDECODE", "A=1", "claude", "--session-id", id, "--model", "haiku"]);
        let r = resume_for(&argv, Some(id)).unwrap();
        assert_eq!(r.argv, v(&["env", "-u", "CLAUDECODE", "A=1", "claude", "--resume", id]));
    }

    #[test]
    fn finds_the_program_behind_env() {
        assert_eq!(program(&v(&["/usr/bin/env", "-i", "--unset=X", "-uY", "K=v", "claude", "-p"])), Some("claude"));
        assert_eq!(program(&v(&["env", "--", "codex"])), Some("codex"));
        assert_eq!(program(&v(&["env", "-S", "claude"])), None);
        assert_eq!(program(&v(&["env", "A=1"])), None);
        assert_eq!(program(&v(&["bash", "-c", "claude"])), Some("bash"));
    }

    #[test]
    fn injects_a_uuid_right_after_claude() {
        let l = prepare(&v(&["env", "A=1", "claude", "--model", "haiku", "fix it"]), Some(HOOK)).unwrap();
        let id = l.agent_session_id.unwrap();
        assert!(is_uuid(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        let settings = claude_hook_settings(HOOK);
        assert_eq!(
            l.argv,
            v(&["env", "A=1", "claude", "--session-id", &id, "--settings", &settings, "--model", "haiku", "fix it"])
        );
        let other = prepare(&v(&["claude"]), Some(HOOK)).unwrap().agent_session_id.unwrap();
        assert_ne!(id, other);
    }

    #[test]
    fn leaves_other_programs_subcommands_and_session_flags_alone() {
        for argv in [
            v(&["bash"]),
            v(&["codex"]),
            v(&["gemini"]),
            v(&["claude", "mcp", "list"]),
            v(&["claude", "--version"]),
            v(&["claude", "-h"]),
            v(&["claude", "--settings", "{}", "--continue"]),
        ] {
            let l = prepare(&argv, Some(HOOK)).unwrap();
            assert_eq!(l.argv, argv);
            assert_eq!(l.agent_session_id, None, "{argv:?}");
        }
        for argv in [
            v(&["claude", "--continue"]),
            v(&["claude", "-r"]),
            v(&["claude", "--resume", "my-session-name"]),
            v(&["claude", "--resume", "4b41da33-24de-41bb-92ef-77589bb77d13", "--fork-session"]),
            v(&["claude", "--no-session-persistence", "-p", "hi"]),
            v(&["claude", "--from-pr=12"]),
        ] {
            let l = prepare(&argv, Some(HOOK)).unwrap();
            let mut want = vec![argv[0].clone(), "--settings".into(), claude_hook_settings(HOOK)];
            want.extend_from_slice(&argv[1..]);
            assert_eq!(l.argv, want, "hooks only");
            assert_eq!(l.agent_session_id, None, "{argv:?}");
        }
        let l = prepare(&v(&["claude", "--model", "haiku"]), None).unwrap();
        assert_eq!(l.argv, v(&["claude", "--model", "haiku"]));
        assert_eq!(l.agent_session_id, None);
    }

    #[test]
    fn records_a_session_id_the_user_passed() {
        let id = "4b41da33-24de-41bb-92ef-77589bb77d13";
        for argv in [
            v(&["claude", "--session-id", id]),
            v(&["claude", &format!("--session-id={id}")]),
            v(&["claude", "-r", id]),
            v(&["claude", "--resume", id]),
        ] {
            let l = prepare(&argv, None).unwrap();
            assert_eq!(l.argv, argv);
            assert_eq!(l.agent_session_id.as_deref(), Some(id), "{argv:?}");
            let hooked = prepare(&argv, Some(HOOK)).unwrap();
            assert_eq!(hooked.agent_session_id.as_deref(), Some(id), "{argv:?}");
            assert!(!hooked.argv.contains(&"--session-id".to_string()) || argv.contains(&"--session-id".to_string()));
        }
    }

    #[test]
    fn finds_the_session_a_claude_command_resumes() {
        let id = "4b41da33-24de-41bb-92ef-77589bb77d13";
        assert_eq!(resumed_session(&v(&["env", "A=1", "claude", "--resume", id])).as_deref(), Some(id));
        assert_eq!(resumed_session(&v(&["claude", "-r", id, "--model", "x"])).as_deref(), Some(id));
        assert_eq!(resumed_session(&v(&["claude", &format!("--resume={id}")])).as_deref(), Some(id));
        assert_eq!(resumed_session(&v(&["claude", "--session-id", id])), None);
        assert_eq!(resumed_session(&v(&["claude", "--resume", id, "--fork-session"])), None);
        assert_eq!(resumed_session(&v(&["claude", "--resume", "name"])), None);
        assert_eq!(resumed_session(&v(&["codex", "resume", id])), None);
    }

    #[test]
    fn removes_only_what_was_injected() {
        let id = "4b41da33-24de-41bb-92ef-77589bb77d13";
        let ours = claude_hook_settings("/other/path/agentrecorder");
        let argv = v(&["claude", "--session-id", id, "--settings", &ours, "--model", "haiku"]);
        assert_eq!(without_injected(&argv, Some(id)), v(&["claude", "--model", "haiku"]));
        let eq = v(&["claude", &format!("--session-id={id}"), "-p", "x"]);
        assert_eq!(without_injected(&eq, Some(id)), v(&["claude", "-p", "x"]));
        let other = v(&["claude", "--session-id", "00000000-0000-4000-8000-000000000000"]);
        assert_eq!(without_injected(&other, Some(id)), other);
        let users =
            v(&["claude", "--settings", r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#]);
        assert_eq!(without_injected(&users, None), users);
        let file = v(&["claude", "--settings", "./settings.json"]);
        assert_eq!(without_injected(&file, None), file);
    }

    #[test]
    fn hook_settings_quote_the_program_and_cover_every_event() {
        let s: serde_json::Value = serde_json::from_str(&claude_hook_settings("/a b/it's/agentrecorder")).unwrap();
        for e in CLAUDE_HOOK_EVENTS {
            let h = &s["hooks"][e][0]["hooks"][0];
            assert_eq!(h["type"], "command");
            assert_eq!(h["timeout"], HOOK_TIMEOUT_S);
            assert_eq!(h["command"], r"'/a b/it'\''s/agentrecorder' hook --agent claude");
        }
    }
}
