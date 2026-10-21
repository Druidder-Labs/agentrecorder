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

pub fn prepare(argv: &[String], inject: bool) -> Result<Launch> {
    let unchanged = |id| Launch { argv: argv.to_vec(), agent_session_id: id };
    let Some(at) = agent_index(argv) else { return Ok(unchanged(None)) };
    if program(argv) != Some("claude") {
        return Ok(unchanged(None));
    }
    let args = &argv[at + 1..];
    if let Some(id) = claude_session_in(args) {
        return Ok(unchanged(Some(id)));
    }
    let blocked = args.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        CLAUDE_NO_INJECT.contains(&flag) || CLAUDE_SUBCOMMANDS.contains(&a.as_str())
    });
    if !inject || blocked {
        return Ok(unchanged(None));
    }
    let id = new_uuid()?;
    let mut out = argv[..=at].to_vec();
    out.extend(["--session-id".to_string(), id.clone()]);
    out.extend_from_slice(args);
    Ok(Launch { argv: out, agent_session_id: Some(id) })
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

pub fn without_session_id(argv: &[String], id: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--session-id" && argv.get(i + 1).is_some_and(|v| v == id) {
            i += 2;
            continue;
        }
        if argv[i].strip_prefix("--session-id=") == Some(id) {
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
        let l = prepare(&v(&["env", "A=1", "claude", "--model", "haiku", "fix it"]), true).unwrap();
        let id = l.agent_session_id.unwrap();
        assert!(is_uuid(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        assert_eq!(l.argv, v(&["env", "A=1", "claude", "--session-id", &id, "--model", "haiku", "fix it"]));
        let other = prepare(&v(&["claude"]), true).unwrap().agent_session_id.unwrap();
        assert_ne!(id, other);
    }

    #[test]
    fn leaves_other_programs_subcommands_and_session_flags_alone() {
        for argv in [
            v(&["bash"]),
            v(&["codex"]),
            v(&["gemini"]),
            v(&["claude", "mcp", "list"]),
            v(&["claude", "--continue"]),
            v(&["claude", "-r"]),
            v(&["claude", "--resume", "my-session-name"]),
            v(&["claude", "--resume", "4b41da33-24de-41bb-92ef-77589bb77d13", "--fork-session"]),
            v(&["claude", "--version"]),
            v(&["claude", "--no-session-persistence", "-p", "hi"]),
            v(&["claude", "--from-pr=12"]),
        ] {
            let l = prepare(&argv, true).unwrap();
            assert_eq!(l.argv, argv);
            assert_eq!(l.agent_session_id, None, "{argv:?}");
        }
        let l = prepare(&v(&["claude", "--model", "haiku"]), false).unwrap();
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
            let l = prepare(&argv, true).unwrap();
            assert_eq!(l.argv, argv);
            assert_eq!(l.agent_session_id.as_deref(), Some(id), "{argv:?}");
        }
    }

    #[test]
    fn removes_only_the_matching_session_id() {
        let id = "4b41da33-24de-41bb-92ef-77589bb77d13";
        let argv = v(&["claude", "--session-id", id, "--model", "haiku"]);
        assert_eq!(without_session_id(&argv, id), v(&["claude", "--model", "haiku"]));
        let eq = v(&["claude", &format!("--session-id={id}"), "-p", "x"]);
        assert_eq!(without_session_id(&eq, id), v(&["claude", "-p", "x"]));
        let other = v(&["claude", "--session-id", "00000000-0000-4000-8000-000000000000"]);
        assert_eq!(without_session_id(&other, id), other);
    }
}
