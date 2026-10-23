use std::path::Path;
use std::time::Duration;

use regex::bytes::Regex;
use serde::Serialize;
use witnessed_store::{Session, kind};

use super::assemble::{Fate, Item};
use super::redact::{drop_binary_patches, mask};
use crate::chain;

pub(crate) const MAX_DIFF_SHOWN: usize = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffStatus {
    Matches,
    Differs,
    NotCompared,
    Redacted,
    NoWorkTree,
    NoCommit,
    NoCwd,
    GitFailed,
}

impl DiffStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Matches => "matches the session end",
            Self::Differs => "differs from the session end: the work tree changed after the session",
            Self::NotCompared => "not compared: the recording has no diff hash from the session end",
            Self::Redacted => "redacted: not comparable with the session end",
            Self::NoWorkTree => "unavailable: the session did not run in a git work tree",
            Self::NoCommit => "unavailable: the repository had no commit when the session started",
            Self::NoCwd => "unavailable: the session has no working directory",
            Self::GitFailed => {
                "unavailable: git could not produce the diff in the session's working directory (it failed, timed out, or the diff or the untracked files are over 32 MiB)"
            }
        }
    }
}

impl Serialize for DiffStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl std::fmt::Display for DiffStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// The `diff` object of the manifest: a status alone when no diff was produced.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DiffInfo {
    pub status: DiffStatus,
    #[serde(flatten)]
    pub produced: Option<Produced>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Produced {
    pub base: String,
    pub binary_dropped: usize,
    pub bytes: usize,
    pub files: usize,
    pub sha256_at_exit: Option<String>,
}

pub(crate) struct SessionDiff {
    pub info: DiffInfo,
    pub bytes: Option<Vec<u8>>,
    pub matches: Vec<String>,
}

impl SessionDiff {
    const fn unavailable(status: DiffStatus) -> Self {
        Self { info: DiffInfo { status, produced: None }, bytes: None, matches: Vec::new() }
    }
}

pub(crate) fn session_diff(s: &Session, items: &[Item], patterns: &[Regex]) -> SessionDiff {
    let snapshot = |phase: &str| {
        items
            .iter()
            .find(|it| {
                it.fate != Fate::Cut && it.event.kind == kind::GIT_SNAPSHOT && it.event.payload["phase"] == phase
            })
            .map(|it| &it.event.payload)
    };
    let Some(start) = snapshot("start") else { return SessionDiff::unavailable(DiffStatus::NoWorkTree) };
    let Some(base) = start["head"].as_str().map(str::to_string) else {
        return SessionDiff::unavailable(DiffStatus::NoCommit);
    };
    let at_exit = snapshot("exit").and_then(|p| p["diff_sha256"].as_str()).map(str::to_string);
    let Some(cwd) = s.cwd.as_deref() else { return SessionDiff::unavailable(DiffStatus::NoCwd) };
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let Some(mut bytes) = witnessed_supervisor::git::worktree_diff(Path::new(cwd), &base, deadline) else {
        return SessionDiff::unavailable(DiffStatus::GitFailed);
    };
    let digest = chain::file_digest(&bytes);
    let mut status = match &at_exit {
        Some(h) if *h == digest => DiffStatus::Matches,
        Some(_) => DiffStatus::Differs,
        None => DiffStatus::NotCompared,
    };
    let mut matches: Vec<String> = mask(&mut bytes, patterns).into_iter().map(|(_, p)| p).collect();
    let mut binary_dropped = 0;
    if !patterns.is_empty() {
        (bytes, binary_dropped) = drop_binary_patches(&bytes);
        if binary_dropped > 0 {
            matches.push(format!("{binary_dropped} binary files reduced to \"Binary files differ\""));
        }
    }
    if !matches.is_empty() {
        status = DiffStatus::Redacted;
    }
    let files = bytes.split(|b| *b == b'\n').filter(|l| l.starts_with(b"diff --git ")).count();
    let produced = Produced { base, binary_dropped, bytes: bytes.len(), files, sha256_at_exit: at_exit };
    SessionDiff { info: DiffInfo { status, produced: Some(produced) }, bytes: Some(bytes), matches }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn diff_info_serializes_a_bare_status_or_the_full_record() {
        let bare = DiffInfo { status: DiffStatus::NoCommit, produced: None };
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"status": "unavailable: the repository had no commit when the session started"})
        );
        let full = DiffInfo {
            status: DiffStatus::Matches,
            produced: Some(Produced {
                base: "abc".into(),
                binary_dropped: 0,
                bytes: 12,
                files: 1,
                sha256_at_exit: None,
            }),
        };
        assert_eq!(
            serde_json::to_value(&full).unwrap(),
            json!({"status": "matches the session end", "bytes": 12, "files": 1, "base": "abc", "sha256_at_exit": null, "binary_dropped": 0})
        );
    }
}
