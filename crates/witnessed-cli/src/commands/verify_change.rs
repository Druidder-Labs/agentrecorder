use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use witnessed_assurance::engine::{self, EvidenceFile, Inputs, SessionEvidence, evidence_id};
use witnessed_assurance::policy::Policy;
use witnessed_assurance::{EXIT_ERROR, git, report};

use super::verify::{Source, VerifyArgs, check};

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Text,
    Json,
    Markdown,
    Intoto,
}

#[derive(clap::Args)]
pub struct VerifyChangeArgs {
    #[arg(long, default_value = ".", help = "Git repository that holds the commit")]
    pub repo: PathBuf,
    #[arg(long, help = "Commit to verify; resolved to its full id, and every piece of evidence must name it")]
    pub commit: String,
    #[arg(long, help = "Base the change is compared with, such as the pull request base commit or origin/main")]
    pub base: String,
    #[arg(
        long,
        env = "GITHUB_REPOSITORY",
        default_value = "",
        help = "Repository as owner/name; a pull request snapshot for another repository fails"
    )]
    pub repository: String,
    #[arg(long, help = "Policy directory: .rego modules and keys/NAME.pub.pem trusted keys")]
    pub policy: String,
    #[arg(
        long,
        value_name = "REF",
        help = "Read --policy as a path inside the repository at REF (the base branch), not from the work tree"
    )]
    pub policy_ref: Option<String>,
    #[arg(long = "attestation", value_name = "FILE|DIR", help = "DSSE envelope, or a directory of them; repeatable")]
    pub attestations: Vec<PathBuf>,
    #[arg(long, value_name = "FILE", help = "Pull request snapshot written by `witnessed collect github-pr`")]
    pub pull_request: Option<PathBuf>,
    #[arg(long, value_name = "BUNDLE", help = "Session bundle (directory or zip) written by `witnessed bundle`")]
    pub session: Option<PathBuf>,
    #[arg(long, value_name = "FILE", requires = "session", help = "Anchor lines received while the session ran")]
    pub session_anchors: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "text", help = "Report format on stdout or in --output")]
    pub format: Format,
    #[arg(long, short, value_name = "FILE", help = "Write the report to FILE instead of stdout")]
    pub output: Option<PathBuf>,
    #[arg(long, value_name = "FILE", help = "Append the Markdown report to FILE, such as $GITHUB_STEP_SUMMARY")]
    pub summary: Option<PathBuf>,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn verify_change(args: &VerifyChangeArgs) -> Result<i32> {
    match run(args) {
        Ok(code) => Ok(code),
        Err(e) => {
            let message = format!("{e:#}");
            eprintln!("witnessed: verify-change could not decide: {message}");
            eprintln!("witnessed: decision ERROR (exit {EXIT_ERROR}); this is not an approval");
            if args.format == Format::Json {
                let body = report::error_json(Some(&args.commit), VERSION, &message);
                let text = serde_json::to_string_pretty(&body)? + "\n";
                emit(args.output.as_deref(), &text)?;
            }
            if let Some(path) = &args.summary {
                append(path, &report::error_markdown(&args.commit, VERSION, &message))?;
            }
            Ok(EXIT_ERROR)
        }
    }
}

