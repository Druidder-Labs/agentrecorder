pub(crate) mod snapshot;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

pub const DIFF_FILE: &str = "diff.patch";
pub const MAX_DIFF_BYTES: usize = 32 << 20;

struct Run<'a> {
    index: Option<&'a Path>,
    input: Option<&'a [u8]>,
    deadline: Instant,
    cap: usize,
}

fn git(cwd: &Path, args: &[&str], run: &Run) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if run.input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(index) = run.index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd.spawn().ok()?;
    if let Some(data) = run.input {
        let mut stdin = child.stdin.take()?;
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take()?;
    let cap = run.cap;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let ok = (&mut stdout).take(cap as u64 + 1).read_to_end(&mut buf).is_ok();
        let _ = tx.send(ok.then_some(buf));
    });
    let timeout = run.deadline.saturating_duration_since(Instant::now());
    let out = match rx.recv_timeout(timeout) {
        Ok(Some(buf)) if buf.len() <= cap => buf,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let status = child.wait().ok()?;
    status.success().then_some(out)
}

/// Runs git in `cwd` and returns stdout, or None when git fails or the deadline passes; git is killed then.
#[must_use]
pub fn output(cwd: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    git(cwd, args, &Run { index: None, input: None, deadline, cap: 64 << 20 })
}

struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("lock"));
    }
}

fn line(out: Vec<u8>) -> Option<String> {
    let s = String::from_utf8(out).ok()?;
    Some(s.trim_end_matches('\n').to_string())
}

/// The work tree against `base`, from the repository's top level, with untracked files as new files.
/// Returns None when git fails, the deadline passes or the diff exceeds `MAX_DIFF_BYTES`.
pub fn worktree_diff(cwd: &Path, base: &str, deadline: Instant) -> Option<Vec<u8>> {
    let small = Run { index: None, input: None, deadline, cap: 1 << 20 };
    let top = PathBuf::from(line(git(cwd, &["rev-parse", "--show-toplevel"], &small)?)?);
    let real_index = top.join(line(git(&top, &["rev-parse", "--git-path", "index"], &small)?)?);
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = TempIndex(std::env::temp_dir().join(format!("witnessed-index-{}-{nanos}-{serial}", std::process::id())));
    if real_index.is_file() {
        std::fs::copy(&real_index, &tmp.0).ok()?;
    }
    let with_index = Run { index: Some(&tmp.0), input: None, deadline, cap: MAX_DIFF_BYTES };
    let untracked = git(&top, &["ls-files", "--others", "--exclude-standard", "-z"], &with_index)?;
    let untracked_bytes: u64 = untracked
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::fs::symlink_metadata(top.join(String::from_utf8_lossy(p).as_ref())).ok())
        .map(|m| m.len())
        .sum();
    if untracked_bytes > MAX_DIFF_BYTES as u64 {
        return None;
    }
    if !untracked.is_empty() {
        let add = Run { input: Some(&untracked), ..with_index };
        git(
            &top,
            &[
                "--literal-pathspecs",
                "-c",
                "core.splitIndex=false",
                "add",
                "--intent-to-add",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            &add,
        )?;
    }
    git(
        &top,
        &[
            "-c",
            "core.quotepath=false",
            "-c",
            "core.splitIndex=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            "--",
        ],
        &with_index,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=a@b", "-c", "user.name=t"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn covers_the_whole_repository_from_a_subdirectory_without_touching_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        run(repo, &["init", "-q"]);
        std::fs::create_dir(repo.join("docs")).unwrap();
        std::fs::write(repo.join("README.md"), "one\n").unwrap();
        std::fs::write(repo.join("docs/guide.md"), "guide\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "-m", "init"]);
        let base = String::from_utf8(
            Command::new("git").arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(repo.join("README.md"), "one\ntwo\n").unwrap();
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("root new.txt"), "at the root\n").unwrap();
        std::fs::write(repo.join("docs/new.md"), "in docs\n").unwrap();
        std::fs::write(repo.join("docs/bin.dat"), [0u8, 1, 2, 255]).unwrap();
        std::fs::write(repo.join("latin.txt"), b"caf\xe9\n").unwrap();
        std::fs::write(repo.join(":magic.txt"), "colon\n").unwrap();
        let index_before = std::fs::read(repo.join(".git/index")).unwrap();

        let from_sub = worktree_diff(&repo.join("docs"), &base, later()).unwrap();
        let from_top = worktree_diff(repo, &base, later()).unwrap();
        assert_eq!(from_sub, from_top);
        assert!(from_top.windows(4).any(|w| w == b"caf\xe9"), "raw bytes are kept");
        let text = String::from_utf8_lossy(&from_top);
        for header in [
            "diff --git a/README.md b/README.md",
            "diff --git a/docs/bin.dat b/docs/bin.dat",
            "diff --git a/docs/new.md b/docs/new.md",
            "diff --git a/gone.txt b/gone.txt",
            "diff --git a/root new.txt b/root new.txt",
            "diff --git a/:magic.txt b/:magic.txt",
        ] {
            assert!(text.contains(header), "{header} in {text}");
        }
        assert!(text.contains("+two") && text.contains("+at the root") && text.contains("GIT binary patch"));
        run(repo, &["config", "diff.noprefix", "true"]);
        run(repo, &["config", "diff.mnemonicPrefix", "true"]);
        assert_eq!(worktree_diff(repo, &base, later()).unwrap(), from_top, "prefixes do not follow the user's config");
        run(repo, &["config", "--unset", "diff.noprefix"]);
        assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index_before);
        let status = Command::new("git").arg("-C").arg(repo).args(["status", "--porcelain"]).output().unwrap().stdout;
        assert!(String::from_utf8_lossy(&status).contains("?? docs/new.md"));
        assert!(worktree_diff(&dir.path().join("missing"), &base, later()).is_none());
        assert!(worktree_diff(repo, &base, Instant::now()).is_none(), "a passed deadline gives no diff");
        std::fs::write(repo.join("huge.bin"), vec![7u8; MAX_DIFF_BYTES + 1]).unwrap();
        let started = Instant::now();
        assert!(worktree_diff(repo, &base, later()).is_none(), "untracked files over the cap give no diff");
        assert!(started.elapsed() < Duration::from_secs(2), "the cap is checked before running the diff");
        std::fs::remove_file(repo.join("huge.bin")).unwrap();
        let leftovers = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with(&format!("witnessed-index-{}-", std::process::id()))
            })
            .count();
        assert_eq!(leftovers, 0, "temporary indexes are removed");
    }
}
