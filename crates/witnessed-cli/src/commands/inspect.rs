use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use witnessed_store::{
    AgentHook, Body, ChangedPath, GIT_MAX_PATHS, GitPhase, Store, format_rfc3339, kind, now_unix_ns,
};
use witnessed_supervisor::Layout;

use crate::agent_state;
use crate::fmt;
use crate::procinfo::{self, ProcNode};
use crate::state;
use crate::state::{duration_ms, open_and_resolve, program};
use crate::transcript::{self, InteractionJson};

#[derive(clap::Args)]
pub struct InspectArgs {
    #[arg(default_value = "last", help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(long, help = "Print JSON instead of text")]
    pub json: bool,
    #[arg(long, help = "Show the text of each prompt read from the agent transcript (hidden by default)")]
    pub prompts: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Commands {
    pub count: u64,
    pub failed: u64,
    pub exit_unknown: u64,
}

/// Everything `inspect` reads from the event log, collected in one pass.
#[derive(Debug, Default)]
pub struct SessionFacts {
    pub notices: Vec<Value>,
    pub commands: Option<Commands>,
    pub marks: Vec<(i64, String)>,
    pub hooks: Vec<(i64, AgentHook)>,
    pub notifications: Vec<Value>,
    pub git: Vec<Value>,
    pub snapshots: Vec<Body>,
}

const FACT_KINDS: &[&str] = &[
    kind::RUNTIME_WARNING,
    kind::RUNTIME_ERROR,
    kind::SHELL_COMMAND_FINISHED,
    kind::USER_MARK,
    kind::AGENT_HOOK,
    kind::TERMINAL_NOTIFICATION,
    kind::GIT_SNAPSHOT,
];

impl SessionFacts {
    pub fn collect(store: &Store, session_id: &str) -> Result<Self> {
        let mut facts = Self::default();
        store.for_each_event(session_id, Some(FACT_KINDS), |e| {
            match e.kind.as_str() {
                kind::RUNTIME_WARNING | kind::RUNTIME_ERROR => facts
                    .notices
                    .push(json!({"type": e.kind, "timestamp": format_rfc3339(e.ts_ns), "payload": e.payload})),
                kind::SHELL_COMMAND_FINISHED => {
                    let c = facts.commands.get_or_insert_default();
                    c.count += 1;
                    match e.payload.get("exit_code").and_then(Value::as_i64) {
                        Some(0) => {}
                        Some(_) => c.failed += 1,
                        None => c.exit_unknown += 1,
                    }
                }
                kind::USER_MARK => {
                    if let Some(Body::UserMark { label }) = e.body() {
                        facts.marks.push((e.mono_ns, label));
                    }
                }
                kind::AGENT_HOOK => {
                    if let Some(Body::AgentHook(h)) = e.body() {
                        facts.hooks.push((e.ts_ns, *h));
                    }
                }
                kind::TERMINAL_NOTIFICATION => {
                    facts.notifications.push(json!({"timestamp": format_rfc3339(e.ts_ns), "payload": e.payload}));
                }
                kind::GIT_SNAPSHOT => {
                    facts.snapshots.extend(e.body());
                    facts.git.push(json!({"timestamp": format_rfc3339(e.ts_ns), "payload": e.payload}));
                }
                _ => {}
            }
            Ok(())
        })?;
        Ok(facts)
    }

    pub fn hooks(&self) -> impl Iterator<Item = &AgentHook> {
        self.hooks.iter().map(|(_, h)| h)
    }
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolCalls {
    pub count: u64,
    pub failed: u64,
    pub source: String,
    pub by_tool: Vec<ToolCount>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolCount {
    pub tool: String,
    pub count: u64,
    pub failed: u64,
}

pub fn tool_calls<'a>(hooks: impl IntoIterator<Item = &'a AgentHook>) -> Option<ToolCalls> {
    let mut by: BTreeMap<String, ToolCount> = BTreeMap::new();
    let mut out = ToolCalls::default();
    for h in hooks {
        let failed = match h.hook_event_name.as_str() {
            "PostToolUse" => false,
            "PostToolUseFailure" => true,
            _ => continue,
        };
        let tool = h.tool_name.clone().unwrap_or_else(|| "(unnamed)".into());
        let e = by.entry(tool.clone()).or_insert_with(|| ToolCount { tool, ..ToolCount::default() });
        e.count += 1;
        out.count += 1;
        if failed {
            e.failed += 1;
            out.failed += 1;
        }
        out.source = format!("{} hooks", h.agent);
    }
    if out.count == 0 {
        return None;
    }
    out.by_tool = by.into_values().collect();
    out.by_tool.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tool.cmp(&b.tool)));
    Some(out)
}

