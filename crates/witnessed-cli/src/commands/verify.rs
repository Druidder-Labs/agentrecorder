use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use witnessed_supervisor::git::DIFF_FILE;
use zip::ZipArchive;

use crate::bundle::{BUNDLE_FORMATS_ACCEPTED, BUNDLE_VERSION};
use crate::chain::{self, Hash};

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

pub(crate) enum Source {
    Dir(PathBuf),
    Zip { archive: ZipArchive<File>, prefix: String },
}

impl Source {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            return Ok(Self::Dir(path.to_path_buf()));
        }
        if !path.exists() {
            bail!("{} does not exist; pass the bundle directory or zip file", path.display());
        }
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let archive = ZipArchive::new(file)
            .with_context(|| format!("{} is neither a directory nor a zip file", path.display()))?;
        let prefix = archive
            .file_names()
            .filter_map(std::result::Result::ok)
            .filter_map(|n| n.strip_suffix("manifest.json").map(str::to_string))
            .min_by_key(String::len)
            .ok_or_else(|| anyhow!("{} is not a witnessed bundle: it has no manifest.json", path.display()))?;
        Ok(Self::Zip { archive, prefix })
    }

    pub(crate) fn read(&mut self, name: &str) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Dir(dir) => fs::read(dir.join(name)),
            Self::Zip { archive, prefix } => {
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

    pub(crate) fn text(&mut self, name: &str) -> Result<String> {
        let bytes = self.read(name).with_context(|| format!("reading {name}"))?;
        String::from_utf8(bytes).with_context(|| format!("{name} is not UTF-8"))
    }
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

/// One way the bundle is inconsistent; the text is what `verify` prints and puts in `failures`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    FileDigest { name: String },
    FileRead { name: String, error: String },
    TokenNotBound,
    TokenMismatch,
    EntryIncomplete { index: usize },
    EntrySeq { index: usize, seq: Value },
    EntryChain { index: usize },
    ManifestChain,
    EventUnchained { seq: i64 },
    EventForeign { seq: i64 },
    EventContent { seq: i64 },
    EventPresent { seq: i64, fate: String },
    EventMissing { seq: usize, fate: String },
    AnchorSession { seq: i64, named: Value, expected: String },
    AnchorGenesis { seq: i64 },
    AnchorNoEvent { seq: i64 },
    AnchorChain { seq: i64 },
    NoAnchors { path: String },
}

impl Failure {
    const fn is_token(&self) -> bool {
        matches!(self, Self::TokenNotBound | Self::TokenMismatch)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileDigest { name } => write!(f, "{name}: sha256 differs from manifest.json"),
            Self::FileRead { name, error } => write!(f, "{name}: {error}"),
            Self::TokenNotBound => f.write_str("token: the bundle is not bound to a token"),
            Self::TokenMismatch => {
                f.write_str("token: its SHA-256 is not the chain genesis; the session was started with another token")
            }
            Self::EntryIncomplete { index } => write!(f, "chain.jsonl entry {index}: missing leaf or chain"),
            Self::EntrySeq { index, seq } => write!(f, "chain.jsonl entry {index}: seq {seq} breaks the sequence"),
            Self::EntryChain { index } => {
                write!(f, "chain.jsonl seq {index}: chain does not follow from the previous entries")
            }
            Self::ManifestChain => f.write_str("manifest.json chain head or event count does not match chain.jsonl"),
            Self::EventUnchained { seq } => write!(f, "events.jsonl seq {seq}: not in chain.jsonl"),
            Self::EventForeign { seq } => write!(f, "events.jsonl seq {seq}: belongs to another session"),
            Self::EventContent { seq } => write!(f, "events.jsonl seq {seq}: content differs from its recorded hash"),
            Self::EventPresent { seq, fate } => {
                write!(f, "events.jsonl seq {seq}: present although chain.jsonl marks it {fate:?}")
            }
            Self::EventMissing { seq, fate } => {
                write!(f, "events.jsonl seq {seq}: missing although chain.jsonl marks it {fate:?}")
            }
            Self::AnchorSession { seq, named, expected } => {
                write!(f, "anchor at seq {seq}: names session {named}, not {expected}")
            }
            Self::AnchorGenesis { seq } => {
                write!(f, "anchor at seq {seq}: genesis differs; the anchors were made for another token")
            }
            Self::AnchorNoEvent { seq } => write!(f, "anchor at seq {seq}: the bundle has no such event"),
            Self::AnchorChain { seq } => write!(f, "anchor at seq {seq}: chain differs; events up to it were changed"),
            Self::NoAnchors { path } => write!(f, "{path}: no anchor lines"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Proof {
    Consistent,
    Anchored,
    TokenMatched,
    DiffMatches,
}

impl std::fmt::Display for Proof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Consistent => "consistent",
            Self::Anchored => "anchored",
            Self::TokenMatched => "token matched",
            Self::DiffMatches => "diff matches the session end",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gap {
    NotAnchored,
    TokenNotChecked,
    DiffUnanchored,
    DiffRedacted,
    DiffDiffers,
    DiffExitRedacted,
    DiffNotRecorded,
}

impl std::fmt::Display for Gap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAnchored => "not anchored",
            Self::TokenNotChecked => "token not checked",
            Self::DiffUnanchored => "diff matches the recorded hash, but the session end is not anchored",
            Self::DiffRedacted => "diff differs; the bundle says it was redacted",
            Self::DiffDiffers => "diff differs from the session end",
            Self::DiffExitRedacted => "diff not compared: the session-end record was redacted",
            Self::DiffNotRecorded => "diff not recorded at the session end",
        })
    }
}

