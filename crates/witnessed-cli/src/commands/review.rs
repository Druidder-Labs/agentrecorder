use std::fmt::Write as _;
use std::io::IsTerminal;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::json;
use witnessed_store::{Annotation, Store};
use witnessed_supervisor::Layout;

use crate::decisions::{self, Decisions, Kind, Moment};
use crate::fmt;
use crate::outcome::{self, Outcome};
use crate::state::{duration_ms, open_and_resolve};

const HINT_BUDGET: Duration = Duration::from_millis(300);

#[derive(clap::Args)]
pub struct ReviewArgs {
    #[arg(default_value = "last", help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(long, help = "Print JSON instead of text")]
    pub json: bool,
    #[arg(long = "kind", value_name = "KIND", value_enum, help = "Show only moments of this kind; repeat for several")]
    pub kinds: Vec<Kind>,
    #[arg(
        long,
        value_name = "MOMENT_ID",
        requires = "verdict",
        conflicts_with_all = ["clear", "json", "kinds"],
        help = "Record your judgment of a moment; needs --verdict"
    )]
    pub mark: Option<String>,
    #[arg(long, value_enum, requires = "mark", help = "Would you decide the same way again")]
    pub verdict: Option<Verdict>,
    #[arg(long, value_name = "TEXT", requires = "mark", help = "A note kept with the verdict")]
    pub note: Option<String>,
    #[arg(long, value_name = "MOMENT_ID", conflicts_with_all = ["json", "kinds"], help = "Remove your annotation of a moment")]
    pub clear: Option<String>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Again,
    Differently,
    Unsure,
}

impl Verdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Again => "again",
            Self::Differently => "differently",
            Self::Unsure => "unsure",
        }
    }
}

pub enum Action<'a> {
    Mark { moment: &'a str, verdict: Verdict, note: Option<&'a str> },
    Clear { moment: &'a str },
}

pub struct Header<'a> {
    pub id: &'a str,
    pub command: String,
    pub duration_ms: Option<i64>,
    pub running: bool,
}

pub fn review(layout: &Layout, args: ReviewArgs) -> Result<i32> {
    let (store, s, state) = open_and_resolve(layout, &args.session)?;
    let d = decisions::derive(&store, &s, &[])?;
    let action = match (&args.mark, args.verdict, &args.clear) {
        (Some(moment), Some(verdict), _) => Some(Action::Mark { moment, verdict, note: args.note.as_deref() }),
        (_, _, Some(moment)) => Some(Action::Clear { moment }),
        _ => None,
    };
    if let Some(action) = action {
        println!("{}", annotate(&store, &s.id, &d, &action)?);
        return Ok(0);
    }
    let annotations = store.annotations(&s.id)?;
    let out = outcome::for_session(&store, &s);
    let shown = filter(&d, &args.kinds);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": s.id,
                "decisions": shown,
                "annotations": annotations,
                "outcome": out,
            }))?
        );
        return Ok(0);
    }
    let head = Header {
        id: &s.id,
        command: crate::adapters::display(&crate::adapters::without_injected(
            &s.command,
            s.agent_session_id.as_deref(),
        )),
        duration_ms: duration_ms(&s),
        running: !state.is_finished(),
    };
    print!("{}", render(&head, &d, &args.kinds, &annotations, &out));
    Ok(0)
}

pub fn annotate(store: &Store, id: &str, d: &Decisions, action: &Action) -> Result<String> {
    match *action {
        Action::Mark { moment, verdict, note } => {
            if !d.available {
                bail!(
                    "cannot annotate session {id}: its decision moments are unavailable ({})",
                    d.reason.as_deref().unwrap_or("no reason given")
                );
            }
            if !d.moments.iter().any(|m| m.id == moment) {
                bail!("session {id} has no moment {moment:?}; list its moments with `witnessed review {id}`");
            }
            let note = note.map(str::trim).filter(|n| !n.is_empty());
            store.set_annotation(id, moment, verdict.as_str(), note, witnessed_store::now_unix_ns())?;
            Ok(format!("{id} {moment}: you would decide {}{}", verdict.as_str(), note_suffix(note)))
        }
        Action::Clear { moment } => {
            if !store.clear_annotation(id, moment)? {
                bail!("moment {moment:?} of session {id} has no annotation; list them with `witnessed review {id}`");
            }
            Ok(format!("{id} {moment}: annotation removed"))
        }
    }
}

