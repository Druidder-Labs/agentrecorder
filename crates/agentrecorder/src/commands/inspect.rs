use std::collections::BTreeMap;
use std::path::Path;

use agentrecorder_store::{AgentHook, Body, ChangedPath, GIT_MAX_PATHS, GitPhase, format_rfc3339, kind, now_unix_ns};
use agentrecorder_supervisor::Layout;
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::agent_state;
use crate::fmt;
use crate::procinfo::{self, ProcNode};
use crate::state;
use crate::state::{duration_ms, open_and_resolve, program};
use crate::transcript;

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

fn session_window(s: &agentrecorder_store::Session) -> (i64, i64) {
    (s.started_at_ns.unwrap_or(s.created_at_ns), s.ended_at_ns.unwrap_or_else(now_unix_ns))
}

const RULE: &str = "----------------------------------------";

pub fn inspect(layout: &Layout, id: &str, as_json: bool) -> Result<i32> {
    let (store, s, state) = open_and_resolve(layout, id)?;
    let st = state.label();
    let live = state.live();
    let counts = store.event_counts(&s.id)?;
    let count_of = |k: &str| counts.iter().find(|c| c.kind == k).map_or(0, |c| c.count);
    let bytes_of = |k: &str| counts.iter().find(|c| c.kind == k).map_or(0, |c| c.bytes);
    let mut notices: Vec<Value> = Vec::new();
    store.for_each_event(&s.id, Some(&[kind::RUNTIME_WARNING, kind::RUNTIME_ERROR]), |e| {
        notices.push(json!({"type": e.kind, "timestamp": format_rfc3339(e.ts_ns), "payload": e.payload}));
        Ok(())
    })?;
    let tree = live.and_then(|l| procinfo::sample_tree(l.child_pid as u32));
    let mut commands: Option<(u64, u64, u64)> = None;
    store.for_each_event(&s.id, Some(&[kind::SHELL_COMMAND_FINISHED]), |e| {
        let c = commands.get_or_insert((0, 0, 0));
        c.0 += 1;
        match e.payload.get("exit_code").and_then(Value::as_i64) {
            Some(0) => {}
            Some(_) => c.1 += 1,
            None => c.2 += 1,
        }
        Ok(())
    })?;
    let marks = super::replay::marks(&store, &s.id)?;
    let running = live.is_some_and(|l| !l.exited);
    let agent = agent_state::from_store(&store, &s.id, running)?;
    let resume = agent_state::resume_check(&store, &s, running, now_unix_ns())?;
    let mut hooks: Vec<AgentHook> = Vec::new();
    store.for_each_event(&s.id, Some(&[kind::AGENT_HOOK]), |e| {
        if let Some(Body::AgentHook(h)) = e.body() {
            hooks.push(*h);
        }
        Ok(())
    })?;
    let tools = tool_calls(&hooks);
    let transcript = hooks.iter().rev().find_map(|h| h.transcript_path.clone().map(|p| (h.agent.clone(), p)));
    let (win_from, win_to) = session_window(&s);
    let tokens: Result<(String, String, transcript::Usage), String> = match &transcript {
        None if hooks.is_empty() => Err("no agent hook events".into()),
        None => Err("the agent hooks reported no transcript path".into()),
        Some((agent, path)) => transcript::usage(Path::new(path), win_from, win_to)
            .map(|u| (format!("{agent} transcript"), path.clone(), u))
            .map_err(|e| format!("{e:#}")),
    };
    let mut notifications: Vec<Value> = Vec::new();
    let mut git: Vec<Value> = Vec::new();
    let mut snapshots: Vec<Body> = Vec::new();
    store.for_each_event(&s.id, Some(&[kind::TERMINAL_NOTIFICATION, kind::GIT_SNAPSHOT]), |e| {
        let entry = json!({"timestamp": format_rfc3339(e.ts_ns), "payload": e.payload});
        if e.kind == kind::GIT_SNAPSHOT {
            snapshots.extend(e.body());
            git.push(entry)
        } else {
            notifications.push(entry)
        }
        Ok(())
    })?;
    let files = files_changed(&snapshots);
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
                "commands": commands.map(|(count, failed, unknown)| json!({
                    "count": count,
                    "failed": failed,
                    "exit_unknown": unknown,
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
                })),
                "files_changed": files.as_ref().ok(),
                "unavailable": unavailable,
            }))?
        );
        return Ok(0);
    }

    let na = "unavailable";
    println!("EXECUTION PROFILE");
    println!("{RULE}");
    println!("Session:         {}", s.id);
    println!("Command:         {}", crate::adapters::display(&s.command));
    println!("Program:         {}", program(&s));
    println!("Working dir:     {}", s.cwd.as_deref().unwrap_or(na));
    println!("Status:          {st}");
    println!("Created:         {}", format_rfc3339(s.created_at_ns));
    println!("Duration:        {}", duration_ms(&s).map(fmt::duration_ms).unwrap_or_else(|| na.into()));
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
        None => println!("Agent state:     {na} (no agent hook events)"),
    }
    if let Some(r) = &resume {
        println!("Resume:          {} ({})", r.verdict.as_str(), r.reason);
    }
    println!();
    println!("PROCESS");
    println!("Supervisor PID:  {}", s.supervisor_pid.map_or(na.into(), |p| p.to_string()));
    println!("PID:             {}", s.child_pid.map_or(na.into(), |p| p.to_string()));
    match &tree {
        Some(t) => {
            println!("CPU:             {:.1}% (process tree, 250 ms sample)", t.total_cpu());
            println!("Memory:          {} RSS (process tree)", fmt::bytes(t.total_rss()));
            println!("Child processes: {}", t.descendants());
            print_tree(t, 0);
        }
        None => {
            let why = if live.is_some() { na } else { "unavailable (process not running)" };
            println!("CPU:             {why}");
            println!("Memory:          {why}");
            println!("Child processes: {why}");
        }
    }
    println!();
    println!("TERMINAL");
    let (rows, cols) = live.map_or((s.rows, s.cols), |v| (v.rows as i64, v.cols as i64));
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
    println!("Git:             {}", git_line(&git));
    match &files {
        Ok(f) => {
            let mut line = format!("Files changed:   {} since HEAD {} at start", f.count, f.since_head);
            if f.already_dirty_at_start > 0 {
                line.push_str(&format!(", {} already modified when the session started", f.already_dirty_at_start));
            }
            if f.capped {
                line.push_str(&format!(" (first {GIT_MAX_PATHS} paths recorded)"));
            }
            println!("{line}");
            for p in f.paths.iter().take(10) {
                println!("  {} {}{}", p.status, p.path, if p.dirty_at_start { " (modified at start)" } else { "" });
            }
            if f.paths.len() > 10 {
                println!("  ... {} more in --json", f.paths.len() - 10);
            }
        }
        Err(why) => println!("Files changed:   {na} ({why})"),
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
        println!("  {} at {} {label}", i + 1, fmt::offset_ms(t / 1_000_000));
    }
    println!("Attaches:        {}", count_of(kind::SESSION_ATTACHED));
    println!("Resizes:         {}", count_of(kind::TERMINAL_RESIZED));
    if let Some(v) = live {
        println!("Dropped events:  {}", v.dropped_events);
    }
    println!();
    println!("EXECUTION");
    match commands {
        Some((count, failed, unknown)) => {
            println!("Commands:        {count} (OSC 133 shell integration marks)");
            let unknown = if unknown > 0 { format!(", {unknown} without an exit code") } else { String::new() };
            println!("Failures:        {failed} (non-zero exit code{unknown})");
        }
        None => {
            println!("Commands:        {na} (no OSC 133 command marks; the shell has no integration enabled)");
            println!("Failures:        {na} (no OSC 133 command marks)");
        }
    }
    match &tools {
        Some(t) => {
            let top: Vec<String> = t.by_tool.iter().take(8).map(|c| format!("{} {}", c.tool, c.count)).collect();
            let more = if t.by_tool.len() > 8 { ", ..." } else { "" };
            println!("Tool calls:      {} ({} failed; {}: {}{more})", t.count, t.failed, t.source, top.join(", "));
        }
        None => println!("Tool calls:      {na} (no PostToolUse hook events)"),
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
        Err(why) => println!("Tokens:          {na} ({why})"),
    }
    println!("Cost:            {na} (no price table)");
    println!("Warnings:        {}", count_of(kind::RUNTIME_WARNING));
    println!("Errors:          {}", count_of(kind::RUNTIME_ERROR));
    for n in notices.iter().rev().take(5) {
        let code = n["payload"]["code"].as_str().unwrap_or("");
        let msg = n["payload"]["message"].as_str().unwrap_or("");
        println!("  {} {} {code} {msg}", n["timestamp"].as_str().unwrap_or(""), n["type"].as_str().unwrap_or(""));
    }
    println!("{RULE}");
    Ok(0)
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

fn git_line(git: &[Value]) -> String {
    let short = |v: &Value| v["payload"]["head"].as_str().map(|h| h.chars().take(12).collect::<String>());
    let phase = |p: &str| git.iter().find(|g| g["payload"]["phase"] == p);
    match (phase("start"), phase("exit")) {
        (None, None) => "unavailable (not a git work tree, or git not installed)".into(),
        (start, exit) => {
            let head = |v: Option<&Value>| v.and_then(short).unwrap_or_else(|| "-".into());
            let dirty = exit.or(start).and_then(|v| v["payload"]["dirty_paths"].as_u64());
            let mut line =
                format!("HEAD {} -> {}", head(start), if exit.is_some() { head(exit) } else { "(running)".into() });
            if let Some(d) = dirty {
                line.push_str(&format!(", {d} paths modified or untracked"));
            }
            line
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
