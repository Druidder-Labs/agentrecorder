use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use witnessed_store::{Session, Store};
use witnessed_supervisor::git::output;

const MAX_COMMITS: usize = 50;
const LOG_SCAN: &str = "--max-count=2000";
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub added: u64,
    pub in_head: u64,
    pub in_worktree: u64,
    pub files: Vec<FileOutcome>,
    pub commits: Vec<CommitOutcome>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct FileOutcome {
    pub path: String,
    pub added: u64,
    pub in_head: u64,
    pub in_worktree: u64,
    pub state: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct CommitOutcome {
    pub sha: String,
    pub subject: String,
    pub at: String,
    pub reverts: bool,
}

fn unavailable(reason: impl Into<String>) -> Outcome {
    Outcome { reason: Some(reason.into()), ..Outcome::default() }
}

fn blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

fn unquote(raw: &[u8]) -> Option<Vec<u8>> {
    let Some(inner) = raw.strip_prefix(b"\"").and_then(|r| r.strip_suffix(b"\"")) else {
        return Some(raw.to_vec());
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut it = inner.iter().copied().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next()? {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            d @ b'0'..=b'7' => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    let n = it.next_if(|c| (b'0'..=b'7').contains(c))?;
                    v = v * 8 + u32::from(n - b'0');
                }
                out.push(u8::try_from(v).ok()?);
            }
            c => out.push(c),
        }
    }
    Some(out)
}

fn new_path(rest: &[u8]) -> Option<Option<String>> {
    let rest = rest.strip_suffix(b"\t").unwrap_or(rest);
    if rest == b"/dev/null" {
        return Some(None);
    }
    let raw = unquote(rest)?;
    let path = raw.strip_prefix(b"b/").or_else(|| raw.strip_prefix(b"w/")).unwrap_or(&raw);
    let path = String::from_utf8(path.to_vec()).ok()?;
    let safe = !path.is_empty()
        && Path::new(&path).components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    Some(safe.then_some(path))
}

#[derive(Default)]
struct Section {
    path: Option<String>,
    lines: Vec<Vec<u8>>,
    binary: bool,
}

fn parse_diff(diff: &[u8]) -> Vec<(String, Vec<Vec<u8>>)> {
    let mut files: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
    let mut cur: Option<Section> = None;
    let mut in_hunk = false;
    let finish = |s: Option<Section>, files: &mut Vec<(String, Vec<Vec<u8>>)>| {
        if let Some(Section { path: Some(path), lines, binary: false }) = s
            && !lines.is_empty()
        {
            match files.iter_mut().find(|(p, _)| *p == path) {
                Some((_, l)) => l.extend(lines),
                None => files.push((path, lines)),
            }
        }
    };
    for line in diff.split(|b| *b == b'\n') {
        if line.starts_with(b"diff --git ") {
            finish(cur.take(), &mut files);
            cur = Some(Section::default());
            in_hunk = false;
            continue;
        }
        let Some(sec) = cur.as_mut() else { continue };
        if in_hunk {
            match line.first() {
                Some(b'+') => {
                    let content = &line[1..];
                    if !blank(content) {
                        sec.lines.push(content.to_vec());
                    }
                    continue;
                }
                Some(b' ' | b'-' | b'\\') => continue,
                _ => in_hunk = false,
            }
        }
        if line.starts_with(b"@@") {
            in_hunk = true;
        } else if let Some(rest) = line.strip_prefix(b"+++ ") {
            sec.path = new_path(rest).flatten();
        } else if line == b"GIT binary patch" || (line.starts_with(b"Binary files ") && line.ends_with(b" differ")) {
            sec.binary = true;
        }
    }
    finish(cur.take(), &mut files);
    files
}

fn kept(added: &[Vec<u8>], content: Option<&[u8]>) -> u64 {
    let Some(content) = content else { return 0 };
    let mut have: HashMap<&[u8], u64> = HashMap::new();
    for l in content.split(|b| *b == b'\n').filter(|l| !blank(l)) {
        *have.entry(l).or_default() += 1;
    }
    added
        .iter()
        .filter(|l| match have.get_mut(l.as_slice()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        })
        .count() as u64
}