#[derive(Debug, Serialize)]
pub struct FilesChanged {
    pub count: u64,
    pub since_head: String,
    pub already_dirty_at_start: u64,
    pub capped: bool,
    pub paths: Vec<FileChange>,
}

#[derive(Debug, Serialize)]
pub struct FileChange {
    pub status: String,
    pub path: String,
    pub dirty_at_start: bool,
}

pub fn files_changed(snapshots: &[Body]) -> Result<FilesChanged, &'static str> {
    let find = |want: GitPhase| {
        snapshots.iter().find_map(|b| match b {
            Body::GitSnapshot { phase, head, dirty, changed_paths, changed, .. } if *phase == want => {
                Some((head, dirty, changed_paths, changed))
            }
            _ => None,
        })
    };
    let Some((start_head, start_dirty, _, _)) = find(GitPhase::Start) else {
        return Err("no git snapshot at start");
    };
    let Some((_, _, changed_paths, changed)) = find(GitPhase::Exit) else {
        return Err("no git snapshot at exit (session still running, or lost)");
    };
    let (Some(count), Some(head)) = (changed_paths, start_head) else {
        return Err("no HEAD at start to compare against");
    };
    let paths: Vec<FileChange> = changed
        .iter()
        .map(|ChangedPath { status, path }| FileChange {
            status: status.clone(),
            path: path.clone(),
            dirty_at_start: start_dirty.binary_search(path).is_ok(),
        })
        .collect();
    Ok(FilesChanged {
        count: *count,
        since_head: head.chars().take(12).collect(),
        already_dirty_at_start: paths.iter().filter(|p| p.dirty_at_start).count() as u64,
        capped: *count as usize > GIT_MAX_PATHS || start_dirty.len() >= GIT_MAX_PATHS,
        paths,
    })
}

pub fn session_window(s: &witnessed_store::Session) -> (i64, i64) {
    (s.started_at_ns.unwrap_or(s.created_at_ns), s.ended_at_ns.unwrap_or_else(now_unix_ns))
}

const RULE: &str = "----------------------------------------";

