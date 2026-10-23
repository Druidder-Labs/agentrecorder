use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use witnessed_store::{AgentHook, Body, Session, Store, kind};

/// What the person did at a decision moment; the JSON and `--kind` names are the snake_case variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum Kind {
    Approved,
    Declined,
    Stopped,
    Unanswered,
    NotApproved,
    Unknown,
    Interrupted,
    Corrected,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Declined => "declined",
            Self::Stopped => "stopped",
            Self::Unanswered => "unanswered",
            Self::NotApproved => "not_approved",
            Self::Unknown => "unknown",
            Self::Interrupted => "interrupted",
            Self::Corrected => "corrected",
        }
    }
}

/// Sorted by name, so maps keyed by `Kind` print in the same order as the string-keyed ones they replaced.
impl Ord for Kind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for Kind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Decisions {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub moments: Vec<Moment>,
    pub counts: BTreeMap<Kind, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Moment {
    pub id: String,
    pub t_ms: i64,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_before: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub then: Option<String>,
    pub facts: Vec<String>,
}

impl Moment {
    fn new(id: String, t_ms: i64, kind: Kind) -> Self {
        Self {
            id,
            t_ms,
            kind,
            prompt: None,
            tool: None,
            subject: None,
            wait_ms: None,
            checked_before: None,
            then: None,
            facts: Vec::new(),
        }
    }
}

pub fn derive(store: &Store, s: &Session, cuts_ms: &[(i64, i64)]) -> Result<Decisions> {
    let mut base_ns = None;
    let mut inputs = Inputs { duration_ms: crate::state::duration_ms(s), cwd: s.cwd.clone(), ..Inputs::default() };
    inputs.outputs =
        store.event_times(&s.id, kind::TERMINAL_OUTPUT)?.into_iter().map(|(_, mono)| mono / 1_000_000).collect();
    store.for_each_event(&s.id, Some(&[kind::SESSION_CREATED, kind::AGENT_HOOK]), |e| {
        base_ns.get_or_insert(e.ts_ns - e.mono_ns);
        let t_ms = e.mono_ns / 1_000_000;
        if e.kind == kind::AGENT_HOOK
            && let Some(Body::AgentHook(h)) = e.body()
        {
            inputs.hooks.push(Hook::new(e.seq, t_ms, *h));
        }
        Ok(())
    })?;
    let unavailable =
        |u: &crate::transcript::Unavailable| Ok(Decisions { reason: Some(u.reason()), ..Decisions::default() });
    let located = match crate::transcript::locate_in(inputs.hooks.iter().map(|h| &h.h)) {
        Ok(l) => l,
        Err(u) => return unavailable(&u),
    };
    let (from_ns, to_ns) = crate::commands::inspect::session_window(s);
    let usage = match located.usage(from_ns, to_ns) {
        Ok(u) => u,
        Err(u) => return unavailable(&u),
    };
    let base_ns = base_ns.unwrap_or(s.created_at_ns);
    let ms = |at: i64| ((at - base_ns) / 1_000_000).max(0);
    inputs.prompts = usage
        .interactions
        .iter()
        .enumerate()
        .filter_map(|(i, t)| Some(Prompt { index: i as u64 + 1, t_ms: ms(t.at_ns?), origin: t.origin.clone() }))
        .collect();
    inputs.interrupts = usage.interrupts.iter().map(|a| ms(*a)).collect();
    inputs.refusals = usage.refusals.iter().map(|a| ms(*a)).collect();
    Ok(decide(inputs, cuts_ms))
}

const INF: i64 = i64::MAX;
const EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];
static TEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:[A-Z_][A-Z0-9_]*=\S*\s+|timeout\s+\S+\s+|time\s+|nice\s+(?:-n\s*-?\d+\s+)?|env\s+(?:\S+=\S*\s+)*|command\s+)*",
        r"(?:node\s+--test|(?:\S*/)?python3?(?:\.\d+)?\s+\S*test\S*\.py|bun test|swift test|npx playwright test",
        r"|(?:pnpm|yarn) exec (?:jest|vitest)|(?:\S*/)?python3?(?:\.\d+)?\s+(?:-\S+\s+)*-m\s+(?:pytest|unittest)",
        r"|(?:\S*/)?pytest|uv run (?:pytest|python3? -m (?:pytest|unittest))|poetry run pytest|cargo (?:test|nextest)",
        r"|go test|(?:npm|yarn|pnpm|bun)(?: run)? test|npx (?:jest|vitest|mocha)|jest|vitest|mocha|(?:bundle exec )?rspec",
        r"|(?:\S*/)?phpunit|just (?:check|test)|make (?:test|check)|tox|(?:\./)?gradlew? test|mvn test|ctest|dotnet test",
        r"|deno test|mix test)(?:\s|$)"
    ))
    .expect("valid test regex")
});
static SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*(?:&&|\|\||;|\|)\s*").expect("valid regex"));

#[derive(Debug, Clone)]
struct Hook {
    seq: i64,
    t_ms: i64,
    test: bool,
    h: AgentHook,
}

impl Hook {
    fn new(seq: i64, t_ms: i64, h: AgentHook) -> Self {
        let test = h.tool_name.as_deref() == Some("Bash")
            && h.tool
                .as_ref()
                .and_then(|t| t.command.as_deref())
                .is_some_and(|c| segments(c).iter().any(|x| TEST_RE.is_match(x)));
        Self { seq, t_ms, test, h }
    }
}

