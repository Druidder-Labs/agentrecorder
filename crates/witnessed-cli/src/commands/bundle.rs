use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;
use witnessed_store::Session;
use witnessed_supervisor::Layout;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::bundle::assemble::{
    Assembled, Edits, Files, Include, assemble, count_fates, decisions_line, session_root, story_line, summary_line,
    write_bundle,
};
use crate::bundle::cut::{CutReport, parse_cut};
use crate::bundle::diff::DiffStatus;
use crate::bundle::redact::{Match, patterns};
use crate::fmt::{abbreviate_home, offset_ms, offset_ns};
use crate::state::{Need, open_needing};

#[derive(clap::Args)]
pub struct BundleArgs {
    #[arg(
        default_value = "last",
        help = "Session id, a unique prefix of it, or `last`; the session must have finished"
    )]
    pub session: String,
    #[arg(
        short,
        long,
        help = "Directory, or with --zip a file, to create (default: ~/witnessed-<ID>, or ~/witnessed-<ID>.zip with --zip); it must not exist or be empty"
    )]
    pub out: Option<PathBuf>,
    #[arg(long, help = "Write one zip file with the same files instead of a directory")]
    pub zip: bool,
    #[arg(long, help = "Replace the output directory or file if it exists")]
    pub force: bool,
    #[arg(
        long,
        value_name = "REGEX",
        help = "Replace every match in output, input, snapshots, event payloads, prompt text and the diff with '*' (repeatable)"
    )]
    pub redact: Vec<String>,
    #[arg(long, value_name = "TEXT", help = "Replace every occurrence of this exact text with '*' (repeatable)")]
    pub redact_literal: Vec<String>,
    #[arg(
        long,
        help = "Also redact common secret shapes: OpenAI and Anthropic keys, GitHub and GitLab tokens, AWS key ids, Google API keys, Slack tokens, bearer tokens, JWTs, private key blocks"
    )]
    pub redact_secrets: bool,
    #[arg(
        long,
        value_name = "FROM-TO",
        help = "Remove terminal data, hooks, marks and notifications between two offsets, such as 00:12:03-00:12:09 (repeatable)"
    )]
    pub cut: Vec<String>,
    #[arg(
        long,
        help = "Include the text of each prompt from the agent transcript, after redaction; without it the bundle has only lengths, times and tokens"
    )]
    pub prompts: bool,
    #[arg(
        long,
        help = "Include the code changes (git diff against the HEAD at start, plus untracked files), after redaction; verify compares them with the hash recorded when the session ended"
    )]
    pub diff: bool,
    #[arg(
        long,
        help = "Include your own verdicts and notes on the decision moments (witnessed review --mark), after redaction"
    )]
    pub annotations: bool,
    #[arg(long, help = "List what --redact and --cut would change, with offsets, and write nothing")]
    pub dry_run: bool,
    #[arg(long, help = "Print the result as JSON")]
    pub json: bool,
}

impl BundleArgs {
    fn include(&self) -> Include {
        Include { prompts: self.prompts, diff: self.diff, annotations: self.annotations }
    }

    fn edits(&self) -> Result<Edits> {
        let cuts = self.cut.iter().map(|c| parse_cut(c)).collect::<Result<Vec<_>>>()?;
        let patterns = patterns(&self.redact, &self.redact_literal, self.redact_secrets)?;
        Ok(Edits { patterns, cuts })
    }
}

pub fn bundle(layout: &Layout, args: &BundleArgs) -> Result<i32> {
    let (store, s, _) = open_needing(layout, &args.session, Need::Finished("a bundle"))?;
    let edits = args.edits()?;
    let out = args.out.clone().unwrap_or_else(|| default_out(&s, args.zip));
    let a = assemble(&store, &s, args.include(), &edits)?;
    let warnings = warnings(args, &edits, &a);
    if args.dry_run {
        return dry_run(&s, &out, &a, &warnings, args.json);
    }
    for w in &warnings {
        eprintln!("witnessed: warning: {w}");
    }
    if args.zip {
        prepare_file(&out, args.force)?;
    } else {
        prepare_dir(&out, args.force)?;
    }
    let name = out.file_name().map_or_else(|| format!("witnessed-{}", s.id), |n| n.to_string_lossy().into_owned());
    let root = session_root(&a.items).or_else(|| s.cwd.clone());
    let (files, summary) = write_bundle(&store, &s, &name, a, &edits.patterns)?;
    let bytes: u64 = if args.zip {
        write_zip(&out, &files)?;
        fs::metadata(&out).map_or(0, |m| m.len())
    } else {
        write_dir(&out, &files)?;
        files.iter().map(|(_, b)| b.len() as u64).sum()
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"session": s.id, "path": out, "zip": args.zip, "bytes": bytes, "summary": summary, "warnings": warnings})
            )?
        );
        return Ok(0);
    }
    let shown = abbreviate_home(&out.display().to_string());
    let size = crate::fmt::bytes(bytes);
    if args.zip {
        println!("wrote {shown} ({size})");
    } else {
        println!("wrote {shown} ({size} in {} files; --zip makes one file)", files.len());
    }
    println!("  {}", summary_line(&summary));
    println!("  {}", story_line(&summary.prompts));
    if let Some(line) = decisions_line(&summary.decisions, args.annotations) {
        println!("  {line}");
    }
    if let Some(d) = &summary.diff {
        match &d.produced {
            Some(p) => println!("  code changes: {} files, {}", p.files, d.status),
            None => println!("  code changes: {}", d.status),
        }
    }
    let token = if s.anchor_token_digest.is_some() { " --token TOKEN" } else { "" };
    println!("  open:   witnessed view {shown}");
    println!(
        "  verify: witnessed verify {shown} --anchors FILE{token}   (FILE: the anchors the reviewer received while the session ran)"
    );
    if let Some(root) = root
        && let (Ok(root), Some(parent)) = (fs::canonicalize(&root), out.parent())
        && fs::canonicalize(if parent.as_os_str().is_empty() { Path::new(".") } else { parent })
            .is_ok_and(|p| p.starts_with(&root))
    {
        eprintln!(
            "witnessed: warning: the bundle is inside the session's repository ({}); a later bundle --diff of this session would include it. Move it, or write it elsewhere with -o",
            root.display()
        );
    }
    Ok(0)
}

