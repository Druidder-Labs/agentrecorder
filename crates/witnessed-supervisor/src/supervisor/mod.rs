mod connection;
mod delivery;
mod reader;
mod shared;
mod stop;

use std::fs::File;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded};
use witnessed_store::{Body, DetachReason, GitPhase, NewSession, Notice, Store, TerminationReason, now_unix_ns};

use crate::git::snapshot::{record_git, store_session_diff};
use crate::layout::Layout;
use crate::protocol::{ExitInfo, Frame};
use crate::pty;
use crate::recorder::{Meta, Options, Recorder};
use crate::screen::ScreenModel;
use shared::{Gauge, Latch, NotificationLog, Shared, Term};

const CLIENT_DRAIN_ON_EXIT: Duration = Duration::from_secs(10);
const PIPE_CLIENT_DRAIN_ON_EXIT: Duration = Duration::from_secs(120);
const SCROLLBACK_LINES: usize = 5000;
const OUTPUT_DRAIN: Duration = Duration::from_millis(500);
const FINAL_FLUSH_TIMEOUT: Duration = Duration::from_secs(65);
const INPUT_QUEUE: usize = 1024;
const DEFAULT_TERM: &str = "xterm-256color";

pub struct Config {
    pub layout: Layout,
    pub id: String,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub rows: u16,
    pub cols: u16,
    pub record_output: bool,
    pub record_input: bool,
    pub max_record_bytes: u64,
    pub restarted_from: Option<String>,
    pub agent_session_id: Option<String>,
    pub anchor_token_digest: Option<String>,
    pub hold_output: bool,
    pub umask: Option<u32>,
}