#[derive(Debug, Clone)]
struct Prompt {
    index: u64,
    t_ms: i64,
    origin: String,
}

#[derive(Debug, Default, Clone)]
struct Inputs {
    hooks: Vec<Hook>,
    outputs: Vec<i64>,
    prompts: Vec<Prompt>,
    interrupts: Vec<i64>,
    refusals: Vec<i64>,
    duration_ms: Option<i64>,
    cwd: Option<String>,
}

#[derive(Debug, Default)]
struct Turn {
    index: Option<u64>,
    typed: bool,
    t_ms: i64,
    next: i64,
    end: Option<i64>,
    events: Vec<usize>,
    interrupts: Vec<i64>,
    refusals: Vec<i64>,
    interrupted_at: Option<i64>,
}

#[derive(Debug)]
struct Ask {
    turn: usize,
    pos: usize,
    wait_ms: Option<i64>,
    measured: bool,
    outcome: Kind,
    ran: Option<usize>,
}

fn name(h: &Hook) -> &str {
    &h.h.hook_event_name
}

fn tool_name(h: &Hook) -> &str {
    h.h.tool_name.as_deref().unwrap_or("")
}

fn is_call(h: &Hook) -> bool {
    matches!(name(h), "PostToolUse" | "PostToolUseFailure")
}

fn failed(h: &Hook) -> bool {
    name(h) == "PostToolUseFailure"
}

fn command(h: &Hook) -> Option<&str> {
    h.h.tool.as_ref()?.command.as_deref()
}

fn segments(cmd: &str) -> Vec<String> {
    SEPARATORS.split(cmd).map(|x| x.trim_start_matches(['(', '{', ' ', '\t', '\n']).trim().to_string()).collect()
}

const fn is_test(h: &Hook) -> bool {
    h.test
}

fn chained(cmd: &str) -> bool {
    let parts = segments(cmd);
    parts.iter().position(|x| TEST_RE.is_match(x)).is_some_and(|i| i + 1 < parts.len())
}

fn is_edit(h: &Hook) -> bool {
    EDIT_TOOLS.contains(&tool_name(h))
}

fn rel(path: &str, cwd: Option<&str>) -> String {
    let cwd = cwd.unwrap_or("").trim_end_matches('/');
    match path.strip_prefix(&format!("{cwd}/")) {
        Some(r) if !cwd.is_empty() => r.to_string(),
        _ => path.to_string(),
    }
}

fn subject(h: &Hook, cwd: Option<&str>) -> Option<String> {
    let t = h.h.tool.as_ref()?;
    let or = |a: &Option<String>, b: &Option<String>| a.clone().or_else(|| b.clone());
    let s = match tool_name(h) {
        "Bash" => or(&t.command, &t.description),
        "Grep" | "Glob" => t.pattern.clone().map(|p| match &t.path {
            Some(path) => format!("{p} in {}", rel(path, cwd)),
            None => p,
        }),
        "WebFetch" | "WebSearch" => or(&t.url, &t.pattern),
        "Task" | "Agent" => or(&t.subagent, &t.description),
        _ => t.path.as_deref().map(|p| rel(p, cwd)).or_else(|| t.description.clone()),
    };
    s.filter(|s| !s.trim().is_empty()).map(|s| crate::transcript::one_line(&s, 120))
}

fn secs(ms: i64) -> String {
    if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{} s", (ms + 500) / 1000)
    } else {
        crate::fmt::duration_ms(ms)
    }
}

fn sentence(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| format!("{}{}.", f.to_uppercase(), c.as_str()))
}

fn span(xs: &[i64], a: i64, b: i64) -> std::ops::Range<usize> {
    let lo = xs.partition_point(|&x| x < a);
    lo..xs.partition_point(|&x| x < b).max(lo)
}

fn decide(mut inp: Inputs, cuts: &[(i64, i64)]) -> Decisions {
    let in_cut = |t: i64| cuts.iter().any(|&(a, b)| t >= a && t < b);
    inp.hooks.retain(|h| !in_cut(h.t_ms));
    inp.outputs.retain(|&t| !in_cut(t));
    inp.prompts.retain(|p| !in_cut(p.t_ms));
    inp.interrupts.retain(|&t| !in_cut(t));
    inp.refusals.retain(|&t| !in_cut(t));
    inp.hooks.sort_by_key(|h| (h.t_ms, h.seq));
    inp.outputs.sort_unstable();
    inp.interrupts.sort_unstable();
    inp.refusals.sort_unstable();
    let moments: Vec<Moment> = moments(&inp, cuts).into_iter().filter(|m| !in_cut(m.t_ms)).collect();
    let mut counts = BTreeMap::new();
    for m in &moments {
        *counts.entry(m.kind).or_default() += 1;
    }
    Decisions { available: true, reason: None, moments, counts }
}

