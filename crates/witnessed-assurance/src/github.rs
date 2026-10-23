use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SNAPSHOT_SCHEMA: &str = "witnessed.github-pull-request/v0.1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: u64,
    pub user: Option<Account>,
    pub state: String,
    pub commit_id: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub author_login: Option<String>,
    pub committer_login: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: String,
    pub repository: String,
    pub number: u64,
    pub state: String,
    pub author: Option<Account>,
    pub head_sha: String,
    pub base_sha: String,
    pub base_ref: String,
    pub commits: Vec<Commit>,
    pub reviews: Vec<Review>,
}

fn account(v: &Value) -> Option<Account> {
    Some(Account { login: v["login"].as_str()?.to_string(), kind: v["type"].as_str().unwrap_or("").to_string() })
}

fn pages(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten().flat_map(|page| match page.as_array() {
        Some(items) => items.iter().collect::<Vec<_>>(),
        None => vec![page],
    })
}

impl Snapshot {
    pub fn from_api(repository: &str, pull: &Value, reviews: &Value, commits: &Value) -> Result<Self, String> {
        let text =
            |v: &Value, what: &str| v.as_str().map(str::to_string).ok_or_else(|| format!("pull request has no {what}"));
        Ok(Self {
            schema: SNAPSHOT_SCHEMA.to_string(),
            repository: repository.to_string(),
            number: pull["number"].as_u64().ok_or("pull request has no number")?,
            state: text(&pull["state"], "state")?,
            author: account(&pull["user"]),
            head_sha: text(&pull["head"]["sha"], "head.sha")?,
            base_sha: text(&pull["base"]["sha"], "base.sha")?,
            base_ref: text(&pull["base"]["ref"], "base.ref")?,
            commits: pages(commits)
                .filter_map(|c| {
                    Some(Commit {
                        sha: c["sha"].as_str()?.to_string(),
                        author_login: c["author"]["login"].as_str().map(str::to_string),
                        committer_login: c["committer"]["login"].as_str().map(str::to_string),
                    })
                })
                .collect(),
            reviews: pages(reviews)
                .filter_map(|r| {
                    Some(Review {
                        id: r["id"].as_u64()?,
                        user: account(&r["user"]),
                        state: r["state"].as_str()?.to_string(),
                        commit_id: r["commit_id"].as_str().map(str::to_string),
                        submitted_at: r["submitted_at"].as_str().map(str::to_string),
                    })
                })
                .collect(),
        })
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let s: Self = serde_json::from_slice(bytes).map_err(|e| format!("not a pull request snapshot: {e}"))?;
        if s.schema != SNAPSHOT_SCHEMA {
            return Err(format!("snapshot schema {:?} is not {SNAPSHOT_SCHEMA}", s.schema));
        }
        Ok(s)
    }

