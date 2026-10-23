use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::Result;
use regex::bytes::Regex;
use serde::Serialize;
use serde::ser::SerializeStruct;
use serde_json::Value;
use witnessed_store::{Annotation, Event, Session, Store, format_rfc3339, kind, now_unix_ns};
use witnessed_supervisor::git::DIFF_FILE;

use super::cut::{CutRange, CutReport, apply_cuts, in_cut};
use super::diff::{DiffInfo, MAX_DIFF_SHOWN, SessionDiff, session_diff};
use super::redact::{Match, apply_redactions, mask_string, mask_value};
use super::{BUNDLE_FORMAT, BUNDLE_VERSION};
use crate::chain::{self, Hash};
use crate::commands::export::{Cast, CastWriter, cast_header, cast_line, recorded_term};
use crate::decisions::{Decisions, Kind};
use crate::outcome::Outcome;
use crate::render::initial_size;
use crate::transcript::{self, InteractionJson};

const DASHBOARD: &str = include_str!("../dashboard.html");
const PLAYER_JS: &str = include_str!("../vendor/asciinema-player/asciinema-player.min.js");
const PLAYER_CSS: &str = include_str!("../vendor/asciinema-player/asciinema-player.css");
const DATA_PLACEHOLDER: &str = "__WITNESSED_DATA__";
const TIMELINE_SKIP: &[&str] = &[kind::TERMINAL_OUTPUT, kind::TERMINAL_INPUT, kind::TERMINAL_SNAPSHOT];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fate {
    Kept,
    Redacted,
    Cut,
}

impl Fate {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::Redacted => "redacted",
            Self::Cut => "cut",
        }
    }
}

/// One recorded event with its leaf hash and the JSON line it was hashed from (stale once redacted).
pub(crate) struct Item {
    pub event: Event,
    pub leaf: Hash,
    pub fate: Fate,
    pub line: String,
}

impl Item {
    pub(crate) fn new(event: Event) -> Self {
        let line = chain::event_line(&event);
        let leaf = chain::leaf(&line);
        Self { event, leaf, fate: Fate::Kept, line }
    }
}

#[cfg(test)]
pub(crate) fn test_item(seq: i64, kind: &str, mono_ms: i64, data: Option<&[u8]>, payload: Value) -> Item {
    Item::new(Event {
        session_id: "abc123".into(),
        seq,
        ts_ns: 0,
        mono_ns: mono_ms * 1_000_000,
        kind: kind.into(),
        payload,
        data: data.map(<[u8]>::to_vec),
    })
}

pub(crate) fn load(store: &Store, id: &str) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    store.for_each_event(id, None, |e| {
        items.push(Item::new(e));
        Ok(())
    })?;
    Ok(items)
}

pub(crate) fn count_fates(items: &[Item]) -> BTreeMap<&'static str, u64> {
    let mut fates = BTreeMap::new();
    for it in items {
        *fates.entry(it.fate.as_str()).or_default() += 1;
    }
    fates
}

