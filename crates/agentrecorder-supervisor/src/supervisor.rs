use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agentrecorder_store::{
    Body, DetachReason, GitPhase, NewSession, Notice, NotificationSource, SnapshotReason, Store, TerminationReason,
    now_unix_ns,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, TrySendError, bounded};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, geteuid};

use crate::client::SessionStatus;
use crate::layout::Layout;
use crate::modes::Observed;
use crate::peer::peer_uid;
use crate::protocol::{Control, ExitInfo, Frame, Hello, Mode, PROTOCOL_VERSION, read_frame, write_frame};
use crate::pty;
use crate::recorder::{Meta, Options, Recorder};
use crate::screen::ScreenModel;

const CLIENT_QUEUE: usize = 256;
const CLIENT_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_SEND_SLICE: Duration = Duration::from_millis(100);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_CLOSE_GRACE: Duration = Duration::from_secs(1);
const CLIENT_DRAIN_ON_EXIT: Duration = Duration::from_secs(10);
const PIPE_CLIENT_DRAIN_ON_EXIT: Duration = Duration::from_secs(120);
const SCROLLBACK_LINES: usize = 5000;
const HISTORY_ON_ATTACH: usize = 1000;
const SNAPSHOT_CHUNK: usize = 64 << 10;
const SNAPSHOT_EVERY_BYTES: u64 = 8 << 20;
const OUTPUT_DRAIN: Duration = Duration::from_millis(500);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const FINAL_FLUSH_TIMEOUT: Duration = Duration::from_secs(65);
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);
const INPUT_QUEUE: usize = 1024;
const MEMBER_POLL: Duration = Duration::from_millis(50);
const NOTIFICATION_INTERVAL_MS: u64 = 1000;
const GIT_TIMEOUT: Duration = Duration::from_secs(3);
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
    pub umask: Option<u32>,
}

struct Client {
    id: u64,
    tx: Sender<Frame>,
    backpressure: Arc<AtomicBool>,
    sock: UnixStream,
    last: Arc<Mutex<Option<Frame>>>,
    abort: Arc<AtomicBool>,
    done: Receiver<()>,
}

impl Client {
    fn close(self, last: Frame, drop_pending: bool) -> (Receiver<()>, bool) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(last);
        self.abort.store(drop_pending, Ordering::SeqCst);
        drop(self.tx);
        (self.done, self.backpressure.load(Ordering::SeqCst))
    }

    fn close_detached(self, reason: DetachReason) {
        let sock = self.sock.try_clone();
        let (done, _) = self.close(Frame::Detached { reason: reason.as_str().to_string() }, true);
        if let Ok(sock) = sock {
            std::thread::spawn(move || {
                if done.recv_timeout(CLIENT_CLOSE_GRACE).is_err() {
                    let _ = sock.shutdown(std::net::Shutdown::Both);
                }
            });
        }
    }
}

struct Term {
    screen: ScreenModel,
    rows: u16,
    cols: u16,
    client: Option<Client>,
    next_client_id: u64,
    bytes_since_snapshot: u64,
    drops_at_snapshot: u64,
}

#[derive(Default)]
struct NotificationLog {
    total: u64,
    last_at_ms: [Option<u64>; 4],
}

struct Shared {
    session_id: String,
    socket_path: PathBuf,
    term: Mutex<Term>,
    master: File,
    child_pid: i32,
    recorder: Recorder,
    exited: AtomicBool,
    reaped: AtomicBool,
    exit: Mutex<Option<ExitInfo>>,
    output_bytes: AtomicU64,
    input_bytes: AtomicU64,
    last_output_ms: AtomicU64,
    notifications: Mutex<NotificationLog>,
    started: Instant,
    stop: Mutex<Option<Instant>>,
    input: Sender<Vec<u8>>,
}