fn turns(inp: &Inputs, cuts: &[(i64, i64)]) -> Vec<Turn> {
    let hooks = &inp.hooks;
    let hook_t: Vec<i64> = hooks.iter().map(|h| h.t_ms).collect();
    let is_work = |h: &&Hook| !matches!(name(h), "SessionStart" | "SessionEnd" | "UserPromptSubmit");
    let mut turns: Vec<Turn> = inp
        .prompts
        .iter()
        .filter(|p| p.origin != "none")
        .map(|p| Turn {
            index: Some(p.index),
            typed: p.origin == "human" || p.origin == "user",
            t_ms: p.t_ms,
            ..Turn::default()
        })
        .collect();
    let real: Vec<i64> = turns.iter().map(|t| t.t_ms).collect();
    let mut orphans = Vec::new();
    let first_start = real.first().copied().unwrap_or(INF);
    if hooks.iter().filter(is_work).any(|h| h.t_ms < first_start)
        && let Some(t) = hooks.iter().filter(is_work).map(|h| h.t_ms).min()
    {
        orphans.push(t);
    }
    for &(_, to) in cuts {
        let until = real.iter().copied().find(|&t| t >= to).unwrap_or(INF);
        if let Some(w) = hooks.iter().filter(is_work).find(|h| h.t_ms >= to && h.t_ms < until)
            && !orphans.contains(&w.t_ms)
        {
            orphans.push(w.t_ms);
        }
    }
    turns.extend(orphans.into_iter().map(|t_ms| Turn { t_ms, ..Turn::default() }));
    turns.sort_by_key(|t| t.t_ms);
    let cut_after = |ms: i64| cuts.iter().map(|c| c.0).filter(|&f| f > ms).min().unwrap_or(INF);
    let starts: Vec<i64> = turns.iter().map(|t| t.t_ms).collect();
    for (i, t) in turns.iter_mut().enumerate() {
        t.next = starts.get(i + 1).copied().unwrap_or(INF).min(cut_after(t.t_ms));
        let r = span(&hook_t, t.t_ms, t.next);
        t.end = r.clone().map(|j| &hooks[j]).find(|h| matches!(name(h), "Stop" | "StopFailure")).map(|h| h.t_ms);
        t.events = r.filter(|&j| name(&hooks[j]) != "UserPromptSubmit").collect();
        t.interrupts = inp.interrupts[span(&inp.interrupts, t.t_ms, t.next)].to_vec();
        t.refusals = inp.refusals[span(&inp.refusals, t.t_ms, t.next)].to_vec();
        if t.end.is_none() {
            t.interrupted_at = t.interrupts.iter().chain(&t.refusals).copied().max();
        }
    }
    turns
}

const INSTANT_TOOLS: [&str; 7] = ["Edit", "MultiEdit", "NotebookEdit", "Write", "Read", "Glob", "Grep"];

fn asks(inp: &Inputs, turns: &[Turn], cuts: &[(i64, i64)], escaped: &mut BTreeSet<i64>) -> Vec<(usize, Ask)> {
    let hooks = &inp.hooks;
    let cut_after = |ms: i64| cuts.iter().map(|c| c.0).filter(|&f| f > ms).min().unwrap_or(INF);
    let mut out = Vec::new();
    for (ti, t) in turns.iter().enumerate() {
        for (k, &j) in t.events.iter().enumerate() {
            let h = &hooks[j];
            if name(h) != "PermissionRequest" {
                continue;
            }
            let later = &t.events[k + 1..];
            let resumed =
                later.iter().map(|&x| &hooks[x]).find(|x| !matches!(name(x), "Notification" | "PermissionRequest"));
            let session_end = inp.duration_ms.filter(|&d| d != 0).unwrap_or(h.t_ms);
            let mut limit = resumed.map(|x| x.t_ms).or(t.end).unwrap_or(t.next.min(session_end));
            let after = |xs: &[i64]| xs.iter().copied().find(|&ms| ms > h.t_ms && ms < t.next);
            let refused = after(&inp.refusals).filter(|&r| r <= limit);
            if let Some(r) = refused {
                limit = r;
            }
            let esc = if refused.is_none() { after(&inp.interrupts).filter(|&e| e <= limit) } else { None };
            if let Some(e) = esc {
                limit = e;
                escaped.insert(e);
            }
            let cut_at = cut_after(h.t_ms);
            let cut_away = resumed.is_none() && t.end.is_none() && esc.is_none() && cut_at != INF && cut_at <= limit;
            if limit == INF {
                limit = session_end;
            }
            let notes: Vec<i64> = t.events[k..]
                .iter()
                .map(|&x| &hooks[x])
                .filter(|x| name(x) == "Notification" && x.t_ms >= h.t_ms && x.t_ms < limit)
                .map(|x| x.t_ms)
                .collect();
            let instant = resumed
                .filter(|x| {
                    refused.is_none()
                        && esc.is_none()
                        && matches!(name(x), "PostToolUse" | "PostToolUseFailure")
                        && x.h.tool_name == h.h.tool_name
                        && h.h.tool_name.as_deref().is_some_and(|t| INSTANT_TOOLS.contains(&t))
                })
                .map(|x| x.t_ms);
            let from = inp.outputs.partition_point(|&o| o < h.t_ms + 501);
            let resume = instant.or(refused).or(esc).or_else(|| {
                inp.outputs[from..]
                    .iter()
                    .copied()
                    .take_while(|&o| o < limit)
                    .find(|&o| !notes.iter().any(|&n| o >= n - 1000 && o <= n + 1000))
            });
            let wait_ms = (!cut_away).then(|| (resume.unwrap_or(limit) - h.t_ms).max(0));
            let unanswered = resumed.is_none()
                && t.end.is_none()
                && esc.is_none()
                && refused.is_none()
                && !cut_away
                && resume.is_none();
            let next_ask = later
                .iter()
                .position(|&x| matches!(name(&hooks[x]), "PermissionRequest" | "Stop" | "StopFailure"))
                .unwrap_or(later.len());
            let same_cmd = |x: &Hook| command(h).is_none() || command(x).is_none() || command(h) == command(x);
            let ran = later[..next_ask].iter().copied().find(|&x| {
                let x = &hooks[x];
                is_call(x) && x.h.tool_name == h.h.tool_name && same_cmd(x)
            });
            let outcome = if refused.is_some() {
                Kind::Declined
            } else if esc.is_some() {
                Kind::Stopped
            } else if ran.is_some() {
                Kind::Approved
            } else if cut_away {
                Kind::Unknown
            } else if unanswered {
                Kind::Unanswered
            } else {
                Kind::NotApproved
            };
            out.push((j, Ask { turn: ti, pos: k, wait_ms, measured: resume.is_some(), outcome, ran }));
        }
    }
    out
}

