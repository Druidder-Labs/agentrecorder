use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use nix::sys::signal::Signal;
use serde_json::json;
use witnessed_store::{Deletion, Status, Store, now_unix_ns};
use witnessed_supervisor::Layout;
use witnessed_supervisor::client;

use super::open_store;
use crate::attach::{self, DetachKey};
use crate::fmt;
use crate::state::{self, Need, State, duration_ms, open_and_resolve, open_needing};

#[derive(clap::Args)]
pub struct SessionsArgs {
    #[arg(long, help = "Print JSON instead of a table")]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct AttachArgs {
    #[arg(default_value = "last", help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(
        long,
        env = "WITNESSED_DETACH_KEY",
        default_value = crate::DEFAULT_DETACH_KEY,
        help = "Key that detaches: ctrl-<key> or none"
    )]
    pub detach_key: String,
}

#[derive(clap::Args)]
pub struct DetachArgs {
    #[arg(help = crate::SESSION_HELP)]
    pub session: String,
}

#[derive(clap::Args)]
pub struct MarkArgs {
    #[arg(help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(default_value = "", help = "Label shown by inspect, replay and asciicast export")]
    pub label: String,
}

#[derive(clap::Args)]
pub struct StopArgs {
    #[arg(help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(long, default_value = "HUP", help = "First signal: HUP, TERM, INT or KILL")]
    pub signal: String,
    #[arg(long, default_value_t = 5.0, help = "Seconds before escalating to SIGKILL")]
    pub timeout: f64,
}

#[derive(clap::Args)]
pub struct RmArgs {
    #[arg(required = true, help = "Session ids or unique prefixes")]
    pub sessions: Vec<String>,
}

#[derive(clap::Args)]
pub struct PruneArgs {
    #[arg(long, default_value = "30d", help = "Age such as 12h, 7d or 90m")]
    pub older_than: String,
}