pub(crate) fn session_root(items: &[Item]) -> Option<String> {
    items
        .iter()
        .find(|it| it.event.kind == kind::GIT_SNAPSHOT)
        .and_then(|it| it.event.payload["root"].as_str().map(str::to_string))
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StoryInteraction {
    pub t_ms: Option<i64>,
    #[serde(flatten)]
    pub base: InteractionJson,
}

/// The `prompts` object of the manifest: what the agent transcript said, after cuts.
#[derive(Debug, Clone, Default)]
pub(crate) struct Story {
    pub available: bool,
    pub reason: Option<String>,
    pub source: String,
    pub text: bool,
    pub cut: u64,
    pub interactions: Vec<StoryInteraction>,
    pub interrupts: Vec<i64>,
    pub refusals: Vec<i64>,
    pub compactions: Vec<i64>,
}

impl Story {
    fn unavailable(reason: String) -> Self {
        Self { available: false, reason: Some(reason), ..Self::default() }
    }

    fn mask(&mut self, patterns: &[Regex]) -> Vec<String> {
        let mut found = Vec::new();
        if !self.text {
            return found;
        }
        for t in &mut self.interactions {
            if let Some(m) = t.base.model.as_mut() {
                found.extend(mask_string(m, patterns));
            }
            found.extend(mask_string(&mut t.base.origin, patterns));
            if let Some(text) = t.base.text.as_mut() {
                found.extend(mask_string(text, patterns));
            }
        }
        found
    }
}

impl Serialize for Story {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.available {
            let mut s = serializer.serialize_struct("Story", 2)?;
            s.serialize_field("available", &false)?;
            s.serialize_field("reason", &self.reason)?;
            return s.end();
        }
        let mut s = serializer.serialize_struct("Story", 8)?;
        s.serialize_field("available", &true)?;
        s.serialize_field("compactions", &self.compactions)?;
        s.serialize_field("cut", &self.cut)?;
        s.serialize_field("interactions", &self.interactions)?;
        s.serialize_field("interrupts", &self.interrupts)?;
        s.serialize_field("refusals", &self.refusals)?;
        s.serialize_field("source", &self.source)?;
        s.serialize_field("text", &self.text)?;
        s.end()
    }
}