fn moments(inp: &Inputs, cuts: &[(i64, i64)]) -> Vec<Moment> {
    let hooks = &inp.hooks;
    let cwd = inp.cwd.as_deref();
    let turns = turns(inp, cuts);
    let mut escaped = BTreeSet::new();
    let asks = asks(inp, &turns, cuts, &mut escaped);
    let mut out = Vec::new();
    for (j, a) in &asks {
        let h = &hooks[*j];
        let t = &turns[a.turn];
        let mut m = Moment::new(format!("p{}", h.seq), h.t_ms, a.outcome);
        m.prompt = t.index;
        m.tool = h.h.tool_name.clone();
        m.subject = subject(h, cwd);
        m.wait_ms = a.wait_ms;
        let wait = a.wait_ms.map(secs).unwrap_or_default();
        match a.outcome {
            Kind::Approved => approved(&mut m, inp, &turns, a, wait),
            Kind::Declined => m.facts.push(format!("Declined at the permission prompt {wait} after the request.")),
            Kind::Stopped => m.facts.push(format!("Pressed Esc at the permission prompt {wait} after the request.")),
            Kind::Unknown => m.facts.push("The answer falls inside a cut range of this bundle.".into()),
            Kind::Unanswered => m.facts.push(
                "No answer recorded: the screen did not change after the request and the agent did not continue."
                    .into(),
            ),
            Kind::NotApproved | Kind::Interrupted | Kind::Corrected => m.facts.push(format!(
                "No {} call followed the request before the next request or stop; the screen changed {wait} after it.",
                h.h.tool_name.as_deref().unwrap_or("tool")
            )),
        }
        out.push(m);
    }
    for &ms in inp.interrupts.iter().filter(|ms| !escaped.contains(ms)) {
        let turn = turns.iter().find(|t| ms >= t.t_ms && ms < t.next);
        let last = turn.and_then(|t| {
            t.events
                .iter()
                .rev()
                .map(|&j| &hooks[j])
                .find(|h| h.t_ms <= ms && matches!(name(h), "PreToolUse" | "PostToolUse" | "PostToolUseFailure"))
        });
        let mut m = Moment::new(format!("i{ms}"), ms, Kind::Interrupted);
        m.prompt = turn.and_then(|t| t.index);
        m.tool = last.and_then(|h| h.h.tool_name.clone());
        m.subject = last.and_then(|h| subject(h, cwd));
        m.facts.push(match turn.and_then(|t| t.index.map(|i| (i, t.t_ms))) {
            Some((i, start)) => {
                format!("Pressed Esc {} after prompt {i} started, while the agent worked.", secs(ms - start))
            }
            None => "Pressed Esc while the agent worked.".into(),
        });
        m.facts.push(match last {
            Some(h) if name(h) == "PreToolUse" => {
                format!("The agent was running {} when it was interrupted.", tool_name(h))
            }
            Some(h) => format!("The last tool call before it was {}.", tool_name(h)),
            None => "No tool call ran before it in this prompt.".into(),
        });
        out.push(m);
    }
    let cut_between = |a: i64, b: i64| cuts.iter().any(|&(from, to)| to > a && from < b);
    for i in 1..turns.len() {
        let (prev, t) = (&turns[i - 1], &turns[i]);
        let Some(index) = t.index.filter(|_| t.typed) else { continue };
        if cut_between(prev.end.or(prev.interrupted_at).unwrap_or(prev.t_ms), t.t_ms) {
            continue;
        }
        let calls: Vec<&Hook> = prev.events.iter().map(|&j| &hooks[j]).filter(|h| is_call(h)).collect();
        let mut causes: Vec<(i64, String, Option<&Hook>)> = Vec::new();
        if let Some(&ms) = prev.interrupts.last() {
            let what = if escaped.contains(&ms) { "Esc at a permission prompt" } else { "an interruption" };
            causes.push((ms, what.into(), None));
        }
        if let Some(&ms) = prev.refusals.last() {
            let ask = asks
                .iter()
                .rev()
                .find(|(_, a)| a.turn == i - 1 && a.outcome == Kind::Declined)
                .map(|(j, _)| &hooks[*j]);
            causes.push((ms, "a declined permission request".into(), ask));
        }
        if let Some(h) = calls.iter().rev().find(|h| is_test(h)).filter(|h| failed(h)) {
            causes.push((h.t_ms, "a failed test run".into(), Some(h)));
        } else if let Some(h) = calls.last().filter(|h| failed(h)) {
            causes.push((h.t_ms, format!("a failed {} call", tool_name(h)), Some(h)));
        }
        let Some((_, cause, by)) = causes.into_iter().max_by_key(|c| c.0) else { continue };
        let wait_ms = match (prev.index, prev.end, prev.interrupted_at) {
            (None, _, _) => None,
            (_, Some(end), _) => Some((t.t_ms - end).max(0)),
            (_, None, Some(at)) => Some((t.t_ms - at).max(0)),
            _ => None,
        };
        let mut m = Moment::new(format!("c{index}"), t.t_ms, Kind::Corrected);
        m.prompt = Some(index);
        m.tool = by.and_then(|h| h.h.tool_name.clone());
        m.subject = by.and_then(|h| subject(h, cwd));
        m.wait_ms = wait_ms;
        let when = wait_ms.map(|w| format!(" {} later", secs(w))).unwrap_or_default();
        m.facts.push(match prev.index {
            Some(p) => format!("Redirected the agent{when}, after {cause} in prompt {p}."),
            None => format!("Redirected the agent{when}, after {cause}."),
        });
        out.push(m);
    }
    out.sort_by(|a, b| a.t_ms.cmp(&b.t_ms).then_with(|| a.id.cmp(&b.id)));
    out
}

