use std::fmt;
use std::io::{self, ErrorKind};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentrecorder_store::DetachReason;
use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::protocol::{Control, Frame, Hello, Mode, PROTOCOL_VERSION, read_frame, write_frame};

pub use crate::protocol::ExitInfo;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub supervisor_pid: u32,
    pub child_pid: i32,
    pub foreground_pgrp: Option<i32>,
    pub attached: bool,
    pub rows: u16,
    pub cols: u16,
    pub alternate_screen: bool,
    pub uptime_ms: u64,
    pub output_bytes: u64,
    pub input_bytes: u64,
    pub recorded_events: u64,
    pub dropped_events: u64,
    pub dropped_bytes: u64,
    pub recording_limit_reached: bool,
    pub exited: bool,
    #[serde(default)]
    pub output_idle_ms: Option<u64>,
    #[serde(default)]
    pub notifications: u64,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug)]
pub enum ConnectError {
    NotRunning,
    Io(io::Error),
    Rejected(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NotRunning => write!(f, "the session supervisor is not running"),
            ConnectError::Io(e) => write!(f, "session supervisor unreachable: {e}"),
            ConnectError::Rejected(m) => write!(f, "session supervisor refused the request: {m}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            ErrorKind::NotFound | ErrorKind::ConnectionRefused => ConnectError::NotRunning,
            _ => ConnectError::Io(e),
        }
    }
}

fn connect(layout: &Layout, id: &str) -> Result<UnixStream, ConnectError> {
    let path = layout.socket_path(id).map_err(|_| ConnectError::NotRunning)?;
    Ok(UnixStream::connect(path)?)
}

fn hello(mode: Mode, rows: u16, cols: u16, backpressure: bool) -> Frame {
    Frame::Hello(Hello { version: PROTOCOL_VERSION, mode, rows, cols, client_pid: std::process::id(), backpressure })
}

fn control(layout: &Layout, id: &str, req: Control) -> Result<serde_json::Value, ConnectError> {
    let mut stream = connect(layout, id)?;
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    write_frame(&mut stream, &hello(Mode::Control, 0, 0, false))?;
    write_frame(&mut stream, &Frame::Control(req))?;
    match read_frame(&mut stream)? {
        Some(Frame::Reply(v)) => Ok(v),
        Some(Frame::Error(m)) => Err(ConnectError::Rejected(m)),
        Some(other) => Err(ConnectError::Rejected(format!("unexpected reply {other:?}"))),
        None => Err(ConnectError::Io(io::Error::from(ErrorKind::UnexpectedEof))),
    }
}

pub fn is_ready(layout: &Layout, id: &str) -> bool {
    status(layout, id).is_ok()
}

pub fn status(layout: &Layout, id: &str) -> Result<SessionStatus, ConnectError> {
    let v = control(layout, id, Control::Status)?;
    serde_json::from_value(v).map_err(|e| ConnectError::Rejected(format!("malformed status: {e}")))
}

pub fn stop(layout: &Layout, id: &str, signal: i32, grace: Duration) -> Result<(), ConnectError> {
    control(layout, id, Control::Stop { signal, grace_ms: grace.as_millis() as u64 }).map(|_| ())
}

#[derive(Deserialize)]
struct DetachReply {
    detached: bool,
}

pub fn detach(layout: &Layout, id: &str) -> Result<bool, ConnectError> {
    let reply: DetachReply = serde_json::from_value(control(layout, id, Control::Detach)?)
        .map_err(|e| ConnectError::Rejected(format!("malformed detach reply: {e}")))?;
    Ok(reply.detached)
}

#[derive(Debug)]
pub enum AttachEvent {
    Output(Vec<u8>),
    Exited(ExitInfo),
    Detached(DetachReason),
    Closed,
}

pub struct Attachment {
    reader: UnixStream,
    sender: AttachSender,
}

#[derive(Clone)]
pub struct AttachSender {
    stream: Arc<Mutex<UnixStream>>,
}

impl Attachment {
    pub fn open(
        layout: &Layout,
        id: &str,
        rows: u16,
        cols: u16,
        backpressure: bool,
    ) -> Result<Attachment, ConnectError> {
        let stream = connect(layout, id)?;
        let mut writer = stream.try_clone()?;
        write_frame(&mut writer, &hello(Mode::Attach, rows, cols, backpressure))?;
        Ok(Attachment { reader: stream, sender: AttachSender { stream: Arc::new(Mutex::new(writer)) } })
    }

    pub fn sender(&self) -> AttachSender {
        self.sender.clone()
    }

    pub fn recv(&mut self) -> Result<AttachEvent, ConnectError> {
        loop {
            return match read_frame(&mut self.reader) {
                Ok(Some(Frame::Output(d))) => Ok(AttachEvent::Output(d)),
                Ok(Some(Frame::Exited(info))) => Ok(AttachEvent::Exited(info)),
                Ok(Some(Frame::Detached { reason })) => Ok(AttachEvent::Detached(
                    serde_json::from_value(serde_json::Value::String(reason)).unwrap_or(DetachReason::ProtocolError),
                )),
                Ok(Some(Frame::Error(m))) => Err(ConnectError::Rejected(m)),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => Ok(AttachEvent::Closed),
            };
        }
    }
}

impl AttachSender {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        let mut s = self.stream.lock().unwrap_or_else(|p| p.into_inner());
        write_frame(&mut *s, frame)
    }

    pub fn input(&self, data: &[u8]) -> io::Result<()> {
        self.send(&Frame::Input(data.to_vec()))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.send(&Frame::Resize { rows, cols })
    }

    pub fn detach(&self) -> io::Result<()> {
        self.send(&Frame::Detach)
    }
}