pub fn inspect(layout: &Layout, args: &InspectArgs) -> Result<i32> {
    let (as_json, prompts) = (args.json, args.prompts);
    let (store, s, state) = open_and_resolve(layout, &args.session)?;
    let st = state.label();
    let live = state.live();
    let counts = store.event_counts(&s.id)?;
    let count_of = |k: &str| counts.iter().find(|c| c.kind == k).map_or(0, |c| c.count);
    let bytes_of = |k: &str| counts.iter().find(|c| c.kind == k).map_or(0, |c| c.bytes);
    let facts = SessionFacts::collect(&store, &s.id)?;
    let SessionFacts { notices, commands, marks, notifications, git, snapshots, .. } = &facts;
    let tree = live.and_then(|l| procinfo::sample_tree(l.child_pid as u32));
    let running = live.is_some_and(|l| !l.exited);
    let agent = agent_state::from_hooks(facts.hooks.iter().map(|(t, h)| (*t, h)), running);
    let resume = agent_state::resume_check_in(facts.hooks(), &s, running, now_unix_ns());
    let tools = tool_calls(facts.hooks());
    let (win_from, win_to) = session_window(&s);
    let tokens: Result<(String, String, transcript::Usage), String> = match transcript::locate_in(facts.hooks()) {
        Err(u) => Err(u.reason()),
        Ok(located) => transcript::usage(std::path::Path::new(&located.path), win_from, win_to)
            .map(|u| (format!("{} transcript", located.agent), located.path.clone(), u))
            .map_err(|e| format!("{e:#}")),
    };
    let files = files_changed(snapshots);
    let mut unavailable = vec!["model_latency", "cost"];
    if commands.is_none() {
        unavailable.extend(["commands", "failures"]);
    }
    if tools.is_none() {
        unavailable.push("tool_calls");
    }
    if tokens.is_err() {
        unavailable.push("tokens");
    }
    if files.is_err() {
        unavailable.push("files_changed");
    }

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": s,
                "status": st,
                "duration_ms": duration_ms(&s),
                "host_restarted": state::host_restarted(&s),
                "supervisor": live,
                "process_tree": tree,
                "event_counts": counts,
                "notices": notices,
                "notifications": notifications,
                "git": git,
                "resume": resume,
                "agent_state": agent.as_ref().map(|a| json!({
                    "state": a.state,
                    "source": a.source,
                    "event": a.event,
                    "detail": a.detail,
                    "timestamp": format_rfc3339(a.at_ns),
                })),
                "markers": marks
                    .iter()
                    .map(|(t, label)| json!({"offset_ms": t / 1_000_000, "label": label}))
                    .collect::<Vec<_>>(),
                "commands": commands.map(|c| json!({
                    "count": c.count,
                    "failed": c.failed,
                    "exit_unknown": c.exit_unknown,
                    "source": "osc133",
                })),
                "tool_calls": tools,
                "tokens": tokens.as_ref().ok().map(|(source, path, u)| json!({
                    "source": source,
                    "transcript_path": path,
                    "api_calls": u.api_calls,
                    "input": u.input,
                    "output": u.output,
                    "cache_read": u.cache_read,
                    "cache_creation": u.cache_creation,
                    "total": u.total(),
                    "models": u.models,
                    "outside_session": u.outside_session,
                    "interactions": interactions_json(&u.interactions, s.created_at_ns, prompts),
                    "interrupts": u.interrupts.len(),
                    "refusals": u.refusals.len(),
                    "compactions": u.compactions.len(),
                })),
                "files_changed": files.as_ref().ok(),
                "unavailable": unavailable,
            }))?
        );
        return Ok(0);
    }

    let na = "unavailable";
    let mut missing: Vec<String> = Vec::new();
    let mut na_line = |label: &str, reason: &str| missing.push(format!("{label:<17}unavailable ({reason})"));
    println!("EXECUTION PROFILE");
    println!("{RULE}");
    println!("Session:         {}", s.id);
    println!(
        "Command:         {}",
        crate::adapters::display(&crate::adapters::without_injected(&s.command, s.agent_session_id.as_deref()))
    );
    println!("Program:         {}", program(&s));
    println!("Working dir:     {}", s.cwd.as_deref().unwrap_or(na));
    println!("Status:          {st}");
    println!("Created:         {}", witnessed_store::format_rfc3339_seconds(s.created_at_ns));
    println!("Duration:        {}", duration_ms(&s).map_or_else(|| na.into(), fmt::duration_ms));
    match (s.exit_code, s.exit_signal) {
        (Some(c), _) => println!("Exit:            code {c}"),
        (None, Some(sig)) => println!("Exit:            signal {sig}"),
        _ => {}
    }
    if let Some(r) = &s.lost_reason {
        println!("Lost:            {}", r.as_str().replace('_', " "));
    }
    if let Some(r) = &s.restarted_from {
        println!("Restarted from:  {r}");
    }
    if let Some(a) = &s.agent_session_id {
        println!("Agent session:   {a}");
    }
    match &agent {
        Some(a) => {
            let ago = fmt::duration_ms((now_unix_ns() - a.at_ns) / 1_000_000);
            let detail = a.detail.as_ref().map(|d| format!(" {d}")).unwrap_or_default();
            println!("Agent state:     {} ({}: {}{detail}, {ago} ago)", a.state.as_str(), a.source, a.event);
        }
        None => na_line("Agent state:", crate::adapters::NOT_AGENT),
    }
    if let Some(r) = &resume {
        println!("Resume:          {} ({})", r.verdict.as_str(), r.reason);
    }
    println!();
    println!("PROCESS");
    println!("Supervisor PID:  {}", s.supervisor_pid.map_or(na.into(), |p| p.to_string()));
    println!("PID:             {}", s.child_pid.map_or(na.into(), |p| p.to_string()));
    if let Some(t) = &tree {
        println!("CPU:             {:.1}% (process tree, 250 ms sample)", t.total_cpu());
        println!("Memory:          {} RSS (process tree)", fmt::bytes(t.total_rss()));
        println!("Child processes: {}", t.descendants());
        print_tree(t, 0);
    } else {
        let why = if live.is_some() { "no sample" } else { "process not running" };
        for label in ["CPU:", "Memory:", "Child processes:"] {
            na_line(label, why);
        }
    }
    println!();
    println!("TERMINAL");
    let (rows, cols) = live.map_or((s.rows, s.cols), |v| (i64::from(v.rows), i64::from(v.cols)));
    println!("Size:            {cols}x{rows}");
    println!("Attached:        {}", if st == "running" { "yes" } else { "no" });
    if let Some(v) = live {
        println!("Output:          {}", fmt::bytes(v.output_bytes));
        println!("Input:           {}", fmt::bytes(v.input_bytes));
        println!("Alternate screen: {}", if v.alternate_screen { "yes" } else { "no" });
        match v.output_idle_ms {
            Some(ms) => println!("Last output:     {} ago", fmt::duration_ms(ms as i64)),
            None => println!("Last output:     none yet"),
        }
        if let Some(t) = &v.title {
            println!("Title:           {t}");
        }
    }
    println!(
        "Notifications:   {} recorded (OSC 9/99/777 and BEL; repeats within 1 s are counted, not recorded)",
        notifications.len()
    );
    for n in notifications.iter().rev().take(3) {
        let p = &n["payload"];
        println!(
            "  {} {} {}",
            n["timestamp"].as_str().unwrap_or(""),
            p["source"].as_str().unwrap_or(""),
            p["text"].as_str().unwrap_or("")
        );
    }
    match git_line(git) {
        Some(line) => println!("Git:             {line}"),
        None => na_line("Git:", "not a git work tree, or git not installed"),
    }
    match &files {
        Ok(f) => {
            let mut line = format!("Files changed:   {} since HEAD {} at start", f.count, f.since_head);
            if f.already_dirty_at_start > 0 {
                let _ = write!(line, ", {} already modified when the session started", f.already_dirty_at_start);
            }
            if f.capped {
                let _ = write!(line, " (first {GIT_MAX_PATHS} paths recorded)");
            }
            println!("{line}");
            for p in f.paths.iter().take(10) {
                println!("  {} {}{}", p.status, p.path, if p.dirty_at_start { " (modified at start)" } else { "" });
            }
            if f.paths.len() > 10 {
                println!("  ... {} more in --json", f.paths.len() - 10);
            }
        }
        Err(why) => na_line("Files changed:", why),
    }
    println!();
    println!("RECORDING");
    println!("Output recorded: {}", if s.record_output { "yes" } else { "no (--no-record)" });
    println!("Input recorded:  {}", if s.record_input { "yes (--record-input)" } else { "no" });
    println!(
        "Output events:   {} ({})",
        count_of(kind::TERMINAL_OUTPUT),
        fmt::bytes(bytes_of(kind::TERMINAL_OUTPUT) as u64)
    );
    println!("Markers:         {}", marks.len());
    for (i, (t, label)) in marks.iter().enumerate().take(10) {
        println!("  {} at {} {label}", i + 1, fmt::offset_ns(*t));
    }
    println!("Attaches:        {}", count_of(kind::SESSION_ATTACHED));
    println!("Resizes:         {}", count_of(kind::TERMINAL_RESIZED));
    if let Some(v) = live {
        println!("Dropped events:  {}", v.dropped_events);
    }
    println!();
    println!("EXECUTION");
    if let Some(Commands { count, failed, exit_unknown }) = *commands {
        println!("Commands:        {count} (OSC 133 shell integration marks)");
        let unknown = if exit_unknown > 0 { format!(", {exit_unknown} without an exit code") } else { String::new() };
        println!("Failures:        {failed} (non-zero exit code{unknown})");
    } else {
        na_line("Commands:", "no OSC 133 command marks; the shell has no integration enabled");
        na_line("Failures:", "no OSC 133 command marks");
    }
    match &tools {
        Some(t) => {
            let top: Vec<String> = t.by_tool.iter().take(8).map(|c| format!("{} {}", c.tool, c.count)).collect();
            let more = if t.by_tool.len() > 8 { ", ..." } else { "" };
            println!("Tool calls:      {} ({} failed; {}: {}{more})", t.count, t.failed, t.source, top.join(", "));
        }
        None => na_line("Tool calls:", "no PostToolUse hook events"),
    }
    match &tokens {
        Ok((source, _, u)) => {
            let models = if u.models.is_empty() { String::new() } else { format!(", {}", u.models.join(" ")) };
            let skipped = match u.outside_session {
                0 => String::new(),
                1 => "; 1 line outside the session skipped".to_string(),
                n => format!("; {n} lines outside the session skipped"),
            };
            println!(
                "Tokens:          input {}, output {}, cache read {}, cache write {} ({} API calls{models}; {source}{skipped})",
                u.input, u.output, u.cache_read, u.cache_creation, u.api_calls
            );
        }
        Err(why) => na_line("Tokens:", why),
    }
    match &tokens {
        Ok((source, _, u)) if !u.interactions.is_empty() => {
            print_interactions(&u.interactions, s.created_at_ns, prompts, source);
        }
        Ok((source, _, _)) => println!("Interactions:    0 (no prompts in the {source} during the session)"),
        Err(_) => na_line("Interactions:", "no transcript"),
    }
    na_line("Cost:", "no price table");
    println!("Warnings:        {}", count_of(kind::RUNTIME_WARNING));
    println!("Errors:          {}", count_of(kind::RUNTIME_ERROR));
    for n in notices.iter().rev().take(5) {
        let code = n["payload"]["code"].as_str().unwrap_or("");
        let msg = n["payload"]["message"].as_str().unwrap_or("");
        println!("  {} {} {code} {msg}", n["timestamp"].as_str().unwrap_or(""), n["type"].as_str().unwrap_or(""));
    }
    if !missing.is_empty() {
        println!();
        println!("NOT MEASURED");
        for line in &missing {
            println!("{line}");
        }
    }
    println!("{RULE}");
    Ok(0)
}