    pub fn facts(&self, commit: &str) -> ReviewFacts {
        let author = self.author.as_ref().map(|a| a.login.clone());
        let commit_authors: BTreeSet<&str> = self
            .commits
            .iter()
            .flat_map(|c| [c.author_login.as_deref(), c.committer_login.as_deref()])
            .flatten()
            .collect();
        let mut ordered: Vec<&Review> = self.reviews.iter().collect();
        ordered.sort_by(|a, b| (a.submitted_at.as_deref(), a.id).cmp(&(b.submitted_at.as_deref(), b.id)));
        let mut latest: BTreeMap<&str, &Review> = BTreeMap::new();
        for r in ordered {
            let Some(user) = &r.user else { continue };
            if matches!(r.state.as_str(), "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
                latest.insert(user.login.as_str(), r);
            }
        }
        let reviewers: Vec<Reviewer> = latest
            .into_iter()
            .map(|(login, r)| {
                let kind = r.user.as_ref().map_or("", |u| u.kind.as_str());
                let at_commit = r.commit_id.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(commit));
                let is_author = author.as_deref() == Some(login);
                let is_commit_author = commit_authors.contains(login);
                let human = kind == "User";
                Reviewer {
                    login: login.to_string(),
                    account_type: kind.to_string(),
                    state: r.state.clone(),
                    commit_id: r.commit_id.clone(),
                    at_commit,
                    is_author,
                    is_commit_author,
                    independent: human && !is_author && !is_commit_author,
                }
            })
            .collect();
        let independent_approvals = reviewers
            .iter()
            .filter(|r| r.state == "APPROVED" && r.at_commit && r.independent)
            .map(|r| r.login.clone())
            .collect();
        let changes_requested =
            reviewers.iter().filter(|r| r.state == "CHANGES_REQUESTED").map(|r| r.login.clone()).collect();
        ReviewFacts {
            repository: self.repository.clone(),
            number: self.number,
            state: self.state.clone(),
            author,
            head_sha: self.head_sha.clone(),
            base_sha: self.base_sha.clone(),
            head_matches_commit: self.head_sha.eq_ignore_ascii_case(commit),
            commit_authors: commit_authors.into_iter().map(str::to_string).collect(),
            reviewers,
            independent_approvals,
            changes_requested,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reviewer {
    pub login: String,
    pub account_type: String,
    pub state: String,
    pub commit_id: Option<String>,
    pub at_commit: bool,
    pub is_author: bool,
    pub is_commit_author: bool,
    pub independent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewFacts {
    pub repository: String,
    pub number: u64,
    pub state: String,
    pub author: Option<String>,
    pub head_sha: String,
    pub base_sha: String,
    pub head_matches_commit: bool,
    pub commit_authors: Vec<String>,
    pub reviewers: Vec<Reviewer>,
    pub independent_approvals: Vec<String>,
    pub changes_requested: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HEAD: &str = "2222222222222222222222222222222222222222";
    const OLD: &str = "1111111111111111111111111111111111111111";

    fn review(id: u64, login: &str, kind: &str, state: &str, commit: &str, at: &str) -> Value {
        json!({"id": id, "user": {"login": login, "type": kind}, "state": state, "commit_id": commit,
               "submitted_at": at, "body": "looks fine"})
    }

    fn snapshot(reviews: Vec<Value>) -> Snapshot {
        let pull = json!({"number": 7, "state": "open", "user": {"login": "dev", "type": "User"},
                          "head": {"sha": HEAD}, "base": {"sha": OLD, "ref": "main"}});
        let commits = json!([[{"sha": HEAD, "author": {"login": "agent-bot"}, "committer": {"login": "web-flow"}}]]);
        Snapshot::from_api("example/infra", &pull, &json!([reviews]), &commits).unwrap()
    }

    #[test]
    fn only_a_human_approval_at_the_head_from_someone_else_counts() {
        let s = snapshot(vec![
            review(1, "alice", "User", "APPROVED", HEAD, "2026-10-23T10:00:00Z"),
            review(2, "dev", "User", "APPROVED", HEAD, "2026-10-23T10:01:00Z"),
            review(3, "agent-bot", "Bot", "APPROVED", HEAD, "2026-10-23T10:02:00Z"),
            review(4, "bob", "User", "APPROVED", OLD, "2026-10-23T09:00:00Z"),
        ]);
        let f = s.facts(HEAD);
        assert_eq!(f.independent_approvals, ["alice"]);
        assert!(f.head_matches_commit);
        let dev = f.reviewers.iter().find(|r| r.login == "dev").unwrap();
        assert!(dev.is_author && !dev.independent);
        let bob = f.reviewers.iter().find(|r| r.login == "bob").unwrap();
        assert!(!bob.at_commit);
    }

    #[test]
    fn a_commit_author_is_not_an_independent_approver() {
        let s = snapshot(vec![review(1, "agent-bot", "User", "APPROVED", HEAD, "2026-10-23T10:00:00Z")]);
        let f = s.facts(HEAD);
        assert!(f.independent_approvals.is_empty());
        assert!(f.reviewers[0].is_commit_author);
    }

    #[test]
    fn the_latest_decisive_review_wins_and_comments_do_not_override() {
        let s = snapshot(vec![
            review(1, "alice", "User", "APPROVED", HEAD, "2026-10-23T10:00:00Z"),
            review(2, "alice", "User", "COMMENTED", HEAD, "2026-10-23T10:05:00Z"),
            review(3, "carol", "User", "APPROVED", HEAD, "2026-10-23T10:00:00Z"),
            review(4, "carol", "User", "DISMISSED", HEAD, "2026-10-23T10:06:00Z"),
            review(5, "erin", "User", "CHANGES_REQUESTED", HEAD, "2026-10-23T10:07:00Z"),
        ]);
        let f = s.facts(HEAD);
        assert_eq!(f.independent_approvals, ["alice"]);
        assert_eq!(f.changes_requested, ["erin"]);
    }

    #[test]
    fn a_snapshot_for_another_head_says_so_and_drops_review_bodies() {
        let s = snapshot(vec![]);
        assert!(!s.facts(OLD).head_matches_commit);
        let text = serde_json::to_string(&snapshot(vec![review(1, "a", "User", "APPROVED", HEAD, "t")])).unwrap();
        assert!(!text.contains("looks fine"));
        assert_eq!(Snapshot::parse(text.as_bytes()).unwrap().reviews.len(), 1);
        assert!(Snapshot::parse(b"{\"schema\":\"other\"}").is_err());
    }
}