fn agent_story(store: &Store, s: &Session, items: &[Item], cuts: &[CutReport], with_text: bool) -> Result<Story> {
    let located = match transcript::locate(store, &s.id)? {
        Ok(l) => l,
        Err(u) => return Ok(Story::unavailable(u.reason())),
    };
    let (from_ns, to_ns) = crate::commands::inspect::session_window(s);
    let usage = match located.usage(from_ns, to_ns) {
        Ok(u) => u,
        Err(u) => return Ok(Story::unavailable(u.reason())),
    };
    let base_ns = items.first().map_or(s.created_at_ns, |it| it.event.ts_ns - it.event.mono_ns);
    let ms = |at: i64| ((at - base_ns) / 1_000_000).max(0);
    let mut interactions = Vec::new();
    let mut cut = 0u64;
    for (i, t) in usage.interactions.iter().enumerate() {
        let t_ms = t.at_ns.map(ms);
        if t_ms.is_some_and(|t| in_cut(cuts, t)) {
            cut += 1;
            continue;
        }
        interactions.push(StoryInteraction { t_ms, base: InteractionJson::new(i + 1, t, with_text) });
    }
    let offsets = |list: &[i64]| -> Vec<i64> { list.iter().map(|at| ms(*at)).filter(|t| !in_cut(cuts, *t)).collect() };
    Ok(Story {
        available: true,
        reason: None,
        source: format!("{} transcript", located.agent),
        text: with_text,
        cut,
        interactions,
        interrupts: offsets(&usage.interrupts),
        refusals: offsets(&usage.refusals),
        compactions: offsets(&usage.compactions),
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Include {
    pub prompts: bool,
    pub diff: bool,
    pub annotations: bool,
}

impl Include {
    pub(crate) const ALL: Self = Self { prompts: true, diff: true, annotations: true };
}

#[derive(Default)]
pub(crate) struct Edits {
    pub patterns: Vec<Regex>,
    pub cuts: Vec<CutRange>,
}

/// The review material of a bundle before redaction.
struct ReviewData {
    pub decisions: Decisions,
    pub annotations: Option<Vec<Annotation>>,
    pub outcome: Option<Outcome>,
    pub prompts: Story,
}

impl ReviewData {
    fn gather(store: &Store, s: &Session, cuts: &[CutReport], include: Include, prompts: Story) -> Result<Self> {
        let ranges: Vec<(i64, i64)> = cuts.iter().map(|c| (c.from_ms, c.to_ms)).collect();
        let decisions = crate::decisions::derive(store, s, &ranges)?;
        let ids: BTreeSet<&str> = decisions.moments.iter().map(|m| m.id.as_str()).collect();
        let annotations = if include.annotations {
            Some(store.annotations(&s.id)?.into_iter().filter(|a| ids.contains(a.moment.as_str())).collect())
        } else {
            None
        };
        let outcome = include.diff.then(|| crate::outcome::for_session(store, s));
        Ok(Self { decisions, annotations, outcome, prompts })
    }

    /// Serializes once and masks every string; the previews come back in the order the manifest lists them.
    fn mask(mut self, patterns: &[Regex]) -> Result<(Review, Vec<String>)> {
        let summary = DecisionsSummary {
            annotated: self.annotations.as_ref().map(Vec::len),
            available: self.decisions.available,
            counts: self.decisions.counts.clone(),
        };
        let mut found = self.prompts.mask(patterns);
        let mut json = ReviewJson {
            decisions: serde_json::to_value(&self.decisions)?,
            annotations: serde_json::to_value(&self.annotations)?,
            outcome: serde_json::to_value(&self.outcome)?,
        };
        for v in [&mut json.decisions, &mut json.annotations, &mut json.outcome] {
            found.extend(mask_value(v, patterns));
        }
        Ok((Review { prompts: self.prompts, decisions: summary, json }, found))
    }
}

/// The `decisions` object of the manifest.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DecisionsSummary {
    pub annotated: Option<usize>,
    pub available: bool,
    pub counts: BTreeMap<Kind, u64>,
}

pub(crate) struct ReviewJson {
    pub decisions: Value,
    pub annotations: Value,
    pub outcome: Value,
}

pub(crate) struct Review {
    pub prompts: Story,
    pub decisions: DecisionsSummary,
    pub json: ReviewJson,
}

pub(crate) struct Assembled {
    pub items: Vec<Item>,
    pub cut_report: Vec<CutReport>,
    pub diff: Option<SessionDiff>,
    pub matches: Vec<Match>,
    pub prompt_matches: Vec<String>,
    pub review: Review,
    pub duration_ns: i64,
}

impl Assembled {
    fn total_matches(&self) -> u64 {
        (self.matches.len() + self.prompt_matches.len()) as u64
    }
}

/// Loads, cuts and redacts a finished session; the diff is read before redaction so its base commit is intact.
pub(crate) fn assemble(store: &Store, s: &Session, include: Include, edits: &Edits) -> Result<Assembled> {
    let mut items = load(store, &s.id)?;
    let duration_ns = items.last().map_or(0, |it| it.event.mono_ns);
    let cut_report = apply_cuts(&mut items, &edits.cuts);
    let diff = include.diff.then(|| session_diff(s, &items, &edits.patterns));
    let matches = apply_redactions(&mut items, &edits.patterns);
    let story = agent_story(store, s, &items, &cut_report, include.prompts)?;
    let (review, mut prompt_matches) =
        ReviewData::gather(store, s, &cut_report, include, story)?.mask(&edits.patterns)?;
    if let Some(d) = &diff {
        prompt_matches.extend(d.matches.iter().cloned());
    }
    Ok(Assembled { items, cut_report, diff, matches, prompt_matches, review, duration_ns })
}

pub(crate) type Files = Vec<(&'static str, Vec<u8>)>;

#[derive(Serialize)]
struct ChainLine {
    chain: String,
    fate: &'static str,
    leaf: String,
    seq: i64,
}

#[derive(Serialize)]
struct TimelineEntry<'a> {
    fate: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a Value>,
    seq: i64,
    t_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: &'a str,
}

#[derive(Serialize)]
struct Generator {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct ChainInfo {
    algorithm: &'static str,
    events: usize,
    genesis: String,
    token_bound: bool,
    head: String,
}

#[derive(Serialize)]
struct Redaction {
    patterns: usize,
    matches: u64,
    events: u64,
}

#[derive(Serialize)]
struct FileDigest {
    sha256: String,
    bytes: usize,
}

/// `manifest.json`; written through a `Value`, so the keys come out sorted whatever the field order here.
#[derive(Serialize)]
struct Manifest<'a> {
    format: &'static str,
    bundle_version: u32,
    generator: Generator,
    created_at: String,
    session: Value,
    command_display: String,
    program: String,
    duration_ms: Option<i64>,
    event_counts: BTreeMap<String, u64>,
    chain: ChainInfo,
    fates: BTreeMap<&'static str, u64>,
    redaction: Redaction,
    cuts: &'a [CutReport],
    files: BTreeMap<&'static str, FileDigest>,
    prompts: &'a Story,
    diff: Option<&'a DiffInfo>,
    decisions: &'a DecisionsSummary,
    outcome: &'a Value,
}

/// The data object embedded in `index.html`; fields are in key order because it is serialized directly.
#[derive(Serialize)]
struct Dashboard<'a> {
    #[serde(skip_serializing_if = "Value::is_null")]
    annotations: &'a Value,
    bundle_name: &'a str,
    cast: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    decisions: &'a Value,
    diff: Option<&'a str>,
    manifest: &'a Value,
    #[serde(skip_serializing_if = "Value::is_null")]
    outcome: &'a Value,
    timeline: &'a [TimelineEntry<'a>],
}

