use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use regex::bytes::Regex;
use serde_json::{Value, json};
use witnessed_store::{Body, Event, Session, Store, format_rfc3339, kind, now_unix_ns};
use witnessed_supervisor::Layout;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::export::{Cast, CastWriter, cast_header, cast_line, recorded_term};
use super::{Follow, open_store};
use crate::chain::{self, Hash};
use crate::render::initial_size;
use witnessed_supervisor::git::DIFF_FILE;

pub const BUNDLE_FORMAT: &str = "witnessed-bundle";
const BUNDLE_FORMATS_ACCEPTED: &[&str] = &["witnessed-bundle", "agentrecorder-bundle"];
pub const BUNDLE_VERSION: u32 = 1;
const DASHBOARD: &str = include_str!("../dashboard.html");
const PLAYER_JS: &str = include_str!("../vendor/asciinema-player/asciinema-player.min.js");
const PLAYER_CSS: &str = include_str!("../vendor/asciinema-player/asciinema-player.css");
const DATA_PLACEHOLDER: &str = "__WITNESSED_DATA__";
const CUTTABLE: &[&str] = &[
    kind::TERMINAL_OUTPUT,
    kind::TERMINAL_INPUT,
    kind::TERMINAL_SNAPSHOT,
    kind::TERMINAL_NOTIFICATION,
    kind::AGENT_HOOK,
    kind::SHELL_COMMAND_FINISHED,
    kind::USER_MARK,
];
const BYTE_STREAMS: &[&str] = &[kind::TERMINAL_OUTPUT, kind::TERMINAL_INPUT];
const TIMELINE_SKIP: &[&str] = &[kind::TERMINAL_OUTPUT, kind::TERMINAL_INPUT, kind::TERMINAL_SNAPSHOT];

pub const SECRET_PATTERNS: &[(&str, &str)] = &[
    ("openai or anthropic key", r"sk-[A-Za-z0-9_-]{16,}"),
    ("github token", r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,}"),
    ("gitlab token", r"glpat-[A-Za-z0-9_-]{20,}"),
    ("aws access key id", r"(?:AKIA|ASIA)[0-9A-Z]{16}"),
    ("google api key", r"AIza[0-9A-Za-z_-]{35}"),
    ("slack token", r"xox[abprs]-[A-Za-z0-9-]{10,}"),
    ("bearer token", r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{20,}"),
    ("jwt", r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
    ("private key block", r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
];

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

pub fn patterns(args: &BundleArgs) -> Result<Vec<Regex>> {
    let mut out = Vec::new();
    for p in &args.redact {
        out.push(Regex::new(p).with_context(|| format!("invalid --redact pattern {p:?}"))?);
    }
    for t in &args.redact_literal {
        if t.is_empty() {
            bail!("--redact-literal needs a non-empty text");
        }
        out.push(Regex::new(&regex::escape(t)).expect("escaped literal"));
    }
    if args.redact_secrets {
        for (_, p) in SECRET_PATTERNS {
            out.push(Regex::new(p).expect("built-in pattern"));
        }
    }
    Ok(out)
}

#[derive(clap::Args)]
pub struct AnchorArgs {
    #[arg(help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(long, default_value_t = 30, help = "Seconds between anchors while the session runs")]
    pub interval: u64,
}

#[derive(clap::Args)]
pub struct VerifyArgs {
    #[arg(help = "Bundle directory or zip file written by `witnessed bundle`")]
    pub dir: PathBuf,
    #[arg(long, help = "File of anchor lines received from `witnessed anchor` while the session ran")]
    pub anchors: Option<PathBuf>,
    #[arg(
        long,
        value_name = "TOKEN",
        env = "WITNESSED_ANCHOR_TOKEN",
        hide_env_values = true,
        help = "Token issued for the session; its SHA-256 must be the chain genesis recorded in the bundle"
    )]
    pub token: Option<String>,
    #[arg(long, help = "Print the result as JSON")]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct ViewArgs {
    #[arg(
        default_value = "last",
        help = "Bundle directory or zip file, or a session id, a unique prefix of it, or `last`"
    )]
    pub target: String,
    #[arg(long, help = "Print the dashboard path instead of opening it in a browser")]
    pub no_open: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fate {
    Kept,
    Redacted,
    Cut,
}

impl Fate {
    fn as_str(self) -> &'static str {
        match self {
            Fate::Kept => "kept",
            Fate::Redacted => "redacted",
            Fate::Cut => "cut",
        }
    }
}

struct Item {
    event: Event,
    leaf: Hash,
    fate: Fate,
}

#[derive(Debug, PartialEq, Clone)]
struct CutRange {
    from_ns: i64,
    to_ns: i64,
}

fn parse_cut(s: &str) -> Result<CutRange> {
    let Some((from, to)) = s.split_once('-') else {
        bail!("cut {s:?} must look like FROM-TO, such as 00:12:03-00:12:09")
    };
    let (from_ns, to_ns) = (crate::fmt::parse_offset(from)?, crate::fmt::parse_offset(to)?);
    if to_ns <= from_ns {
        bail!("cut {s:?} ends before it starts");
    }
    Ok(CutRange { from_ns, to_ns })
}

