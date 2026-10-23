use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use witnessed_store::{Body, ChangedPath, GIT_MAX_PATHS, GitCommit, GitPhase, Store, now_unix_ns};

use crate::recorder::Recorder;

pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const DIFF_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_STORED_DIFF_BYTES: usize = 8 << 20;
const GIT_MAX_COMMITS: usize = 100;

fn hex_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

struct GitInfo {
    head: Option<String>,
    dirty: Vec<String>,
    changed: Option<Vec<ChangedPath>>,
    commits: Option<Vec<GitCommit>>,
    commit_count: Option<u64>,
    root: String,
}

fn parse_log(out: &[u8]) -> Vec<GitCommit> {
    String::from_utf8_lossy(out)
        .split('\x1e')
        .filter_map(|rec| {
            let (hash, subject) = rec.trim_start_matches('\n').split_once('\x1f')?;
            let subject: String = subject.trim_end().chars().take(200).collect();
            (!hash.is_empty()).then(|| GitCommit { hash: hash.to_string(), subject })
        })
        .collect()
}

fn git_snapshot(cwd: &Path, since: Option<&str>, deadline: Instant) -> Option<GitInfo> {
    let top = crate::git::output(cwd, &["rev-parse", "--show-toplevel"], deadline)
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim_end_matches('\n')))?;
    let run = |args: &[&str]| -> Option<Vec<u8>> {
        let mut all = vec!["-c", "core.quotepath=false"];
        all.extend_from_slice(args);
        crate::git::output(&top, &all, deadline)
    };
    let text = |b: Vec<u8>| String::from_utf8_lossy(&b).into_owned();
    if text(run(&["rev-parse", "--is-inside-work-tree"])?).trim() != "true" {
        return None;
    }
    let head = run(&["rev-parse", "HEAD"]).map(|s| text(s).trim().to_string());
    let status = run(&["status", "--porcelain=v1", "-z"]);
    let dirty = status.as_deref().map(parse_porcelain_z).unwrap_or_default();
    let changed = since.and_then(|base| {
        let mut changed = parse_name_status_z(&run(&["diff", "--name-status", "-z", base])?);
        let untracked = text(run(&["ls-files", "--others", "--exclude-standard", "-z"])?);
        changed.extend(
            untracked
                .split('\0')
                .filter(|p| !p.is_empty())
                .map(|p| ChangedPath { status: "?".into(), path: p.to_string() }),
        );
        changed.sort_by(|a, b| a.path.cmp(&b.path));
        changed.dedup_by(|a, b| a.path == b.path);
        Some(changed)
    });
    let range = since.map(|base| format!("{base}..HEAD"));
    let commit_count = range
        .as_deref()
        .and_then(|r| run(&["rev-list", "--count", r]))
        .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<u64>().ok());
    let max = format!("--max-count={GIT_MAX_COMMITS}");
    let commits = range
        .as_deref()
        .and_then(|r| run(&["log", "--no-show-signature", &max, "--format=%H%x1f%s%x1e", r]))
        .map(|o| parse_log(&o));
    Some(GitInfo { head, dirty, changed, commits, commit_count, root: top.to_string_lossy().into_owned() })
}

fn cap<T>(mut v: Vec<T>) -> Vec<T> {
    v.truncate(GIT_MAX_PATHS);
    v
}

fn parse_porcelain_z(out: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = String::from_utf8_lossy(out).split('\0').map(str::to_string).collect::<Vec<_>>().into_iter();
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = entry.split_at(3);
        if xy.contains('R') || xy.contains('C') {
            entries.next();
        }
        paths.push(path.to_string());
    }
    paths.sort();
    paths
}

fn parse_name_status_z(out: &[u8]) -> Vec<ChangedPath> {
    let mut changed = Vec::new();
    let mut fields = String::from_utf8_lossy(out).split('\0').map(str::to_string).collect::<Vec<_>>().into_iter();
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let Some(path) = fields.next() else { break };
        let letter = status.chars().next().unwrap_or('M');
        let path = if matches!(letter, 'R' | 'C') { fields.next().unwrap_or(path) } else { path };
        changed.push(ChangedPath { status: letter.to_string(), path });
    }
    changed
}

