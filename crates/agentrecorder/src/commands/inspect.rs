use agentrecorder_store::{format_rfc3339, kind, now_unix_ns};
use agentrecorder_supervisor::Layout;
use anyhow::Result;
use serde_json::{Value, json};

use crate::agent_state;
use crate::fmt;
use crate::procinfo::{self, ProcNode};
use crate::state;
use crate::state::{duration_ms, open_and_resolve, program};

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
    let running = live.is_some_and(|l| !l.exited);
    let agent = agent_state::from_store(&store, &s.id, running)?;
    let resume = agent_state::resume_check(&store, &s, running, now_unix_ns())?;
    let mut notifications: Vec<Value> = Vec::new();
    let mut git: Vec<Value> = Vec::new();
    store.for_each_event(&s.id, Some(&[kind::TERMINAL_NOTIFICATION, kind::GIT_SNAPSHOT]), |e| {
        let entry = json!({"timestamp": format_rfc3339(e.ts_ns), "payload": e.payload});
        if e.kind == kind::GIT_SNAPSHOT {
            git.push(entry)
        } else {
            notifications.push(entry)
        }
        Ok(())
    })?;

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
                "unavailable": ["commands", "failures", "tokens", "model_latency", "tool_calls", "cost"],
            }))?
        );
        return Ok(0);
    }

    let na = "unavailable";
    println!("EXECUTION PROFILE");
    println!("{RULE}");
    println!("Session:         {}", s.id);
    println!("Command:         {}", s.command.join(" "));
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
    println!();
    println!("RECORDING");
    println!("Output recorded: {}", if s.record_output { "yes" } else { "no (--no-record)" });
    println!("Input recorded:  {}", if s.record_input { "yes (--record-input)" } else { "no" });
    println!(
        "Output events:   {} ({})",
        count_of(kind::TERMINAL_OUTPUT),
        fmt::bytes(bytes_of(kind::TERMINAL_OUTPUT) as u64)
    );
    println!("Attaches:        {}", count_of(kind::SESSION_ATTACHED));
    println!("Resizes:         {}", count_of(kind::TERMINAL_RESIZED));
    if let Some(v) = live {
        println!("Dropped events:  {}", v.dropped_events);
    }
    println!();
    println!("EXECUTION");
    println!("Commands:        {na} (no shell or agent integration)");
    println!("Failures:        {na} (no shell or agent integration)");
    println!("Tokens / cost:   {na} (not collected)");
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
