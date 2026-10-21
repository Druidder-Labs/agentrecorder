use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agentrecorder_store::{Event, Session, Store, format_rfc3339, kind, now_unix_ns};
use agentrecorder_supervisor::Layout;
use anyhow::{Context, Result, bail};
use regex::bytes::Regex;
use serde_json::{Value, json};

use super::export::{Cast, CastWriter, cast_header, cast_line, recorded_term};
use super::{Follow, open_store};
use crate::chain::{self, Hash};
use crate::render::initial_size;

pub const BUNDLE_FORMAT: &str = "agentrecorder-bundle";
pub const BUNDLE_VERSION: u32 = 1;
const DASHBOARD: &str = include_str!("../dashboard.html");
const DATA_PLACEHOLDER: &str = "__AGENTRECORDER_DATA__";
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

#[derive(clap::Args)]
pub struct BundleArgs {
    #[arg(help = "Session id or a unique prefix of it; the session must have finished")]
    pub session: String,
    #[arg(short, long, help = "Directory to create; it must not exist or be empty")]
    pub out: PathBuf,
    #[arg(
        long,
        value_name = "REGEX",
        help = "Replace every match in output, input, snapshots and event payloads with '*' (repeatable)"
    )]
    pub redact: Vec<String>,
    #[arg(
        long,
        value_name = "FROM-TO",
        help = "Remove terminal data, hooks, marks and notifications between two offsets, such as 00:12:03-00:12:09 (repeatable)"
    )]
    pub cut: Vec<String>,
}

#[derive(clap::Args)]
pub struct AnchorArgs {
    #[arg(help = "Session id or a unique prefix of it")]
    pub session: String,
    #[arg(long, default_value_t = 30, help = "Seconds between anchors while the session runs")]
    pub interval: u64,
}

