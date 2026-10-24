use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::dsse::{self, INTOTO_PAYLOAD_TYPE};
use crate::git::ChangeSet;
use crate::github::{ReviewFacts, Snapshot};
use crate::intoto::{self, Statement, TestResult};
use crate::policy::{Policy, PolicySource, Requirement, Status};
use crate::{INPUT_SCHEMA, REPORT_SCHEMA, hex};

pub struct EvidenceFile {
    pub label: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionEvidence {
    pub label: String,
    pub id: String,
    pub session_id: String,
    pub program: Option<String>,
    pub consistent: bool,
    pub failures: Vec<String>,
    pub anchored: bool,
    pub anchored_through_seq: Option<i64>,
    pub token_matched: bool,
    pub exit_head: Option<String>,
    pub commits: Vec<String>,
    pub dropped_events: u64,
    pub terminated: bool,
}

pub struct Inputs<'a> {
    pub repository: String,
    pub change: ChangeSet,
    pub policy: &'a Policy,
    pub attestations: Vec<EvidenceFile>,
    pub pull_request: Option<EvidenceFile>,
    pub sessions: Vec<SessionEvidence>,
    pub verifier_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    Verified,
    Rejected,
    Inconclusive,
}

impl Decision {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Verified => 0,
            Self::Rejected => 1,
            Self::Inconclusive => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub status: &'static str,
    pub trust: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicate_type: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyRef {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub source: PolicySource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verifier {
    pub name: &'static str,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeRef {
    pub repository: String,
    pub commit: String,
    pub base: String,
    pub merge_base: String,
    pub files: Vec<crate::git::ChangedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub decision: Decision,
    pub verifier: Verifier,
    pub change: ChangeRef,
    pub policy: PolicyRef,
    pub input_digest: String,
    pub requirements: Vec<Requirement>,
    pub evidence: Vec<EvidenceRecord>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    PullRequest(String),
    Policy(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PullRequest(e) => write!(f, "pull request snapshot: {e}"),
            Self::Policy(e) => f.write_str(e),
        }
    }
}

pub fn evidence_id(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn verifier_req(id: &str, status: Status, reason: String, evidence: Vec<String>) -> Requirement {
    Requirement { id: format!("verifier.{id}"), status, reason, evidence, source: "verifier" }
}

struct Attestations {
    verified: Vec<Value>,
    records: Vec<EvidenceRecord>,
    invalid: Vec<String>,
    other_commit: Vec<String>,
}

fn check_attestations(files: &[EvidenceFile], policy: &Policy, commit: &str) -> Attestations {
    let mut out =
        Attestations { verified: Vec::new(), records: Vec::new(), invalid: Vec::new(), other_commit: Vec::new() };
    let mut seen = std::collections::BTreeSet::new();
    for f in files {
        let id = evidence_id(&f.bytes);
        if !seen.insert(id.clone()) {
            continue;
        }
        let record =
            |status, trust, signer: Option<String>, predicate_type: Option<String>, detail: String| EvidenceRecord {
                id: id.clone(),
                kind: "attestation",
                label: f.label.clone(),
                status,
                trust,
                signer,
                predicate_type,
                detail,
            };
        let checked = dsse::parse(&f.bytes)
            .and_then(|env| dsse::verify(&env, &policy.keys))
            .map_err(|e| e.to_string())
            .and_then(|v| {
                if v.payload_type != INTOTO_PAYLOAD_TYPE {
                    return Err(format!("payload type {:?} is not {INTOTO_PAYLOAD_TYPE}", v.payload_type));
                }
                let s = Statement::parse(&v.payload)?;
                if s.predicate_type == intoto::TEST_RESULT_V0_1 {
                    TestResult::from_predicate(&s.predicate)?;
                }
                Ok((v.signer, s))
            });
        match checked {
            Err(e) => {
                out.invalid.push(id.clone());
                out.records.push(record("invalid", "none", None, None, e));
            }
            Ok((signer, s)) if !s.names_commit(commit) => {
                out.other_commit.push(id.clone());
                let detail =
                    format!("signed by {signer}, but its subject names commit {:?}, not {commit}", s.subject_commits());
                out.records.push(record("other_commit", "signature", Some(signer), Some(s.predicate_type), detail));
            }
            Ok((signer, s)) => {
                out.verified.push(json!({
                    "id": id,
                    "signer": signer,
                    "trust": "signature",
                    "predicate_type": s.predicate_type,
                    "subjects": s.subject,
                    "predicate": s.predicate,
                }));
                let detail = format!("signature by trusted key {signer}; subject is commit {commit}");
                out.records.push(record("verified", "signature", Some(signer), Some(s.predicate_type), detail));
            }
        }
    }
    out
}

fn under(path: &str, dir: &str) -> bool {
    path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

pub fn evaluate(inputs: &Inputs) -> Result<Report, EngineError> {
    let commit = inputs.change.commit.as_str();
    let mut requirements = Vec::new();
    let mut evidence = Vec::new();

    let att = check_attestations(&inputs.attestations, inputs.policy, commit);
    evidence.extend(att.records.iter().cloned());
    if !att.invalid.is_empty() {
        requirements.push(verifier_req(
            "evidence-authentic",
            Status::Failed,
            format!(
                "{} attestation(s) failed authentication or parsing; presented evidence must verify",
                att.invalid.len()
            ),
            att.invalid.clone(),
        ));
    }
    if !att.other_commit.is_empty() {
        requirements.push(verifier_req(
            "evidence-commit",
            Status::Failed,
            format!(
                "{} attestation(s) are signed for another commit; evidence must name {commit}",
                att.other_commit.len()
            ),
            att.other_commit.clone(),
        ));
    }

    let pr: Option<(String, ReviewFacts)> = match &inputs.pull_request {
        None => None,
        Some(f) => {
            let snap = Snapshot::parse(&f.bytes).map_err(EngineError::PullRequest)?;
            let id = evidence_id(&f.bytes);
            let facts = snap.facts(commit);
            let same_repo = inputs.repository.is_empty() || snap.repository.eq_ignore_ascii_case(&inputs.repository);
            let status = match (same_repo, facts.head_matches_commit) {
                (false, _) => "other_repository",
                (true, false) => "stale",
                (true, true) => "collected",
            };
            if !same_repo {
                requirements.push(verifier_req(
                    "pull-request-repository",
                    Status::Failed,
                    format!("the pull request snapshot is for {}, not {}", snap.repository, inputs.repository),
                    vec![id.clone()],
                ));
            } else if !facts.head_matches_commit {
                requirements.push(verifier_req(
                    "pull-request-head",
                    Status::Inconclusive,
                    format!(
                        "the pull request head is {}, not {commit}; the run for that commit decides",
                        facts.head_sha
                    ),
                    vec![id.clone()],
                ));
            }
            evidence.push(EvidenceRecord {
                id: id.clone(),
                kind: "pull_request",
                label: f.label.clone(),
                status,
                trust: "collected",
                signer: None,
                predicate_type: None,
                detail: format!(
                    "{}#{} head {}; {} independent approval(s) at this commit",
                    snap.repository,
                    snap.number,
                    facts.head_sha,
                    facts.independent_approvals.len()
                ),
            });
            (same_repo && facts.head_matches_commit).then_some((id, facts))
        }
    };

    for s in &inputs.sessions {
        let (status, trust) = match (s.consistent, s.anchored) {
            (false, _) => ("invalid", "none"),
            (true, true) => ("consistent", "anchored"),
            (true, false) => ("consistent", "self_reported"),
        };
        if !s.consistent {
            requirements.push(verifier_req(
                "session-integrity",
                Status::Failed,
                format!("session bundle {} failed verification: {}", s.label, s.failures.join("; ")),
                vec![s.id.clone()],
            ));
        }
        let mut detail = format!("session {}", s.session_id);
        if s.dropped_events > 0 || !s.terminated {
            detail.push_str(&format!(
                "; partial capture ({} dropped events{})",
                s.dropped_events,
                if s.terminated { "" } else { ", no session.terminated" }
            ));
        }
        evidence.push(EvidenceRecord {
            id: s.id.clone(),
            kind: "session",
            label: s.label.clone(),
            status,
            trust,
            signer: None,
            predicate_type: None,
            detail,
        });
    }

    if let Some(dir) = &inputs.policy.repo_path {
        let touched: Vec<&str> = inputs
            .change
            .files
            .iter()
            .filter(|f| under(&f.path, dir) || f.old_path.as_deref().is_some_and(|p| under(p, dir)))
            .map(|f| f.path.as_str())
            .collect();
        if !touched.is_empty() {
            let (status, reason, ev) = match &pr {
                Some((id, f)) if !f.independent_approvals.is_empty() => (
                    Status::Passed,
                    format!(
                        "the change edits the policy ({}); approved at this commit by {}",
                        touched.join(", "),
                        f.independent_approvals.join(", ")
                    ),
                    vec![id.clone()],
                ),
                _ => (
                    Status::Inconclusive,
                    format!(
                        "the change edits the policy that judges it ({}); it was evaluated with the policy from {} and needs an independent approval at this commit",
                        touched.join(", "),
                        match &inputs.policy.source {
                            PolicySource::GitTree { commit, .. } => commit.as_str(),
                            PolicySource::Directory { path } => path.as_str(),
                        }
                    ),
                    Vec::new(),
                ),
            };
            requirements.push(verifier_req("policy-change-approved", status, reason, ev));
        }
    }

    let sessions: Vec<Value> = inputs
        .sessions
        .iter()
        .filter(|s| s.consistent)
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.remove("label");
            }
            v["trust"] = json!(if s.anchored { "anchored" } else { "self_reported" });
            v["linked_to_commit"] = json!(
                s.exit_head.as_deref() == Some(commit) || s.commits.iter().any(|c| c.eq_ignore_ascii_case(commit))
            );
            v["partial"] = json!(s.dropped_events > 0 || !s.terminated);
            v
        })
        .collect();
    let input = json!({
        "schema": INPUT_SCHEMA,
        "change": {
            "repository": inputs.repository,
            "commit": commit,
            "base": inputs.change.base,
            "merge_base": inputs.change.merge_base,
            "files": inputs.change.files,
            "terraform": inputs.change.terraform,
        },
        "evidence": {
            "attestations": att.verified,
            "pull_request": pr.as_ref().map(|(id, f)| { let mut v = serde_json::to_value(f).unwrap_or(Value::Null); v["id"] = json!(id); v }),
            "sessions": sessions,
        },
        "declared": {"repository": inputs.repository},
    });
    let eval = inputs.policy.evaluate(&input).map_err(|e| EngineError::Policy(e.0))?;
    if eval.requirements.is_empty() {
        requirements.push(verifier_req(
            "policy-requirements",
            Status::Inconclusive,
            "the policy stated no requirement for this change; nothing was verified".into(),
            Vec::new(),
        ));
    }
    requirements.extend(eval.requirements);
    requirements.sort_by(|a, b| a.id.cmp(&b.id));