fn emit(output: Option<&Path>, text: &str) -> Result<()> {
    match output {
        Some(path) => fs::write(path, text).with_context(|| format!("writing {}", path.display())),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

const MAX_EVIDENCE_BYTES: u64 = 16 << 20;

fn read(path: &Path) -> Result<EvidenceFile> {
    let len = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.len();
    if len > MAX_EVIDENCE_BYTES {
        bail!("{} is {len} bytes; evidence files are limited to {MAX_EVIDENCE_BYTES}", path.display());
    }
    Ok(EvidenceFile {
        label: path.display().to_string(),
        bytes: fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    })
}

fn append(path: &Path, text: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

fn attestation_files(paths: &[PathBuf]) -> Result<Vec<EvidenceFile>> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut entries: Vec<PathBuf> = fs::read_dir(p)
                .with_context(|| format!("reading {}", p.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|e| e.is_file())
                .collect();
            entries.sort();
            for e in entries {
                out.push(read(&e)?);
            }
        } else {
            out.push(read(p)?);
        }
    }
    Ok(out)
}

fn run(args: &VerifyChangeArgs) -> Result<i32> {
    if args.pull_request.is_some() && args.repository.is_empty() {
        bail!("--pull-request needs --repository (or GITHUB_REPOSITORY) to check which repository the snapshot is for");
    }
    let change = git::change_set(&args.repo, &args.commit, &args.base).map_err(|e| anyhow!("{e}"))?;
    let policy = match &args.policy_ref {
        Some(rev) => Policy::from_git(&args.repo, rev, &args.policy).map_err(|e| anyhow!("{e}"))?,
        None => {
            let mut p = Policy::from_dir(Path::new(&args.policy)).map_err(|e| anyhow!("{e}"))?;
            p.repo_path = path_in_repo(&args.repo, Path::new(&args.policy))?;
            p
        }
    };
    let attestations = attestation_files(&args.attestations)?;
    let pull_request = args.pull_request.as_deref().map(read).transpose()?;
    let sessions = match &args.session {
        Some(b) => vec![session_evidence(b, args.session_anchors.clone())?],
        None => Vec::new(),
    };
    let inputs = Inputs {
        repository: args.repository.clone(),
        change,
        policy: &policy,
        attestations,
        pull_request,
        sessions,
        verifier_version: VERSION.to_string(),
    };
    let r = engine::evaluate(&inputs).map_err(|e| anyhow!("{e}"))?;
    let text = match args.format {
        Format::Text => report::text(&r),
        Format::Json => serde_json::to_string_pretty(&r)? + "\n",
        Format::Markdown => report::markdown(&r),
        Format::Intoto => serde_json::to_string_pretty(&report::statement(&r))? + "\n",
    };
    emit(args.output.as_deref(), &text)?;
    if let Some(path) = &args.summary {
        append(path, &report::markdown(&r))?;
    }
    Ok(r.decision.exit_code())
}

fn path_in_repo(repo: &Path, dir: &Path) -> Result<Option<String>> {
    let top = git::git(repo, &["rev-parse", "--show-toplevel"]).map_err(|e| anyhow!("{e}"))?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim_end());
    let top = fs::canonicalize(&top).with_context(|| format!("resolving {}", top.display()))?;
    let dir = fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    Ok(dir.strip_prefix(&top).ok().map(|rel| rel.to_string_lossy().into_owned()).filter(|r| !r.is_empty()))
}

pub(crate) fn session_evidence(bundle: &Path, anchors: Option<PathBuf>) -> Result<SessionEvidence> {
    let verdict = check(&VerifyArgs { dir: bundle.to_path_buf(), anchors, token: None, json: false })?;
    let id = if bundle.is_dir() {
        evidence_id(&fs::read(bundle.join("manifest.json")).context("reading the bundle manifest")?)
    } else {
        evidence_id(&fs::read(bundle).with_context(|| format!("reading {}", bundle.display()))?)
    };
    let mut src = Source::open(bundle)?;
    let manifest: Value = serde_json::from_str(&src.text("manifest.json")?).context("manifest.json is not JSON")?;
    let (mut exit_head, mut commits, mut dropped, mut terminated) = (None, Vec::new(), 0u64, false);
    for line in src.text("events.jsonl")?.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let p = &e["payload"];
        match e["event_type"].as_str() {
            Some("git.snapshot") if p["phase"] == "exit" => {
                exit_head = p["head"].as_str().map(str::to_string);
                commits = p["commits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["hash"].as_str().map(str::to_string))
                    .collect();
            }
            Some("session.terminated") => {
                terminated = true;
                dropped = dropped.max(p["dropped_events"].as_u64().unwrap_or(0));
            }
            Some("runtime.warning") if p["code"] == "events_dropped" => {
                dropped = dropped.max(p["dropped_events"].as_u64().unwrap_or(1));
            }
            _ => {}
        }
    }
    let anchored_through_seq = match verdict.anchors {
        super::verify::AnchorCheck::Through(seq) => Some(seq),
        _ => None,
    };
    let last_seq = verdict.events as i64 - 1;
    Ok(SessionEvidence {
        label: bundle.display().to_string(),
        id,
        session_id: verdict.session.clone(),
        program: manifest["program"].as_str().map(str::to_string),
        consistent: verdict.ok(),
        failures: verdict.failures.iter().map(ToString::to_string).collect(),
        anchored: anchored_through_seq == Some(last_seq),
        anchored_through_seq,
        token_matched: verdict.token == super::verify::TokenCheck::Matches,
        exit_head,
        commits,
        dropped_events: dropped,
        terminated,
    })
}

#[cfg(test)]
mod tests {
    use witnessed_store::{Body, NewEvent, NewSession, Notice, Store, TerminationReason};

    use super::*;
    use crate::bundle::assemble::{Edits, Include, assemble, write_bundle};