impl Shared {
    fn term(&self) -> MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn stop_deadline(&self) -> Option<Instant> {
        *self.stop.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn resize(&self, term: &mut Term, rows: u16, cols: u16) {
        let rows = rows.clamp(1, pty::MAX_ROWS);
        let cols = cols.clamp(1, pty::MAX_COLS);
        if (rows, cols) == (term.rows, term.cols) {
            return;
        }
        if let Err(e) = pty::resize(&self.master, rows, cols) {
            self.recorder.event(Body::RuntimeError(Notice::new("resize_failed", e.to_string())));
            return;
        }
        term.screen.resize(rows, cols);
        term.rows = rows;
        term.cols = cols;
        self.recorder.event(Body::TerminalResized { rows, cols });
        self.recorder.meta(Meta::Size(rows, cols));
    }

    fn detach_client(&self, term: &mut Term, reason: DetachReason) {
        if let Some(c) = term.client.take() {
            let id = c.id;
            c.close_detached(reason);
            self.recorder.event(Body::SessionDetached { client: id, reason });
            self.recorder.meta(Meta::Attached(false));
        }
    }

    fn signal_all(&self, sig: Signal) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        for pid in pty::session_members(self.child_pid) {
            if pid != self.child_pid {
                let _ = kill(Pid::from_raw(pid), sig);
            }
        }
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        if let Some(pg) = pty::foreground_pgrp(&self.master)
            && pg != self.child_pid
        {
            let _ = killpg(Pid::from_raw(pg), sig);
        }
        let _ = killpg(Pid::from_raw(self.child_pid), sig);
    }

    fn stop(self: &Arc<Self>, signal: Signal, grace: Duration, origin: &str) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        let first = {
            let mut stop = self.stop.lock().unwrap_or_else(|p| p.into_inner());
            let first = stop.is_none();
            if first {
                *stop = Some(Instant::now() + grace);
            }
            first
        };
        if let Some(c) = &self.term().client {
            c.backpressure.store(false, Ordering::SeqCst);
        }
        self.recorder.event(Body::StopRequested {
            signal: signal as i32,
            grace_ms: grace.as_millis() as u64,
            origin: origin.to_string(),
        });
        self.signal_all(signal);
        if signal == Signal::SIGKILL || !first {
            return;
        }
        let shared = self.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if shared.exited.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if !shared.exited.load(Ordering::SeqCst) {
                shared.recorder.event(escalation_warning());
                shared.signal_all(Signal::SIGKILL);
            }
        });
    }

    fn reap_session_after_stop(&self) {
        let Some(deadline) = self.stop_deadline() else { return };
        loop {
            if pty::session_members(self.child_pid).is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(MEMBER_POLL);
        }
        let left = pty::session_members(self.child_pid);
        if !left.is_empty() {
            self.recorder.event(escalation_warning());
            for pid in left {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }

    fn note(&self, source: NotificationSource, text: String) {
        let now = self.uptime_ms();
        let slot = source as usize;
        let mut log = self.notifications.lock().unwrap_or_else(|p| p.into_inner());
        log.total += 1;
        if log.last_at_ms[slot].is_some_and(|t| now.saturating_sub(t) < NOTIFICATION_INTERVAL_MS) {
            return;
        }
        log.last_at_ms[slot] = Some(now);
        drop(log);
        self.recorder.event(Body::TerminalNotification { source, text });
    }

    fn bind(self: &Arc<Self>) -> Result<()> {
        let _ = std::fs::remove_file(&self.socket_path);
        let listener =
            UnixListener::bind(&self.socket_path).with_context(|| format!("binding {}", self.socket_path.display()))?;
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        let shared = self.clone();
        std::thread::Builder::new().name("listener".into()).spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("agentrecorder supervisor: accept failed: {e}");
                        std::thread::sleep(ACCEPT_BACKOFF);
                        continue;
                    }
                };
                let shared = shared.clone();
                let _ = std::thread::Builder::new().name("conn".into()).spawn(move || {
                    if let Err(e) = handle_connection(&shared, stream) {
                        eprintln!("agentrecorder supervisor: connection error: {e:#}");
                    }
                });
            }
        })?;
        Ok(())
    }
}