macro_rules! serialize_as_display {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
    )*};
}

serialize_as_display!(Failure, Proof, Gap);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenCheck {
    NotBound,
    Unchecked,
    Matches,
    Mismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorCheck {
    NotChecked,
    NoneMatched,
    Through(i64),
}

/// How `diff.patch` relates to the hash the session-end record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffCheck {
    MatchesAnchored,
    MatchesUnanchored,
    DiffersRedacted,
    Differs,
    ExitRedacted,
    NotRecorded,
}

impl DiffCheck {
    const fn line(self) -> &'static str {
        match self {
            Self::MatchesAnchored => "matches the hash recorded when the session ended",
            Self::MatchesUnanchored => {
                "matches the hash in the session-end record, but no anchor covers that record, so the bundle author could have written both"
            }
            Self::DiffersRedacted => {
                "differs from the hash recorded when the session ended; the bundle says redaction changed it"
            }
            Self::Differs => "differs from the hash recorded when the session ended",
            Self::ExitRedacted => "not compared; the session-end record was redacted for sharing",
            Self::NotRecorded => "no hash was recorded when the session ended; not compared",
        }
    }

    const fn outcome(self) -> Result<Proof, Gap> {
        match self {
            Self::MatchesAnchored => Ok(Proof::DiffMatches),
            Self::MatchesUnanchored => Err(Gap::DiffUnanchored),
            Self::DiffersRedacted => Err(Gap::DiffRedacted),
            Self::Differs => Err(Gap::DiffDiffers),
            Self::ExitRedacted => Err(Gap::DiffExitRedacted),
            Self::NotRecorded => Err(Gap::DiffNotRecorded),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Verdict {
    pub bundle: PathBuf,
    pub session: String,
    pub failures: Vec<Failure>,
    pub events: usize,
    pub kept: u64,
    pub redacted: u64,
    pub cut: usize,
    pub head: String,
    pub token: TokenCheck,
    pub token_checked: bool,
    pub anchors: AnchorCheck,
    pub diff: Option<DiffCheck>,
}

impl Verdict {
    pub(crate) fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// What a consistent bundle proves and what it leaves open; both empty when any check failed.
    fn proves_and_gaps(&self) -> (Vec<Proof>, Vec<Gap>) {
        let (mut proves, mut gaps) = (Vec::new(), Vec::new());
        if !self.ok() {
            return (proves, gaps);
        }
        proves.push(Proof::Consistent);
        match self.anchors {
            AnchorCheck::Through(_) => proves.push(Proof::Anchored),
            AnchorCheck::NotChecked | AnchorCheck::NoneMatched => gaps.push(Gap::NotAnchored),
        }
        match self.token {
            TokenCheck::Matches => proves.push(Proof::TokenMatched),
            TokenCheck::Unchecked => gaps.push(Gap::TokenNotChecked),
            TokenCheck::NotBound | TokenCheck::Mismatch => {}
        }
        if let Some(d) = self.diff {
            match d.outcome() {
                Ok(p) => proves.push(p),
                Err(g) => gaps.push(g),
            }
        }
        (proves, gaps)
    }

    pub(crate) fn to_json(&self) -> Value {
        let (proves, gaps) = self.proves_and_gaps();
        json!({
            "bundle": self.bundle,
            "session": self.session,
            "ok": self.ok(),
            "proves": proves,
            "gaps": gaps,
            "events": self.events,
            "kept": self.kept,
            "redacted": self.redacted,
            "cut": self.cut,
            "head": self.head,
            "anchored_through_seq": match self.anchors { AnchorCheck::Through(seq) => Some(seq), _ => None },
            "anchors_checked": self.anchors != AnchorCheck::NotChecked,
            "token_bound": self.token != TokenCheck::NotBound,
            "token_checked": self.token_checked,
            "failures": self.failures,
        })
    }

    pub(crate) fn text(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "bundle:   {}", self.bundle.display());
        let _ = writeln!(o, "session:  {}", self.session);
        let _ = writeln!(
            o,
            "events:   {} in chain ({} kept and hash-checked, {} redacted, {} cut)",
            self.events, self.kept, self.redacted, self.cut
        );
        let _ = writeln!(o, "head:     {}", self.head);
        let _ = writeln!(
            o,
            "token:    {}",
            match self.token {
                TokenCheck::Mismatch => "does not match",
                TokenCheck::Matches => "matches the chain genesis",
                TokenCheck::Unchecked => "the chain is bound to a token; pass --token to check it",
                TokenCheck::NotBound => "not bound to a token",
            }
        );
        match self.anchors {
            AnchorCheck::Through(seq) => {
                let tail = self.events as i64 - 1 - seq;
                if tail == 0 {
                    let _ = writeln!(o, "anchors:  all {} events match the anchors", self.events);
                } else {
                    let _ =
                        writeln!(o, "anchors:  events 0-{seq} match the anchors; {tail} later events are not anchored");
                }
            }
            AnchorCheck::NoneMatched => o.push_str("anchors:  none matched\n"),
            AnchorCheck::NotChecked => o.push_str(
                "anchors:  not checked; without anchors this shows the files are consistent, not that they match the session\n",
            ),
        }
        if let Some(d) = self.diff {
            let _ = writeln!(o, "diff:     {}", d.line());
        }
        let (proves, gaps) = self.proves_and_gaps();
        if self.ok() {
            let proves: Vec<String> = proves.iter().map(ToString::to_string).collect();
            let gaps: Vec<String> = gaps.iter().map(ToString::to_string).collect();
            let gaps = if gaps.is_empty() { String::new() } else { format!("; {}", gaps.join(", ")) };
            let _ = writeln!(o, "result:   ok: {}{gaps}", proves.join(", "));
            return o;
        }
        o.push_str("result:   FAILED\n");
        for f in &self.failures {
            let _ = writeln!(o, "  {f}");
        }
        o
    }
}

pub(crate) fn verify(args: &VerifyArgs) -> Result<i32> {
    let verdict = check(args)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&verdict.to_json())?);
    } else {
        print!("{}", verdict.text());
    }
    Ok(if verdict.ok() { 0 } else { 1 })
}