/// What `bundle` reports after writing, and the `summary` of `bundle --json`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Summary {
    pub events: usize,
    pub fates: BTreeMap<&'static str, u64>,
    pub matches: u64,
    pub patterns: usize,
    pub chain_head: String,
    pub token_bound: bool,
    pub prompts: Story,
    pub diff: Option<DiffInfo>,
    pub decisions: DecisionsSummary,
    pub outcome: Value,
}

pub(crate) fn write_bundle(
    store: &Store,
    s: &Session,
    bundle_name: &str,
    assembled: Assembled,
    patterns: &[Regex],
) -> Result<(Files, Summary)> {
    let Assembled { items, cut_report, diff, review, .. } = &assembled;
    let matches = assembled.total_matches();
    let n_items = items.len();
    let mut events = String::with_capacity(items.iter().map(|it| it.line.len() + 1).sum());
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
        let line =
            ChainLine { chain: chain::hex(&head), fate: it.fate.as_str(), leaf: chain::hex(&it.leaf), seq: e.seq };
        chain_lines.push_str(&serde_json::to_string(&line)?);
        chain_lines.push('\n');
        if it.fate == Fate::Cut {
            continue;
        }
        *counts.entry(e.kind.clone()).or_default() += 1;
        match it.fate {
            Fate::Kept => events.push_str(&it.line),
            Fate::Redacted | Fate::Cut => events.push_str(&chain::event_line(e)),
        }
        events.push('\n');
        let skipped = TIMELINE_SKIP.contains(&e.kind.as_str());
        if !skipped || it.fate == Fate::Redacted {
            timeline.push(TimelineEntry {
                fate: it.fate.as_str(),
                payload: (!skipped).then_some(&e.payload),
                seq: e.seq,
                t_ms: e.mono_ns / 1_000_000,
                timestamp: (!skipped).then(|| format_rfc3339(e.ts_ns)),
                kind: &e.kind,
            });
        }
    }

    let cast = render_cast(store, s, items, cut_report, patterns)?;
    let mut files: Files = vec![("events.jsonl", events.into_bytes()), ("chain.jsonl", chain_lines.into_bytes())];
    let cast_digest = FileDigest { sha256: chain::file_digest(cast.as_bytes()), bytes: cast.len() };
    let diff_info = diff.as_ref().map(|d| d.info.clone());
    let diff_bytes = diff.as_ref().and_then(|d| d.bytes.as_deref());
    let shown = diff_bytes.filter(|b| b.len() <= MAX_DIFF_SHOWN).map(String::from_utf8_lossy);
    let mut digests: BTreeMap<&'static str, FileDigest> = files
        .iter()
        .map(|(name, body)| (*name, FileDigest { sha256: chain::file_digest(body), bytes: body.len() }))
        .collect();
    digests.insert("session.cast", cast_digest);
    if let Some(bytes) = diff_bytes {
        digests.insert(DIFF_FILE, FileDigest { sha256: chain::file_digest(bytes), bytes: bytes.len() });
    }
    let mut session = serde_json::to_value(s)?;
    mask_value(&mut session, patterns);
    let mut command_display = crate::adapters::display(&s.command);
    mask_string(&mut command_display, patterns);
    let mut program = crate::state::program(s);
    mask_string(&mut program, patterns);
    let redacted = fates.get("redacted").copied().unwrap_or(0);
    let manifest = serde_json::to_value(Manifest {
        format: BUNDLE_FORMAT,
        bundle_version: BUNDLE_VERSION,
        generator: Generator { name: "witnessed", version: env!("CARGO_PKG_VERSION") },
        created_at: format_rfc3339(now_unix_ns()),
        session,
        command_display,
        program,
        duration_ms: crate::state::duration_ms(s),
        event_counts: counts,
        chain: ChainInfo {
            algorithm: chain::ALGORITHM,
            events: items.len(),
            genesis: chain::hex(&genesis),
            token_bound: s.anchor_token_digest.is_some(),
            head: chain::hex(&head),
        },
        fates: fates.clone(),
        redaction: Redaction { patterns: patterns.len(), matches, events: redacted },
        cuts: cut_report,
        files: digests,
        prompts: &review.prompts,
        diff: diff_info.as_ref(),
        decisions: &review.decisions,
        outcome: &review.json.outcome,
    })?;
    let manifest_text = serde_json::to_string_pretty(&manifest)? + "\n";
    let html = dashboard(&Dashboard {
        annotations: &review.json.annotations,
        bundle_name,
        cast: &cast,
        decisions: &review.json.decisions,
        diff: shown.as_deref(),
        manifest: &manifest,
        outcome: &review.json.outcome,
        timeline: &timeline,
    })?;
    drop(timeline);
    files.push(("session.cast", cast.into_bytes()));
    let Assembled { diff, review, .. } = assembled;
    if let Some(bytes) = diff.and_then(|d| d.bytes) {
        files.push((DIFF_FILE, bytes));
    }
    files.push(("manifest.json", manifest_text.into_bytes()));
    files.push(("index.html", html.into_bytes()));
    let summary = Summary {
        events: n_items,
        fates,
        matches,
        patterns: patterns.len(),
        chain_head: chain::hex(&head),
        token_bound: s.anchor_token_digest.is_some(),
        prompts: review.prompts,
        diff: diff_info,
        decisions: review.decisions,
        outcome: review.json.outcome,
    };
    Ok((files, summary))
}