fn escalation_warning() -> Body {
    Body::RuntimeWarning(
        Notice::new("stop_escalated", "processes did not exit within the grace period; sent SIGKILL")
            .with("signal", Signal::SIGKILL as i32),
    )
}

fn git_snapshot(cwd: &Path) -> Option<(Option<String>, Option<u64>)> {
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if run(&["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
        return None;
    }
    let head = run(&["rev-parse", "HEAD"]).map(|s| s.trim().to_string());
    let dirty = run(&["status", "--porcelain"]).map(|s| s.lines().count() as u64);
    Some((head, dirty))
}

fn record_git(recorder: &Recorder, cwd: Option<PathBuf>, phase: GitPhase) {
    let Some(cwd) = cwd else { return };
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(git_snapshot(&cwd));
    });
    if let Ok(Some((head, dirty_paths))) = rx.recv_timeout(GIT_TIMEOUT) {
        recorder.event(Body::GitSnapshot { phase, head, dirty_paths });
    }
}

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
        supervisor_pid: std::process::id() as i64,
        rows: cfg.rows,
        cols: cfg.cols,
        record_output: cfg.record_output,
        record_input: cfg.record_input,
        boot_time,
        restarted_from: cfg.restarted_from.as_deref(),
        agent_session_id: cfg.agent_session_id.as_deref(),
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
    );
    recorder.event(Body::SessionCreated {
        command: cfg.argv.clone(),
        cwd: cwd_str.clone(),
        rows: cfg.rows,
        cols: cfg.cols,
        record_output: cfg.record_output,
        record_input: cfg.record_input,
        supervisor_pid: std::process::id(),
        restarted_from: cfg.restarted_from.clone(),
        max_record_bytes: Some(cfg.max_record_bytes),
        term: Some(term_name.clone()),
        agent_session_id: cfg.agent_session_id.clone(),
    });

    let mut child_env = vec![("AGENTRECORDER_SESSION", cfg.id.as_str())];
    if term_env.is_none() {
        child_env.push(("TERM", DEFAULT_TERM));
    }
    let rows = cfg.rows.clamp(1, pty::MAX_ROWS);
    let cols = cfg.cols.clamp(1, pty::MAX_COLS);
    let spawned = match pty::spawn(&cfg.argv, cwd.as_deref(), rows, cols, &child_env, cfg.umask.map(|m| m as _)) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("agentrecorder supervisor: {msg}");
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
        }),
        master: spawned.master,
        child_pid,
        recorder: recorder.clone(),
        exited: AtomicBool::new(false),
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
    recorder.meta(Meta::Started { child_pid: child_pid as i64, at_ns: start_ns });
    install_signal_handlers(&shared)?;
    {
        let recorder = recorder.clone();
        let cwd = cwd.clone();
        std::thread::spawn(move || record_git(&recorder, cwd, GitPhase::Start));
    }

    let (reader_done_tx, reader_done_rx) = bounded::<()>(1);
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            read_pty(&shared, reader_master);
            let _ = reader_done_tx.send(());
        })?;
    }

    let status = child.wait().context("waiting for managed process")?;
    shared.reaped.store(true, Ordering::SeqCst);
    let ended_ns = now_unix_ns();
    let info = ExitInfo { exit_code: status.code(), signal: status.signal() };
    shared.reap_session_after_stop();
    let _ = reader_done_rx.recv_timeout(OUTPUT_DRAIN);
    let client = {
        let mut term = shared.term();
        *shared.exit.lock().unwrap_or_else(|p| p.into_inner()) = Some(info.clone());
        shared.exited.store(true, Ordering::SeqCst);
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
    record_git(&recorder, cwd, GitPhase::Exit);
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
        eprintln!("agentrecorder supervisor: exit metadata not confirmed within {FINAL_FLUSH_TIMEOUT:?}");
    }
    let client_done = client.map(|c| c.close(Frame::Exited(info.clone()), false));
    let _ = std::fs::remove_file(&socket_path);
    drop(shared);
    recorder_thread.finish(recorder);
    if let Some((done, backpressure)) = client_done {
        let _ = done.recv_timeout(if backpressure { PIPE_CLIENT_DRAIN_ON_EXIT } else { CLIENT_DRAIN_ON_EXIT });
    }
    Ok(info.shell_code())
}