pub fn bundle(layout: &Layout, args: BundleArgs) -> Result<i32> {
    let store = open_store(layout)?;
    let (s, _) = finished_session(layout, &store, &args.session, "a bundle")?;
    let cuts = args.cut.iter().map(|c| parse_cut(c)).collect::<Result<Vec<_>>>()?;
    let patterns = patterns(&args)?;
    let mut items = load(&store, &s.id)?;
    let out = args.out.clone().unwrap_or_else(|| default_out(&s, args.zip));
    let duration_ns = items.last().map_or(0, |it| it.event.mono_ns);
    let cut_report = apply_cuts(&mut items, &cuts);
    let diff = if args.diff { Some(session_diff(&s, &items, &patterns)) } else { None };
    let matches = apply_redactions(&mut items, &patterns);
    let (story, mut prompt_matches) = agent_story(&store, &s, &items, &cut_report, &patterns, args.prompts)?;
    let (review, review_matches) = review_data(&store, &s, &cut_report, &patterns, args.diff, args.annotations)?;
    prompt_matches.extend(review_matches);
    let mut warnings = Vec::new();
    if let Some(d) = &diff {
        prompt_matches.extend(d.matches.iter().cloned());
        if d.bytes.is_none() || d.info["status"] != "matches the session end" {
            warnings.push(format!("diff: {}", d.info["status"].as_str().unwrap_or("unavailable")));
        }
    }
    for (c, r) in cuts.iter().zip(&cut_report) {
        let (from, to) = (crate::fmt::offset_ms(c.from_ns / 1_000_000), crate::fmt::offset_ms(c.to_ns / 1_000_000));
        if c.from_ns > duration_ns {
            warnings.push(format!(
                "cut {from}-{to} starts after the session ended at {}; nothing removed",
                crate::fmt::offset_ms(duration_ns / 1_000_000)
            ));
        } else if r["events"] == 0 {
            warnings.push(format!("cut {from}-{to} matched no events"));
        }
    }
    if (!args.redact.is_empty() || !args.redact_literal.is_empty()) && matches.is_empty() && prompt_matches.is_empty() {
        warnings.push("no redaction pattern matched anything".into());
    }
    if args.prompts && story["available"] == false {
        warnings.push(format!("no prompt text included: {}", story["reason"].as_str().unwrap_or("no transcript")));
    }
    if args.dry_run {
        return dry_run(&s, &out, &items, (&matches, &prompt_matches), &cut_report, &warnings, args.json);
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
    let total_matches = (matches.len() + prompt_matches.len()) as u64;
    let (files, summary) = write_bundle(
        &store,
        &s,
        &name,
        &items,
        &Edits { patterns: &patterns, cut_report: &cut_report, matches: total_matches },
        diff,
        with_story(review, story),
    )?;
    let bytes: u64 = if args.zip {
        write_zip(&out, &files)?;
        fs::metadata(&out).map(|m| m.len()).unwrap_or(0)
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
    let shown = crate::fmt::abbreviate_home(&out.display().to_string());
    let size = crate::fmt::bytes(bytes);
    if args.zip {
        println!("wrote {shown} ({size})");
    } else {
        println!("wrote {shown} ({size} in {} files; --zip makes one file)", files.len());
    }
    println!("  {}", summary_line(&summary));
    println!("  {}", story_line(&summary["prompts"]));
    if let Some(line) = decisions_line(&summary["decisions"], args.annotations) {
        println!("  {line}");
    }
    if let Some(d) = summary.get("diff").filter(|d| !d.is_null()) {
        let status = d["status"].as_str().unwrap_or("unavailable");
        match d["files"].as_u64() {
            Some(n) => println!("  code changes: {n} files, {status}"),
            None => println!("  code changes: {status}"),
        }
    }
    let token = if s.anchor_token_digest.is_some() { " --token TOKEN" } else { "" };
    println!("  open:   witnessed view {shown}");
    println!(
        "  verify: witnessed verify {shown} --anchors FILE{token}   (FILE: the anchors the reviewer received while the session ran)"
    );
    if let Some(root) = session_root(&items).or(s.cwd.clone())
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

fn dry_run(
    s: &Session,
    out: &Path,
    items: &[Item],
    (matches, prompt_matches): (&[Match], &[String]),
    cut_report: &[Value],
    warnings: &[String],
    as_json: bool,
) -> Result<i32> {
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
        println!("    {} {} {}", crate::fmt::offset_ms(m.mono_ns / 1_000_000), m.kind, m.preview);
    }
    if !prompt_matches.is_empty() {
        println!(
            "  {} redaction matches in prompt text and the diff: {}",
            prompt_matches.len(),
            prompt_matches.join(", ")
        );
    }
    for c in cut_report {
        println!(
            "  cut {}-{}: {} events removed",
            crate::fmt::offset_ms(c["from_ms"].as_i64().unwrap_or(0)),
            crate::fmt::offset_ms(c["to_ms"].as_i64().unwrap_or(0)),
            c["events"]
        );
    }
    for w in warnings {
        println!("  warning: {w}");
    }
    Ok(0)
}

fn finished_session(layout: &Layout, store: &Store, id: &str, what: &str) -> Result<(Session, crate::state::State)> {
    let mut s = store.resolve(id)?;
    let st = crate::state::resolve(layout, store, &mut s);
    if !st.is_finished() {
        bail!(
            "session {} is {}; {what} needs a finished session. Stop it with `witnessed stop {}`, or follow it \
             live with `witnessed replay {} --follow`",
            s.id,
            st.label(),
            s.id,
            s.id
        );
    }
    Ok((s, st))
}

struct Match {
    mono_ns: i64,
    kind: String,
    preview: String,
}

impl Match {
    fn to_json(&self) -> Value {
        json!({"offset_ms": self.mono_ns / 1_000_000, "type": self.kind, "preview": self.preview})
    }
}

fn preview(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let shown: String = text.chars().take(4).collect();
    format!("{shown}{} ({} bytes)", "*".repeat(text.chars().count().saturating_sub(4).min(12)), bytes.len())
}

fn count_fates(items: &[Item]) -> BTreeMap<&'static str, u64> {
    let mut fates = BTreeMap::new();
    for it in items {
        *fates.entry(it.fate.as_str()).or_default() += 1;
    }
    fates
}

fn prepare_dir(dir: &Path, replace: bool) -> Result<()> {
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

fn write_dir(dir: &Path, files: &[(&str, Vec<u8>)]) -> Result<()> {
    for (name, body) in files {
        fs::write(dir.join(name), body).with_context(|| format!("writing {name}"))?;
    }
    Ok(())
}

fn write_zip(path: &Path, files: &[(&str, Vec<u8>)]) -> Result<()> {
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

enum Source {
    Dir(PathBuf),
    Zip { archive: ZipArchive<File>, prefix: String },
}

impl Source {
    fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            return Ok(Source::Dir(path.to_path_buf()));
        }
        if !path.exists() {
            bail!("{} does not exist; pass the bundle directory or zip file", path.display());
        }
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let archive = ZipArchive::new(file)
            .with_context(|| format!("{} is neither a directory nor a zip file", path.display()))?;
        let prefix = archive
            .file_names()
            .filter_map(|n| n.ok())
            .filter_map(|n| n.strip_suffix("manifest.json").map(str::to_string))
            .min_by_key(String::len)
            .ok_or_else(|| anyhow!("{} is not a witnessed bundle: it has no manifest.json", path.display()))?;
        Ok(Source::Zip { archive, prefix })
    }

    fn read(&mut self, name: &str) -> std::io::Result<Vec<u8>> {
        match self {
            Source::Dir(dir) => fs::read(dir.join(name)),
            Source::Zip { archive, prefix } => {
                let Some(index) = archive.index_for_name(&format!("{prefix}{name}")) else {
                    return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "not in the zip file"));
                };
                let mut entry = archive.by_index(index).map_err(std::io::Error::other)?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }

    fn text(&mut self, name: &str) -> Result<String> {
        let bytes = self.read(name).with_context(|| format!("reading {name}"))?;
        String::from_utf8(bytes).with_context(|| format!("{name} is not UTF-8"))
    }
}

fn load(store: &Store, id: &str) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    store.for_each_event(id, None, |e| {
        let leaf = chain::leaf(&chain::event_line(&e));
        items.push(Item { event: e, leaf, fate: Fate::Kept });
        Ok(())
    })?;
    Ok(items)
}

fn apply_cuts(items: &mut [Item], cuts: &[CutRange]) -> Vec<Value> {
    cuts.iter()
        .map(|c| {
            let mut n = 0;
            for it in items.iter_mut() {
                let e = &it.event;
                if e.mono_ns >= c.from_ns && e.mono_ns < c.to_ns && CUTTABLE.contains(&e.kind.as_str()) {
                    it.fate = Fate::Cut;
                    n += 1;
                }
            }
            json!({"from_ms": c.from_ns / 1_000_000, "to_ms": c.to_ns / 1_000_000, "events": n})
        })
        .collect()
}

