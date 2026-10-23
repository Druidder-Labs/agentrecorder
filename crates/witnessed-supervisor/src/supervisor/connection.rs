use std::io::{ErrorKind, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::TrySendError;
use nix::sys::signal::Signal;
use nix::unistd::geteuid;
use witnessed_store::{Body, DetachReason, Notice};

use super::delivery::{send_chunks, spawn_client_writer};
use super::shared::Shared;
use super::{CLIENT_DRAIN_ON_EXIT, FINAL_FLUSH_TIMEOUT, PIPE_CLIENT_DRAIN_ON_EXIT};
use crate::client::SessionStatus;
use crate::git::snapshot::{DIFF_TIMEOUT, GIT_TIMEOUT};
use crate::peer::peer_uid;
use crate::protocol::{Control, ExitInfo, Frame, Hello, Mode, PROTOCOL_VERSION, read_frame, write_frame};
use crate::pty;
use crate::recorder::Meta;

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);
const HISTORY_ON_ATTACH: usize = 1000;
const MAX_MARK_LABEL: usize = 256;

impl Shared {
    pub(super) fn bind(self: &Arc<Self>) -> Result<()> {
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
                        eprintln!("witnessed supervisor: accept failed: {e}");
                        std::thread::sleep(ACCEPT_BACKOFF);
                        continue;
                    }
                };
                let shared = shared.clone();
                shared.connections.inc();
                let spawned = std::thread::Builder::new().name("conn".into()).spawn({
                    let shared = shared.clone();
                    move || {
                        if let Err(e) = handle_connection(&shared, stream) {
                            eprintln!("witnessed supervisor: connection error: {e:#}");
                        }
                        shared.connections.dec();
                    }
                });
                if spawned.is_err() {
                    shared.connections.dec();
                }
            }
        })?;
        Ok(())
    }
}

fn handle_connection(shared: &Arc<Shared>, stream: UnixStream) -> Result<()> {
    let uid = match peer_uid(&stream) {
        Ok(uid) => uid,
        Err(e) => {
            eprintln!("witnessed supervisor: rejected a connection without peer credentials: {e}");
            return Ok(());
        }
    };
    let euid = geteuid().as_raw();
    let mut writer = stream.try_clone()?;
    if uid != euid {
        eprintln!("witnessed supervisor: rejected a connection from uid {uid} (this session runs as uid {euid})");
        let _ = write_frame(&mut writer, &Frame::Error(format!("uid {uid} is not allowed to connect")));
        return Ok(());
    }
    let mut handshake = Deadline::new(&stream, HANDSHAKE_DEADLINE);
    let hello = if let Some(Frame::Hello(h)) = read_frame(&mut handshake)? {
        h
    } else {
        let _ = write_frame(&mut writer, &Frame::Error("expected hello".into()));
        return Ok(());
    };
    if hello.version != PROTOCOL_VERSION {
        let _ = write_frame(
            &mut writer,
            &Frame::Error(format!(
                "protocol version mismatch: the client speaks {}, this supervisor (witnessed {}) speaks {PROTOCOL_VERSION}; \
                 use the witnessed binary that started the session, or stop it with `witnessed stop`",
                hello.version,
                env!("CARGO_PKG_VERSION")
            )),
        );
        return Ok(());
    }
    match hello.mode {
        Mode::Control => {
            let request = if let Some(Frame::Control(c)) = read_frame(&mut handshake)? {
                c
            } else {
                let _ = write_frame(&mut writer, &Frame::Error("expected control request".into()));
                return Ok(());
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
                exited: shared.exited.load(Ordering::Acquire),
                output_idle_ms: (last != u64::MAX).then(|| shared.uptime_ms().saturating_sub(last)),
                notifications: shared.notifications.lock().unwrap_or_else(std::sync::PoisonError::into_inner).total,
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
        Control::Mark { label } => {
            let label: String = label.chars().filter(|c| !c.is_control()).take(MAX_MARK_LABEL).collect();
            shared.recorder.event(Body::UserMark { label: label.clone() });
            Frame::Reply(serde_json::json!({"label": label, "uptime_ms": shared.uptime_ms()}))
        }
        Control::AgentHook { mut event } => {
            if !shared.recorder.records_output() {
                event.tool = None;
            }
            let recorded = shared.recorder.best_effort(Body::AgentHook(event));
            Frame::Reply(serde_json::json!({"recorded": recorded}))
        }
    }
}

fn attach(shared: &Arc<Shared>, mut reader: UnixStream, writer: UnixStream, hello: &Hello) -> Result<()> {
    let mut client = spawn_client_writer(shared, writer, hello.backpressure)?;
    let stalled = client.stalled.clone();
    let client_id = {
        let mut term = shared.term();
        if shared.exited.load(Ordering::Acquire) {
            if let Some(held) = term.held.take() {
                send_chunks(&client.tx, &held);
            }
            drop(term);
            shared.recorded.wait_until(Instant::now() + FINAL_FLUSH_TIMEOUT + GIT_TIMEOUT * 2 + DIFF_TIMEOUT);
            let info = shared.exit.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            let (done, backpressure) =
                client.close(Frame::Exited(info.unwrap_or(ExitInfo { exit_code: None, signal: None })), false);
            let _ = done.recv_timeout(if backpressure { PIPE_CLIENT_DRAIN_ON_EXIT } else { CLIENT_DRAIN_ON_EXIT });
            return Ok(());
        }
        shared.detach_client(&mut term, DetachReason::AttachedElsewhere);
        shared.resize(&mut term, hello.rows, hello.cols);
        match term.held.take() {
            Some(held) => send_chunks(&client.tx, &held),
            None => send_chunks(&client.tx, term.screen.state_with_history(HISTORY_ON_ATTACH).as_bytes()),
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
            Err(e) if e.kind() == ErrorKind::InvalidData => break Some(DetachReason::ProtocolError),
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
                    Err(TrySendError::Full(data)) => {
                        shared.recorder.best_effort(Body::RuntimeWarning(
                            Notice::new(
                                "input_dropped",
                                "the process is not reading input and the input queue is full; input was dropped",
                            )
                            .with("bytes", data.len()),
                        ));
                    }
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
    let reason = match reason {
        Some(DetachReason::ClientDisconnected) if stalled.load(Ordering::Acquire) => Some(DetachReason::ClientTooSlow),
        other => other,
    };
    let mut term = shared.term();
    if let Some(reason) = reason
        && term.client.as_ref().map(|c| c.id) == Some(client_id)
    {
        shared.detach_client(&mut term, reason);
    }
    Ok(())
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
