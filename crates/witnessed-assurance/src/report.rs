use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::engine::{Decision, Report};
use crate::intoto::{CHANGE_VERIFICATION_V0_1, ResourceDescriptor, STATEMENT_V1, Statement};
use crate::policy::{PolicySource, Status};
use crate::{EXIT_ERROR, REPORT_SCHEMA};

fn decision_word(d: Decision) -> &'static str {
    match d {
        Decision::Verified => "VERIFIED",
        Decision::Rejected => "REJECTED",
        Decision::Inconclusive => "INCONCLUSIVE",
    }
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Passed => "passed",
        Status::Failed => "FAILED",
        Status::Inconclusive => "inconclusive",
    }
}

fn source_text(s: &PolicySource) -> String {
    match s {
        PolicySource::GitTree { commit, path } => format!("{path} at {commit}"),
        PolicySource::Directory { path } => format!("directory {path} (not read from git)"),
    }
}

pub fn text(r: &Report) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "decision:  {}", decision_word(r.decision));
    let _ = writeln!(o, "commit:    {}", r.change.commit);
    if !r.change.repository.is_empty() {
        let _ = writeln!(o, "repo:      {}", r.change.repository);
    }
    let _ = writeln!(o, "base:      {} ({} files changed)", r.change.merge_base, r.change.files.len());
    let _ = writeln!(o, "policy:    {} {} {}", r.policy.id, r.policy.version, r.policy.digest);
    let _ = writeln!(o, "           from {}", source_text(&r.policy.source));
    let _ = writeln!(o, "verifier:  {} {}", r.verifier.name, r.verifier.version);
    o.push_str("requirements:\n");
    for q in &r.requirements {
        let _ = writeln!(o, "  {:<13} {}: {}", status_word(q.status), q.id, q.reason);
    }
    o.push_str("evidence:\n");
    if r.evidence.is_empty() {
        o.push_str("  none presented\n");
    }
    for e in &r.evidence {
        let _ = writeln!(o, "  {:<13} {} {} ({}): {}", e.status, e.kind, e.label, e.id, e.detail);
    }
    o
}

pub fn markdown(r: &Report) -> String {
    let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
    let mut o = String::new();
    let _ = writeln!(o, "### Agent change verification: {}\n", decision_word(r.decision));
    let _ = writeln!(
        o,
        "Commit `{}`, policy `{}` version `{}` (`{}`), from {}.\n",
        r.change.commit,
        r.policy.id,
        r.policy.version,
        r.policy.digest,
        cell(&source_text(&r.policy.source))
    );
    o.push_str("| Status | Requirement | Reason |\n| --- | --- | --- |\n");
    for q in &r.requirements {
        let _ = writeln!(o, "| {} | `{}` | {} |", status_word(q.status), q.id, cell(&q.reason));
    }
    o.push_str("\n| Evidence | Status | Trust | Detail |\n| --- | --- | --- | --- |\n");
    for e in &r.evidence {
        let _ = writeln!(o, "| {} `{}` | {} | {} | {} |", e.kind, cell(&e.label), e.status, e.trust, cell(&e.detail));
    }
    let _ = writeln!(
        o,
        "\nVerifier {} {}. The decision covers this commit only; a new push needs a new run.",
        r.verifier.name, r.verifier.version
    );
    o
}

pub fn statement(r: &Report) -> Statement {
    let name = if r.change.repository.is_empty() { "commit".to_string() } else { r.change.repository.clone() };
    Statement {
        kind: STATEMENT_V1.to_string(),
        subject: vec![ResourceDescriptor::commit(&name, &r.change.commit)],
        predicate_type: CHANGE_VERIFICATION_V0_1.to_string(),
        predicate: serde_json::to_value(r).unwrap_or(Value::Null),
    }
}

pub fn error_markdown(commit: &str, version: &str, error: &str) -> String {
    format!(
        "### Agent change verification: ERROR\n\nCommit `{}`: the verifier could not decide; this is not an approval.\n\n~~~\n{}\n~~~\n\nVerifier witnessed {version}.\n",
        commit.replace('`', ""),
        error.replace("~~~", "- - -")
    )
}

pub fn error_json(commit: Option<&str>, version: &str, error: &str) -> Value {
    json!({
        "schema": REPORT_SCHEMA,
        "decision": "ERROR",
        "exit_code": EXIT_ERROR,
        "verifier": {"name": "witnessed", "version": version},
        "commit": commit,
        "error": error,
    })
}