fn install_signal_handlers(shared: &Arc<Shared>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP, SIGUSR1])?;
    let shared = shared.clone();
    std::thread::Builder::new().name("signals".into()).spawn(move || {
        for sig in signals.forever() {
            if sig == SIGUSR1 {
                if let Err(e) = shared.bind() {
                    eprintln!("agentrecorder supervisor: recreating the socket failed: {e:#}");
                }
                continue;
            }
            shared.stop(Signal::SIGHUP, Duration::from_secs(3), &format!("supervisor_signal_{sig}"));
        }
    })?;
    Ok(())
}

fn deliver(tx: &Sender<Frame>, mut frame: Frame, backpressure: &AtomicBool) -> bool {
    let mut deadline = (!backpressure.load(Ordering::SeqCst)).then(|| Instant::now() + CLIENT_SEND_TIMEOUT);
    loop {
        match tx.send_timeout(frame, CLIENT_SEND_SLICE) {
            Ok(()) => return true,
            Err(SendTimeoutError::Disconnected(_)) => return false,
            Err(SendTimeoutError::Timeout(f)) => {
                frame = f;
                if deadline.is_none() && !backpressure.load(Ordering::SeqCst) {
                    deadline = Some(Instant::now() + CLIENT_SEND_TIMEOUT);
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return false;
                }
            }
        }
    }
}

fn read_pty(shared: &Shared, mut master: File) {
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let data = &buf[..n];
        shared.output_bytes.fetch_add(n as u64, Ordering::Relaxed);
        shared.last_output_ms.store(shared.uptime_ms(), Ordering::Relaxed);
        shared.recorder.output(data);
        let (target, notes) = {
            let mut term = shared.term();
            let notes = term.screen.process(data);
            maybe_snapshot(shared, &mut term, n as u64);
            (term.client.as_ref().map(|c| (c.id, c.tx.clone(), c.backpressure.clone())), notes)
        };
        for signal in notes {
            match signal {
                Observed::Notification(source, text) => shared.note(source, text),
                Observed::CommandFinished { exit_code } => {
                    shared.recorder.best_effort(Body::ShellCommandFinished { exit_code });
                }
            }
        }
        let Some((id, tx, backpressure)) = target else { continue };
        let delivered = deliver(&tx, Frame::Output(data.to_vec()), &backpressure);
        drop(tx);
        if !delivered {
            let mut term = shared.term();
            if term.client.as_ref().map(|c| c.id) == Some(id) {
                shared.detach_client(&mut term, DetachReason::ClientTooSlow);
            }
        }
    }
}

fn maybe_snapshot(shared: &Shared, term: &mut Term, bytes: u64) {
    if !shared.recorder.records_output() {
        return;
    }
    term.bytes_since_snapshot += bytes;
    let drops = shared.recorder.counters.dropped_events.load(Ordering::Relaxed);
    let reason = if drops > term.drops_at_snapshot {
        SnapshotReason::AfterDrop
    } else if term.bytes_since_snapshot >= SNAPSHOT_EVERY_BYTES {
        SnapshotReason::Interval
    } else {
        return;
    };
    let (rows, cols) = term.screen.size();
    let state = term.screen.state().into_bytes();
    shared.recorder.event_with_data(Body::TerminalSnapshot { rows, cols, reason }, state);
    term.bytes_since_snapshot = 0;
    term.drops_at_snapshot = drops;
}