fn mask(buf: &mut [u8], patterns: &[Regex]) -> Vec<(usize, String)> {
    let mut ranges = Vec::new();
    for p in patterns {
        ranges.extend(p.find_iter(buf).filter(|m| !m.is_empty()).map(|m| m.range()));
    }
    ranges.sort_by_key(|r| r.start);
    let found = ranges.iter().map(|r| (r.start, preview(&buf[r.clone()]))).collect();
    for r in &ranges {
        buf[r.clone()].fill(b'*');
    }
    found
}

fn mask_value(v: &mut Value, patterns: &[Regex]) -> Vec<String> {
    match v {
        Value::String(s) => {
            let mut bytes = s.as_bytes().to_vec();
            let found = mask(&mut bytes, patterns);
            if !found.is_empty() {
                *s = String::from_utf8_lossy(&bytes).into_owned();
            }
            found.into_iter().map(|(_, p)| p).collect()
        }
        Value::Array(a) => a.iter_mut().flat_map(|x| mask_value(x, patterns)).collect(),
        Value::Object(o) => o.values_mut().flat_map(|x| mask_value(x, patterns)).collect(),
        _ => Vec::new(),
    }
}

fn apply_redactions(items: &mut [Item], patterns: &[Regex]) -> Vec<Match> {
    let mut matches = Vec::new();
    if patterns.is_empty() {
        return matches;
    }
    for stream in BYTE_STREAMS {
        let idx: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].fate != Fate::Cut && items[i].event.kind == *stream && items[i].event.data.is_some())
            .collect();
        let mut joined = Vec::new();
        let mut starts = Vec::new();
        for &i in &idx {
            starts.push(joined.len());
            joined.extend_from_slice(items[i].event.data.as_deref().unwrap_or_default());
        }
        for (start, preview) in mask(&mut joined, patterns) {
            let n = starts.partition_point(|&s| s <= start).saturating_sub(1);
            let e = &items[idx[n]].event;
            matches.push(Match { mono_ns: e.mono_ns, kind: e.kind.clone(), preview });
        }
        let mut at = 0;
        for &i in &idx {
            let data = items[i].event.data.as_mut().expect("filtered on data");
            let new = &joined[at..at + data.len()];
            at += data.len();
            if new != data.as_slice() {
                data.copy_from_slice(new);
                items[i].fate = Fate::Redacted;
            }
        }
    }
    for it in items.iter_mut().filter(|it| it.fate != Fate::Cut) {
        let mut found = mask_value(&mut it.event.payload, patterns);
        if it.event.kind == kind::TERMINAL_SNAPSHOT
            && let Some(d) = it.event.data.as_mut()
        {
            found.extend(mask(d, patterns).into_iter().map(|(_, p)| p));
        }
        if !found.is_empty() {
            it.fate = Fate::Redacted;
            let e = &it.event;
            matches.extend(found.into_iter().map(|preview| Match {
                mono_ns: e.mono_ns,
                kind: e.kind.clone(),
                preview,
            }));
        }
    }
    matches.sort_by_key(|m| m.mono_ns);
    matches
}

type Files = Vec<(&'static str, Vec<u8>)>;

struct Edits<'a> {
    patterns: &'a [Regex],
    cut_report: &'a [Value],
    matches: u64,
}

fn write_bundle(
    store: &Store,
    s: &Session,
    bundle_name: &str,
    items: &[Item],
    edits: &Edits,
    diff: Option<SessionDiff>,
    review: Value,
) -> Result<(Files, Value)> {
    let story = review["prompts"].clone();
    let Edits { patterns, cut_report, matches } = *edits;
    let mut events = String::new();
    let mut chain_lines = String::new();
    let genesis = chain::genesis(s.anchor_token_digest.as_deref());
    let mut head = genesis;
    let mut fates: BTreeMap<&str, u64> = BTreeMap::new();
    let mut timeline = Vec::new();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for it in items {
        head = chain::link(&head, &it.leaf);
        *fates.entry(it.fate.as_str()).or_default() += 1;
        let e = &it.event;
        chain_lines.push_str(
            &json!({"seq": e.seq, "leaf": chain::hex(&it.leaf), "chain": chain::hex(&head), "fate": it.fate.as_str()})
                .to_string(),
        );
        chain_lines.push('\n');
        if it.fate == Fate::Cut {
            continue;
        }
        *counts.entry(e.kind.clone()).or_default() += 1;
        events.push_str(&chain::event_line(e));
        events.push('\n');
        if !TIMELINE_SKIP.contains(&e.kind.as_str()) || it.fate == Fate::Redacted {
            let mut entry =
                json!({"seq": e.seq, "t_ms": e.mono_ns / 1_000_000, "type": e.kind, "fate": it.fate.as_str()});
            if !TIMELINE_SKIP.contains(&e.kind.as_str()) {
                entry["timestamp"] = json!(format_rfc3339(e.ts_ns));
                entry["payload"] = e.payload.clone();
            }
            timeline.push(entry);
        }
    }

    let cast = render_cast(store, s, items, cut_report, patterns)?;
    let shown_cast = cast.clone();
    let mut files: Files = vec![
        ("events.jsonl", events.into_bytes()),
        ("chain.jsonl", chain_lines.into_bytes()),
        ("session.cast", cast.into_bytes()),
    ];
    let diff_info = diff.as_ref().map(|d| d.info.clone());
    let diff_bytes = diff.and_then(|d| d.bytes);
    let shown =
        diff_bytes.as_ref().filter(|b| b.len() <= MAX_DIFF_SHOWN).map(|b| String::from_utf8_lossy(b).into_owned());
    if let Some(bytes) = diff_bytes {
        files.push((DIFF_FILE, bytes));
    }
    let mut digests = serde_json::Map::new();
    for (name, body) in &files {
        digests.insert((*name).to_string(), json!({"sha256": chain::file_digest(body), "bytes": body.len()}));
    }
    let mut manifest = json!({
        "format": BUNDLE_FORMAT,
        "bundle_version": BUNDLE_VERSION,
        "generator": {"name": "witnessed", "version": env!("CARGO_PKG_VERSION")},
        "created_at": format_rfc3339(now_unix_ns()),
        "session": s,
        "command_display": crate::adapters::display(&s.command),
        "program": crate::state::program(s),
        "duration_ms": crate::state::duration_ms(s),
        "event_counts": counts,
        "chain": {
            "algorithm": chain::ALGORITHM,
            "events": items.len(),
            "genesis": chain::hex(&genesis),
            "token_bound": s.anchor_token_digest.is_some(),
            "head": chain::hex(&head),
        },
        "fates": fates,
        "redaction": {"patterns": patterns.len(), "matches": matches, "events": fates.get("redacted").copied().unwrap_or(0)},
        "cuts": cut_report,
        "files": digests,
        "prompts": story,
        "diff": diff_info,
        "decisions": {"available": review["decisions"]["available"], "counts": review["decisions"]["counts"], "annotated": review["annotations"].as_array().map(Vec::len)},
        "outcome": review["outcome"],
    });
    for field in ["session", "command_display", "program"] {
        mask_value(&mut manifest[field], patterns);
    }
    let manifest_text = serde_json::to_string_pretty(&manifest)? + "\n";
    let mut data = json!({"manifest": manifest, "timeline": timeline, "cast": shown_cast, "bundle_name": bundle_name, "diff": shown});
    for key in ["decisions", "annotations", "outcome"] {
        if !review[key].is_null() {
            data[key] = review[key].clone();
        }
    }
    let html = dashboard(&data);
    files.push(("manifest.json", manifest_text.into_bytes()));
    files.push(("index.html", html.into_bytes()));
    let summary = json!({
        "events": items.len(),
        "fates": fates,
        "matches": matches,
        "patterns": patterns.len(),
        "chain_head": chain::hex(&head),
        "token_bound": s.anchor_token_digest.is_some(),
        "prompts": manifest["prompts"].clone(),
        "diff": manifest["diff"].clone(),
        "decisions": manifest["decisions"].clone(),
        "outcome": manifest["outcome"].clone(),
    });
    Ok((files, summary))
}