pub(crate) fn check(args: &VerifyArgs) -> Result<Verdict> {
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
    if manifest["bundle_version"].as_u64() != Some(u64::from(BUNDLE_VERSION)) {
        bail!("bundle version {} is not supported by this witnessed", manifest["bundle_version"]);
    }
    let mut failures: Vec<Failure> = Vec::new();
    if let Some(files) = manifest["files"].as_object() {
        for (name, meta) in files {
            match src.read(name) {
                Ok(bytes) if Some(chain::file_digest(&bytes).as_str()) == meta["sha256"].as_str() => {}
                Ok(_) => failures.push(Failure::FileDigest { name: name.clone() }),
                Err(e) => failures.push(Failure::FileRead { name: name.clone(), error: e.to_string() }),
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
    let token = match (&args.token, token_bound) {
        (None, false) => TokenCheck::NotBound,
        (None, true) => TokenCheck::Unchecked,
        (Some(_), false) => {
            failures.push(Failure::TokenNotBound);
            TokenCheck::NotBound
        }
        (Some(t), true) if chain::genesis(Some(&chain::token_digest(t))) != genesis => {
            failures.push(Failure::TokenMismatch);
            TokenCheck::Mismatch
        }
        (Some(_), true) => TokenCheck::Matches,
    };
    let mut entries: Vec<ChainEntry> = Vec::new();
    let mut head = genesis;
    for (i, (_, v)) in parse_json_lines("chain.jsonl", &src.text("chain.jsonl")?)?.into_iter().enumerate() {
        let (Some(leaf), Some(link)) =
            (v["leaf"].as_str().and_then(chain::unhex), v["chain"].as_str().and_then(chain::unhex))
        else {
            failures.push(Failure::EntryIncomplete { index: i });
            break;
        };
        if v["seq"].as_i64() != Some(i as i64) {
            failures.push(Failure::EntrySeq { index: i, seq: v["seq"].clone() });
            break;
        }
        head = chain::link(&head, &leaf);
        if head != link {
            failures.push(Failure::EntryChain { index: i });
            break;
        }
        entries.push(ChainEntry { leaf, chain: link, fate: v["fate"].as_str().unwrap_or("").to_string() });
    }
    if manifest["chain"]["head"].as_str() != Some(chain::hex(&head).as_str())
        || manifest["chain"]["events"].as_u64() != Some(entries.len() as u64)
    {
        failures.push(Failure::ManifestChain);
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
            failures.push(Failure::EventUnchained { seq });
            continue;
        };
        if v["session_id"] != session_id.as_str() {
            failures.push(Failure::EventForeign { seq });
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
            "kept" => failures.push(Failure::EventContent { seq }),
            "redacted" => redacted += 1,
            other => failures.push(Failure::EventPresent { seq, fate: other.to_string() }),
        }
    }
    let cut = entries.iter().filter(|e| e.fate == "cut").count();
    for (i, e) in entries.iter().enumerate() {
        if !seen[i] && e.fate != "cut" {
            failures.push(Failure::EventMissing { seq: i, fate: e.fate.clone() });
        }
    }

    let mut anchors = AnchorCheck::NotChecked;
    if let Some(path) = &args.anchors {
        let mut count = 0;
        if !path.is_file() {
            bail!("anchors file {} not found", path.display());
        }
        anchors = AnchorCheck::NoneMatched;
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        for (_, a) in parse_json_lines(&path.display().to_string(), &text)? {
            let seq = a["seq"].as_i64().unwrap_or(-1);
            if a["session_id"] != session_id.as_str() {
                failures.push(Failure::AnchorSession {
                    seq,
                    named: a["session_id"].clone(),
                    expected: session_id.clone(),
                });
                continue;
            }
            if let Some(g) = a.get("genesis")
                && g.as_str() != Some(chain::hex(&genesis).as_str())
            {
                failures.push(Failure::AnchorGenesis { seq });
                continue;
            }
            match (usize::try_from(seq).ok().and_then(|i| entries.get(i)), a["chain"].as_str().and_then(chain::unhex)) {
                (Some(e), Some(h)) if e.chain == h => {
                    count += 1;
                    if !matches!(anchors, AnchorCheck::Through(s) if s >= seq) {
                        anchors = AnchorCheck::Through(seq);
                    }
                }
                (None, _) => failures.push(Failure::AnchorNoEvent { seq }),
                _ => failures.push(Failure::AnchorChain { seq }),
            }
        }
        if count == 0 && failures.is_empty() {
            failures.push(Failure::NoAnchors { path: path.display().to_string() });
        }
    }

    let diff_digest =
        manifest["files"].get(DIFF_FILE).and_then(|_| src.read(DIFF_FILE).ok()).map(|b| chain::file_digest(&b));
    let exit_anchored = matches!((anchors, exit_seq), (AnchorCheck::Through(a), Some(e)) if e <= a);
    let diff_edited = manifest["redaction"]["matches"].as_u64().unwrap_or(0) > 0
        && manifest["diff"]["status"].as_str().is_some_and(|s| s.starts_with("redacted"));
    let diff = diff_digest.map(|d| match &exit_diff {
        Some(h) if *h == d && exit_anchored => DiffCheck::MatchesAnchored,
        Some(h) if *h == d => DiffCheck::MatchesUnanchored,
        Some(_) if diff_edited => DiffCheck::DiffersRedacted,
        Some(_) => DiffCheck::Differs,
        None if exit_redacted => DiffCheck::ExitRedacted,
        None => DiffCheck::NotRecorded,
    });
    debug_assert!(token != TokenCheck::Mismatch || failures.iter().any(Failure::is_token));
    Ok(Verdict {
        bundle: dir.clone(),
        session: session_id,
        failures,
        events: entries.len(),
        kept,
        redacted,
        cut,
        head: chain::hex(&head),
        token,
        token_checked: args.token.is_some(),
        anchors,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use witnessed_store::{Body, NewEvent, NewSession, Store};

    use super::*;
    use crate::bundle::assemble::{Edits, Include, assemble, write_bundle};

    struct Fixture {
        _dir: tempfile::TempDir,
        bundle: PathBuf,
        anchors: PathBuf,
        token: String,
    }

    fn write(token: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        let cmd = vec!["sh".to_string()];
        let digest = token.map(chain::token_digest);
        store
            .create_session(&NewSession {
                id: "abc123",
                command: &cmd,
                cwd: None,
                created_at_ns: 1_000,
                supervisor_pid: 1,
                rows: 24,
                cols: 80,
                record_output: true,
                record_input: false,
                boot_time: None,
                restarted_from: None,
                agent_session_id: None,
                anchor_token_digest: digest.as_deref(),
            })
            .unwrap();
        store.mark_started("abc123", 2, 1_000).unwrap();
        let at = |ms: i64, body: Body, data: Option<&[u8]>| NewEvent {
            ts_ns: 1_000 + ms * 1_000_000,
            mono_ns: ms * 1_000_000,
            body,
            data: data.map(<[u8]>::to_vec),
        };
        let events = [
            at(0, Body::TerminalOutput, Some(b"token sk-SECRET42\n")),
            at(10, Body::TerminalOutput, Some(b"after\n")),
            at(20, Body::ProcessExited { exit_code: Some(2), signal: None, pid: 2, duration_ms: 20 }, None),
        ];
        let mut store = store;
        store.append_events("abc123", 0, &events).unwrap();
        store.mark_exited("abc123", 1_000 + 20_000_000, Some(2), None).unwrap();
        let s = store.session("abc123").unwrap().unwrap();
        let edits = Edits {
            patterns: crate::bundle::redact::patterns(&["sk-[A-Z0-9]+".into()], &[], false).unwrap(),
            cuts: vec![],
        };
        let a = assemble(&store, &s, Include::default(), &edits).unwrap();
        let (files, _) = write_bundle(&store, &s, "b", a, &edits.patterns).unwrap();
        let bundle = dir.path().join("b");
        fs::create_dir(&bundle).unwrap();
        super::super::bundle::write_dir(&bundle, &files).unwrap();
        let chain_lines = fs::read_to_string(bundle.join("chain.jsonl")).unwrap();
        let last: Value = serde_json::from_str(chain_lines.lines().last().unwrap()).unwrap();
        let anchors = dir.path().join("anchors.jsonl");
        let line = json!({"session_id": "abc123", "seq": last["seq"], "chain": last["chain"], "genesis": chain::hex(&chain::genesis(digest.as_deref()))});
        fs::write(&anchors, format!("{line}\n")).unwrap();
        Fixture { _dir: dir, bundle, anchors, token: token.unwrap_or_default().to_string() }
    }

    fn verdict(f: &Fixture, anchors: bool, token: Option<&str>) -> Verdict {
        check(&VerifyArgs {
            dir: f.bundle.clone(),
            anchors: anchors.then(|| f.anchors.clone()),
            token: token.map(str::to_string),
            json: false,
        })
        .unwrap()
    }

    fn edit(f: &Fixture, name: &str, from: &str, to: &str) {
        let path = f.bundle.join(name);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{name} lacks {from:?}");
        fs::write(path, text.replace(from, to)).unwrap();
    }

    #[test]
    fn a_fresh_bundle_is_consistent_and_anchored() {
        let f = write(None);
        let v = verdict(&f, true, None);
        assert!(v.ok(), "{:?}", v.failures);
        assert_eq!((v.events, v.kept, v.redacted, v.cut), (3, 2, 1, 0));
        assert_eq!((v.token, v.anchors, v.diff), (TokenCheck::NotBound, AnchorCheck::Through(2), None));
        assert_eq!(v.proves_and_gaps(), (vec![Proof::Consistent, Proof::Anchored], vec![]));
        let text = v.text();
        assert!(text.contains("anchors:  all 3 events match the anchors\n"), "{text}");
        assert!(text.ends_with("result:   ok: consistent, anchored\n"), "{text}");
        let j = v.to_json();
        assert_eq!((j["ok"].as_bool(), j["anchored_through_seq"].as_i64()), (Some(true), Some(2)));
        assert_eq!(j["proves"], json!(["consistent", "anchored"]));
        let unanchored = verdict(&f, false, None);
        assert_eq!(unanchored.proves_and_gaps(), (vec![Proof::Consistent], vec![Gap::NotAnchored]));
        assert!(unanchored.text().ends_with("result:   ok: consistent; not anchored\n"));
    }

    #[test]
    fn edited_event_content_fails_its_hash() {
        let f = write(None);
        edit(&f, "events.jsonl", "\"exit_code\":2", "\"exit_code\":0");
        let v = verdict(&f, false, None);
        assert_eq!(v.failures, [Failure::FileDigest { name: "events.jsonl".into() }, Failure::EventContent { seq: 2 }]);
        assert!(v.text().ends_with(
            "result:   FAILED\n  events.jsonl: sha256 differs from manifest.json\n  events.jsonl seq 2: content differs from its recorded hash\n"
        ));
        assert_eq!(v.to_json()["failures"][1], "events.jsonl seq 2: content differs from its recorded hash");
        assert!(v.to_json()["proves"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_rewritten_chain_breaks_against_the_anchors_and_the_manifest() {
        let f = write(None);
        let lines = fs::read_to_string(f.bundle.join("chain.jsonl")).unwrap();
        let mut rows: Vec<Value> = lines.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        rows[1]["leaf"] = json!(chain::hex(&chain::leaf("other")));
        let mut head = chain::GENESIS;
        for r in &mut rows {
            head = chain::link(&head, &chain::unhex(r["leaf"].as_str().unwrap()).unwrap());
            r["chain"] = json!(chain::hex(&head));
        }
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        fs::write(f.bundle.join("chain.jsonl"), text).unwrap();
        let v = verdict(&f, true, None);
        assert!(v.failures.contains(&Failure::FileDigest { name: "chain.jsonl".into() }), "{:?}", v.failures);
        assert!(v.failures.contains(&Failure::ManifestChain), "{:?}", v.failures);
        assert!(v.failures.contains(&Failure::EventContent { seq: 1 }), "{:?}", v.failures);
        assert!(v.failures.contains(&Failure::AnchorChain { seq: 2 }), "{:?}", v.failures);
        assert!(v.text().contains("chain differs"));
    }

    #[test]
    fn a_broken_link_and_a_missing_or_extra_event_are_named() {
        let f = write(None);
        let lines = fs::read_to_string(f.bundle.join("chain.jsonl")).unwrap();
        let mut rows: Vec<Value> = lines.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        rows[2]["chain"] = json!(chain::hex(&[7; 32]));
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        fs::write(f.bundle.join("chain.jsonl"), text).unwrap();
        let v = verdict(&f, false, None);
        assert!(v.failures.contains(&Failure::EntryChain { index: 2 }), "{:?}", v.failures);
        assert!(v.failures.contains(&Failure::EventUnchained { seq: 2 }), "{:?}", v.failures);

        let f = write(None);
        let events = fs::read_to_string(f.bundle.join("events.jsonl")).unwrap();
        let kept: String = events.lines().take(2).map(|l| format!("{l}\n")).collect();
        fs::write(f.bundle.join("events.jsonl"), kept).unwrap();
        let v = verdict(&f, false, None);
        assert!(v.failures.contains(&Failure::EventMissing { seq: 2, fate: "kept".into() }), "{:?}", v.failures);
        assert!(v.text().contains("missing although chain.jsonl marks it \"kept\""));
    }

    #[test]
    fn a_missing_file_and_a_foreign_anchor_fail() {
        let f = write(None);
        fs::remove_file(f.bundle.join("session.cast")).unwrap();
        let v = verdict(&f, false, None);
        assert!(
            matches!(&v.failures[..], [Failure::FileRead { name, .. }] if name == "session.cast"),
            "{:?}",
            v.failures
        );
        let f = write(None);
        let line = json!({"session_id": "other1", "seq": 2, "chain": "00"});
        fs::write(&f.anchors, format!("{line}\n")).unwrap();
        let v = verdict(&f, true, None);
        assert_eq!(v.failures, [Failure::AnchorSession { seq: 2, named: json!("other1"), expected: "abc123".into() }]);
        assert_eq!(v.failures[0].to_string(), "anchor at seq 2: names session \"other1\", not abc123");
        assert_eq!(v.anchors, AnchorCheck::NoneMatched);
        assert!(v.text().contains("anchors:  none matched\n"));
        fs::write(&f.anchors, "\n").unwrap();
        let v = verdict(&f, true, None);
        assert!(matches!(&v.failures[..], [Failure::NoAnchors { .. }]), "{:?}", v.failures);
    }

    #[test]
    fn tokens_are_checked_against_the_genesis() {
        let f = write(Some("invite-42"));
        let v = verdict(&f, true, None);
        assert_eq!(v.token, TokenCheck::Unchecked);
        assert_eq!(v.proves_and_gaps().1, [Gap::TokenNotChecked]);
        assert!(v.text().contains("token:    the chain is bound to a token; pass --token to check it\n"));
        let v = verdict(&f, true, Some(&f.token));
        assert_eq!(v.token, TokenCheck::Matches);
        assert_eq!(v.proves_and_gaps().0, [Proof::Consistent, Proof::Anchored, Proof::TokenMatched]);
        assert!(v.text().contains("token:    matches the chain genesis\n"));
        let v = verdict(&f, true, Some("invite-43"));
        assert_eq!((v.token, &v.failures[..]), (TokenCheck::Mismatch, &[Failure::TokenMismatch][..]));
        assert!(v.text().contains("token:    does not match\n") && v.text().contains("result:   FAILED\n"));
        let unbound = write(None);
        let v = verdict(&unbound, false, Some("x"));
        assert_eq!(v.failures, [Failure::TokenNotBound]);
        assert_eq!(v.to_json()["token_checked"], true);
    }

    #[test]
    fn a_genesis_mismatch_in_the_anchors_names_the_token() {
        let f = write(Some("invite-42"));
        let line = json!({"session_id": "abc123", "seq": 2, "chain": "00", "genesis": chain::hex(&chain::GENESIS)});
        fs::write(&f.anchors, format!("{line}\n")).unwrap();
        let v = verdict(&f, true, None);
        assert_eq!(v.failures, [Failure::AnchorGenesis { seq: 2 }]);
    }

    #[test]
    fn unknown_formats_and_versions_are_errors() {
        let f = write(None);
        edit(&f, "manifest.json", "\"bundle_version\": 1", "\"bundle_version\": 9");
        let err = check(&VerifyArgs { dir: f.bundle.clone(), anchors: None, token: None, json: false }).unwrap_err();
        assert!(err.to_string().contains("bundle version 9 is not supported"), "{err:#}");
        edit(&f, "manifest.json", "\"format\": \"witnessed-bundle\"", "\"format\": \"other\"");
        let err = check(&VerifyArgs { dir: f.bundle.clone(), anchors: None, token: None, json: false }).unwrap_err();
        assert!(err.to_string().contains("is not a witnessed bundle"), "{err:#}");
    }

    #[test]
    fn display_strings_are_the_printed_failures() {
        let cases = [
            (Failure::FileDigest { name: "a".into() }, "a: sha256 differs from manifest.json"),
            (Failure::TokenNotBound, "token: the bundle is not bound to a token"),
            (Failure::EntrySeq { index: 1, seq: json!(5) }, "chain.jsonl entry 1: seq 5 breaks the sequence"),
            (Failure::EntryIncomplete { index: 0 }, "chain.jsonl entry 0: missing leaf or chain"),
            (Failure::EventForeign { seq: 3 }, "events.jsonl seq 3: belongs to another session"),
            (
                Failure::EventPresent { seq: 3, fate: "cut".into() },
                "events.jsonl seq 3: present although chain.jsonl marks it \"cut\"",
            ),
            (Failure::AnchorNoEvent { seq: 9 }, "anchor at seq 9: the bundle has no such event"),
        ];
        for (f, want) in cases {
            assert_eq!(f.to_string(), want);
            assert_eq!(serde_json::to_value(&f).unwrap(), json!(want));
        }
        assert_eq!(DiffCheck::MatchesAnchored.outcome(), Ok(Proof::DiffMatches));
        assert_eq!(
            Gap::DiffUnanchored.to_string(),
            "diff matches the recorded hash, but the session end is not anchored"
        );
    }
}