fn note_suffix(note: Option<&str>) -> String {
    note.map(|n| format!(" - {n}")).unwrap_or_default()
}

fn filter(d: &Decisions, kinds: &[Kind]) -> Decisions {
    let mut out = d.clone();
    if !kinds.is_empty() {
        out.moments.retain(|m| kinds.contains(&m.kind));
    }
    out
}

fn subject(m: &Moment) -> String {
    match (&m.tool, &m.subject) {
        (Some(t), Some(s)) => crate::transcript::one_line(&format!("{t}: {s}"), 72),
        (Some(x), None) | (None, Some(x)) => x.clone(),
        (None, None) => "-".into(),
    }
}

pub fn render(head: &Header, d: &Decisions, kinds: &[Kind], annotations: &[Annotation], out: &Outcome) -> String {
    let mut o = String::new();
    let dur = head.duration_ms.map_or_else(|| "duration unavailable".into(), fmt::duration_ms);
    let _ = writeln!(o, "Session {}  {}  {dur}", head.id, head.command);
    if !d.available {
        let reason = d.reason.as_deref().unwrap_or("no reason given");
        let _ = writeln!(o, "Decision moments: unavailable ({reason})");
        return o;
    }
    if head.running {
        let _ = writeln!(o, "The session is still running; the list may grow.");
    }
    let total = d.moments.len();
    let by_kind: Vec<String> = d.counts.iter().filter(|(_, n)| **n > 0).map(|(k, n)| format!("{n} {k}")).collect();
    let _ = write!(o, "{total} decision moment{}", if total == 1 { "" } else { "s" });
    if !by_kind.is_empty() {
        let _ = write!(o, ": {}", by_kind.join(", "));
    }
    o.push('\n');
    let shown: Vec<&Moment> = d.moments.iter().filter(|m| kinds.is_empty() || kinds.contains(&m.kind)).collect();
    if !kinds.is_empty() {
        let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        let _ = writeln!(o, "Showing {} of {total} (--kind {})", shown.len(), names.join(", "));
    }
    if !shown.is_empty() {
        let idw = shown.iter().map(|m| m.id.chars().count()).max().unwrap_or(2).max(2);
        let kw = shown.iter().map(|m| m.kind.as_str().len()).max().unwrap_or(4).max(4);
        o.push('\n');
        let _ = writeln!(o, "{:<idw$}  {:<10}  {:<kw$}  SUBJECT", "ID", "OFFSET", "KIND");
        for m in &shown {
            let _ = writeln!(o, "{:<idw$}  {}  {:<kw$}  {}", m.id, fmt::offset_ms(m.t_ms), m.kind, subject(m));
            let pad = " ".repeat(idw + 2);
            let line =
                m.prompt.map(|p| format!("Prompt {p}.")).into_iter().chain(m.facts.iter().cloned()).collect::<Vec<_>>();
            if !line.is_empty() {
                let _ = writeln!(o, "{pad}{}", line.join(" "));
            }
            if let Some(a) = annotations.iter().find(|a| a.moment == m.id) {
                let _ = writeln!(o, "{pad}You: {}{}", a.verdict, note_suffix(a.note.as_deref()));
            }
        }
    }
    if out.available {
        o.push('\n');
        let _ = writeln!(o, "{}", outcome_line(out));
        for c in out.commits.iter().filter(|c| c.reverts) {
            let sha: String = c.sha.chars().take(12).collect();
            let _ = writeln!(o, "Reverted by {sha} {:?} at {}", c.subject, c.at);
        }
    }
    o.push('\n');
    let example = shown
        .first()
        .map_or(("MOMENT_ID".to_string(), "OFFSET".to_string()), |m| (m.id.clone(), fmt::offset_ms(m.t_ms)));
    let _ = writeln!(
        o,
        "Annotate: witnessed review {} --mark {} --verdict again|differently|unsure [--note TEXT]",
        head.id, example.0
    );
    let _ = writeln!(o, "Watch:    witnessed replay {} --from {}", head.id, example.1);
    o
}