fn agent_story(
    store: &Store,
    s: &Session,
    items: &[Item],
    cut_report: &[Value],
    patterns: &[Regex],
    with_text: bool,
) -> Result<(Value, Vec<String>)> {
    let mut transcript = None;
    let mut hooks = 0u64;
    store.for_each_event(&s.id, Some(&[kind::AGENT_HOOK]), |e| {
        hooks += 1;
        if let Some(Body::AgentHook(h)) = e.body()
            && let Some(path) = h.transcript_path
        {
            transcript = Some((h.agent, path));
        }
        Ok(())
    })?;
    let unavailable = |reason: String| Ok((json!({"available": false, "reason": reason}), Vec::new()));
    let Some((agent, path)) = transcript else {
        let reason = if hooks == 0 { crate::adapters::NOT_AGENT } else { crate::adapters::NO_TRANSCRIPT_PATH };
        return unavailable(reason.into());
    };
    let (from_ns, to_ns) = super::inspect::session_window(s);
    let usage = match crate::transcript::usage(Path::new(&path), from_ns, to_ns) {
        Ok(u) => u,
        Err(_) => return unavailable(crate::adapters::transcript_unreadable(&path)),
    };
    let base_ns = items.first().map_or(s.created_at_ns, |it| it.event.ts_ns - it.event.mono_ns);
    let in_cut = |t_ms: i64| {
        cut_report.iter().any(|c| t_ms >= c["from_ms"].as_i64().unwrap_or(0) && t_ms < c["to_ms"].as_i64().unwrap_or(0))
    };
    let mut list = Vec::new();
    let mut cut = 0u64;
    for (i, t) in usage.interactions.iter().enumerate() {
        let t_ms = t.at_ns.map(|at| ((at - base_ns) / 1_000_000).max(0));
        if t_ms.is_some_and(in_cut) {
            cut += 1;
            continue;
        }
        let mut v = json!({
            "index": i + 1,
            "t_ms": t_ms,
            "origin": t.origin,
            "chars": t.chars,
            "images": t.images,
            "api_calls": t.api_calls,
            "input": t.input,
            "output": t.output,
            "cache_read": t.cache_read,
            "cache_creation": t.cache_creation,
            "total": t.total(),
            "reply_chars": t.reply_chars,
            "model": t.model,
        });
        if with_text {
            v["text"] = json!(t.text);
        }
        list.push(v);
    }
    let offsets = |list: &[i64]| -> Vec<i64> {
        list.iter().map(|at| ((at - base_ns) / 1_000_000).max(0)).filter(|t| !in_cut(*t)).collect()
    };
    let mut story = json!({
        "available": true,
        "source": format!("{agent} transcript"),
        "text": with_text,
        "cut": cut,
        "interactions": list,
        "interrupts": offsets(&usage.interrupts),
        "refusals": offsets(&usage.refusals),
        "compactions": offsets(&usage.compactions),
    });
    let found = if with_text { mask_value(&mut story["interactions"], patterns) } else { Vec::new() };
    Ok((story, found))
}

fn default_out(s: &Session, zip: bool) -> PathBuf {
    let name = format!("witnessed-{}{}", s.id, if zip { ".zip" } else { "" });
    std::env::var_os("HOME").map_or_else(|| PathBuf::from(&name), |home| PathBuf::from(home).join(&name))
}

fn with_story(mut review: Value, story: Value) -> Value {
    review["prompts"] = story;
    review
}

fn review_data(
    store: &Store,
    s: &Session,
    cut_report: &[Value],
    patterns: &[Regex],
    with_outcome: bool,
    with_annotations: bool,
) -> Result<(Value, Vec<String>)> {
    let cuts: Vec<(i64, i64)> =
        cut_report.iter().map(|c| (c["from_ms"].as_i64().unwrap_or(0), c["to_ms"].as_i64().unwrap_or(0))).collect();
    let decisions = crate::decisions::derive(store, s, &cuts)?;
    let ids: std::collections::BTreeSet<&str> = decisions.moments.iter().map(|m| m.id.as_str()).collect();
    let annotations = if with_annotations {
        let list: Vec<_> = store.annotations(&s.id)?.into_iter().filter(|a| ids.contains(a.moment.as_str())).collect();
        serde_json::to_value(list)?
    } else {
        Value::Null
    };
    let outcome = if with_outcome { serde_json::to_value(crate::outcome::for_session(store, s))? } else { Value::Null };
    let mut review = json!({"decisions": decisions, "annotations": annotations, "outcome": outcome});
    let mut found = Vec::new();
    for key in ["decisions", "annotations", "outcome"] {
        found.extend(mask_value(&mut review[key], patterns));
    }
    Ok((review, found))
}

const MAX_DIFF_SHOWN: usize = 4 << 20;

struct SessionDiff {
    info: Value,
    bytes: Option<Vec<u8>>,
    matches: Vec<String>,
}