fn text(out: Vec<u8>) -> Option<String> {
    String::from_utf8(out).ok().map(|s| s.trim_end_matches('\n').to_string())
}

fn head_blobs(top: &Path, deadline: Instant) -> Option<HashMap<String, String>> {
    let out = output(top, &["-c", "core.quotepath=off", "ls-tree", "-r", "-z", "--full-tree", "HEAD"], deadline)?;
    Some(
        out.split(|b| *b == 0)
            .filter_map(|e| {
                let e = std::str::from_utf8(e).ok()?;
                let (meta, path) = e.split_once('\t')?;
                let mut f = meta.split(' ');
                let (_, kind, oid) = (f.next()?, f.next()?, f.next()?);
                (kind == "blob").then(|| (path.to_string(), oid.to_string()))
            })
            .collect(),
    )
}

struct LogEntry {
    commit: CommitOutcome,
    ct: i64,
    paths: Vec<(String, Option<String>)>,
}

fn parse_log(out: &[u8]) -> Vec<LogEntry> {
    out.split(|b| *b == 1)
        .skip(1)
        .filter_map(|chunk| {
            let mut tokens =
                chunk.split(|b| *b == 0).map(|t| String::from_utf8_lossy(t).trim_matches('\n').to_string());
            let sha = tokens.next()?;
            let ct = tokens.next()?.parse().ok()?;
            let at = tokens.next()?;
            let subject = tokens.next()?;
            let body = tokens.next().unwrap_or_default();
            let reverts = subject.starts_with("Revert \"") || body.contains("This reverts commit");
            let mut paths = Vec::new();
            let mut rest = tokens.filter(|t| !t.is_empty());
            while let Some(status) = rest.next() {
                let Some(first) = rest.next() else { break };
                if status.starts_with('R') || status.starts_with('C') {
                    let second = rest.next();
                    paths.push((first, second));
                } else {
                    paths.push((first, None));
                }
            }
            Some(LogEntry { commit: CommitOutcome { sha, subject, at, reverts }, ct, paths })
        })
        .collect()
}

fn session_log(top: &Path, since_ns: i64, deadline: Instant) -> Option<Vec<LogEntry>> {
    let since_s = since_ns.div_euclid(1_000_000_000);
    let since = format!("--since={since_s} +0000");
    let out = output(
        top,
        &[
            "-c",
            "core.quotepath=off",
            "log",
            LOG_SCAN,
            &since,
            "-z",
            "-M",
            "--name-status",
            "--no-color",
            "--format=%x01%H%x00%ct%x00%cI%x00%s%x00%b%x00",
            "HEAD",
        ],
        deadline,
    )?;
    Some(parse_log(&out).into_iter().filter(|e| e.ct >= since_s).collect())
}