fn handle_connection(shared: &Arc<Shared>, stream: UnixStream) -> Result<()> {
    let Ok(uid) = peer_uid(&stream) else { return Ok(()) };
    let euid = geteuid().as_raw();
    let mut writer = stream.try_clone()?;
    if uid != euid {
        let _ = write_frame(&mut writer, &Frame::Error(format!("uid {uid} is not allowed to connect")));
        return Ok(());
    }
    let mut handshake = Deadline::new(&stream, HANDSHAKE_DEADLINE);
    let hello = match read_frame(&mut handshake)? {
        Some(Frame::Hello(h)) => h,
        _ => {
            let _ = write_frame(&mut writer, &Frame::Error("expected hello".into()));
            return Ok(());
        }
    };
    if hello.version != PROTOCOL_VERSION {
        let _ = write_frame(
            &mut writer,
            &Frame::Error(format!(
                "protocol version mismatch: the client speaks {}, this supervisor (agentrecorder {}) speaks {PROTOCOL_VERSION}; \
                 use the agentrecorder binary that started the session, or stop it with `agentrecorder stop`",
                hello.version,
                env!("CARGO_PKG_VERSION")
            )),
        );
        return Ok(());
    }
    match hello.mode {
        Mode::Control => {
            let request = match read_frame(&mut handshake)? {
                Some(Frame::Control(c)) => c,
                _ => {
                    let _ = write_frame(&mut writer, &Frame::Error("expected control request".into()));
                    return Ok(());
                }
            };
            if let Control::Stop { signal, grace_ms } = request {
                let Ok(sig) = Signal::try_from(signal) else {
                    write_frame(&mut writer, &Frame::Error(format!("invalid signal {signal}")))?;
                    return Ok(());
                };
                let _ = write_frame(&mut writer, &Frame::Reply(serde_json::json!({"ok": true})));
                shared.stop(sig, Duration::from_millis(grace_ms), "client");
                return Ok(());
            }
            write_frame(&mut writer, &control(shared, request))?;
            Ok(())
        }
        Mode::Attach => attach(shared, stream, writer, &hello),
    }
}

fn control(shared: &Arc<Shared>, c: Control) -> Frame {
    match c {
        Control::Status => {
            let term = shared.term();
            let c = &shared.recorder.counters;
            let last = shared.last_output_ms.load(Ordering::Relaxed);
            let status = SessionStatus {
                session_id: shared.session_id.clone(),
                supervisor_pid: std::process::id(),
                child_pid: shared.child_pid,
                foreground_pgrp: pty::foreground_pgrp(&shared.master),
                attached: term.client.is_some(),
                rows: term.rows,
                cols: term.cols,
                alternate_screen: term.screen.alternate_screen(),
                uptime_ms: shared.uptime_ms(),
                output_bytes: shared.output_bytes.load(Ordering::Relaxed),
                input_bytes: shared.input_bytes.load(Ordering::Relaxed),
                recorded_events: c.recorded_events.load(Ordering::Relaxed),
                dropped_events: c.dropped_events.load(Ordering::Relaxed),
                dropped_bytes: c.dropped_bytes.load(Ordering::Relaxed),
                recording_limit_reached: c.output_disabled.load(Ordering::Relaxed),
                exited: shared.exited.load(Ordering::SeqCst),
                output_idle_ms: (last != u64::MAX).then(|| shared.uptime_ms().saturating_sub(last)),
                notifications: shared.notifications.lock().unwrap_or_else(|p| p.into_inner()).total,
                title: term.screen.title().map(str::to_string),
            };
            Frame::Reply(serde_json::to_value(status).expect("status serializes"))
        }
        Control::Stop { .. } => Frame::Error("stop is handled before dispatch".into()),
        Control::Detach => {
            let mut term = shared.term();
            let had = term.client.is_some();
            shared.detach_client(&mut term, DetachReason::DetachRequested);
            Frame::Reply(serde_json::json!({"detached": had}))
        }
        Control::AgentHook { event } => {
            let recorded = shared.recorder.best_effort(Body::AgentHook(event));
            Frame::Reply(serde_json::json!({"recorded": recorded}))
        }
    }
}