fn session_diff(s: &Session, items: &[Item], patterns: &[Regex]) -> SessionDiff {
    let snapshot = |phase: &str| {
        items
            .iter()
            .find(|it| {
                it.fate != Fate::Cut && it.event.kind == kind::GIT_SNAPSHOT && it.event.payload["phase"] == phase
            })
            .map(|it| &it.event.payload)
    };
    let unavailable = |reason: &str| SessionDiff { info: json!({"status": reason}), bytes: None, matches: Vec::new() };
    let Some(start) = snapshot("start") else {
        return unavailable("unavailable: the session did not run in a git work tree");
    };
    let Some(base) = start["head"].as_str().map(str::to_string) else {
        return unavailable("unavailable: the repository had no commit when the session started");
    };
    let at_exit = snapshot("exit").and_then(|p| p["diff_sha256"].as_str()).map(str::to_string);
    let Some(cwd) = s.cwd.as_deref() else {
        return unavailable("unavailable: the session has no working directory");
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let Some(mut bytes) = witnessed_supervisor::git::worktree_diff(Path::new(cwd), &base, deadline) else {
        return unavailable(
            "unavailable: git could not produce the diff in the session's working directory (it failed, timed out, or the diff or the untracked files are over 32 MiB)",
        );
    };
    let digest = chain::file_digest(&bytes);
    let mut status = match &at_exit {
        Some(h) if *h == digest => "matches the session end",
        Some(_) => "differs from the session end: the work tree changed after the session",
        None => "not compared: the recording has no diff hash from the session end",
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
        status = "redacted: not comparable with the session end";
    }
    let files = bytes.split(|b| *b == b'\n').filter(|l| l.starts_with(b"diff --git ")).count();
    SessionDiff {
        info: json!({"status": status, "bytes": bytes.len(), "files": files, "base": base, "sha256_at_exit": at_exit, "binary_dropped": binary_dropped}),
        bytes: Some(bytes),
        matches,
    }
}

/// Redaction cannot read compressed binary patch data, so with redaction on, each binary patch is replaced by the
/// line git prints for binary files without --binary.
fn drop_binary_patches(diff: &[u8]) -> (Vec<u8>, usize) {
    let text = String::from_utf8_lossy(diff);
    if !text.contains("\nGIT binary patch\n") {
        return (diff.to_vec(), 0);
    }
    let mut out = Vec::with_capacity(diff.len());
    let mut dropped = 0;
    let mut rest = diff;
    while !rest.is_empty() {
        let next = rest[1..].windows(12).position(|w| w == b"\ndiff --git ").map_or(rest.len(), |i| i + 2);
        let (section, tail) = rest.split_at(next);
        if let Some(at) = section.windows(17).position(|w| w == b"GIT binary patch\n") {
            let header = &section[..at];
            let name = String::from_utf8_lossy(header.split(|b| *b == b'\n').next().unwrap_or_default())
                .trim_start_matches("diff --git ")
                .to_string();
            let (a, b) =
                name.split_once(" b/").map_or((name.clone(), name.clone()), |(a, b)| (a.to_string(), format!("b/{b}")));
            out.extend_from_slice(header);
            out.extend_from_slice(format!("Binary files {a} and {b} differ\n").as_bytes());
            dropped += 1;
        } else {
            out.extend_from_slice(section);
        }
        rest = tail;
    }
    (out, dropped)
}

fn story_line(story: &Value) -> String {
    if story["available"] != true {
        return format!("prompts: none ({})", story["reason"].as_str().unwrap_or("no transcript"));
    }
    let list = story["interactions"].as_array().cloned().unwrap_or_default();
    let typed = list.iter().filter(|t| t["origin"] == "human" || t["origin"] == "user").count();
    let other = list.iter().filter(|t| t["origin"] != "none").count() - typed;
    let n = match other {
        0 => format!("{typed}"),
        1 => format!("{typed} typed and 1 command or notification"),
        _ => format!("{typed} typed and {other} commands or notifications"),
    };
    if story["text"] == true {
        format!("prompts: {n}, with their text after redaction; read them before sending")
    } else {
        format!("prompts: {n}, with times and tokens; text not included (--prompts adds it)")
    }
}

fn decisions_line(d: &Value, with_annotations: bool) -> Option<String> {
    let n: u64 = d["counts"].as_object()?.values().filter_map(Value::as_u64).sum();
    if d["available"] != true || n == 0 {
        return None;
    }
    let mut line = format!("decisions: {n} moment{}", if n == 1 { "" } else { "s" });
    match (with_annotations, d["annotated"].as_u64().unwrap_or(0)) {
        (true, a) => line.push_str(&format!(", {a} with your verdict")),
        (false, _) => line.push_str("; your verdicts are not included (--annotations adds them)"),
    }
    Some(line)
}

fn summary_line(summary: &Value) -> String {
    let f = &summary["fates"];
    let matches = match summary["matches"].as_u64().unwrap_or(0) {
        1 => ", 1 redaction match".to_string(),
        n if summary["patterns"].as_u64().unwrap_or(0) > 0 => format!(", {n} redaction matches"),
        _ => String::new(),
    };
    format!(
        "{} events ({} kept, {} redacted, {} cut){matches}, chain head {}",
        summary["events"],
        f["kept"].as_u64().unwrap_or(0),
        f["redacted"].as_u64().unwrap_or(0),
        f["cut"].as_u64().unwrap_or(0),
        &summary["chain_head"].as_str().unwrap_or("")[..16]
    )
}

fn session_root(items: &[Item]) -> Option<String> {
    items
        .iter()
        .find(|it| it.event.kind == kind::GIT_SNAPSHOT)
        .and_then(|it| it.event.payload["root"].as_str().map(str::to_string))
}

fn render_cast(store: &Store, s: &Session, items: &[Item], cuts: &[Value], patterns: &[Regex]) -> Result<String> {
    let mut header = cast_header(s, Cast::V3, initial_size(store, s)?, recorded_term(store, s)?);
    mask_value(&mut header, patterns);
    let mut out = format!("{header}\n");
    let mut writer = CastWriter::new(Cast::V3);
    let mut pending_cuts: Vec<CutRange> = cuts
        .iter()
        .map(|c| CutRange {
            from_ns: c["from_ms"].as_i64().unwrap_or(0) * 1_000_000,
            to_ns: c["to_ms"].as_i64().unwrap_or(0) * 1_000_000,
        })
        .collect();
    pending_cuts.sort_by_key(|c| c.from_ns);
    let push = |out: &mut String, v: Value| {
        out.push_str(&cast_line(&v));
        out.push('\n');
    };
    for it in items.iter().filter(|it| it.fate != Fate::Cut) {
        while let Some(c) = pending_cuts.first().filter(|c| c.from_ns <= it.event.mono_ns).cloned() {
            let label = format!(
                "cut {}-{}",
                crate::fmt::offset_ms(c.from_ns / 1_000_000),
                crate::fmt::offset_ms(c.to_ns / 1_000_000)
            );
            push(&mut out, writer.marker(c.from_ns, &label));
            pending_cuts.remove(0);
        }
        if let Some(v) = writer.event(&it.event) {
            push(&mut out, v);
        }
    }
    if let Some(v) = writer.finish() {
        push(&mut out, v);
    }
    Ok(out)
}

fn dashboard(data: &Value) -> String {
    let json = data.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026");
    DASHBOARD
        .replacen("__WITNESSED_PLAYER_CSS__", PLAYER_CSS, 1)
        .replacen("__WITNESSED_PLAYER_JS__", PLAYER_JS, 1)
        .replacen(DATA_PLACEHOLDER, &json, 1)
}

pub fn anchor(layout: &Layout, args: AnchorArgs) -> Result<i32> {
    let store = open_store(layout)?;
    let s = store.resolve(&args.session)?;
    let interrupted = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, interrupted.clone())?;
    }
    let follow = Follow { layout, follow: true, interrupted: &interrupted };
    let interval = Duration::from_secs(args.interval.max(1));
    let mut out = std::io::stdout().lock();
    let genesis = chain::genesis(s.anchor_token_digest.as_deref());
    let mut head = genesis;
    let mut last_seq: Option<i64> = None;
    let mut emitted: Option<i64> = None;
    let mut last_emit = Instant::now();
    let emit = |out: &mut dyn Write, seq: i64, head: &Hash| -> Result<()> {
        let line = json!({
            "session_id": s.id,
            "seq": seq,
            "chain": chain::hex(head),
            "genesis": chain::hex(&genesis),
            "timestamp": format_rfc3339(now_unix_ns()),
        });
        writeln!(out, "{line}")?;
        out.flush()?;
        Ok(())
    };
    follow.each(&store, &s.id, None, |e| {
        head = chain::link(&head, &chain::leaf(&chain::event_line(&e)));
        last_seq = Some(e.seq);
        if last_emit.elapsed() >= interval {
            emit(&mut out, e.seq, &head)?;
            emitted = Some(e.seq);
            last_emit = Instant::now();
        }
        Ok(())
    })?;
    if let Some(seq) = last_seq.filter(|q| emitted != Some(*q)) {
        emit(&mut out, seq, &head)?;
    }
    Ok(if interrupted.load(Ordering::Relaxed) { 130 } else { 0 })
}