#[derive(clap::Args)]
pub struct VerifyArgs {
    #[arg(help = "Bundle directory written by `agentrecorder bundle`")]
    pub dir: PathBuf,
    #[arg(long, help = "File of anchor lines received from `agentrecorder anchor` while the session ran")]
    pub anchors: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct ViewArgs {
    #[arg(help = "Bundle directory, or a session id or unique prefix")]
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

#[derive(Debug, PartialEq)]
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
    let (s, _) = finished_session(layout, &store, &args.session)?;
    let cuts = args.cut.iter().map(|c| parse_cut(c)).collect::<Result<Vec<_>>>()?;
    let patterns = args
        .redact
        .iter()
        .map(|p| Regex::new(p).with_context(|| format!("invalid --redact pattern {p:?}")))
        .collect::<Result<Vec<_>>>()?;
    prepare_dir(&args.out, false)?;
    let summary = write_bundle(&store, &s, &args.out, &patterns, &cuts)?;
    println!("wrote bundle for session {} to {}", s.id, args.out.display());
    println!("  {summary}");
    println!("  open it with: agentrecorder view {}", args.out.display());
    println!("  check it with: agentrecorder verify {}", args.out.display());
    Ok(0)
}

fn finished_session(layout: &Layout, store: &Store, id: &str) -> Result<(Session, crate::state::State)> {
    let mut s = store.resolve(id)?;
    let st = crate::state::resolve(layout, store, &mut s);
    if !st.is_finished() {
        bail!(
            "session {} is {}; a bundle needs a finished session (stop it with `agentrecorder stop {}`, or stream \
             anchors meanwhile with `agentrecorder anchor {}`)",
            s.id,
            st.label(),
            s.id,
            s.id
        );
    }
    Ok((s, st))
}

fn prepare_dir(dir: &Path, replace: bool) -> Result<()> {
    if dir.exists() {
        if replace {
            fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
        } else if fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.next().is_some() {
            bail!("{} exists and is not empty; choose a new directory", dir.display());
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
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

fn mask(buf: &mut [u8], patterns: &[Regex]) -> u64 {
    let mut ranges = Vec::new();
    for p in patterns {
        ranges.extend(p.find_iter(buf).filter(|m| !m.is_empty()).map(|m| m.range()));
    }
    for r in &ranges {
        buf[r.clone()].fill(b'*');
    }
    ranges.len() as u64
}

fn mask_value(v: &mut Value, patterns: &[Regex]) -> u64 {
    match v {
        Value::String(s) => {
            let mut bytes = s.as_bytes().to_vec();
            let n = mask(&mut bytes, patterns);
            if n > 0 {
                *s = String::from_utf8_lossy(&bytes).into_owned();
            }
            n
        }
        Value::Array(a) => a.iter_mut().map(|x| mask_value(x, patterns)).sum(),
        Value::Object(o) => o.values_mut().map(|x| mask_value(x, patterns)).sum(),
        _ => 0,
    }
}

fn apply_redactions(items: &mut [Item], patterns: &[Regex]) -> u64 {
    if patterns.is_empty() {
        return 0;
    }
    let mut matches = 0;
    for stream in BYTE_STREAMS {
        let idx: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].fate != Fate::Cut && items[i].event.kind == *stream && items[i].event.data.is_some())
            .collect();
        let mut joined = Vec::new();
        for &i in &idx {
            joined.extend_from_slice(items[i].event.data.as_deref().unwrap_or_default());
        }
        matches += mask(&mut joined, patterns);
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
        let mut n = mask_value(&mut it.event.payload, patterns);
        if it.event.kind == kind::TERMINAL_SNAPSHOT
            && let Some(d) = it.event.data.as_mut()
        {
            n += mask(d, patterns);
        }
        if n > 0 {
            it.fate = Fate::Redacted;
            matches += n;
        }
    }
    matches
}

fn write_bundle(store: &Store, s: &Session, dir: &Path, patterns: &[Regex], cuts: &[CutRange]) -> Result<String> {
    let mut items = load(store, &s.id)?;
    let cut_report = apply_cuts(&mut items, cuts);
    let matches = apply_redactions(&mut items, patterns);

    let mut events = String::new();
    let mut chain_lines = String::new();
    let mut head = chain::GENESIS;
    let mut fates: BTreeMap<&str, u64> = BTreeMap::new();
    let mut timeline = Vec::new();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for it in &items {
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

    let cast = render_cast(store, s, &items, cuts, patterns)?;
    let files = [("events.jsonl", events), ("chain.jsonl", chain_lines), ("session.cast", cast)];
    let mut digests = serde_json::Map::new();
    for (name, body) in &files {
        fs::write(dir.join(name), body).with_context(|| format!("writing {name}"))?;
        digests
            .insert((*name).to_string(), json!({"sha256": chain::file_digest(body.as_bytes()), "bytes": body.len()}));
    }
    let mut manifest = json!({
        "format": BUNDLE_FORMAT,
        "bundle_version": BUNDLE_VERSION,
        "generator": {"name": "agentrecorder", "version": env!("CARGO_PKG_VERSION")},
        "created_at": format_rfc3339(now_unix_ns()),
        "session": s,
        "command_display": crate::adapters::display(&s.command),
        "program": crate::state::program(s),
        "duration_ms": crate::state::duration_ms(s),
        "event_counts": counts,
        "chain": {"algorithm": chain::ALGORITHM, "events": items.len(), "head": chain::hex(&head)},
        "fates": fates,
        "redaction": {"patterns": patterns.len(), "matches": matches, "events": fates.get("redacted").copied().unwrap_or(0)},
        "cuts": cut_report,
        "files": digests,
    });
    for field in ["session", "command_display", "program"] {
        mask_value(&mut manifest[field], patterns);
    }
    let manifest_text = serde_json::to_string_pretty(&manifest)? + "\n";
    fs::write(dir.join("manifest.json"), &manifest_text).context("writing manifest.json")?;
    let data = json!({"manifest": manifest, "timeline": timeline, "cast": files[2].1});
    fs::write(dir.join("index.html"), dashboard(&data)).context("writing index.html")?;
    Ok(format!(
        "{} events ({} kept, {} redacted, {} cut), {} redaction matches, chain head {}",
        items.len(),
        fates.get("kept").copied().unwrap_or(0),
        fates.get("redacted").copied().unwrap_or(0),
        fates.get("cut").copied().unwrap_or(0),
        matches,
        &chain::hex(&head)[..16]
    ))
}

fn render_cast(store: &Store, s: &Session, items: &[Item], cuts: &[CutRange], patterns: &[Regex]) -> Result<String> {
    let mut header = cast_header(s, Cast::V3, initial_size(store, s)?, recorded_term(store, s)?);
    mask_value(&mut header, patterns);
    let mut out = format!("{header}\n");
    let mut writer = CastWriter::new(Cast::V3);
    let mut pending_cuts: Vec<&CutRange> = cuts.iter().collect();
    pending_cuts.sort_by_key(|c| c.from_ns);
    let push = |out: &mut String, v: Value| {
        out.push_str(&cast_line(&v));
        out.push('\n');
    };
    for it in items.iter().filter(|it| it.fate != Fate::Cut) {
        while let Some(c) = pending_cuts.first().filter(|c| c.from_ns <= it.event.mono_ns) {
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
    DASHBOARD.replacen(DATA_PLACEHOLDER, &json, 1)
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
    let mut head = chain::GENESIS;
    let mut last_seq: Option<i64> = None;
    let mut emitted: Option<i64> = None;
    let mut last_emit = Instant::now();
    let emit = |out: &mut dyn Write, seq: i64, head: &Hash| -> Result<()> {
        let line = json!({
            "session_id": s.id,
            "seq": seq,
            "chain": chain::hex(head),
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

fn read_json_lines(path: &Path) -> Result<Vec<(String, Value)>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(n, l)| {
            let v = serde_json::from_str(l).with_context(|| format!("{} line {}: not JSON", path.display(), n + 1))?;
            Ok((l.to_string(), v))
        })
        .collect()
}

pub fn verify(args: VerifyArgs) -> Result<i32> {
    let dir = &args.dir;
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(dir.join("manifest.json"))
            .with_context(|| format!("reading {}/manifest.json", dir.display()))?,
    )
    .context("manifest.json is not JSON")?;
    if manifest["format"] != BUNDLE_FORMAT {
        bail!("{} is not an agentrecorder bundle", dir.display());
    }
    if manifest["bundle_version"].as_u64() != Some(BUNDLE_VERSION as u64) {
        bail!("bundle version {} is not supported by this agentrecorder", manifest["bundle_version"]);
    }
    let mut failures: Vec<String> = Vec::new();
    if let Some(files) = manifest["files"].as_object() {
        for (name, meta) in files {
            match fs::read(dir.join(name)) {
                Ok(bytes) if Some(chain::file_digest(&bytes).as_str()) == meta["sha256"].as_str() => {}
                Ok(_) => failures.push(format!("{name}: sha256 differs from manifest.json")),
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
    }

    let mut entries: Vec<ChainEntry> = Vec::new();
    let mut head = chain::GENESIS;
    for (i, (_, v)) in read_json_lines(&dir.join("chain.jsonl"))?.into_iter().enumerate() {
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
    let (mut kept, mut redacted) = (0u64, 0u64);
    for (line, v) in read_json_lines(&dir.join("events.jsonl"))? {
        let seq = v["seq"].as_i64().unwrap_or(-1);
        let Some(entry) = usize::try_from(seq).ok().and_then(|i| entries.get(i)) else {
            failures.push(format!("events.jsonl seq {seq}: not in chain.jsonl"));
            continue;
        };
        if v["session_id"] != session_id.as_str() {
            failures.push(format!("events.jsonl seq {seq}: belongs to another session"));
        }
        seen[seq as usize] = true;
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
        for (_, a) in read_json_lines(path)? {
            let seq = a["seq"].as_i64().unwrap_or(-1);
            if a["session_id"] != session_id.as_str() {
                failures.push(format!("anchor at seq {seq}: names session {}, not {session_id}", a["session_id"]));
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

    println!("bundle:   {}", dir.display());
    println!("session:  {session_id}");
    println!("events:   {} in chain ({kept} kept and hash-checked, {redacted} redacted, {cut} cut)", entries.len());
    println!("head:     {}", chain::hex(&head));
    match (&args.anchors, anchored) {
        (Some(_), Some(seq)) => {
            let tail = entries.len() as i64 - 1 - seq;
            println!("anchors:  events 0-{seq} match the anchors; {tail} later events are not anchored");
        }
        (Some(_), None) => println!("anchors:  none matched"),
        (None, _) => println!(
            "anchors:  not checked; without anchors this shows the files are consistent, not that they match the session"
        ),
    }
    if failures.is_empty() {
        println!("result:   ok");
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
    let index = if as_dir.join("manifest.json").is_file() {
        as_dir.join("index.html")
    } else {
        let store = open_store(layout)?;
        let (s, _) = finished_session(layout, &store, &args.target)?;
        let dir = layout.view_dir(&s.id);
        prepare_dir(&dir, true)?;
        write_bundle(&store, &s, &dir, &[], &[])?;
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
            _ => eprintln!("agentrecorder: could not run {opener}; open the file above in a browser"),
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
        let n = apply_redactions(&mut items, &[Regex::new("sk-[A-Z0-9]+").unwrap()]);
        assert_eq!(n, 2);
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
    fn parses_cut_ranges() {
        assert_eq!(
            parse_cut("00:01:00-00:01:30").unwrap(),
            CutRange { from_ns: 60_000_000_000, to_ns: 90_000_000_000 }
        );
        assert!(parse_cut("5-2").is_err());
        assert!(parse_cut("5").is_err());
    }

    #[test]
    fn dashboard_data_cannot_close_the_script_element() {
        let html = dashboard(&json!({"cast": "</script><script>alert(1)</script>"}));
        assert!(!html.contains("</script><script>alert"));
        assert!(!html.contains(DATA_PLACEHOLDER));
    }
}