fn warnings(args: &BundleArgs, edits: &Edits, a: &Assembled) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(d) = &a.diff
        && d.info.status != DiffStatus::Matches
    {
        warnings.push(format!("diff: {}", d.info.status));
    }
    for (c, r) in edits.cuts.iter().zip(&a.cut_report) {
        let (from, to) = (offset_ns(c.from_ns), offset_ns(c.to_ns));
        if c.from_ns > a.duration_ns {
            warnings.push(format!(
                "cut {from}-{to} starts after the session ended at {}; nothing removed",
                offset_ns(a.duration_ns)
            ));
        } else if r.events == 0 {
            warnings.push(format!("cut {from}-{to} matched no events"));
        }
    }
    if (!args.redact.is_empty() || !args.redact_literal.is_empty())
        && a.matches.is_empty()
        && a.prompt_matches.is_empty()
    {
        warnings.push("no redaction pattern matched anything".into());
    }
    if args.prompts && !a.review.prompts.available {
        warnings.push(format!(
            "no prompt text included: {}",
            a.review.prompts.reason.as_deref().unwrap_or("no transcript")
        ));
    }
    warnings
}

fn dry_run(s: &Session, out: &Path, a: &Assembled, warnings: &[String], as_json: bool) -> Result<i32> {
    let Assembled { items, cut_report, matches, prompt_matches, .. } = a;
    let fates = count_fates(items);
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": s.id,
                "path": out,
                "dry_run": true,
                "events": items.len(),
                "fates": fates,
                "matches": matches.iter().map(Match::to_json).collect::<Vec<_>>(),
                "prompt_matches": prompt_matches,
                "cuts": cut_report,
                "warnings": warnings,
            }))?
        );
        return Ok(0);
    }
    println!("dry run for session {}: nothing written to {}", s.id, out.display());
    println!(
        "  {} events: {} kept, {} redacted, {} cut",
        items.len(),
        fates.get("kept").copied().unwrap_or(0),
        fates.get("redacted").copied().unwrap_or(0),
        fates.get("cut").copied().unwrap_or(0)
    );
    println!("  {} redaction {}", matches.len(), if matches.len() == 1 { "match" } else { "matches" });
    for m in matches {
        println!("    {} {} {}", offset_ns(m.mono_ns), m.kind, m.preview);
    }
    if !prompt_matches.is_empty() {
        println!(
            "  {} redaction matches in prompt text and the diff: {}",
            prompt_matches.len(),
            prompt_matches.join(", ")
        );
    }
    for CutReport { events, from_ms, to_ms } in cut_report {
        println!("  cut {}-{}: {events} events removed", offset_ms(*from_ms), offset_ms(*to_ms));
    }
    for w in warnings {
        println!("  warning: {w}");
    }
    Ok(0)
}

fn default_out(s: &Session, zip: bool) -> PathBuf {
    let name = format!("witnessed-{}{}", s.id, if zip { ".zip" } else { "" });
    std::env::var_os("HOME").map_or_else(|| PathBuf::from(&name), |home| PathBuf::from(home).join(&name))
}

pub fn prepare_dir(dir: &Path, replace: bool) -> Result<()> {
    if dir.exists() {
        if replace {
            fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
        } else if fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.next().is_some() {
            bail!("{} exists and is not empty; choose another directory or pass --force", dir.display());
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

fn prepare_file(path: &Path, replace: bool) -> Result<()> {
    if path.is_dir() {
        bail!("{} is a directory; --zip writes a file", path.display());
    }
    if path.exists() && !replace {
        bail!("{} exists; choose another file or pass --force", path.display());
    }
    Ok(())
}

pub fn write_dir(dir: &Path, files: &Files) -> Result<()> {
    for (name, body) in files {
        fs::write(dir.join(name), body).with_context(|| format!("writing {name}"))?;
    }
    Ok(())
}

fn write_zip(path: &Path, files: &Files) -> Result<()> {
    let prefix = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map_or_else(String::new, |s| format!("{s}/"));
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).unix_permissions(0o600);
    for (name, body) in files {
        zip.start_file(format!("{prefix}{name}"), options).with_context(|| format!("adding {name} to the zip"))?;
        zip.write_all(body).with_context(|| format!("writing {name} to the zip"))?;
    }
    zip.finish().with_context(|| format!("finishing {}", path.display()))?;
    Ok(())
}