pub fn measure(cwd: &Path, base: &str, diff: &[u8], since_ns: i64, deadline: Instant) -> Outcome {
    let Some(top) = output(cwd, &["rev-parse", "--show-toplevel"], deadline).and_then(text).map(PathBuf::from) else {
        return unavailable("the session directory is no longer in a git work tree");
    };
    let head = output(&top, &["rev-parse", "--verify", "-q", "HEAD"], deadline).and_then(text);
    let files = parse_diff(diff);
    let mut out = Outcome { available: true, base: Some(base.to_string()), head: head.clone(), ..Outcome::default() };
    if files.is_empty() {
        return out;
    }
    let blobs = match head {
        Some(_) => match head_blobs(&top, deadline) {
            Some(b) => b,
            None => return unavailable("listing the files in HEAD failed or timed out"),
        },
        None => HashMap::new(),
    };
    let log = match head {
        Some(_) => match session_log(&top, since_ns, deadline) {
            Some(l) => l,
            None => return unavailable("reading the commits since the session failed or timed out"),
        },
        None => Vec::new(),
    };
    let renamed: HashMap<&str, &str> = log
        .iter()
        .rev()
        .flat_map(|e| e.paths.iter())
        .filter_map(|(from, to)| Some((from.as_str(), to.as_deref()?)))
        .collect();
    let mut touched: HashSet<String> = HashSet::new();
    for (path, lines) in &files {
        touched.insert(path.clone());
        let mut current = path.as_str();
        let mut hops = 0;
        let mut state = None;
        let (head_blob, work) = loop {
            let head_blob = blobs.get(current);
            let work = std::fs::read(top.join(current)).ok();
            if head_blob.is_some() || work.is_some() || hops > 16 {
                break (head_blob, work);
            }
            match renamed.get(current) {
                Some(next) => {
                    current = next;
                    touched.insert(current.to_string());
                    state = Some("renamed");
                    hops += 1;
                }
                None => break (None, None),
            }
        };
        let head_content = match head_blob {
            Some(oid) => match output(&top, &["cat-file", "blob", oid], deadline) {
                Some(c) => Some(c),
                None => return unavailable(format!("reading {current} from HEAD failed or timed out")),
            },
            None => None,
        };
        let state = state.unwrap_or(match (&head_content, &work) {
            (Some(_), _) => "present",
            (None, Some(_)) => "uncommitted",
            (None, None) => "deleted",
        });
        let f = FileOutcome {
            path: path.clone(),
            added: lines.len() as u64,
            in_head: kept(lines, head_content.as_deref()),
            in_worktree: kept(lines, work.as_deref()),
            state: state.to_string(),
        };
        out.added += f.added;
        out.in_head += f.in_head;
        out.in_worktree += f.in_worktree;
        out.files.push(f);
    }
    out.commits = log
        .into_iter()
        .filter(|e| e.paths.iter().any(|(a, b)| touched.contains(a) || b.as_ref().is_some_and(|b| touched.contains(b))))
        .take(MAX_COMMITS)
        .map(|e| e.commit)
        .collect();
    out
}

