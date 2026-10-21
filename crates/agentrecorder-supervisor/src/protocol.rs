use std::io::{self, Read, Write};

use agentrecorder_store::AgentHook;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Attach,
    Control,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub mode: Mode,
    pub rows: u16,
    pub cols: u16,
    pub client_pid: u32,
    #[serde(default)]
    pub backpressure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Control {
    Status,
    Stop { signal: i32, grace_ms: u64 },
    Detach,
    AgentHook { event: Box<AgentHook> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitInfo {
    pub fn shell_code(&self) -> i32 {
        self.exit_code.unwrap_or_else(|| 128 + self.signal.unwrap_or(0))
    }
}

#[derive(Debug, Clone)]
pub enum Frame {
    Hello(Hello),
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Detach,
    Control(Control),
    Output(Vec<u8>),
    Exited(ExitInfo),
    Detached { reason: String },
    Reply(Value),
    Error(String),
}

const HELLO: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const DETACH: u8 = 4;
const CONTROL: u8 = 5;
const OUTPUT: u8 = 101;
const EXITED: u8 = 102;
const DETACHED: u8 = 103;
const REPLY: u8 = 104;
const ERROR: u8 = 105;

fn json<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).expect("protocol types serialize")
}

pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (kind, payload): (u8, std::borrow::Cow<'_, [u8]>) = match frame {
        Frame::Hello(h) => (HELLO, json(h).into()),
        Frame::Input(d) => (INPUT, d.as_slice().into()),
        Frame::Resize { rows, cols } => {
            let mut b = rows.to_be_bytes().to_vec();
            b.extend_from_slice(&cols.to_be_bytes());
            (RESIZE, b.into())
        }
        Frame::Detach => (DETACH, Vec::new().into()),
        Frame::Control(c) => (CONTROL, json(c).into()),
        Frame::Output(d) => (OUTPUT, d.as_slice().into()),
        Frame::Exited(e) => (EXITED, json(e).into()),
        Frame::Detached { reason } => (DETACHED, reason.as_bytes().into()),
        Frame::Reply(v) => (REPLY, json(v).into()),
        Frame::Error(m) => (ERROR, m.as_bytes().into()),
    };
    if payload.len() > MAX_FRAME {
        return Err(invalid(format!("frame of {} bytes exceeds limit", payload.len())));
    }
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header)?;
    w.write_all(&payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(header[1..].try_into().expect("4 bytes")) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes exceeds limit")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    let frame = match header[0] {
        HELLO => Frame::Hello(parse(&payload)?),
        INPUT => Frame::Input(payload),
        RESIZE if len == 4 => Frame::Resize {
            rows: u16::from_be_bytes([payload[0], payload[1]]),
            cols: u16::from_be_bytes([payload[2], payload[3]]),
        },
        DETACH => Frame::Detach,
        CONTROL => Frame::Control(parse(&payload)?),
        OUTPUT => Frame::Output(payload),
        EXITED => Frame::Exited(parse(&payload)?),
        DETACHED => Frame::Detached { reason: String::from_utf8_lossy(&payload).into_owned() },
        REPLY => Frame::Reply(parse(&payload)?),
        ERROR => Frame::Error(String::from_utf8_lossy(&payload).into_owned()),
        k => return Err(invalid(format!("unknown frame kind {k}"))),
    };
    Ok(Some(frame))
}

fn parse<T: serde::de::DeserializeOwned>(p: &[u8]) -> io::Result<T> {
    serde_json::from_slice(p).map_err(|e| invalid(e.to_string()))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = vec![
            Frame::Hello(Hello {
                version: 1,
                mode: Mode::Attach,
                rows: 24,
                cols: 80,
                client_pid: 7,
                backpressure: true,
            }),
            Frame::Input(vec![0, 1, 0xff]),
            Frame::Resize { rows: 50, cols: 132 },
            Frame::Detach,
            Frame::Control(Control::Stop { signal: 1, grace_ms: 10 }),
            Frame::Exited(ExitInfo { exit_code: Some(3), signal: None }),
            Frame::Detached { reason: "x".into() },
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut r = buf.as_slice();
        let mut n = 0;
        while let Some(f) = read_frame(&mut r).unwrap() {
            assert_eq!(format!("{f:?}"), format!("{:?}", frames[n]));
            n += 1;
        }
        assert_eq!(n, frames.len());
    }

    #[test]
    fn rejects_oversized_frames() {
        let mut buf = vec![INPUT];
        buf.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }
}
