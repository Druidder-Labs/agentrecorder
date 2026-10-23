use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TerraformFile {
    pub path: String,
    pub scanned: bool,
    pub block_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeSet {
    pub commit: String,
    pub base: String,
    pub merge_base: String,
    pub files: Vec<ChangedFile>,
    pub terraform: Vec<TerraformFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError(pub String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| GitError(format!("running git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(GitError(format!("git {}: {}", args.join(" "), err.trim())));
    }
    Ok(out.stdout)
}

fn text(out: Vec<u8>) -> Result<String, GitError> {
    String::from_utf8(out)
        .map(|s| s.trim_end().to_string())
        .map_err(|_| GitError("git printed non-UTF-8 output".into()))
}

pub fn resolve_commit(repo: &Path, rev: &str) -> Result<String, GitError> {
    let sha = text(git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")])?)?;
    if !(sha.len() == 40 || sha.len() == 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitError(format!("{rev} did not resolve to a full commit id")));
    }
    Ok(sha)
}

pub fn change_set(repo: &Path, commit: &str, base: &str) -> Result<ChangeSet, GitError> {
    let commit = resolve_commit(repo, commit)?;
    let base = resolve_commit(repo, base)?;
    let merge_base = text(git(repo, &["merge-base", &base, &commit])?)?;
    let raw = git(
        repo,
        &["diff", "--name-status", "-z", "-M", "--no-ext-diff", "--no-textconv", &merge_base, &commit, "--"],
    )?;
    let files = parse_name_status(&raw)?;
    let mut terraform = Vec::new();
    let is_tf = |p: &str| p.ends_with(".tf") || p.ends_with(".tf.json");
    for f in files.iter().filter(|f| is_tf(&f.path) || f.old_path.as_deref().is_some_and(is_tf)) {
        let scanned = !f.path.ends_with(".tf.json") && !f.old_path.as_deref().is_some_and(|p| p.ends_with(".tf.json"));
        if !scanned {
            terraform.push(TerraformFile { path: f.path.clone(), scanned, block_types: Vec::new() });
            continue;
        }
        let mut types = std::collections::BTreeSet::new();
        if f.status != "D" {
            types.extend(crate::terraform::block_types(&blob(repo, &commit, &f.path)?));
        }
        if f.status != "A" {
            let before = f.old_path.as_deref().unwrap_or(&f.path);
            types.extend(crate::terraform::block_types(&blob(repo, &merge_base, before)?));
        }
        terraform.push(TerraformFile { path: f.path.clone(), scanned, block_types: types.into_iter().collect() });
    }
    Ok(ChangeSet { commit, base, merge_base, files, terraform })
}

fn blob(repo: &Path, commit: &str, path: &str) -> Result<String, GitError> {
    Ok(String::from_utf8_lossy(&git(repo, &["cat-file", "blob", &format!("{commit}:{path}")])?).into_owned())
}

fn parse_name_status(raw: &[u8]) -> Result<Vec<ChangedFile>, GitError> {
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut files = Vec::new();
    let utf8 = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| GitError("a changed path is not UTF-8".into()));
    while let Some(status) = fields.next() {
        let status = utf8(status)?;
        let first = utf8(fields.next().ok_or_else(|| GitError("truncated git diff output".into()))?)?;
        if status.starts_with('R') || status.starts_with('C') {
            let second = utf8(fields.next().ok_or_else(|| GitError("truncated git diff output".into()))?)?;
            files.push(ChangedFile { path: second, status: status[..1].to_string(), old_path: Some(first) });
        } else {
            files.push(ChangedFile { path: first, status, old_path: None });
        }
    }
    Ok(files)
}

pub fn files_at(repo: &Path, commit: &str, dir: &str) -> Result<Vec<(String, Vec<u8>)>, GitError> {
    let dir = dir.trim_end_matches('/');
    let listing = git(repo, &["ls-tree", "-r", "-z", "--name-only", commit, "--", &format!("{dir}/")])?;
    let mut out = Vec::new();
    for name in listing.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8(name.to_vec()).map_err(|_| GitError("a policy path is not UTF-8".into()))?;
        let blob = git(repo, &["cat-file", "blob", &format!("{commit}:{name}")])?;
        let rel = name.strip_prefix(&format!("{dir}/")).unwrap_or(&name).to_string();
        out.push((rel, blob));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_keeps_renames_and_paths_with_spaces() {
        let raw = b"M\0infra/iam.tf\0R087\0old name.tf\0new name.tf\0A\0.witnessed/policy/x.rego\0";
        let files = parse_name_status(raw).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1],
            ChangedFile { path: "new name.tf".into(), status: "R".into(), old_path: Some("old name.tf".into()) }
        );
        assert!(parse_name_status(b"M\0").is_err());
    }
}
