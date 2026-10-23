use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use witnessed_assurance::EXIT_ERROR;
use witnessed_assurance::github::Snapshot;

#[derive(clap::Args)]
pub struct CollectArgs {
    #[command(subcommand)]
    pub command: CollectCommand,
}

#[derive(clap::Subcommand)]
pub enum CollectCommand {
    #[command(about = "Snapshot a GitHub pull request's head, commits and reviews through `gh api` (no review text)")]
    GithubPr(GithubPrArgs),
}

#[derive(clap::Args)]
pub struct GithubPrArgs {
    #[arg(long, env = "GITHUB_REPOSITORY", help = "Repository as owner/name")]
    pub repository: String,
    #[arg(long, help = "Pull request number")]
    pub number: u64,
    #[arg(long, short, value_name = "FILE", help = "Write the snapshot to FILE instead of stdout")]
    pub output: Option<PathBuf>,
}

pub fn collect(args: &CollectArgs) -> Result<i32> {
    match &args.command {
        CollectCommand::GithubPr(a) => match github_pr(a) {
            Ok(()) => Ok(0),
            Err(e) => {
                eprintln!("witnessed: collect github-pr failed: {e:#}; nothing was written");
                Ok(EXIT_ERROR)
            }
        },
    }
}

fn gh(endpoint: &str, paginate: bool) -> Result<Value> {
    let program = std::env::var_os("WITNESSED_GH").unwrap_or_else(|| "gh".into());
    let mut cmd = Command::new(&program);
    cmd.args(["api", "-H", "Accept: application/vnd.github+json", "-H", "X-GitHub-Api-Version: 2022-11-28"]);
    if paginate {
        cmd.args(["--paginate", "--slurp"]);
    }
    let out = cmd
        .arg(endpoint)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {} api {endpoint}", program.to_string_lossy()))?;
    if !out.status.success() {
        bail!("gh api {endpoint} exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).with_context(|| format!("gh api {endpoint} did not return JSON"))
}

fn github_pr(a: &GithubPrArgs) -> Result<()> {
    if a.repository.split('/').count() != 2 || a.repository.contains(char::is_whitespace) {
        bail!("--repository must be owner/name, not {:?}", a.repository);
    }
    let base = format!("repos/{}/pulls/{}", a.repository, a.number);
    let pull = gh(&base, false)?;
    let reviews = gh(&format!("{base}/reviews?per_page=100"), true)?;
    let commits = gh(&format!("{base}/commits?per_page=100"), true)?;
    let snapshot = Snapshot::from_api(&a.repository, &pull, &reviews, &commits).map_err(|e| anyhow!("{e}"))?;
    let text = serde_json::to_string_pretty(&snapshot)? + "\n";
    match &a.output {
        Some(path) => fs::write(path, text).with_context(|| format!("writing {}", path.display()))?,
        None => print!("{text}"),
    }
    Ok(())
}