fn spawn_client_writer(mut stream: UnixStream, backpressure: bool) -> Result<Client> {
    let (tx, rx) = bounded::<Frame>(CLIENT_QUEUE);
    let (done_tx, done) = bounded::<()>(1);
    let last: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let abort = Arc::new(AtomicBool::new(false));
    if !backpressure {
        stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    }
    let sock = stream.try_clone()?;
    let (w_last, w_abort) = (last.clone(), abort.clone());
    std::thread::Builder::new().name("client-writer".into()).spawn(move || {
        let mut healthy = true;
        for frame in rx {
            if w_abort.load(Ordering::SeqCst) {
                break;
            }
            if write_frame(&mut stream, &frame).is_err() {
                healthy = false;
                break;
            }
        }
        if healthy && let Some(frame) = w_last.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = write_frame(&mut stream, &frame);
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let _ = done_tx.send(());
    })?;
    Ok(Client { id: 0, tx, backpressure: Arc::new(AtomicBool::new(backpressure)), sock, last, abort, done })
}

fn attach(shared: &Arc<Shared>, mut reader: UnixStream, writer: UnixStream, hello: &Hello) -> Result<()> {
    let mut client = spawn_client_writer(writer, hello.backpressure)?;
    let client_id = {
        let mut term = shared.term();
        if shared.exited.load(Ordering::SeqCst) {
            let info = shared.exit.lock().unwrap_or_else(|p| p.into_inner()).clone();
            client.close(Frame::Exited(info.unwrap_or(ExitInfo { exit_code: None, signal: None })), false);
            return Ok(());
        }
        shared.detach_client(&mut term, DetachReason::AttachedElsewhere);
        shared.resize(&mut term, hello.rows, hello.cols);
        let snapshot = term.screen.state_with_history(HISTORY_ON_ATTACH).into_bytes();
        for chunk in snapshot.chunks(SNAPSHOT_CHUNK) {
            if client.tx.try_send(Frame::Output(chunk.to_vec())).is_err() {
                break;
            }
        }
        client.id = term.next_client_id;
        term.next_client_id += 1;
        let id = client.id;
        term.client = Some(client);
        id
    };
    shared.recorder.event(Body::SessionAttached {
        client: client_id,
        client_pid: hello.client_pid,
        rows: hello.rows,
        cols: hello.cols,
    });
    shared.recorder.meta(Meta::Attached(true));

    let reason = loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => break Some(DetachReason::ClientDisconnected),
        };
        match frame {
            Frame::Input(data) => {
                if !is_current(shared, client_id) {
                    break None;
                }
                shared.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                shared.recorder.input(&data);
                match shared.input.try_send(data) {
                    Ok(()) => {}
                    Err(TrySendError::Full(data)) => shared.recorder.event(Body::RuntimeWarning(
                        Notice::new(
                            "input_dropped",
                            "the process is not reading input and the input queue is full; input was dropped",
                        )
                        .with("bytes", data.len()),
                    )),
                    Err(TrySendError::Disconnected(_)) => break Some(DetachReason::PtyWriteFailed),
                }
            }
            Frame::Resize { rows, cols } => {
                let mut term = shared.term();
                if term.client.as_ref().map(|c| c.id) == Some(client_id) {
                    shared.resize(&mut term, rows, cols);
                }
            }
            Frame::Detach => break Some(DetachReason::ClientRequest),
            _ => break Some(DetachReason::ProtocolError),
        }
    };
    let mut term = shared.term();
    if let Some(reason) = reason
        && term.client.as_ref().map(|c| c.id) == Some(client_id)
    {
        shared.detach_client(&mut term, reason);
    }
    Ok(())
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

struct Deadline<'a> {
    stream: &'a UnixStream,
    until: Instant,
}

impl<'a> Deadline<'a> {
    fn new(stream: &'a UnixStream, within: Duration) -> Self {
        Deadline { stream, until: Instant::now() + within }
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::from(ErrorKind::TimedOut));
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut s = self.stream;
        let n = s.read(buf);
        self.stream.set_read_timeout(None)?;
        n
    }
}

fn is_current(shared: &Shared, client_id: u64) -> bool {
    shared.term().client.as_ref().map(|c| c.id) == Some(client_id)
}