const INTERACTIONS_SHOWN: usize = 50;

#[derive(Serialize)]
struct InspectInteraction {
    timestamp: Option<String>,
    offset_ms: Option<i64>,
    #[serde(flatten)]
    base: InteractionJson,
}

fn interactions_json(list: &[transcript::Interaction], from_ns: i64, prompts: bool) -> Vec<InspectInteraction> {
    list.iter()
        .enumerate()
        .map(|(i, t)| InspectInteraction {
            timestamp: t.at_ns.map(format_rfc3339),
            offset_ms: t.at_ns.map(|at| (at - from_ns).max(0) / 1_000_000),
            base: InteractionJson::new(i + 1, t, prompts),
        })
        .collect()
}

fn print_interactions(list: &[transcript::Interaction], from_ns: i64, prompts: bool, source: &str) {
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for t in list {
        *by.entry(t.origin.as_str()).or_default() += 1;
    }
    let kinds: Vec<String> = by.iter().map(|(k, n)| format!("{n} {k}")).collect();
    let hint = if prompts { "" } else { "; prompt text hidden, --prompts shows it" };
    println!("Interactions:    {} ({}; tokens per prompt from the {source}{hint})", list.len(), kinds.join(", "));
    for (i, t) in list.iter().enumerate().take(INTERACTIONS_SHOWN) {
        let at = t.at_ns.map_or("-".into(), |at| fmt::offset_ns((at - from_ns).max(0)));
        let images = match t.images {
            0 => String::new(),
            1 => ", 1 image".into(),
            n => format!(", {n} images"),
        };
        let mut line = format!(
            "  {:>3} {at} {:<17} {} tokens, {} API calls, output {}, {} chars{images}",
            i + 1,
            t.origin,
            t.total(),
            t.api_calls,
            t.output,
            t.chars
        );
        if prompts && !t.text.is_empty() {
            let _ = write!(line, "  \"{}\"", transcript::one_line(&t.text, 100));
        }
        println!("{line}");
    }
    if list.len() > INTERACTIONS_SHOWN {
        println!("  ... {} more in --json", list.len() - INTERACTIONS_SHOWN);
    }
}

