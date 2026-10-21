use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use agentrecorder_store::{Body, LostReason, Session, Store, format_rfc3339, kind};
use agentrecorder_supervisor::Layout;
use anyhow::{Result, bail};

use super::run::{self, RunArgs};
use crate::adapters;
use crate::attach::{self, DetachKey};
use crate::fmt;
use crate::procinfo::pid_alive;
use crate::render::final_screen;
use crate::state::State;
use crate::state::{duration_ms, open_and_resolve};

#[derive(clap::Args)]
pub struct RecoverArgs {
    #[arg(help = "Session id or a unique prefix of it")]
    pub session: String,
    #[arg(long, help = "Start a new session that re-executes the original command in its working directory")]
    pub restart: bool,
    #[arg(long, conflicts_with = "restart", help = "Start a new session with the agent's native resume command")]
    pub resume: bool,
    #[arg(short, long, help = "Do not ask for confirmation before --restart or --resume")]
    pub yes: bool,
    #[arg(long, help = "Start the new session without attaching")]
    pub detached: bool,
    #[arg(
        long,
        env = "AGENTRECORDER_DETACH_KEY",
        default_value = crate::DEFAULT_DETACH_KEY,
        help = "Key that detaches: ctrl-<key> or none"
    )]
    pub detach_key: String,
}

pub fn recover(layout: &Layout, args: RecoverArgs) -> Result<i32> {
    let key = DetachKey::parse(&args.detach_key)?;
    let (store, s, state) = open_and_resolve(layout, &args.session)?;
    match &state {
        State::Running(v) => {
            if args.restart || args.resume {
                bail!("session {} is still running; attach to it instead of starting a new one", s.id);
            }
            let attached = v.attached;
            println!("Scenario A: the managed process is still running.");
            println!(
                "  supervisor pid {}, process pid {}, terminal {}x{}{}",
                v.supervisor_pid,
                v.child_pid,
                v.cols,
                v.rows,
                if attached { "; another client is attached and will be detached" } else { "" }
            );
            println!("  Reattaching; the screen and up to 1000 lines of scrollback are redrawn from the supervisor.");
            let outcome = attach::attach(layout, &s.id, &key)?;
            Ok(attach::report_outcome(&s.id, &outcome))
        }
        State::Exited => {
            println!("Scenario B: the managed process has exited; its in-memory state cannot be recovered.");
            describe(&store, &s)?;
            offer(layout, &store, &s, &args, &key)
        }
        State::Lost => {
            let host = s.lost_reason == Some(LostReason::HostRestarted);
            if host {
                println!(
                    "Scenario C: the host restarted after this session started; the operating system ended its processes."
                );
            } else {
                println!("Scenario C: the session supervisor ended without recording an exit (killed or crashed).");
                if let Some(pid) = s.child_pid
                    && pid_alive(pid)
                {
                    println!(
                        "  pid {pid} still exists but has no supervisor and no terminal; it may be the original process or a reused pid."
                    );
                }
            }
            println!("  Persisted metadata and recorded events are intact; process memory is not.");
            describe(&store, &s)?;
            offer(layout, &store, &s, &args, &key)
        }
        State::Unreachable(why) => bail!("session {} is unreachable: {why}", s.id),
        State::Starting => bail!("session {} is still starting; try again in a moment", s.id),
    }
}

fn describe(store: &Store, s: &Session) -> Result<()> {
    println!();
    println!("  Command:   {}", s.command.join(" "));
    println!("  Directory: {}", s.cwd.as_deref().unwrap_or("unknown"));
    println!("  Created:   {}", format_rfc3339(s.created_at_ns));
    if let Some(e) = s.ended_at_ns {
        println!("  Ended:     {}", format_rfc3339(e));
    }
    if let Some(d) = duration_ms(s) {
        println!("  Duration:  {}", fmt::duration_ms(d));
    }
    match (s.exit_code, s.exit_signal) {
        (Some(c), _) => println!("  Exit:      code {c}"),
        (None, Some(sig)) => println!("  Exit:      signal {sig}"),
        _ => println!("  Exit:      not recorded"),
    }
    if s.record_output {
        match final_screen(store, s)? {
            Some(screen) => {
                println!();
                println!("  Last recorded screen:");
                for line in screen.lines() {
                    println!("  | {line}");
                }
            }
            None => println!("  No terminal output was recorded."),
        }
    } else {
        println!("  Output was not recorded (--no-record).");
    }
    println!();
    Ok(())
}

fn offer(layout: &Layout, store: &Store, s: &Session, args: &RecoverArgs, key: &DetachKey) -> Result<i32> {
    let resume = adapters::resume_for(&s.command, s.agent_session_id.as_deref());
    let restart_argv = match &s.agent_session_id {
        Some(id) => adapters::without_session_id(&s.command, id),
        None => s.command.clone(),
    };
    if !args.restart && !args.resume {
        println!("Options:");
        println!("  agentrecorder replay {}            view the recorded output", s.id);
        println!("  agentrecorder recover {} --restart re-run `{}` in a new session", s.id, restart_argv.join(" "));
        if let Some(r) = &resume {
            println!("  agentrecorder recover {} --resume  run `{}` ({}: {})", s.id, r.argv.join(" "), r.agent, r.note);
        }
        return Ok(0);
    }
    let argv = if args.resume {
        match resume {
            Some(r) => r.argv,
            None => bail!("no resume adapter is known for `{}`; use --restart", s.command[0]),
        }
    } else {
        restart_argv
    };
    let cwd = s.cwd.as_ref().map(PathBuf::from).filter(|p| p.is_dir());
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to re-execute without confirmation; pass --yes");
        }
        print!(
            "This runs `{}` again in {}. Commands it runs may have side effects. Continue? [y/N] ",
            argv.join(" "),
            cwd.as_ref().map_or("the current directory".into(), |p| p.display().to_string())
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("not restarted");
            return Ok(1);
        }
    }
    let run_args = RunArgs {
        detached: args.detached,
        no_record: !s.record_output,
        record_input: s.record_input,
        max_record_mb: original_record_cap_mb(store, s)?,
        detach_key: args.detach_key.clone(),
        no_agent_session_id: false,
        cwd,
        command: argv,
    };
    let started = run::start(layout, &run_args, Some(&s.id))?;
    if args.detached {
        println!("{}", started.id);
        return Ok(0);
    }
    if started.finished {
        return run::print_finished(layout, &started.id);
    }
    eprintln!("[agentrecorder] session {} started from {}; detach with {}", started.id, s.id, key.label);
    match attach::attach(layout, &started.id, key)? {
        attach::Outcome::Ended => run::print_finished(layout, &started.id),
        outcome => Ok(attach::report_outcome(&started.id, &outcome)),
    }
}

fn original_record_cap_mb(store: &Store, s: &Session) -> Result<u64> {
    let mut cap = None;
    store.for_each_event(&s.id, Some(&[kind::SESSION_CREATED]), |e| {
        if let Some(Body::SessionCreated { max_record_bytes: Some(b), .. }) = e.body() {
            cap = Some(b.div_ceil(1 << 20));
        }
        Ok(())
    })?;
    Ok(cap.unwrap_or(crate::DEFAULT_MAX_RECORD_MB))
}