pub(crate) fn record_git(
    recorder: &Recorder,
    cwd: Option<PathBuf>,
    phase: GitPhase,
    start_head: &Arc<OnceLock<Option<String>>>,
) -> Option<(String, Vec<u8>)> {
    let cwd = cwd?;
    let (tx, rx) = bounded(1);
    // The start phase always sets the lock (a None on failure) within GIT_TIMEOUT, so the wait is bounded.
    let since = match phase {
        GitPhase::Start => None,
        GitPhase::Exit => start_head.wait().clone(),
    };
    let (since_for_diff, diff_cwd) = (since.clone(), cwd.clone());
    std::thread::spawn(move || {
        let _ = tx.send(git_snapshot(&cwd, since.as_deref(), Instant::now() + GIT_TIMEOUT));
    });
    let Ok(Some(info)) = rx.recv_timeout(GIT_TIMEOUT) else {
        if phase == GitPhase::Start {
            let _ = start_head.set(None);
        }
        return None;
    };
    if phase == GitPhase::Start {
        let _ = start_head.set(info.head.clone());
    }
    let diff = match (phase, since_for_diff) {
        (GitPhase::Exit, Some(base)) => {
            crate::git::worktree_diff(&diff_cwd, &base, Instant::now() + DIFF_TIMEOUT).map(|d| (base, d))
        }
        _ => None,
    };
    recorder.event(Body::GitSnapshot {
        phase,
        head: info.head,
        dirty_paths: Some(info.dirty.len() as u64),
        dirty: cap(info.dirty),
        changed_paths: info.changed.as_ref().map(|c| c.len() as u64),
        changed: info.changed.map(cap).unwrap_or_default(),
        new_commits: info.commit_count.or(info.commits.as_ref().map(|c| c.len() as u64)),
        commits: info
            .commits
            .map(|mut c| {
                c.truncate(GIT_MAX_COMMITS);
                c
            })
            .unwrap_or_default(),
        root: Some(info.root),
        diff_sha256: diff.as_ref().map(|(_, d)| hex_sha256(d)),
        diff_bytes: diff.as_ref().map(|(_, d)| d.len() as u64),
    });
    diff
}

pub(crate) fn store_session_diff(db: &Path, id: &str, base: &str, diff: &[u8]) {
    if diff.len() > MAX_STORED_DIFF_BYTES {
        return;
    }
    if let Err(e) = Store::open(db).and_then(|s| s.put_session_diff(id, base, diff, now_unix_ns())) {
        eprintln!("witnessed supervisor: failed to store the session diff: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_paths_skip_rename_sources_and_sort() {
        let out = b" M b.rs\0R  new.rs\0old.rs\0?? a.txt\0";
        assert_eq!(parse_porcelain_z(out), ["a.txt", "b.rs", "new.rs"]);
        assert_eq!(parse_porcelain_z(b""), [] as [std::string::String; 0]);
    }

    #[test]
    fn name_status_keeps_the_letter_and_the_new_path_of_a_rename() {
        let out = b"M\0a.rs\0R100\0old.rs\0new.rs\0D\0gone.rs\0";
        let got: Vec<(String, String)> = parse_name_status_z(out).into_iter().map(|c| (c.status, c.path)).collect();
        let want = [("M", "a.rs"), ("R", "new.rs"), ("D", "gone.rs")];
        assert_eq!(got, want.map(|(s, p)| (s.to_string(), p.to_string())));
    }

    #[test]
    fn log_records_split_into_hash_and_subject() {
        let out = b"abc\x1ffix: one\x1e\ndef\x1ffeat: two words\x1e\n";
        let got: Vec<(String, String)> = parse_log(out).into_iter().map(|c| (c.hash, c.subject)).collect();
        assert_eq!(
            got,
            [("abc".to_string(), "fix: one".to_string()), ("def".to_string(), "feat: two words".to_string())]
        );
        assert_eq!(parse_log(b""), [] as [witnessed_store::GitCommit; 0]);
    }
}