pub(crate) fn story_line(story: &Story) -> String {
    if !story.available {
        return format!("prompts: none ({})", story.reason.as_deref().unwrap_or("no transcript"));
    }
    let typed = story.interactions.iter().filter(|t| t.base.origin == "human" || t.base.origin == "user").count();
    let other = story.interactions.iter().filter(|t| t.base.origin != "none").count() - typed;
    let n = match other {
        0 => format!("{typed}"),
        1 => format!("{typed} typed and 1 command or notification"),
        _ => format!("{typed} typed and {other} commands or notifications"),
    };
    if story.text {
        format!("prompts: {n}, with their text after redaction; read them before sending")
    } else {
        format!("prompts: {n}, with times and tokens; text not included (--prompts adds it)")
    }
}

pub(crate) fn decisions_line(d: &DecisionsSummary, with_annotations: bool) -> Option<String> {
    let n: u64 = d.counts.values().sum();
    if !d.available || n == 0 {
        return None;
    }
    let mut line = format!("decisions: {n} moment{}", if n == 1 { "" } else { "s" });
    match (with_annotations, d.annotated.unwrap_or(0)) {
        (true, a) => {
            let _ = write!(line, ", {a} with your verdict");
        }
        (false, _) => line.push_str("; your verdicts are not included (--annotations adds them)"),
    }
    Some(line)
}

pub(crate) fn summary_line(summary: &Summary) -> String {
    let f = &summary.fates;
    let matches = match summary.matches {
        1 => ", 1 redaction match".to_string(),
        n if summary.patterns > 0 => format!(", {n} redaction matches"),
        _ => String::new(),
    };
    format!(
        "{} events ({} kept, {} redacted, {} cut){matches}, chain head {}",
        summary.events,
        f.get("kept").copied().unwrap_or(0),
        f.get("redacted").copied().unwrap_or(0),
        f.get("cut").copied().unwrap_or(0),
        &summary.chain_head[..16]
    )
}