fn outcome_line(out: &Outcome) -> String {
    let lines = |n: u64| format!("{n} line{}", if n == 1 { "" } else { "s" });
    match (out.added, out.in_head) {
        (0, _) => "Outcome: the session added no lines.".into(),
        (a, 0) => {
            format!("Outcome: {} added, {} still in the work tree, none committed yet.", lines(a), out.in_worktree)
        }
        (a, h) => {
            format!("Outcome: {} added, {h} committed in HEAD, {} still in the work tree.", lines(a), out.in_worktree)
        }
    }
}

pub fn exit_hint(layout: &Layout, id: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let db = layout.db_path();
    let id = id.to_string();
    let worker = id.clone();
    let spawned = std::thread::Builder::new().name("review-hint".into()).spawn(move || {
        let count = (|| -> Option<usize> {
            let store = Store::open(&db).ok()?;
            let s = store.session(&worker).ok()??;
            if crate::adapters::program(&s.command) != Some("claude") {
                return None;
            }
            let d = decisions::derive(&store, &s, &[]).ok()?;
            d.available.then_some(d.moments.len())
        })();
        let _ = tx.send(count);
    });
    if spawned.is_err() {
        return;
    }
    if let Ok(Some(n)) = rx.recv_timeout(HINT_BUDGET)
        && n > 0
    {
        eprintln!(
            "[witnessed] {n} decision moment{}; review them with: witnessed review {id}",
            if n == 1 { "" } else { "s" }
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clap::ValueEnum;
    use witnessed_store::NewSession;

    use super::*;
    use crate::outcome::CommitOutcome;

    fn moment(id: &str, t_ms: i64, kind: Kind) -> Moment {
        Moment {
            id: id.into(),
            t_ms,
            kind,
            prompt: None,
            tool: None,
            subject: None,
            wait_ms: None,
            checked_before: None,
            then: None,
            facts: Vec::new(),
        }
    }

    fn sample() -> Decisions {
        let mut a = moment("m-1", 62_300, Kind::Approved);
        a.tool = Some("Bash".into());
        a.subject = Some("cargo test".into());
        a.prompt = Some(2);
        a.wait_ms = Some(4_000);
        a.checked_before = Some(false);
        a.facts = vec!["Approved in 4.0 s without a test run earlier in this prompt.".into(), "Tests failed.".into()];
        a.then = Some("the agent edited src/lib.rs".into());
        let mut b = moment("m-2", 125_000, Kind::Interrupted);
        b.subject = Some("Edit src/main.rs".into());
        Decisions {
            available: true,
            reason: None,
            moments: vec![a, b],
            counts: BTreeMap::from([(Kind::Approved, 1), (Kind::Interrupted, 1)]),
        }
    }

    fn head(running: bool) -> Header<'static> {
        Header { id: "a91f2c", command: "claude".into(), duration_ms: Some(763_000), running }
    }

    #[test]
    fn renders_moments_annotations_outcome_and_footer() {
        let ann = vec![Annotation {
            moment: "m-1".into(),
            verdict: "differently".into(),
            note: Some("should have read the diff".into()),
            updated_at_ns: 1,
        }];
        let out = Outcome {
            available: true,
            added: 120,
            in_head: 80,
            in_worktree: 95,
            commits: vec![CommitOutcome {
                sha: "0123456789abcdef".into(),
                subject: "Revert \"x\"".into(),
                at: "2026-10-22T10:00:00Z".into(),
                reverts: true,
            }],
            ..Outcome::default()
        };
        let text = render(&head(false), &sample(), &[], &ann, &out);
        let want = "Session a91f2c  claude  12m 43s
2 decision moments: 1 approved, 1 interrupted

ID   OFFSET      KIND         SUBJECT
m-1  00:01:02.3  approved     Bash: cargo test
     Prompt 2. Approved in 4.0 s without a test run earlier in this prompt. Tests failed.
     You: differently - should have read the diff
m-2  00:02:05.0  interrupted  Edit src/main.rs

Outcome: 120 lines added, 80 committed in HEAD, 95 still in the work tree.
Reverted by 0123456789ab \"Revert \\\"x\\\"\" at 2026-10-22T10:00:00Z

Annotate: witnessed review a91f2c --mark m-1 --verdict again|differently|unsure [--note TEXT]
Watch:    witnessed replay a91f2c --from 00:01:02.3
";
        assert_eq!(text, want);
    }

    #[test]
    fn filters_by_kind_and_notes_a_running_session() {
        let text = render(&head(true), &sample(), &[Kind::Interrupted], &[], &Outcome::default());
        assert!(text.contains("still running; the list may grow"), "{text}");
        assert!(text.contains("Showing 1 of 2 (--kind interrupted)"), "{text}");
        assert!(!text.contains("m-1 "), "{text}");
        assert!(text.contains("--mark m-2"), "{text}");
        assert!(!text.contains("lines the session added"), "{text}");
        assert_eq!(filter(&sample(), &[Kind::Interrupted]).moments.len(), 1);
    }

    #[test]
    fn unavailable_decisions_print_the_reason_only() {
        let d = Decisions { reason: Some("not an agent session".into()), ..Decisions::default() };
        let text = render(&head(false), &d, &[], &[], &Outcome::default());
        assert_eq!(text, "Session a91f2c  claude  12m 43s\nDecision moments: unavailable (not an agent session)\n");
    }

    #[test]
    fn verdicts_parse_from_their_names_only() {
        assert_eq!(Verdict::from_str("differently", false), Ok(Verdict::Differently));
        assert_eq!(Verdict::from_str("again", false).map(Verdict::as_str), Ok("again"));
        assert_eq!(Verdict::from_str("unsure", false).map(Verdict::as_str), Ok("unsure"));
        assert!(Verdict::from_str("maybe", false).is_err());
    }

    #[test]
    fn annotations_are_validated_set_replaced_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let cmd = vec!["claude".to_string()];
        store
            .create_session(&NewSession {
                id: "abc123",
                command: &cmd,
                cwd: None,
                created_at_ns: 1,
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
        let d = sample();
        let mark = |moment, verdict, note| annotate(&store, "abc123", &d, &Action::Mark { moment, verdict, note });
        assert_eq!(mark("m-1", Verdict::Again, None).unwrap(), "abc123 m-1: you would decide again");
        assert_eq!(
            mark("m-1", Verdict::Differently, Some(" run the tests ")).unwrap(),
            "abc123 m-1: you would decide differently - run the tests"
        );
        let err = mark("m-9", Verdict::Unsure, None).unwrap_err().to_string();
        assert!(err.contains("no moment \"m-9\"") && err.contains("witnessed review abc123"), "{err}");
        let saved = store.annotations("abc123").unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].verdict.as_str(), saved[0].note.as_deref()), ("differently", Some("run the tests")));

        let none = Decisions { reason: Some("not an agent session".into()), ..Decisions::default() };
        let err =
            annotate(&store, "abc123", &none, &Action::Mark { moment: "m-1", verdict: Verdict::Again, note: None })
                .unwrap_err()
                .to_string();
        assert!(err.contains("unavailable (not an agent session)"), "{err}");

        assert_eq!(
            annotate(&store, "abc123", &d, &Action::Clear { moment: "m-1" }).unwrap(),
            "abc123 m-1: annotation removed"
        );
        assert!(annotate(&store, "abc123", &d, &Action::Clear { moment: "m-1" }).is_err());
        assert!(store.annotations("abc123").unwrap().is_empty());
    }
}