struct ChainEntry {
    leaf: Hash,
    chain: Hash,
    fate: String,
}

fn parse_json_lines(name: &str, text: &str) -> Result<Vec<(String, Value)>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(n, l)| {
            let v = serde_json::from_str(l).with_context(|| format!("{name} line {}: not JSON", n + 1))?;
            Ok((l.to_string(), v))
        })
        .collect()
}

pub fn verify(args: VerifyArgs) -> Result<i32> {
    let dir = &args.dir;
    let mut src = Source::open(dir)?;
    let manifest_text = match src.read("manifest.json") {
        Ok(bytes) => String::from_utf8(bytes).context("manifest.json is not UTF-8")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{} is not a witnessed bundle: it has no manifest.json", dir.display())
        }
        Err(e) => return Err(e).with_context(|| format!("reading manifest.json in {}", dir.display())),
    };
    let manifest: Value = serde_json::from_str(&manifest_text).context("manifest.json is not JSON")?;
    if !manifest["format"].as_str().is_some_and(|f| BUNDLE_FORMATS_ACCEPTED.contains(&f)) {
        bail!("{} is not a witnessed bundle", dir.display());
    }
    if manifest["bundle_version"].as_u64() != Some(BUNDLE_VERSION as u64) {
        bail!("bundle version {} is not supported by this witnessed", manifest["bundle_version"]);
    }
    let mut failures: Vec<String> = Vec::new();
    if let Some(files) = manifest["files"].as_object() {
        for (name, meta) in files {
            match src.read(name) {
                Ok(bytes) if Some(chain::file_digest(&bytes).as_str()) == meta["sha256"].as_str() => {}
                Ok(_) => failures.push(format!("{name}: sha256 differs from manifest.json")),
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
    }

    let genesis = match manifest["chain"].get("genesis") {
        None => chain::GENESIS,
        Some(v) => v
            .as_str()
            .and_then(chain::unhex)
            .ok_or_else(|| anyhow!("manifest.json chain.genesis is not a SHA-256 hex value"))?,
    };
    let token_bound = manifest["chain"]["token_bound"].as_bool().unwrap_or(false);
    let token_checked = args.token.is_some();
    if let Some(token) = &args.token {
        if !token_bound {
            failures.push("token: the bundle is not bound to a token".into());
        } else if chain::genesis(Some(&chain::token_digest(token))) != genesis {
            failures
                .push("token: its SHA-256 is not the chain genesis; the session was started with another token".into());
        }
    }
    let mut entries: Vec<ChainEntry> = Vec::new();
    let mut head = genesis;
    for (i, (_, v)) in parse_json_lines("chain.jsonl", &src.text("chain.jsonl")?)?.into_iter().enumerate() {
        let (Some(leaf), Some(link)) =
            (v["leaf"].as_str().and_then(chain::unhex), v["chain"].as_str().and_then(chain::unhex))
        else {
            failures.push(format!("chain.jsonl entry {i}: missing leaf or chain"));
            break;
        };
        if v["seq"].as_i64() != Some(i as i64) {
            failures.push(format!("chain.jsonl entry {i}: seq {} breaks the sequence", v["seq"]));
            break;
        }
        head = chain::link(&head, &leaf);
        if head != link {
            failures.push(format!("chain.jsonl seq {i}: chain does not follow from the previous entries"));
            break;
        }
        entries.push(ChainEntry { leaf, chain: link, fate: v["fate"].as_str().unwrap_or("").to_string() });
    }
    if manifest["chain"]["head"].as_str() != Some(chain::hex(&head).as_str())
        || manifest["chain"]["events"].as_u64() != Some(entries.len() as u64)
    {
        failures.push("manifest.json chain head or event count does not match chain.jsonl".into());
    }

    let session_id = manifest["session"]["id"].as_str().unwrap_or("").to_string();
    let mut seen = vec![false; entries.len()];
    let mut exit_diff: Option<String> = None;
    let mut exit_seq: Option<i64> = None;
    let mut exit_redacted = false;
    let (mut kept, mut redacted) = (0u64, 0u64);
    for (line, v) in parse_json_lines("events.jsonl", &src.text("events.jsonl")?)? {
        let seq = v["seq"].as_i64().unwrap_or(-1);
        let Some(entry) = usize::try_from(seq).ok().and_then(|i| entries.get(i)) else {
            failures.push(format!("events.jsonl seq {seq}: not in chain.jsonl"));
            continue;
        };
        if v["session_id"] != session_id.as_str() {
            failures.push(format!("events.jsonl seq {seq}: belongs to another session"));
        }
        seen[seq as usize] = true;
        if v["event_type"] == "git.snapshot" && v["payload"]["phase"] == "exit" {
            exit_seq = Some(seq);
            exit_redacted = entry.fate == "redacted";
            if entry.fate == "kept" {
                exit_diff = v["payload"]["diff_sha256"].as_str().map(str::to_string);
            }
        }
        match entry.fate.as_str() {
            "kept" if chain::leaf(&line) == entry.leaf => kept += 1,
            "kept" => failures.push(format!("events.jsonl seq {seq}: content differs from its recorded hash")),
            "redacted" => redacted += 1,
            other => failures.push(format!("events.jsonl seq {seq}: present although chain.jsonl marks it {other:?}")),
        }
    }
    let cut = entries.iter().filter(|e| e.fate == "cut").count();
    for (i, e) in entries.iter().enumerate() {
        if !seen[i] && e.fate != "cut" {
            failures.push(format!("events.jsonl seq {i}: missing although chain.jsonl marks it {:?}", e.fate));
        }
    }

    let mut anchored = None;
    if let Some(path) = &args.anchors {
        let mut count = 0;
        if !path.is_file() {
            bail!("anchors file {} not found", path.display());
        }
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        for (_, a) in parse_json_lines(&path.display().to_string(), &text)? {
            let seq = a["seq"].as_i64().unwrap_or(-1);
            if a["session_id"] != session_id.as_str() {
                failures.push(format!("anchor at seq {seq}: names session {}, not {session_id}", a["session_id"]));
                continue;
            }
            if let Some(g) = a.get("genesis")
                && g.as_str() != Some(chain::hex(&genesis).as_str())
            {
                failures.push(format!("anchor at seq {seq}: genesis differs; the anchors were made for another token"));
                continue;
            }
            match (usize::try_from(seq).ok().and_then(|i| entries.get(i)), a["chain"].as_str().and_then(chain::unhex)) {
                (Some(e), Some(h)) if e.chain == h => {
                    count += 1;
                    anchored = anchored.max(Some(seq));
                }
                (None, _) => failures.push(format!("anchor at seq {seq}: the bundle has no such event")),
                _ => failures.push(format!("anchor at seq {seq}: chain differs; events up to it were changed")),
            }
        }
        if count == 0 && failures.is_empty() {
            failures.push(format!("{}: no anchor lines", path.display()));
        }
    }

    let token_ok = token_bound && token_checked && !failures.iter().any(|f| f.starts_with("token:"));
    let diff_digest =
        manifest["files"].get(DIFF_FILE).and_then(|_| src.read(DIFF_FILE).ok()).map(|b| chain::file_digest(&b));
    let exit_anchored = matches!((anchored, exit_seq), (Some(a), Some(e)) if e <= a);
    let diff_edited = manifest["redaction"]["matches"].as_u64().unwrap_or(0) > 0
        && manifest["diff"]["status"].as_str().is_some_and(|s| s.starts_with("redacted"));
    let mut proves: Vec<&str> = Vec::new();
    let mut gaps: Vec<&str> = Vec::new();
    if failures.is_empty() {
        proves.push("consistent");
        if anchored.is_some() {
            proves.push("anchored");
        } else {
            gaps.push("not anchored");
        }
        if token_ok {
            proves.push("token matched");
        } else if token_bound {
            gaps.push("token not checked");
        }
        if let Some(d) = &diff_digest {
            match &exit_diff {
                Some(h) if h == d && exit_anchored => proves.push("diff matches the session end"),
                Some(h) if h == d => gaps.push("diff matches the recorded hash, but the session end is not anchored"),
                Some(_) if diff_edited => gaps.push("diff differs; the bundle says it was redacted"),
                Some(_) => gaps.push("diff differs from the session end"),
                None if exit_redacted => gaps.push("diff not compared: the session-end record was redacted"),
                None => gaps.push("diff not recorded at the session end"),
            }
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "bundle": dir,
                "session": session_id,
                "ok": failures.is_empty(),
                "proves": proves,
                "gaps": gaps,
                "events": entries.len(),
                "kept": kept,
                "redacted": redacted,
                "cut": cut,
                "head": chain::hex(&head),
                "anchored_through_seq": anchored,
                "anchors_checked": args.anchors.is_some(),
                "token_bound": token_bound,
                "token_checked": token_checked,
                "failures": failures,
            }))?
        );
        return Ok(if failures.is_empty() { 0 } else { 1 });
    }
    println!("bundle:   {}", dir.display());
    println!("session:  {session_id}");
    println!("events:   {} in chain ({kept} kept and hash-checked, {redacted} redacted, {cut} cut)", entries.len());
    println!("head:     {}", chain::hex(&head));
    match (token_bound, token_checked) {
        (true, true) if failures.iter().any(|f| f.starts_with("token:")) => println!("token:    does not match"),
        (true, true) => println!("token:    matches the chain genesis"),
        (true, false) => println!("token:    the chain is bound to a token; pass --token to check it"),
        (false, _) => println!("token:    not bound to a token"),
    }
    match (&args.anchors, anchored) {
        (Some(_), Some(seq)) => {
            let tail = entries.len() as i64 - 1 - seq;
            if tail == 0 {
                println!("anchors:  all {} events match the anchors", entries.len());
            } else {
                println!("anchors:  events 0-{seq} match the anchors; {tail} later events are not anchored");
            }
        }
        (Some(_), None) => println!("anchors:  none matched"),
        (None, _) => println!(
            "anchors:  not checked; without anchors this shows the files are consistent, not that they match the session"
        ),
    }
    if let Some(d) = &diff_digest {
        match &exit_diff {
            Some(h) if h == d && exit_anchored => {
                println!("diff:     matches the hash recorded when the session ended")
            }
            Some(h) if h == d => println!(
                "diff:     matches the hash in the session-end record, but no anchor covers that record, so the bundle author could have written both"
            ),
            Some(_) if diff_edited => println!(
                "diff:     differs from the hash recorded when the session ended; the bundle says redaction changed it"
            ),
            Some(_) => println!("diff:     differs from the hash recorded when the session ended"),
            None if exit_redacted => {
                println!("diff:     not compared; the session-end record was redacted for sharing")
            }
            None => println!("diff:     no hash was recorded when the session ended; not compared"),
        }
    }
    if failures.is_empty() {
        let gaps = if gaps.is_empty() { String::new() } else { format!("; {}", gaps.join(", ")) };
        println!("result:   ok: {}{gaps}", proves.join(", "));
        return Ok(0);
    }
    println!("result:   FAILED");
    for f in &failures {
        println!("  {f}");
    }
    Ok(1)
}