pub fn list(layout: &Layout, args: &SessionsArgs) -> Result<i32> {
    let as_json = args.json;
    if !layout.db_path().exists() {
        println!("{}", if as_json { "[]" } else { "no sessions" });
        return Ok(0);
    }
    let store = open_store(layout)?;
    let mut rows = Vec::new();
    for mut s in store.sessions()? {
        let st = state::resolve(layout, &store, &mut s);
        rows.push((st.label(), s));
    }
    if as_json {
        let out: Vec<_> = rows
            .iter()
            .map(|(st, s)| {
                json!({
                    "session": s.id,
                    "command": s.command,
                    "status": st,
                    "duration_ms": duration_ms(s),
                    "exit_code": s.exit_code,
                    "exit_signal": s.exit_signal,
                    "created_at": witnessed_store::format_rfc3339(s.created_at_ns),
                    "lost_reason": s.lost_reason,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    if rows.is_empty() {
        println!("no sessions");
        return Ok(0);
    }
    let now = witnessed_store::now_unix_ns();
    println!(
        "{:<8}  {:<9}  {:<11}  {:>9}  {:<9}  {:<24}  COMMAND",
        "SESSION", "STARTED", "STATUS", "DURATION", "EXIT", "DIRECTORY"
    );
    for (st, s) in &rows {
        let exit = match (s.exit_code, s.exit_signal) {
            (Some(c), _) => c.to_string(),
            (None, Some(sig)) => format!("signal {sig}"),
            _ => "-".to_string(),
        };
        let dur = duration_ms(s).map_or_else(|| "-".into(), fmt::duration_ms);
        let ago = fmt::ago((now - s.created_at_ns) / 1_000_000);
        let dir = s.cwd.as_deref().map_or_else(|| "-".into(), fmt::abbreviate_home);
        let cmd = crate::adapters::display_quoted(&crate::adapters::without_injected(
            &s.command,
            s.agent_session_id.as_deref(),
        ));
        println!(
            "{:<8}  {:<9}  {:<11}  {:>9}  {:<9}  {:<24}  {}",
            s.id,
            ago,
            st,
            dur,
            exit,
            fmt::truncate_front(&dir, 24),
            fmt::truncate_back(&cmd, 60)
        );
    }
    Ok(0)
}

pub fn attach(layout: &Layout, args: &AttachArgs) -> Result<i32> {
    let key = DetachKey::parse(&args.detach_key)?;
    let (_, s, st) = open_and_resolve(layout, &args.session)?;
    if st.live().is_none() {
        bail!("session {} is {}; nothing to attach to. See `witnessed recover {}`", s.id, st.label(), s.id);
    }
    eprintln!("[witnessed] attached to session {}; detach with {}", s.id, key.label);
    let outcome = attach::attach(layout, &s.id, &key)?;
    Ok(attach::report_outcome(&s.id, &outcome))
}

pub fn mark(layout: &Layout, args: &MarkArgs) -> Result<i32> {
    let (_, s, st) = open_and_resolve(layout, &args.session)?;
    if st.live().is_none_or(|l| l.exited) {
        bail!("session {} is {}; markers can only be added while it runs", s.id, st.label());
    }
    let m = client::mark(layout, &s.id, &args.label)?;
    let shown = if m.label.is_empty() { String::new() } else { format!(": {}", m.label) };
    println!("marked session {} at {}{shown}", s.id, crate::fmt::offset_ms(m.uptime_ms as i64));
    Ok(0)
}

pub fn detach(layout: &Layout, args: &DetachArgs) -> Result<i32> {
    let (_, s, _) = open_needing(layout, &args.session, Need::Live)?;
    if client::detach(layout, &s.id)? {
        println!("detached the client from session {}", s.id);
    } else {
        println!("session {} had no attached client", s.id);
    }
    Ok(0)
}

fn parse_signal(s: &str) -> Result<Signal> {
    Ok(match s.trim_start_matches("SIG").to_ascii_uppercase().as_str() {
        "HUP" => Signal::SIGHUP,
        "TERM" => Signal::SIGTERM,
        "INT" => Signal::SIGINT,
        "KILL" => Signal::SIGKILL,
        other => bail!("unsupported signal {other:?}; use HUP, TERM, INT or KILL"),
    })
}

pub fn stop(layout: &Layout, args: &StopArgs) -> Result<i32> {
    let sig = parse_signal(&args.signal)?;
    if !(0.0..=3600.0).contains(&args.timeout) {
        bail!("--timeout must be between 0 and 3600 seconds");
    }
    let (store, s, st) = open_and_resolve(layout, &args.session)?;
    let grace = Duration::from_secs_f64(args.timeout);
    match &st {
        State::Running(_) => client::stop(layout, &s.id, sig as i32, grace)?,
        State::Unreachable(why) => match state::supervisor_alive(&s) {
            Some(pid) => {
                eprintln!(
                    "witnessed: session {} is unreachable ({why}); sending SIGTERM to supervisor pid {pid}",
                    s.id
                );
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGTERM)?;
            }
            None => bail!("session {} is unreachable: {why}", s.id),
        },
        State::Starting => bail!("session {} is still starting; try again in a moment", s.id),
        State::Exited | State::Lost => {
            println!("session {} is already {}", s.id, st.label());
            return Ok(0);
        }
    }
    let deadline = Instant::now() + grace + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(cur) = store.session(&s.id)?
            && cur.status == Status::Exited
        {
            match (cur.exit_code, cur.exit_signal) {
                (Some(c), _) => println!("session {} stopped; exit code {c}", s.id),
                (None, Some(sg)) => println!("session {} stopped; killed by signal {sg}", s.id),
                _ => println!("session {} stopped", s.id),
            }
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("session {} did not stop within {:.1}s", s.id, grace.as_secs_f64() + 5.0)
}

pub fn remove(layout: &Layout, args: &RmArgs) -> Result<i32> {
    let mut store = open_store(layout)?;
    for id in &args.sessions {
        let mut s = store.resolve(id)?;
        let st = state::resolve(layout, &store, &mut s);
        if !st.is_finished() {
            bail!("session {} is {}; stop it first with `witnessed stop {}`", s.id, st.label(), s.id);
        }
        delete(layout, &mut store, &s.id)?;
        println!("removed session {}", s.id);
    }
    finish_deletion(&mut store)?;
    Ok(0)
}

fn delete(layout: &Layout, store: &mut Store, id: &str) -> Result<()> {
    store.delete_session(id)?;
    let _ = std::fs::remove_file(layout.log_path(id));
    let _ = std::fs::remove_dir_all(layout.view_dir(id));
    Ok(())
}

fn finish_deletion(store: &mut Store) -> Result<()> {
    store.delete_orphan_events()?;
    if store.checkpoint()? == Deletion::WalPending {
        eprintln!(
            "witnessed: warning: another witnessed process is reading the store, so deleted data stays in \
             state.db-wal until that process finishes; run `witnessed prune` again afterwards to scrub it"
        );
    }
    Ok(())
}

pub fn prune_before(layout: &Layout, store: &mut Store, cutoff_ns: i64, verbose: bool) -> Result<usize> {
    let mut n = 0;
    for mut s in store.sessions()? {
        if s.ended_at_ns.unwrap_or(s.created_at_ns) >= cutoff_ns {
            continue;
        }
        let st = state::resolve(layout, store, &mut s);
        let last = s.ended_at_ns.unwrap_or(s.created_at_ns);
        if st.is_finished() && last < cutoff_ns {
            delete(layout, store, &s.id)?;
            if verbose {
                println!("removed session {}", s.id);
            }
            n += 1;
        }
    }
    if n > 0 || verbose {
        finish_deletion(store)?;
    }
    Ok(n)
}

pub fn prune(layout: &Layout, args: &PruneArgs) -> Result<i32> {
    let older_than = &args.older_than;
    let age = fmt::parse_age(older_than)?;
    if !layout.db_path().exists() {
        println!("no sessions");
        return Ok(0);
    }
    let mut store = open_store(layout)?;
    let n = prune_before(layout, &mut store, now_unix_ns() - age, true)?;
    println!("pruned {n} session{} older than {older_than}", if n == 1 { "" } else { "s" });
    Ok(0)
}