fn approved(m: &mut Moment, inp: &Inputs, turns: &[Turn], a: &Ask, wait: String) {
    let hooks = &inp.hooks;
    let t = &turns[a.turn];
    let h = &hooks[t.events[a.pos]];
    let edit = is_edit(h);
    let mut first = format!("Approved in {wait}.");
    if edit || tool_name(h) == "Bash" {
        let before: Vec<&Hook> = t.events[..a.pos].iter().map(|&j| &hooks[j]).collect();
        let last_edit = before.iter().rposition(|x| is_edit(x) && name(x) == "PostToolUse");
        let since = &before[last_edit.map_or(0, |p| p + 1)..];
        let tested = since.iter().any(|x| is_call(x) && is_test(x));
        let path = h.h.tool.as_ref().and_then(|t| t.path.as_deref());
        let read = edit
            && since.iter().any(|x| {
                name(x) == "PostToolUse"
                    && tool_name(x) == "Read"
                    && path.is_some()
                    && x.h.tool.as_ref().and_then(|t| t.path.as_deref()) == path
            });
        m.checked_before = Some(tested || read);
        let scope = if last_edit.is_some() { "since the last edit" } else { "earlier in this prompt" };
        let check = match (tested, read, edit) {
            (true, true, _) => format!("after a test run and a read of the file {scope}"),
            (true, false, _) => format!("after a test run {scope}"),
            (false, true, _) => format!("after a read of the file {scope}"),
            (false, false, true) => format!("without a test or a read of the file {scope}"),
            (false, false, false) => format!("without a test run {scope}"),
        };
        first = format!("Approved in {wait} {check}.");
    }
    m.facts.push(first);
    if !a.measured {
        m.facts.push("The screen did not change before the agent continued, so the time is an upper bound.".into());
    }
    let Some(r) = a.ran else { return };
    let ran = &hooks[r];
    let then = if ran.h.tool.as_ref().is_some_and(|t| t.interrupted) {
        Some("the call was interrupted".to_string())
    } else if tool_name(ran) == "Bash" {
        Some(
            match (is_test(ran), failed(ran)) {
                (true, true) => "tests failed",
                (true, false) => "tests passed",
                (false, true) => "the command failed",
                (false, false) => "the command exited 0",
            }
            .to_string(),
        )
    } else if edit && failed(ran) {
        Some("the edit failed".to_string())
    } else if edit {
        let next = turns.get(a.turn + 1).map_or(&[][..], |n| &n.events[..]);
        let test = t
            .events
            .iter()
            .chain(next)
            .copied()
            .filter(|&j| j > r)
            .map(|j| &hooks[j])
            .find(|x| is_call(x) && is_test(x));
        Some(match test {
            Some(x) if failed(x) => "tests failed next".into(),
            Some(_) => "tests passed next".into(),
            None => "no test ran next".into(),
        })
    } else {
        failed(ran).then(|| "the call failed".to_string())
    };
    if let Some(then) = &then {
        m.facts.push(sentence(then));
    }
    if tool_name(ran) == "Bash" && is_test(ran) && command(ran).is_some_and(chained) {
        m.facts.push("Other commands ran after the tests, so the exit status may not be theirs.".into());
    }
    m.then = then;
}

#[cfg(test)]
mod tests {
    use witnessed_store::ToolSummary;

    use super::*;

    fn hook(seq: i64, t_ms: i64, name: &str, tool: Option<(&str, ToolSummary)>) -> Hook {
        let (tool_name, tool) = tool.map_or((None, None), |(n, t)| (Some(n.to_string()), Some(t)));
        Hook::new(
            seq,
            t_ms,
            AgentHook { agent: "claude".into(), hook_event_name: name.into(), tool_name, tool, ..AgentHook::default() },
        )
    }

    fn bash(cmd: &str) -> Option<(&'static str, ToolSummary)> {
        Some(("Bash", ToolSummary { command: Some(cmd.into()), ..ToolSummary::default() }))
    }