pub fn view(layout: &Layout, args: ViewArgs) -> Result<i32> {
    let as_dir = Path::new(&args.target);
    let looks_like_path = args.target.contains('/') || args.target.ends_with(".zip") || args.target.starts_with('.');
    if looks_like_path && !as_dir.exists() {
        bail!("{} does not exist; pass a bundle directory, a zip file or a session id", as_dir.display());
    }
    let index = if as_dir.join("manifest.json").is_file() {
        as_dir.join("index.html")
    } else if as_dir.is_file() {
        let mut src = Source::open(as_dir)?;
        let manifest: Value = serde_json::from_slice(&src.read("manifest.json").context("reading manifest.json")?)
            .context("manifest.json is not JSON")?;
        let Some(id) = manifest["session"]["id"].as_str().filter(|id| !id.is_empty() && !id.contains('/')) else {
            bail!("{} has no session id in its manifest", as_dir.display());
        };
        let dir = layout.view_dir(id);
        prepare_dir(&dir, true)?;
        fs::write(dir.join("index.html"), src.read("index.html").context("reading index.html")?)
            .context("writing index.html")?;
        dir.join("index.html")
    } else {
        let store = open_store(layout)?;
        let (s, _) = finished_session(layout, &store, &args.target, "the dashboard")?;
        let dir = layout.view_dir(&s.id);
        prepare_dir(&dir, true)?;
        let items = load(&store, &s.id)?;
        let (story, _) = agent_story(&store, &s, &items, &[], &[], true)?;
        let (review, _) = review_data(&store, &s, &[], &[], true, true)?;
        let (files, _) = write_bundle(
            &store,
            &s,
            &dir.display().to_string(),
            &items,
            &Edits { patterns: &[], cut_report: &[], matches: 0 },
            Some(session_diff(&s, &items, &[])),
            with_story(review, story),
        )?;
        write_dir(&dir, &files)?;
        dir.join("index.html")
    };
    if !index.is_file() {
        bail!("{} has no index.html", as_dir.display());
    }
    println!("{}", index.display());
    if !args.no_open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        match std::process::Command::new(opener).arg(&index).status() {
            Ok(st) if st.success() => {}
            _ => eprintln!("witnessed: could not run {opener}; open the file above in a browser"),
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: i64, kind: &str, mono_ms: i64, data: Option<&[u8]>, payload: Value) -> Item {
        let event = Event {
            session_id: "abc123".into(),
            seq,
            ts_ns: 0,
            mono_ns: mono_ms * 1_000_000,
            kind: kind.into(),
            payload,
            data: data.map(<[u8]>::to_vec),
        };
        let leaf = chain::leaf(&chain::event_line(&event));
        Item { event, leaf, fate: Fate::Kept }
    }

    #[test]
    fn redacts_matches_split_across_output_events_and_in_payloads() {
        let mut items = vec![
            ev(0, kind::TERMINAL_OUTPUT, 1, Some(b"token=sk-AB"), Value::Null),
            ev(1, kind::TERMINAL_OUTPUT, 2, Some(b"CD done"), Value::Null),
            ev(2, kind::TERMINAL_NOTIFICATION, 3, None, json!({"source": "osc9", "text": "key sk-XY1"})),
            ev(3, kind::TERMINAL_OUTPUT, 4, Some(b"clean"), Value::Null),
        ];
        let found = apply_redactions(&mut items, &[Regex::new("sk-[A-Z0-9]+").unwrap()]);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].mono_ns, found[0].preview.as_str()), (1_000_000, "sk-A*** (7 bytes)"));
        assert_eq!(found[1].kind, kind::TERMINAL_NOTIFICATION);
        assert_eq!(items[0].event.data.as_deref(), Some(&b"token=*****"[..]));
        assert_eq!(items[1].event.data.as_deref(), Some(&b"** done"[..]));
        assert_eq!(items[2].event.payload["text"], "key ******");
        let fates: Vec<Fate> = items.iter().map(|i| i.fate).collect();
        assert_eq!(fates, [Fate::Redacted, Fate::Redacted, Fate::Redacted, Fate::Kept]);
    }

    #[test]
    fn cuts_only_content_events_inside_the_range() {
        let mut items = vec![
            ev(0, kind::TERMINAL_OUTPUT, 900, Some(b"a"), Value::Null),
            ev(1, kind::TERMINAL_OUTPUT, 1000, Some(b"b"), Value::Null),
            ev(2, kind::TERMINAL_RESIZED, 1500, None, json!({"rows": 1, "cols": 1})),
            ev(3, kind::USER_MARK, 1999, None, json!({"label": "x"})),
            ev(4, kind::TERMINAL_OUTPUT, 2000, Some(b"c"), Value::Null),
        ];
        let report = apply_cuts(&mut items, &[parse_cut("1-2").unwrap()]);
        assert_eq!(report[0]["events"], 2);
        let fates: Vec<Fate> = items.iter().map(|i| i.fate).collect();
        assert_eq!(fates, [Fate::Kept, Fate::Cut, Fate::Kept, Fate::Cut, Fate::Kept]);
    }

    #[test]
    fn built_in_secret_patterns_compile_and_match_their_shapes() {
        let args = BundleArgs {
            session: "last".into(),
            out: None,
            zip: false,
            force: false,
            redact: vec![],
            redact_literal: vec!["a(b".into()],
            redact_secrets: true,
            cut: vec![],
            prompts: false,
            diff: false,
            annotations: false,
            dry_run: true,
            json: false,
        };
        let ps = patterns(&args).unwrap();
        assert_eq!(ps.len(), SECRET_PATTERNS.len() + 1);
        assert!(ps[0].is_match(b"xa(bx") && !ps[0].is_match(b"ab"));
        let samples: &[&[u8]] = &[
            b"sk-ant-api03-abcdefghijklmnop",
            b"ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            b"AKIAABCDEFGHIJKLMNOP",
            b"Bearer abcdefghijklmnopqrstuvwxyz",
            b"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.abcdefghijklmnop",
            b"-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----",
        ];
        for s in samples {
            assert!(ps[1..].iter().any(|p| p.is_match(s)), "{}", String::from_utf8_lossy(s));
        }
        assert!(!ps[1..].iter().any(|p| p.is_match(b"cargo build --release")));
    }

    #[test]
    fn parses_cut_ranges() {
        assert_eq!(
            parse_cut("00:01:00-00:01:30").unwrap(),
            CutRange { from_ns: 60_000_000_000, to_ns: 90_000_000_000 }
        );
        assert!(parse_cut("5-2").is_err());
        assert!(parse_cut("5").is_err());
    }

    #[test]
    fn binary_patches_are_reduced_when_redacting() {
        let diff = b"diff --git a/t.txt b/t.txt\n--- a/t.txt\n+++ b/t.txt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/r.zip b/r.zip\nnew file mode 100644\nindex 0000000..1111111\nGIT binary patch\nliteral 3\nKcmZ?l0000\n\nliteral 0\nHcmV?d00001\n\ndiff --git a/u.txt b/u.txt\n--- a/u.txt\n+++ b/u.txt\n@@ -1 +1 @@\n-c\n+d\n";
        let (out, dropped) = drop_binary_patches(diff);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(dropped, 1);
        assert!(!text.contains("GIT binary patch") && !text.contains("KcmZ"), "{text}");
        assert!(
            text.contains("new file mode 100644\nindex 0000000..1111111\nBinary files a/r.zip and b/r.zip differ\n"),
            "{text}"
        );
        assert!(text.contains("+b\n") && text.ends_with("+d\n"), "{text}");
        let plain = b"diff --git a/t.txt b/t.txt\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(drop_binary_patches(plain), (plain.to_vec(), 0));
    }

    #[test]
    fn dashboard_data_cannot_close_the_script_element() {
        let html = dashboard(&json!({"cast": "</script><script>alert(1)</script>"}));
        assert!(!html.contains("</script><script>alert"));
        assert!(!html.contains(DATA_PLACEHOLDER));
        assert!(!html.contains("__WITNESSED_PLAYER") && !html.contains("cdn.jsdelivr.net"));
        assert!(!PLAYER_JS.contains("</script") && !PLAYER_CSS.contains("</style"));
    }
}