    fn bundle(dir: &Path, with_drop: bool, terminate: bool) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let store = Store::open(&dir.join("s.db")).unwrap();
        let cmd = vec!["claude".to_string()];
        store
            .create_session(&NewSession {
                id: "abc123",
                command: &cmd,
                cwd: None,
                created_at_ns: 1_000,
                supervisor_pid: 1,
                rows: 24,
                cols: 80,
                record_output: true,
                record_input: false,
                boot_time: None,
                restarted_from: None,
                agent_session_id: None,
                anchor_token_digest: None,
            })
            .unwrap();
        store.mark_started("abc123", 2, 1_000).unwrap();
        let at =
            |ms: i64, body: Body| NewEvent { ts_ns: 1_000 + ms * 1_000_000, mono_ns: ms * 1_000_000, body, data: None };
        let mut events = vec![at(0, Body::TerminalOutput)];
        if with_drop {
            events.push(at(
                5,
                Body::RuntimeWarning(Notice::new("events_dropped", "queue full").with("dropped_events", 7)),
            ));
        }
        if terminate {
            events.push(at(
                10,
                Body::SessionTerminated {
                    reason: TerminationReason::ProcessExited,
                    exit_code: Some(0),
                    signal: None,
                    dropped_events: 0,
                },
            ));
        }
        let mut store = store;
        store.append_events("abc123", 0, &events).unwrap();
        store.mark_exited("abc123", 1_000 + 20_000_000, Some(0), None).unwrap();
        let s = store.session("abc123").unwrap().unwrap();
        let edits = Edits { patterns: Vec::new(), cuts: vec![] };
        let a = assemble(&store, &s, Include::default(), &edits).unwrap();
        let (files, _) = write_bundle(&store, &s, "b", a, &edits.patterns).unwrap();
        let out = dir.join("b");
        fs::create_dir(&out).unwrap();
        super::super::bundle::write_dir(&out, &files).unwrap();
        out
    }

    #[test]
    fn a_complete_bundle_is_consistent_and_not_partial() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_evidence(&bundle(dir.path(), false, true), None).unwrap();
        assert!(s.consistent && s.terminated && !s.anchored, "{s:?}");
        assert_eq!((s.session_id.as_str(), s.dropped_events), ("abc123", 0));
        assert!(s.id.starts_with("sha256:"));
    }

    #[test]
    fn dropped_events_and_a_missing_termination_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_evidence(&bundle(dir.path(), true, false), None).unwrap();
        assert!(s.consistent);
        assert_eq!((s.dropped_events, s.terminated), (7, false));
    }

    fn decide(session: SessionEvidence) -> witnessed_assurance::engine::Report {
        let policy = Policy::from_files(
            witnessed_assurance::policy::PolicySource::Directory { path: "examples".into() },
            None,
            vec![("change.rego".into(), include_bytes!("../../../../examples/assurance/policy/change.rego").to_vec())],
        )
        .unwrap();
        let commit = "1".repeat(40);
        let change = git::ChangeSet {
            commit: commit.clone(),
            base: "0".repeat(40),
            merge_base: "0".repeat(40),
            files: vec![git::ChangedFile { path: "README.md".into(), status: "M".into(), old_path: None }],
            terraform: vec![],
        };
        let inputs = Inputs {
            repository: String::new(),
            change,
            policy: &policy,
            attestations: vec![],
            pull_request: None,
            sessions: vec![SessionEvidence { exit_head: Some(commit), ..session }],
            verifier_version: "test".into(),
        };
        engine::evaluate(&inputs).unwrap()
    }

    #[test]
    fn a_partial_session_for_the_commit_is_inconclusive_and_an_edited_one_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let partial = session_evidence(&bundle(&dir.path().join("p"), true, false), None).unwrap();
        let r = decide(partial);
        let q = r.requirements.iter().find(|q| q.id == "agent.session-complete").expect("session requirement");
        assert_eq!(q.status, witnessed_assurance::policy::Status::Inconclusive);
        assert_eq!(r.evidence[0].trust, "self_reported");
        assert!(
            r.evidence[0].detail.contains("partial capture (7 dropped events, no session.terminated)"),
            "{:?}",
            r.evidence
        );
        let complete = session_evidence(&bundle(&dir.path().join("c"), false, true), None).unwrap();
        assert!(decide(complete).requirements.iter().all(|q| q.id != "agent.session-complete"));
        let b = bundle(&dir.path().join("e"), false, true);
        let events = fs::read_to_string(b.join("events.jsonl")).unwrap();
        fs::write(b.join("events.jsonl"), events.replace("\"exit_code\":0", "\"exit_code\":1")).unwrap();
        let r = decide(session_evidence(&b, None).unwrap());
        assert_eq!(r.decision, engine::Decision::Rejected);
        assert!(r.requirements.iter().any(|q| q.id == "verifier.session-integrity"));
    }

    #[test]
    fn an_edited_bundle_is_not_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let b = bundle(dir.path(), false, true);
        let events = fs::read_to_string(b.join("events.jsonl")).unwrap();
        fs::write(b.join("events.jsonl"), events.replace("\"exit_code\":0", "\"exit_code\":1")).unwrap();
        let s = session_evidence(&b, None).unwrap();
        assert!(!s.consistent);
        assert!(s.failures.iter().any(|f| f.contains("events.jsonl")), "{:?}", s.failures);
    }
}