    fn file(tool: &'static str, path: &str) -> Option<(&'static str, ToolSummary)> {
        Some((tool, ToolSummary { path: Some(path.into()), ..ToolSummary::default() }))
    }

    fn prompt(index: u64, t_ms: i64) -> Prompt {
        Prompt { index, t_ms, origin: "human".into() }
    }

    fn inputs(hooks: Vec<Hook>, outputs: Vec<i64>, prompts: Vec<Prompt>) -> Inputs {
        Inputs { hooks, outputs, prompts, duration_ms: Some(60_000), cwd: Some("/w".into()), ..Inputs::default() }
    }

    fn only(d: &Decisions, kind: Kind) -> Moment {
        let found: Vec<&Moment> = d.moments.iter().filter(|m| m.kind == kind).collect();
        assert_eq!(found.len(), 1, "{kind}: {:#?}", d.moments);
        found[0].clone()
    }

    #[test]
    fn approval_wait_skips_the_notification_repaint() {
        let hooks = vec![
            hook(1, 1000, "PostToolUse", file("Edit", "/w/src/a.rs")),
            hook(2, 2000, "PermissionRequest", bash("rm -rf build")),
            hook(3, 8000, "Notification", None),
            hook(4, 10_000, "PostToolUse", bash("rm -rf build")),
            hook(5, 12_000, "Stop", None),
        ];
        let d = decide(inputs(hooks, vec![2100, 7500, 8200, 9500], vec![prompt(1, 0)]), &[]);
        let m = only(&d, Kind::Approved);
        assert_eq!((m.id.as_str(), m.prompt, m.wait_ms), ("p2", Some(1), Some(7500)));
        assert_eq!((m.tool.as_deref(), m.subject.as_deref()), (Some("Bash"), Some("rm -rf build")));
        assert_eq!(m.checked_before, Some(false));
        assert_eq!(m.then.as_deref(), Some("the command exited 0"));
        assert_eq!(m.facts[0], "Approved in 7.5 s without a test run since the last edit.");
        assert_eq!(d.counts, BTreeMap::from([(Kind::Approved, 1)]));
    }

    #[test]
    fn an_unmeasured_wait_ends_when_the_agent_resumes() {
        let hooks = vec![
            hook(1, 2000, "PermissionRequest", bash("ls")),
            hook(2, 2000, "Notification", None),
            hook(3, 4000, "PostToolUse", bash("ls")),
        ];
        let m = only(&decide(inputs(hooks, vec![2600], vec![prompt(1, 0)]), &[]), Kind::Approved);
        assert_eq!(m.wait_ms, Some(2000));
        assert!(m.facts[1].contains("upper bound"), "{:?}", m.facts);
    }

    #[test]
    fn a_file_edit_is_answered_when_it_runs_not_when_the_prompt_blinks() {
        let hooks = vec![
            hook(1, 2000, "PermissionRequest", file("Write", "/w/hello.py")),
            hook(2, 15_600, "PostToolUse", file("Write", "/w/hello.py")),
            hook(3, 16_000, "Stop", None),
        ];
        let blinks: Vec<i64> = (0..22).map(|i| 2520 + i * 600).collect();
        let d = decide(inputs(hooks, blinks, vec![prompt(1, 0)]), &[]);
        let m = only(&d, Kind::Approved);
        assert_eq!(m.wait_ms, Some(13_600));
    }

    #[test]
    fn checked_before_counts_a_test_or_a_read_after_the_last_edit() {
        let hooks = vec![
            hook(1, 1000, "PostToolUse", file("Edit", "/w/src/a.rs")),
            hook(2, 1500, "PostToolUse", bash("cargo test -p x")),
            hook(3, 2000, "PermissionRequest", file("Edit", "/w/src/a.rs")),
            hook(4, 3000, "PostToolUse", file("Edit", "/w/src/a.rs")),
            hook(5, 3500, "PostToolUse", file("Read", "/w/src/b.rs")),
            hook(6, 3600, "PermissionRequest", file("Write", "/w/src/b.rs")),
            hook(7, 3900, "PostToolUse", file("Write", "/w/src/b.rs")),
            hook(8, 4000, "PostToolUseFailure", bash("cargo test")),
            hook(9, 4100, "PermissionRequest", file("Edit", "/w/src/c.rs")),
            hook(10, 4300, "PostToolUse", file("Edit", "/w/src/c.rs")),
            hook(11, 4500, "PermissionRequest", file("Edit", "/w/src/d.rs")),
            hook(12, 4700, "PostToolUse", file("Edit", "/w/src/d.rs")),
            hook(13, 5000, "Stop", None),
        ];
        let d = decide(inputs(hooks, vec![2600, 3700, 4200, 4600], vec![prompt(1, 0)]), &[]);
        let by_id = |id: &str| d.moments.iter().find(|m| m.id == id).unwrap().clone();
        let first = by_id("p3");
        assert_eq!((first.checked_before, first.subject.as_deref()), (Some(true), Some("src/a.rs")));
        assert_eq!(first.then.as_deref(), Some("tests failed next"));
        assert_eq!(first.facts[0], "Approved in 1.0 s after a test run since the last edit.");
        assert_eq!(by_id("p6").checked_before, Some(true));
        assert!(by_id("p6").facts[0].contains("after a read of the file"));
        assert_eq!(by_id("p9").checked_before, Some(true));
        let last = by_id("p11");
        assert_eq!((last.checked_before, last.then.as_deref()), (Some(false), Some("no test ran next")));
        assert_eq!(last.facts[0], "Approved in 0.2 s without a test or a read of the file since the last edit.");
        assert_eq!(d.counts[&Kind::Approved], 4);
    }

