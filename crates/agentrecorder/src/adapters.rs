use std::path::Path;

pub struct Resume {
    pub agent: &'static str,
    pub argv: Vec<String>,
    pub note: &'static str,
}

pub fn resume_for(argv: &[String]) -> Option<Resume> {
    let program = Path::new(argv.first()?).file_name()?.to_str()?;
    let bin = argv[0].clone();
    let (agent, args, note): (_, &[&str], _) = match program {
        "claude" => ("Claude Code", &["--continue"], "continues the most recent conversation in the working directory"),
        "codex" => ("Codex CLI", &["resume", "--last"], "resumes the most recent session in the working directory"),
        "gemini" => ("Gemini CLI", &["--resume"], "resumes the most recent session for the project"),
        "opencode" => ("OpenCode", &["--continue"], "continues the last session"),
        _ => return None,
    };
    let mut argv = vec![bin];
    argv.extend(args.iter().map(|s| s.to_string()));
    Some(Resume { agent, argv, note })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_agents_and_keeps_the_binary_path() {
        let r = resume_for(&["/opt/bin/codex".into(), "--model".into(), "x".into()]).unwrap();
        assert_eq!(r.argv, vec!["/opt/bin/codex", "resume", "--last"]);
        assert_eq!(resume_for(&["claude".into()]).unwrap().argv, vec!["claude", "--continue"]);
        assert!(resume_for(&["bash".into()]).is_none());
        assert!(resume_for(&[]).is_none());
    }
}
