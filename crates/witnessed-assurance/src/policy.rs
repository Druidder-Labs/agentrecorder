use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::dsse::TrustedKey;
use crate::git;
use crate::hex;

pub const PACKAGE: &str = "data.witnessed.change";

const NONDETERMINISTIC: &[(&str, u8)] = &[
    ("rand.intn", 2),
    ("time.now_ns", 0),
    ("uuid.rfc4122", 1),
    ("opa.runtime", 0),
    ("http.send", 1),
    ("net.lookup_ip_addr", 1),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicySource {
    Directory { path: String },
    GitTree { commit: String, path: String },
}

pub struct Policy {
    pub source: PolicySource,
    pub repo_path: Option<String>,
    pub digest: String,
    modules: Vec<(String, String)>,
    pub keys: Vec<TrustedKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(pub String);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Inconclusive,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    pub id: String,
    pub status: Status,
    pub reason: String,
    pub evidence: Vec<String>,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub id: String,
    pub version: String,
    pub requirements: Vec<Requirement>,
}

impl Policy {
    pub fn from_files(
        source: PolicySource,
        repo_path: Option<String>,
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<Self, PolicyError> {
        let mut files = files;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut digest = Sha256::new();
        let (mut modules, mut keys) = (Vec::new(), Vec::new());
        for (name, bytes) in files {
            let is_rego = name.ends_with(".rego") && !name.ends_with("_test.rego");
            let key_name = name.strip_prefix("keys/").and_then(|n| n.strip_suffix(".pub.pem"));
            if !is_rego && key_name.is_none() {
                continue;
            }
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(Sha256::digest(&bytes));
            let text = String::from_utf8(bytes).map_err(|_| PolicyError(format!("policy file {name} is not UTF-8")))?;
            match key_name {
                Some(k) => keys
                    .push(TrustedKey::from_pem(k, &text).map_err(|e| PolicyError(format!("policy key {name}: {e}")))?),
                None => modules.push((name, text)),
            }
        }
        if modules.is_empty() {
            return Err(PolicyError("the policy has no .rego module".into()));
        }
        Ok(Self { source, repo_path, digest: format!("sha256:{}", hex(&digest.finalize())), modules, keys })
    }

    pub fn from_dir(dir: &Path) -> Result<Self, PolicyError> {
        let mut files = Vec::new();
        let read = |p: &Path| std::fs::read(p).map_err(|e| PolicyError(format!("reading {}: {e}", p.display())));
        let entries = std::fs::read_dir(dir)
            .map_err(|e| PolicyError(format!("reading policy directory {}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| PolicyError(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_file() {
                files.push((name, read(&entry.path())?));
            }
        }
        if let Ok(keys) = std::fs::read_dir(dir.join("keys")) {
            for entry in keys.flatten() {
                files.push((format!("keys/{}", entry.file_name().to_string_lossy()), read(&entry.path())?));
            }
        }
        Self::from_files(PolicySource::Directory { path: dir.display().to_string() }, None, files)
    }

    pub fn from_git(repo: &Path, rev: &str, path: &str) -> Result<Self, PolicyError> {
        let commit = git::resolve_commit(repo, rev).map_err(|e| PolicyError(format!("policy ref: {e}")))?;
        let files = git::files_at(repo, &commit, path).map_err(|e| PolicyError(format!("policy at {rev}: {e}")))?;
        let path = path.trim_end_matches('/').to_string();
        Self::from_files(PolicySource::GitTree { commit, path: path.clone() }, Some(path), files)
    }

    pub fn evaluate(&self, input: &Value) -> Result<Evaluation, PolicyError> {
        let err = |what: &str, e: &dyn std::fmt::Display| PolicyError(format!("policy {what}: {e}"));
        let mut engine = regorus::Engine::new();
        engine.set_rego_v0(false);
        for (name, nargs) in NONDETERMINISTIC {
            let refuse = move |_: Vec<regorus::Value>| -> anyhow::Result<regorus::Value> {
                anyhow::bail!("{name} is disabled: a decision must not depend on time, randomness or the network")
            };
            engine.add_extension((*name).to_string(), *nargs, Box::new(refuse)).map_err(|e| err("setup", &e))?;
        }
        for (name, text) in &self.modules {
            engine.add_policy(name.clone(), text.clone()).map_err(|e| err(&format!("module {name}"), &e))?;
        }
        let input = regorus::Value::from_json_str(&input.to_string()).map_err(|e| err("input", &e))?;
        engine.set_input(input);
        let mut rule = |name: &str| -> Result<Value, PolicyError> {
            let v = engine.eval_rule(format!("{PACKAGE}.{name}")).map_err(|e| err(&format!("rule {name}"), &e))?;
            let json = v.to_json_str().map_err(|e| err(&format!("rule {name}"), &e))?;
            serde_json::from_str(&json).map_err(|e| err(&format!("rule {name}"), &e))
        };
        let id = rule("policy_id")?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| PolicyError("policy_id must be a string".into()))?;
        let version = rule("policy_version")?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| PolicyError("policy_version must be a string".into()))?;
        let mut requirements = match rule("requirements")? {
            Value::Array(items) => items.iter().map(requirement).collect::<Result<Vec<_>, _>>()?,
            other => return Err(PolicyError(format!("requirements must be a set of objects, got {other}"))),
        };
        requirements.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(w) = requirements.windows(2).find(|w| w[0].id == w[1].id) {
            return Err(PolicyError(format!("requirement {} is defined more than once", w[0].id)));
        }
        Ok(Evaluation { id, version, requirements })
    }
}

fn requirement(v: &Value) -> Result<Requirement, PolicyError> {
    let field = |k: &str| {
        v[k].as_str().map(str::to_string).ok_or_else(|| PolicyError(format!("requirement {v} lacks a string {k:?}")))
    };
    let id = field("id")?;
    if id.starts_with("verifier.") {
        return Err(PolicyError(format!("requirement id {id} uses the reserved verifier. prefix")));
    }
    let status = match field("status")?.as_str() {
        "passed" => Status::Passed,
        "failed" => Status::Failed,
        "inconclusive" => Status::Inconclusive,
        other => {
            return Err(PolicyError(format!(
                "requirement {id}: status {other:?} is not passed, failed or inconclusive"
            )));
        }
    };
    let evidence = match &v["evidence"] {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .iter()
            .map(|e| {
                e.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| PolicyError(format!("requirement {id}: evidence ids must be strings")))
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(PolicyError(format!("requirement {id}: evidence must be an array"))),
    };
    let mut evidence: Vec<String> = evidence;
    evidence.sort();
    evidence.dedup();
    Ok(Requirement { id, status, reason: field("reason")?, evidence, source: "policy" })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn policy(rego: &str) -> Policy {
        Policy::from_files(
            PolicySource::Directory { path: "t".into() },
            None,
            vec![("p.rego".into(), rego.as_bytes().to_vec())],
        )
        .unwrap()
    }

    const HEADER: &str = "package witnessed.change\nimport rego.v1\npolicy_id := \"t\"\npolicy_version := \"1\"\n";

    #[test]
    fn requirements_come_back_sorted_with_their_status() {
        let p = policy(&format!(
            "{HEADER}requirements contains {{\"id\": \"b\", \"status\": \"failed\", \"reason\": \"x\"}} if input.fail\n\
             requirements contains {{\"id\": \"a\", \"status\": \"passed\", \"reason\": \"y\", \"evidence\": [\"e2\", \"e1\"]}}\n"
        ));
        let e = p.evaluate(&json!({"fail": true})).unwrap();
        assert_eq!((e.id.as_str(), e.version.as_str()), ("t", "1"));
        assert_eq!(
            e.requirements.iter().map(|r| (r.id.as_str(), r.status)).collect::<Vec<_>>(),
            [("a", Status::Passed), ("b", Status::Failed)]
        );
        assert_eq!(e.requirements[0].evidence, ["e1", "e2"]);
        assert_eq!(p.evaluate(&json!({"fail": false})).unwrap().requirements.len(), 1);
    }

    #[test]
    fn a_policy_whose_requirements_match_nothing_returns_none() {
        let p = policy(&format!(
            "{HEADER}requirements contains {{\"id\": \"a\", \"status\": \"passed\", \"reason\": \"\"}} if input.never\n"
        ));
        assert!(p.evaluate(&json!({})).unwrap().requirements.is_empty());
        assert!(policy(HEADER).evaluate(&json!({})).unwrap_err().0.contains("requirements"));
    }

    #[test]
    fn malformed_policies_and_outputs_are_errors_not_decisions() {
        let bad = Policy::from_files(
            PolicySource::Directory { path: "t".into() },
            None,
            vec![("p.rego".into(), b"package x\nthis is not rego".to_vec())],
        )
        .unwrap();
        assert!(bad.evaluate(&json!({})).is_err());
        let p =
            policy(&format!("{HEADER}requirements contains {{\"id\": \"a\", \"status\": \"ok\", \"reason\": \"\"}}\n"));
        assert!(p.evaluate(&json!({})).unwrap_err().0.contains("is not passed"));
        let p = policy(&format!(
            "{HEADER}requirements contains {{\"id\": \"verifier.x\", \"status\": \"passed\", \"reason\": \"\"}}\n"
        ));
        assert!(p.evaluate(&json!({})).unwrap_err().0.contains("reserved"));
        let p = policy("package witnessed.change\nimport rego.v1\npolicy_version := \"1\"\n");
        assert!(p.evaluate(&json!({})).unwrap_err().0.contains("policy_id"));
        assert!(Policy::from_files(PolicySource::Directory { path: "t".into() }, None, vec![]).is_err());
    }

    #[test]
    fn policies_cannot_read_the_clock_the_network_or_randomness() {
        for call in [
            "time.now_ns()",
            "http.send({\"method\": \"get\", \"url\": \"https://example.com\"})",
            "rand.intn(\"x\", 10)",
            "uuid.rfc4122(\"x\")",
        ] {
            let p = policy(&format!(
                "{HEADER}requirements contains {{\"id\": \"a\", \"status\": \"passed\", \"reason\": sprintf(\"%v\", [{call}])}}\n"
            ));
            assert!(p.evaluate(&json!({})).is_err(), "{call} evaluated");
        }
    }

    #[test]
    fn the_digest_covers_modules_and_keys_but_not_other_files() {
        let files = |extra: &[u8]| {
            vec![
                ("p.rego".to_string(), HEADER.as_bytes().to_vec()),
                ("README.md".to_string(), extra.to_vec()),
                ("keys/ci.pub.pem".to_string(), include_bytes!("../tests/fixtures/ci.pub.pem").to_vec()),
            ]
        };
        let a = Policy::from_files(PolicySource::Directory { path: "t".into() }, None, files(b"a")).unwrap();
        let b = Policy::from_files(PolicySource::Directory { path: "t".into() }, None, files(b"b")).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.keys.len(), 1);
        assert_eq!(a.keys[0].name, "ci");
        let c = Policy::from_files(
            PolicySource::Directory { path: "t".into() },
            None,
            vec![("p.rego".to_string(), format!("{HEADER}# changed\n").into_bytes())],
        )
        .unwrap();
        assert_ne!(a.digest, c.digest);
    }
}