    #[test]
    fn decline_then_redirect_is_a_correction() {
        let hooks = vec![hook(1, 2000, "PermissionRequest", bash("rm -rf dist")), hook(2, 2000, "Notification", None)];
        let mut inp = inputs(hooks, vec![5100], vec![prompt(1, 0), prompt(2, 9000)]);
        inp.refusals = vec![5000];
        let d = decide(inp, &[]);
        let m = only(&d, Kind::Declined);
        assert_eq!((m.id.as_str(), m.wait_ms), ("p1", Some(3000)));
        let c = only(&d, Kind::Corrected);
        assert_eq!((c.id.as_str(), c.prompt, c.wait_ms, c.t_ms), ("c2", Some(2), Some(4000), 9000));
        assert_eq!(c.subject.as_deref(), Some("rm -rf dist"));
        assert_eq!(c.facts[0], "Redirected the agent 4.0 s later, after a declined permission request in prompt 1.");
    }

    #[test]
    fn esc_at_a_prompt_is_stopped_and_esc_during_work_is_interrupted() {
        let hooks = vec![
            hook(1, 1000, "PreToolUse", bash("cargo build")),
            hook(2, 2000, "PermissionRequest", bash("cargo build")),
            hook(3, 6000, "PreToolUse", file("Read", "/w/README.md")),
        ];
        let mut inp = inputs(hooks, vec![4100], vec![prompt(1, 0), prompt(2, 20_000)]);
        inp.interrupts = vec![4000, 7000];
        let d = decide(inp, &[]);
        let s = only(&d, Kind::Stopped);
        assert_eq!((s.id.as_str(), s.wait_ms), ("p2", Some(2000)));
        let i = only(&d, Kind::Interrupted);
        assert_eq!(
            (i.id.as_str(), i.prompt, i.tool.as_deref(), i.subject.as_deref()),
            ("i7000", Some(1), Some("Read"), Some("README.md"))
        );
        assert_eq!(i.facts[0], "Pressed Esc 7.0 s after prompt 1 started, while the agent worked.");
        let c = only(&d, Kind::Corrected);
        assert_eq!(
            (c.wait_ms, c.facts[0].as_str()),
            (Some(13_000), "Redirected the agent 13 s later, after an interruption in prompt 1.")
        );
    }

    #[test]
    fn unanswered_and_not_approved() {
        let hooks = vec![
            hook(1, 1000, "PermissionRequest", bash("make deploy")),
            hook(2, 1000, "Notification", None),
            hook(3, 3000, "Stop", None),
            hook(4, 11_000, "PermissionRequest", bash("make clean")),
        ];
        let notice = Prompt { index: 2, t_ms: 10_000, origin: "task-notification".into() };
        let d = decide(inputs(hooks, vec![1900, 2500], vec![prompt(1, 0), notice]), &[]);
        assert_eq!(only(&d, Kind::NotApproved).id, "p1");
        let u = only(&d, Kind::Unanswered);
        assert_eq!((u.id.as_str(), u.prompt, u.wait_ms), ("p4", Some(2), Some(49_000)));
        assert!(d.moments.iter().all(|m| m.kind != Kind::Corrected));
    }

    #[test]
    fn cuts_drop_moments_and_mark_answers_cut_away() {
        let hooks = vec![
            hook(1, 2000, "PermissionRequest", bash("rm a")),
            hook(2, 2500, "PostToolUse", bash("rm a")),
            hook(3, 5000, "PermissionRequest", bash("rm b")),
        ];
        let d = decide(inputs(hooks.clone(), vec![], vec![prompt(1, 0)]), &[(1500, 3000), (8000, 9000)]);
        assert_eq!(d.moments.len(), 1, "{:#?}", d.moments);
        let m = only(&d, Kind::Unknown);
        assert_eq!((m.id.as_str(), m.wait_ms), ("p3", None));
        let all = decide(inputs(hooks, vec![], vec![prompt(1, 0)]), &[]);
        assert_eq!(all.counts, BTreeMap::from([(Kind::Approved, 1), (Kind::Unanswered, 1)]));
    }

    #[test]
    fn a_failed_test_then_a_typed_prompt_is_a_correction() {
        let hooks = vec![
            hook(1, 1000, "PostToolUse", bash("cargo build")),
            hook(2, 1500, "PostToolUseFailure", bash("FOO=1 cargo test --all && echo ok")),
            hook(3, 2000, "Stop", None),
            hook(4, 6000, "PostToolUse", bash("cargo test")),
            hook(5, 7000, "Stop", None),
        ];
        let prompts = vec![prompt(1, 0), prompt(2, 5000), prompt(3, 9000)];
        let d = decide(inputs(hooks, vec![], prompts), &[]);
        let c = only(&d, Kind::Corrected);
        assert_eq!((c.id.as_str(), c.prompt, c.wait_ms), ("c2", Some(2), Some(3000)));
        assert_eq!(
            (c.tool.as_deref(), c.subject.as_deref()),
            (Some("Bash"), Some("FOO=1 cargo test --all && echo ok"))
        );
        assert_eq!(c.facts, vec!["Redirected the agent 3.0 s later, after a failed test run in prompt 1."]);
    }