pub fn for_session(store: &Store, s: &Session) -> Outcome {
    let row = match store.session_diff(&s.id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            return unavailable(
                "no diff stored at session end (recorded before this version, outside a git work tree, with --no-record, or over 8 MiB)",
            );
        }
        Err(e) => return unavailable(format!("reading the stored diff failed: {e:#}")),
    };
    let Some(cwd) = s.cwd.as_deref() else { return unavailable("the session has no working directory") };
    measure(Path::new(cwd), &row.base, &row.diff, s.started_at_ns.unwrap_or(s.created_at_ns), Instant::now() + TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const SINCE: i64 = 1_000_000_000 * 1_000_000_000;

    fn git(dir: &Path, args: &[&str]) -> String {
        git_env(dir, args, &[])
    }

    fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = Command::new("git")
            .envs(env.iter().copied())
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=a@b", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
            .args(args)
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    struct Repo {
        _dir: tempfile::TempDir,
        path: PathBuf,
        base: String,
    }

    fn repo(files: &[(&str, &str)]) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        git(&path, &["init", "-q"]);
        std::fs::write(path.join("seed.txt"), "seed\n").unwrap();
        for (name, body) in files {
            std::fs::write(path.join(name), body).unwrap();
        }
        git(&path, &["add", "-A"]);
        let old = "2000-01-01T00:00:00Z";
        git_env(&path, &["commit", "-q", "-m", "init"], &[("GIT_AUTHOR_DATE", old), ("GIT_COMMITTER_DATE", old)]);
        let base = git(&path, &["rev-parse", "HEAD"]);
        Repo { _dir: dir, path, base }
    }

    fn session_diff(r: &Repo) -> Vec<u8> {
        witnessed_supervisor::git::worktree_diff(&r.path, &r.base, later()).unwrap()
    }

    fn file<'a>(o: &'a Outcome, path: &str) -> &'a FileOutcome {
        o.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} in {o:?}"))
    }

    #[test]
    fn kept_lines_count_in_head_and_worktree_and_blank_lines_are_ignored() {
        let r = repo(&[("a.txt", "one\n")]);
        std::fs::write(r.path.join("a.txt"), "one\ntwo\n\n   \nthree\ntwo\n").unwrap();
        let diff = session_diff(&r);
        git(&r.path, &["commit", "-qam", "keep"]);
        let o = measure(&r.path, &r.base, &diff, SINCE, later());
        assert!(o.available, "{o:?}");
        assert_eq!((o.added, o.in_head, o.in_worktree), (3, 3, 3));
        assert_eq!(file(&o, "a.txt").state, "present");
        assert_eq!(o.head.as_deref(), Some(git(&r.path, &["rev-parse", "HEAD"]).as_str()));
        assert_eq!(o.commits.len(), 1);
        assert_eq!(o.commits[0].subject, "keep");
        assert!(!o.commits[0].reverts);
    }

    #[test]
    fn partially_rewritten_lines_count_once_per_surviving_copy() {
        let r = repo(&[("a.txt", "one\n")]);
        std::fs::write(r.path.join("a.txt"), "one\nx = 1\nx = 1\ny = 2\n").unwrap();
        let diff = session_diff(&r);
        std::fs::write(r.path.join("a.txt"), "one\nx = 1\ny = 3\n").unwrap();
        git(&r.path, &["commit", "-qam", "rewrite"]);
        std::fs::write(r.path.join("a.txt"), "one\nz\n").unwrap();
        let o = measure(&r.path, &r.base, &diff, SINCE, later());
        let f = file(&o, "a.txt");
        assert_eq!((f.added, f.in_head, f.in_worktree, f.state.as_str()), (3, 1, 0, "present"));
    }

    #[test]
    fn deleted_and_uncommitted_files() {
        let r = repo(&[]);
        std::fs::write(r.path.join("gone.txt"), "temporary\n").unwrap();
        std::fs::write(r.path.join("new.txt"), "fresh\n").unwrap();
        let diff = session_diff(&r);
        std::fs::remove_file(r.path.join("gone.txt")).unwrap();
        let o = measure(&r.path, &r.base, &diff, SINCE, later());
        assert_eq!(file(&o, "gone.txt").state, "deleted");
        let n = file(&o, "new.txt");
        assert_eq!((n.state.as_str(), n.in_head, n.in_worktree), ("uncommitted", 0, 1));
        assert!(o.commits.is_empty());
    }

    #[test]
    fn revert_commits_are_flagged_newest_first_and_unrelated_commits_skipped() {
        let r = repo(&[("a.txt", "one\n")]);
        std::fs::write(r.path.join("a.txt"), "one\nadded\n").unwrap();
        let diff = session_diff(&r);
        git(&r.path, &["commit", "-qam", "add line"]);
        std::fs::write(r.path.join("other.txt"), "x\n").unwrap();
        git(&r.path, &["add", "other.txt"]);
        git(&r.path, &["commit", "-qm", "unrelated"]);
        git(&r.path, &["revert", "--no-edit", "HEAD~1"]);
        let o = measure(&r.path, &r.base, &diff, SINCE, later());
        assert_eq!((o.added, o.in_head, o.in_worktree), (1, 0, 0));
        let subjects: Vec<_> = o.commits.iter().map(|c| (c.subject.as_str(), c.reverts)).collect();
        assert_eq!(subjects, [("Revert \"add line\"", true), ("add line", false)]);
        assert!(o.commits.iter().all(|c| c.sha.len() == 40 && c.at.contains('T')));
        let future = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
            + 3600)
            * 1_000_000_000;
        assert!(measure(&r.path, &r.base, &diff, future, later()).commits.is_empty());
    }

    #[test]
    fn binary_patches_are_skipped_and_odd_paths_survive() {
        let r = repo(&[]);
        std::fs::write(r.path.join("blob.bin"), [0u8, 1, 2, 255, b'\n', b'+', b'x']).unwrap();
        std::fs::write(r.path.join("with space.txt"), "spaced\n").unwrap();
        std::fs::write(r.path.join(":magic.txt"), "colon\n").unwrap();
        let diff = session_diff(&r);
        assert!(String::from_utf8_lossy(&diff).contains("GIT binary patch"));
        git(&r.path, &["add", "-A"]);
        git(&r.path, &["commit", "-qm", "odd"]);
        let o = measure(&r.path, &r.base, &diff, SINCE, later());
        assert!(o.files.iter().all(|f| f.path != "blob.bin"), "{o:?}");
        for p in ["with space.txt", ":magic.txt"] {
            let f = file(&o, p);
            assert_eq!((f.added, f.in_head, f.in_worktree, f.state.as_str()), (1, 1, 1, "present"));
        }
        assert_eq!(o.commits.len(), 1);
    }

    #[test]
    fn renamed_files_follow_the_commit_log() {
        let r = repo(&[("a.txt", "one\n")]);
        std::fs::write(r.path.join("a.txt"), "one\nkept after rename\n").unwrap();
        let diff = session_diff(&r);
        git(&r.path, &["commit", "-qam", "add"]);
        git(&r.path, &["mv", "a.txt", "b.txt"]);
        git(&r.path, &["commit", "-qm", "move"]);
        let o = measure(&r.path, &r.base, &diff, SINCE, later());
        let f = file(&o, "a.txt");
        assert_eq!((f.state.as_str(), f.in_head, f.in_worktree), ("renamed", 1, 1));
        assert_eq!(o.commits.len(), 2);
    }

    #[test]
    fn diff_parsing_handles_quotes_deletions_and_plus_lines() {
        let diff = b"diff --git a/x b/x\ndeleted file mode 100644\n--- a/x\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n\
diff --git \"a/t\\tab\" \"b/t\\tab\"\nnew file mode 100644\n--- /dev/null\n+++ \"b/t\\tab\"\n@@ -0,0 +1,2 @@\n+++ looks like a header\n+\n\
diff --git a/../evil b/../evil\n--- a/../evil\n+++ b/../evil\n@@ -0,0 +1 @@\n+x\n\
diff --git a/i.png b/i.png\nBinary files a/i.png and b/i.png differ\n";
        let files = parse_diff(diff);
        assert_eq!(files, [("t\tab".to_string(), vec![b"++ looks like a header".to_vec()])]);
    }

    #[test]
    fn failures_become_unavailable_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let o = measure(&dir.path().join("missing"), "abc", b"", 0, later());
        assert!(!o.available && o.reason.is_some());
        let r = repo(&[("a.txt", "one\n")]);
        std::fs::write(r.path.join("a.txt"), "one\ntwo\n").unwrap();
        let diff = session_diff(&r);
        let o = measure(&r.path, &r.base, &diff, SINCE, Instant::now());
        assert!(!o.available && o.reason.is_some());
    }

    #[test]
    fn for_session_reads_the_stored_diff() {
        let r = repo(&[("a.txt", "one\n")]);
        std::fs::write(r.path.join("a.txt"), "one\ntwo\n").unwrap();
        let diff = session_diff(&r);
        let db = tempfile::tempdir().unwrap();
        let store = Store::open(&db.path().join("s.db")).unwrap();
        let cwd = r.path.to_string_lossy().into_owned();
        let command = vec!["sh".to_string()];
        store
            .create_session(&witnessed_store::NewSession {
                id: "s1",
                command: &command,
                cwd: Some(&cwd),
                created_at_ns: 0,
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
        let s = store.session("s1").unwrap().unwrap();
        let o = for_session(&store, &s);
        assert!(!o.available && o.reason.as_deref().unwrap().starts_with("no diff stored"));
        store.put_session_diff("s1", &r.base, &diff, 1).unwrap();
        let o = for_session(&store, &s);
        assert!(o.available, "{o:?}");
        assert_eq!((o.added, o.in_head, o.in_worktree), (1, 0, 1));
    }
}