/// Supervises one session and returns its shell exit code. Single-shot per process: the threads it starts are
/// never joined and the signal handlers are registered once.
pub fn run(cfg: Config) -> Result<i32> {
    let started = Instant::now();
    let created_at = now_unix_ns();
    let socket_path = cfg.layout.socket_path(&cfg.id)?;
    let store = Store::open(&cfg.layout.db_path())?;
    let boot_time = Some(sysinfo::System::boot_time() as i64);
    let cwd = cfg.cwd.clone().or_else(|| std::env::current_dir().ok());
    let cwd_str = cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    let term_env = std::env::var("TERM").ok().filter(|t| !t.is_empty());
    let term_name = term_env.clone().unwrap_or_else(|| DEFAULT_TERM.to_string());
    store.create_session(&NewSession {
        id: &cfg.id,
        command: &cfg.argv,
        cwd: cwd_str.as_deref(),
        created_at_ns: created_at,
        supervisor_pid: i64::from(std::process::id()),
        rows: cfg.rows,
        cols: cfg.cols,
        record_output: cfg.record_output,
        record_input: cfg.record_input,
        boot_time,
        restarted_from: cfg.restarted_from.as_deref(),
        agent_session_id: cfg.agent_session_id.as_deref(),
        anchor_token_digest: cfg.anchor_token_digest.as_deref(),
    })?;

    let (recorder, recorder_thread) = Recorder::spawn(
        Some(store),
        cfg.id.clone(),
        started,
        Options {
            record_output: cfg.record_output,
            record_input: cfg.record_input,
            max_record_bytes: cfg.max_record_bytes,
        },
    )
    .context("spawning the recorder thread")?;
    recorder.event(Body::SessionCreated {
        command: cfg.argv.clone(),
        cwd: cwd_str,
        rows: cfg.rows,
        cols: cfg.cols,
        record_output: cfg.record_output,
        record_input: cfg.record_input,
        supervisor_pid: std::process::id(),
        restarted_from: cfg.restarted_from.clone(),
        max_record_bytes: Some(cfg.max_record_bytes),
        term: Some(term_name),
        agent_session_id: cfg.agent_session_id.clone(),
    });

    let mut child_env = vec![("WITNESSED_SESSION", cfg.id.as_str())];
    if term_env.is_none() {
        child_env.push(("TERM", DEFAULT_TERM));
    }
    let rows = cfg.rows.clamp(1, pty::MAX_ROWS);
    let cols = cfg.cols.clamp(1, pty::MAX_COLS);
    let spawned = match pty::spawn(&cfg.argv, cwd.as_deref(), rows, cols, &child_env, cfg.umask.map(|m| m as _)) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("witnessed supervisor: {msg}");
            recorder.event(Body::RuntimeError(Notice::new("spawn_failed", msg)));
            recorder.event(Body::SessionTerminated {
                reason: TerminationReason::SpawnFailed,
                exit_code: Some(127),
                signal: None,
                dropped_events: 0,
            });
            recorder.meta(Meta::Exited { at_ns: now_unix_ns(), exit_code: Some(127), signal: None });
            recorder_thread.finish(recorder);
            return Ok(127);
        }
    };
    let mut child = spawned.child;
    let child_pid = child.id() as i32;
    let start_ns = now_unix_ns();
    recorder.event(Body::ProcessStarted { pid: child_pid, command: cfg.argv.clone() });

    let reader_master = spawned.master.try_clone()?;
    let input = spawn_input_writer(spawned.master.try_clone()?)?;
    let shared = Arc::new(Shared {
        session_id: cfg.id.clone(),
        socket_path: socket_path.clone(),
        term: Mutex::new(Term {
            screen: ScreenModel::new(rows, cols, SCROLLBACK_LINES),
            rows,
            cols,
            client: None,
            next_client_id: 1,
            bytes_since_snapshot: 0,
            drops_at_snapshot: 0,
            held: cfg.hold_output.then(Vec::new),
        }),
        master: spawned.master,
        child_pid,
        recorder: recorder.clone(),
        exited: AtomicBool::new(false),
        recorded: Latch::default(),
        connections: Gauge::default(),
        reaped: AtomicBool::new(false),
        exit: Mutex::new(None),
        output_bytes: AtomicU64::new(0),
        input_bytes: AtomicU64::new(0),
        last_output_ms: AtomicU64::new(u64::MAX),
        notifications: Mutex::new(NotificationLog::default()),
        started,
        stop: Mutex::new(None),
        input,
    });
    shared.bind()?;
    recorder.meta(Meta::Started { child_pid: i64::from(child_pid), at_ns: start_ns });
    stop::install_signal_handlers(&shared)?;
    let start_head = Arc::new(OnceLock::new());
    {
        let recorder = recorder.clone();
        let cwd = cwd.clone();
        let start_head = start_head.clone();
        std::thread::spawn(move || {
            record_git(&recorder, cwd, GitPhase::Start, &start_head);
        });
    }

    let (reader_done_tx, reader_done_rx) = bounded::<()>(1);
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            reader::read_pty(&shared, reader_master);
            let _ = reader_done_tx.send(());
        })?;
    }

    let status = child.wait().context("waiting for managed process")?;
    shared.reaped.store(true, Ordering::Release);
    let ended_ns = now_unix_ns();
    let info = ExitInfo { exit_code: status.code(), signal: status.signal() };
    shared.reap_session_after_stop();
    let _ = reader_done_rx.recv_timeout(OUTPUT_DRAIN);
    let client = {
        let mut term = shared.term();
        *shared.exit.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(info.clone());
        shared.exited.store(true, Ordering::Release);
        term.client.take()
    };
    recorder.event(Body::ProcessExited {
        pid: child_pid,
        exit_code: info.exit_code,
        signal: info.signal,
        duration_ms: (ended_ns - start_ns) / 1_000_000,
    });
    if let Some(c) = &client {
        recorder.event(Body::SessionDetached { client: c.id, reason: DetachReason::ProcessExited });
    }
    if let Some((base, diff)) = record_git(&recorder, cwd, GitPhase::Exit, &start_head)
        && cfg.record_output
    {
        store_session_diff(&cfg.layout.db_path(), &cfg.id, &base, &diff);
    }
    let reason =
        if shared.stop_deadline().is_some() { TerminationReason::Stopped } else { TerminationReason::ProcessExited };
    recorder.event(Body::SessionTerminated {
        reason,
        exit_code: info.exit_code,
        signal: info.signal,
        dropped_events: recorder.counters.dropped_events.load(Ordering::Relaxed),
    });
    recorder.meta(Meta::Exited { at_ns: ended_ns, exit_code: info.exit_code, signal: info.signal });
    if !recorder.flush(FINAL_FLUSH_TIMEOUT) {
        eprintln!("witnessed supervisor: exit metadata not confirmed within {FINAL_FLUSH_TIMEOUT:?}");
    }
    shared.recorded.set();
    let client_done = client.map(|c| c.close(Frame::Exited(info.clone()), false));
    let _ = std::fs::remove_file(&socket_path);
    recorder_thread.finish(recorder);
    if let Some((done, backpressure)) = client_done {
        let _ = done.recv_timeout(if backpressure { PIPE_CLIENT_DRAIN_ON_EXIT } else { CLIENT_DRAIN_ON_EXIT });
    }
    // A client that attached after the process exited gets its Exited frame from its own connection thread;
    // wait for those threads so the frame is sent before this process ends.
    let deadline = Instant::now() + PIPE_CLIENT_DRAIN_ON_EXIT;
    std::thread::sleep(Duration::from_millis(20));
    shared.connections.wait_zero_until(deadline);
    Ok(info.shell_code())
}

fn spawn_input_writer(mut master: File) -> Result<Sender<Vec<u8>>> {
    let (tx, rx) = bounded::<Vec<u8>>(INPUT_QUEUE);
    std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
        for data in rx {
            if master.write_all(&data).is_err() {
                break;
            }
        }
    })?;
    Ok(tx)
}