    #[test]
    fn ids_are_stable_across_runs_and_input_order() {
        let hooks = vec![
            hook(7, 2000, "PermissionRequest", bash("ls")),
            hook(8, 2500, "PostToolUseFailure", bash("ls")),
            hook(9, 3000, "Stop", None),
        ];
        let mut inp = inputs(hooks, vec![2400], vec![prompt(1, 0), prompt(2, 5000)]);
        inp.interrupts = vec![4000];
        let a = decide(inp.clone(), &[]);
        inp.hooks.reverse();
        let b = decide(inp, &[]);
        assert_eq!(a, b);
        let ids: Vec<&str> = a.moments.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["p7", "i4000", "c2"]);
        assert_eq!(a.moments[0].then.as_deref(), Some("the command failed"));
    }

    #[test]
    fn kinds_serialize_and_parse_as_snake_case() {
        use clap::ValueEnum;
        assert_eq!(serde_json::to_string(&Kind::NotApproved).unwrap(), "\"not_approved\"");
        assert_eq!(Kind::from_str("not_approved", false), Ok(Kind::NotApproved));
        assert!(Kind::from_str("not-approved", false).is_err());
        let counts: BTreeMap<Kind, u64> = BTreeMap::from([(Kind::Unanswered, 2), (Kind::NotApproved, 1)]);
        assert_eq!(serde_json::to_string(&counts).unwrap(), r#"{"not_approved":1,"unanswered":2}"#);
        assert_eq!(
            Kind::value_variants().iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            ["approved", "declined", "stopped", "unanswered", "not_approved", "unknown", "interrupted", "corrected"]
        );
    }

    #[test]
    fn test_commands_match_like_the_dashboard() {
        let t = |c: &str| is_test(&hook(1, 0, "PostToolUse", bash(c)));
        for c in ["cargo test", "cd x && pytest -q", "FOO=1 npm run test", "python3 -m unittest", "{ just check; }"] {
            assert!(t(c), "{c}");
        }
        for c in ["cargo build", "echo pytest", "cat test.py"] {
            assert!(!t(c), "{c}");
        }
        assert!(chained("cargo test && echo done"));
        assert!(!chained("cd x && cargo test"));
    }

    #[test]
    fn derive_reads_hooks_outputs_and_the_transcript_from_the_store() {
        use witnessed_store::{NewEvent, NewSession, parse_rfc3339};
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        let base = parse_rfc3339("2026-10-21T10:00:00Z").unwrap();
        let cmd = vec!["claude".to_string()];
        let new = NewSession {
            id: "abc123",
            command: &cmd,
            cwd: Some("/w"),
            created_at_ns: base,
            supervisor_pid: 1,
            rows: 24,
            cols: 80,
            record_output: true,
            record_input: false,
            boot_time: None,
            restarted_from: None,
            agent_session_id: None,
            anchor_token_digest: None,
        };
        store.create_session(&new).unwrap();
        store.mark_started("abc123", 2, base).unwrap();
        store.mark_exited("abc123", base + 60_000_000_000, Some(0), None).unwrap();
        let s = store.session("abc123").unwrap().unwrap();
        assert_eq!(derive(&store, &s, &[]).unwrap().reason.as_deref(), Some(crate::adapters::NOT_AGENT));
        let transcript = dir.path().join("t.jsonl");
        let lines = [
            r#"{"type":"user","timestamp":"2026-10-21T10:00:01Z","origin":{"kind":"human"},"message":{"content":"go"}}"#,
            r#"{"type":"user","timestamp":"2026-10-21T10:00:05Z","message":{"content":"[Request interrupted by user for tool use]"}}"#,
        ];
        std::fs::write(&transcript, lines.join("\n")).unwrap();
        let at = |ms: i64, body: Body, data: Option<&[u8]>| NewEvent {
            ts_ns: base + ms * 1_000_000,
            mono_ns: ms * 1_000_000,
            body,
            data: data.map(<[u8]>::to_vec),
        };
        let h = |name: &str| {
            Body::AgentHook(Box::new(AgentHook {
                agent: "claude".into(),
                hook_event_name: name.into(),
                transcript_path: Some(transcript.display().to_string()),
                tool_name: Some("Bash".into()),
                tool: Some(witnessed_store::ToolSummary {
                    command: Some("rm -rf /w/dist".into()),
                    ..Default::default()
                }),
                ..AgentHook::default()
            }))
        };
        let events = [
            at(0, Body::TerminalOutput, Some(b"$ ")),
            at(2000, h("PermissionRequest"), None),
            at(4800, Body::TerminalOutput, Some(b"x")),
        ];
        store.append_events("abc123", 0, &events).unwrap();
        let d = derive(&store, &s, &[]).unwrap();
        assert!(d.available, "{:?}", d.reason);
        assert_eq!(d.moments.len(), 1);
        let m = &d.moments[0];
        assert_eq!((m.id.as_str(), m.kind, m.prompt, m.wait_ms), ("p1", Kind::Declined, Some(1), Some(3000)));
        assert_eq!(derive(&store, &s, &[(1000, 3000)]).unwrap().moments, vec![]);
    }
}