    let decision = if requirements.iter().any(|r| r.status == Status::Failed) {
        Decision::Rejected
    } else if requirements.iter().any(|r| r.status == Status::Inconclusive) {
        Decision::Inconclusive
    } else {
        Decision::Verified
    };
    let reasons = requirements
        .iter()
        .filter(|r| r.status != Status::Passed)
        .map(|r| {
            format!("{} {}: {}", if r.status == Status::Failed { "failed" } else { "inconclusive" }, r.id, r.reason)
        })
        .collect();
    evidence.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    Ok(Report {
        schema: REPORT_SCHEMA,
        decision,
        verifier: Verifier { name: "witnessed", version: inputs.verifier_version.clone() },
        change: ChangeRef {
            repository: inputs.repository.clone(),
            commit: commit.to_string(),
            base: inputs.change.base.clone(),
            merge_base: inputs.change.merge_base.clone(),
            files: inputs.change.files.clone(),
        },
        policy: PolicyRef {
            id: eval.id,
            version: eval.version,
            digest: inputs.policy.digest.clone(),
            source: inputs.policy.source.clone(),
        },
        input_digest: evidence_id(input.to_string().as_bytes()),
        requirements,
        evidence,
        reasons,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_a_directory_match_whole_components() {
        assert!(under(".witnessed/policy/a.rego", ".witnessed/policy"));
        assert!(under(".witnessed/policy", ".witnessed/policy"));
        assert!(!under(".witnessed/policy-old/a.rego", ".witnessed/policy"));
    }

    #[test]
    fn truncated_or_corrupted_inputs_are_errors_not_panics() {
        let envelope = include_bytes!("../tests/fixtures/openssl-signed.dsse.json");
        let keys = vec![crate::dsse::TrustedKey::from_pem("ci", include_str!("../tests/fixtures/ci.pub.pem")).unwrap()];
        let snapshot = br#"{"schema":"witnessed.github-pull-request/v0.1","repository":"o/r","number":1,"state":"open","author":null,"head_sha":"a","base_sha":"b","base_ref":"main","commits":[],"reviews":[]}"#;
        for cut in 0..envelope.len() {
            let mut bytes = envelope[..cut].to_vec();
            let _ = crate::dsse::parse(&bytes).and_then(|e| crate::dsse::verify(&e, &keys));
            let _ = Statement::parse(&bytes);
            bytes.reverse();
            let _ = crate::dsse::parse(&bytes);
        }
        for cut in 0..snapshot.len() {
            let _ = Snapshot::parse(&snapshot[..cut]);
        }
        let mut flipped = envelope.to_vec();
        for i in (0..flipped.len()).step_by(7) {
            flipped[i] ^= 0x20;
            let _ = crate::dsse::parse(&flipped).and_then(|e| crate::dsse::verify(&e, &keys));
            flipped[i] ^= 0x20;
        }
        assert!(Snapshot::parse(snapshot).is_ok());
        assert!(crate::dsse::verify(&crate::dsse::parse(envelope).unwrap(), &keys).is_ok());
    }

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(
            [Decision::Verified, Decision::Rejected, Decision::Inconclusive].map(Decision::exit_code),
            [0, 1, 3]
        );
    }
}
