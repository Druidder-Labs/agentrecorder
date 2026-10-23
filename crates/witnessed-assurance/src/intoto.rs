use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATEMENT_V1: &str = "https://in-toto.io/Statement/v1";
pub const TEST_RESULT_V0_1: &str = "https://in-toto.io/attestation/test-result/v0.1";
pub const CHANGE_VERIFICATION_V0_1: &str =
    "https://github.com/Druidder-Labs/witnessed/blob/main/docs/assurance/predicates/change-verification-v0.1.md";
pub const GIT_COMMIT: &str = "gitCommit";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub kind: String,
    pub subject: Vec<ResourceDescriptor>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    #[serde(default)]
    pub predicate: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub digest: BTreeMap<String, String>,
}

impl ResourceDescriptor {
    pub fn commit(name: &str, sha: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            uri: None,
            digest: BTreeMap::from([(GIT_COMMIT.to_string(), sha.to_ascii_lowercase())]),
        }
    }
}

impl Statement {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let s: Self = serde_json::from_slice(bytes).map_err(|e| format!("not an in-toto Statement: {e}"))?;
        if s.kind != STATEMENT_V1 {
            return Err(format!("statement type {:?} is not {STATEMENT_V1}", s.kind));
        }
        if s.subject.is_empty() {
            return Err("statement has no subject".into());
        }
        Ok(s)
    }

    pub fn names_commit(&self, sha: &str) -> bool {
        self.subject.iter().any(|d| d.digest.get(GIT_COMMIT).is_some_and(|c| c.eq_ignore_ascii_case(sha)))
    }

    pub fn subject_commits(&self) -> Vec<String> {
        self.subject.iter().filter_map(|d| d.digest.get(GIT_COMMIT).map(|c| c.to_ascii_lowercase())).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TestOutcome {
    Passed,
    Warned,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub result: TestOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configuration: Vec<ResourceDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub passed_tests: Vec<String>,
    #[serde(default)]
    pub warned_tests: Vec<String>,
    #[serde(default)]
    pub failed_tests: Vec<String>,
}

impl TestResult {
    pub fn from_predicate(v: &Value) -> Result<Self, String> {
        serde_json::from_value(v.clone()).map_err(|e| format!("not a test-result v0.1 predicate: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SHA: &str = "1111111111111111111111111111111111111111";

    fn statement(subject: Value) -> Value {
        json!({"_type": STATEMENT_V1, "subject": subject, "predicateType": TEST_RESULT_V0_1,
               "predicate": {"result": "PASSED", "passedTests": ["terraform-validate"]}})
    }

    #[test]
    fn a_statement_names_a_commit_only_through_the_git_commit_digest() {
        let s =
            Statement::parse(statement(json!([{"digest": {"gitCommit": SHA.to_uppercase()}}])).to_string().as_bytes())
                .unwrap();
        assert!(s.names_commit(SHA));
        assert_eq!(s.subject_commits(), [SHA]);
        let s = Statement::parse(statement(json!([{"digest": {"sha1": SHA}}])).to_string().as_bytes()).unwrap();
        assert!(!s.names_commit(SHA));
        let short =
            Statement::parse(statement(json!([{"digest": {"gitCommit": &SHA[..12]}}])).to_string().as_bytes()).unwrap();
        assert!(!short.names_commit(SHA));
    }

    #[test]
    fn other_statement_types_and_empty_subjects_are_refused() {
        let mut v = statement(json!([{"digest": {"gitCommit": SHA}}]));
        v["_type"] = json!("https://in-toto.io/Statement/v0.1");
        assert!(Statement::parse(v.to_string().as_bytes()).unwrap_err().contains("is not"));
        assert!(Statement::parse(statement(json!([])).to_string().as_bytes()).unwrap_err().contains("no subject"));
    }

    #[test]
    fn the_test_result_predicate_parses_and_rejects_unknown_outcomes() {
        let v = statement(json!([{"digest": {"gitCommit": SHA}}]));
        let t = TestResult::from_predicate(&v["predicate"]).unwrap();
        assert_eq!(
            (t.result, t.passed_tests.as_slice()),
            (TestOutcome::Passed, ["terraform-validate".to_string()].as_slice())
        );
        assert!(TestResult::from_predicate(&json!({"result": "OK"})).is_err());
    }
}