fn render_cast(store: &Store, s: &Session, items: &[Item], cuts: &[CutReport], patterns: &[Regex]) -> Result<String> {
    let mut header = cast_header(s, Cast::V3, initial_size(store, s)?, recorded_term(store, s)?);
    mask_value(&mut header, patterns);
    let mut out = format!("{header}\n");
    let mut writer = CastWriter::new(Cast::V3);
    let mut pending: Vec<&CutReport> = cuts.iter().collect();
    pending.sort_by_key(|c| c.from_ms);
    let mut pending = pending.into_iter().peekable();
    let push = |out: &mut String, v: Value| {
        out.push_str(&cast_line(&v));
        out.push('\n');
    };
    for it in items.iter().filter(|it| it.fate != Fate::Cut) {
        while let Some(c) = pending.next_if(|c| c.range().from_ns <= it.event.mono_ns) {
            push(&mut out, writer.marker(c.range().from_ns, &c.label()));
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

fn dashboard(data: &impl Serialize) -> Result<String> {
    let json = serde_json::to_string(data)?.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026");
    Ok(DASHBOARD
        .replacen("__WITNESSED_PLAYER_CSS__", PLAYER_CSS, 1)
        .replacen("__WITNESSED_PLAYER_JS__", PLAYER_JS, 1)
        .replacen(DATA_PLACEHOLDER, &json, 1))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn story(origins: &[&str], text: bool) -> Story {
        let interactions = origins
            .iter()
            .enumerate()
            .map(|(i, o)| StoryInteraction {
                t_ms: Some(i as i64 * 1000),
                base: InteractionJson {
                    index: i + 1,
                    origin: (*o).to_string(),
                    chars: 3,
                    images: 0,
                    api_calls: 1,
                    input: 1,
                    output: 2,
                    cache_read: 0,
                    cache_creation: 0,
                    total: 3,
                    reply_chars: 0,
                    model: Some("m".into()),
                    text: text.then(|| "fix".to_string()),
                },
            })
            .collect();
        Story { available: true, source: "claude transcript".into(), text, interactions, ..Story::default() }
    }

    fn summary() -> Summary {
        Summary {
            events: 9,
            fates: BTreeMap::from([("kept", 6), ("redacted", 3)]),
            matches: 0,
            patterns: 0,
            chain_head: "50ade4e92d9bf41d9e62d825b0a206c2b186b1cec2230f2adabaa4a54eedd287".into(),
            token_bound: false,
            prompts: Story::unavailable("no".into()),
            diff: None,
            decisions: DecisionsSummary { annotated: None, available: false, counts: BTreeMap::new() },
            outcome: Value::Null,
        }
    }

    #[test]
    fn summary_line_counts_fates_and_matches() {
        let mut s = summary();
        assert_eq!(summary_line(&s), "9 events (6 kept, 3 redacted, 0 cut), chain head 50ade4e92d9bf41d");
        s.matches = 1;
        assert!(summary_line(&s).contains("0 cut), 1 redaction match, chain"), "{}", summary_line(&s));
        s.matches = 6;
        assert!(!summary_line(&s).contains("redaction"));
        s.patterns = 2;
        assert!(summary_line(&s).contains(", 6 redaction matches, chain"), "{}", summary_line(&s));
    }

    #[test]
    fn story_line_explains_availability_text_and_counts() {
        assert_eq!(story_line(&Story::unavailable("why".into())), "prompts: none (why)");
        assert_eq!(
            story_line(&story(&["none", "human", "user"], false)),
            "prompts: 2, with times and tokens; text not included (--prompts adds it)"
        );
        assert_eq!(
            story_line(&story(&["human", "command"], true)),
            "prompts: 1 typed and 1 command or notification, with their text after redaction; read them before sending"
        );
        assert!(
            story_line(&story(&["human", "command", "task-notification"], true))
                .starts_with("prompts: 1 typed and 2 commands or notifications,")
        );
    }

    #[test]
    fn decisions_line_needs_available_moments() {
        let mut d = DecisionsSummary { annotated: None, available: true, counts: BTreeMap::new() };
        assert_eq!(decisions_line(&d, true), None);
        d.counts.insert(Kind::Approved, 1);
        assert_eq!(decisions_line(&d, true).as_deref(), Some("decisions: 1 moment, 0 with your verdict"));
        d.counts.insert(Kind::Corrected, 2);
        d.annotated = Some(2);
        assert_eq!(decisions_line(&d, true).as_deref(), Some("decisions: 3 moments, 2 with your verdict"));
        assert_eq!(
            decisions_line(&d, false).as_deref(),
            Some("decisions: 3 moments; your verdicts are not included (--annotations adds them)")
        );
        d.available = false;
        assert_eq!(decisions_line(&d, true), None);
    }

    #[test]
    fn story_serializes_the_reason_alone_or_the_full_record() {
        assert_eq!(
            serde_json::to_value(Story::unavailable("why".into())).unwrap(),
            json!({"available": false, "reason": "why"})
        );
        let v = serde_json::to_value(story(&["human"], false)).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["available", "compactions", "cut", "interactions", "interrupts", "refusals", "source", "text"]
        );
        let t = &v["interactions"][0];
        assert_eq!((t["index"].as_u64(), t["t_ms"].as_i64(), t["origin"].as_str()), (Some(1), Some(0), Some("human")));
        assert!(t.get("text").is_none());
        let with = serde_json::to_value(story(&["human"], true)).unwrap();
        assert_eq!(with["interactions"][0]["text"], "fix");
    }

    #[test]
    fn story_masking_covers_text_origin_and_model_only_with_text() {
        let ps = [Regex::new("fix|human").unwrap()];
        let mut hidden = story(&["human"], false);
        assert!(hidden.mask(&ps).is_empty());
        assert_eq!(hidden.interactions[0].base.origin, "human");
        let mut shown = story(&["human"], true);
        assert_eq!(shown.mask(&ps).len(), 2);
        assert_eq!(shown.interactions[0].base.origin, "*****");
        assert_eq!(shown.interactions[0].base.text.as_deref(), Some("***"));
    }

    #[test]
    fn dashboard_and_chain_lines_print_keys_in_sorted_order() {
        let payload = json!({"phase": "exit"});
        let timeline = [
            TimelineEntry {
                fate: "kept",
                payload: Some(&payload),
                seq: 1,
                t_ms: 5,
                timestamp: Some("T".into()),
                kind: "git.snapshot",
            },
            TimelineEntry {
                fate: "redacted",
                payload: None,
                seq: 2,
                t_ms: 6,
                timestamp: None,
                kind: "terminal.output",
            },
        ];
        let manifest = json!({"format": "x"});
        let data = Dashboard {
            annotations: &Value::Null,
            bundle_name: "b",
            cast: "c",
            decisions: &json!({"available": true}),
            diff: None,
            manifest: &manifest,
            outcome: &Value::Null,
            timeline: &timeline,
        };
        let direct = serde_json::to_string(&data).unwrap();
        let sorted = serde_json::to_value(&data).unwrap().to_string();
        assert_eq!(direct, sorted);
        assert!(direct.starts_with(r#"{"bundle_name":"b","cast":"c","decisions""#), "{direct}");
        let line = ChainLine { chain: "c".into(), fate: "kept", leaf: "l".into(), seq: 3 };
        assert_eq!(serde_json::to_string(&line).unwrap(), r#"{"chain":"c","fate":"kept","leaf":"l","seq":3}"#);
    }

    #[test]
    fn dashboard_data_cannot_close_the_script_element() {
        let html = dashboard(&json!({"cast": "</script><script>alert(1)</script>"})).unwrap();
        assert!(!html.contains("</script><script>alert"));
        assert!(!html.contains(DATA_PLACEHOLDER));
        assert!(!html.contains("__WITNESSED_PLAYER") && !html.contains("cdn.jsdelivr.net"));
        assert!(!PLAYER_JS.contains("</script") && !PLAYER_CSS.contains("</style"));
    }
}