fn print_tree(node: &ProcNode, depth: usize) {
    if depth > 0 || !node.children.is_empty() {
        println!(
            "  {}{} {} cpu {:.1}% rss {}",
            "  ".repeat(depth),
            node.pid,
            node.name,
            node.cpu_percent,
            fmt::bytes(node.rss_bytes)
        );
    }
    for c in &node.children {
        print_tree(c, depth + 1);
    }
}

fn git_line(git: &[Value]) -> Option<String> {
    let short = |v: &Value| v["payload"]["head"].as_str().map(|h| h.chars().take(12).collect::<String>());
    let phase = |p: &str| git.iter().find(|g| g["payload"]["phase"] == p);
    match (phase("start"), phase("exit")) {
        (None, None) => None,
        (start, exit) => {
            let head = |v: Option<&Value>| v.and_then(short).unwrap_or_else(|| "-".into());
            let dirty = exit.or(start).and_then(|v| v["payload"]["dirty_paths"].as_u64());
            let mut line =
                format!("HEAD {} -> {}", head(start), if exit.is_some() { head(exit) } else { "(running)".into() });
            if let Some(d) = dirty {
                let _ = write!(line, ", {d} paths modified or untracked");
            }
            Some(line)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(name: &str, tool: Option<&str>) -> AgentHook {
        AgentHook {
            agent: "claude".into(),
            hook_event_name: name.into(),
            tool_name: tool.map(str::to_string),
            ..AgentHook::default()
        }
    }

    #[test]
    fn tool_calls_count_post_tool_use_per_tool_with_failures() {
        let hooks = [
            hook("SessionStart", None),
            hook("PostToolUse", Some("Bash")),
            hook("PostToolUse", Some("Edit")),
            hook("PostToolUseFailure", Some("Bash")),
            hook("PostToolUse", Some("Bash")),
            hook("PostToolUse", None),
            hook("Stop", None),
        ];
        let t = tool_calls(&hooks).unwrap();
        assert_eq!((t.count, t.failed, t.source.as_str()), (5, 1, "claude hooks"));
        let by: Vec<(String, u64, u64)> = t.by_tool.into_iter().map(|c| (c.tool, c.count, c.failed)).collect();
        assert_eq!(by, [("Bash", 3, 1), ("(unnamed)", 1, 0), ("Edit", 1, 0)].map(|(t, c, f)| (t.to_string(), c, f)));
        assert!(tool_calls(&[hook("SessionStart", None)]).is_none());
    }

    fn snapshot(phase: GitPhase, head: Option<&str>, dirty: &[&str], changed: Option<&[(&str, &str)]>) -> Body {
        Body::GitSnapshot {
            phase,
            head: head.map(str::to_string),
            dirty_paths: Some(dirty.len() as u64),
            dirty: dirty.iter().map(|p| p.to_string()).collect(),
            changed_paths: changed.map(|c| c.len() as u64),
            changed: changed
                .unwrap_or_default()
                .iter()
                .map(|(s, p)| ChangedPath { status: s.to_string(), path: p.to_string() })
                .collect(),
            new_commits: None,
            commits: Vec::new(),
            diff_sha256: None,
            diff_bytes: None,
            root: None,
        }
    }

    #[test]
    fn files_changed_marks_paths_that_were_dirty_at_start() {
        let start = snapshot(GitPhase::Start, Some("abcdef0123456789"), &["pre.txt"], None);
        let exit = snapshot(GitPhase::Exit, Some("abcdef0123456789"), &[], Some(&[("M", "a.rs"), ("?", "pre.txt")]));
        let f = files_changed(&[start.clone(), exit]).unwrap();
        assert_eq!((f.count, f.since_head.as_str(), f.already_dirty_at_start, f.capped), (2, "abcdef012345", 1, false));
        assert_eq!(f.paths.iter().map(|p| p.dirty_at_start).collect::<Vec<_>>(), [false, true]);
        assert_eq!(files_changed(&[start]).unwrap_err(), "no git snapshot at exit (session still running, or lost)");
        assert_eq!(files_changed(&[]).unwrap_err(), "no git snapshot at start");
        let no_head = snapshot(GitPhase::Start, None, &[], None);
        let exit = snapshot(GitPhase::Exit, Some("a"), &[], None);
        assert_eq!(files_changed(&[no_head, exit]).unwrap_err(), "no HEAD at start to compare against");
    }
}
