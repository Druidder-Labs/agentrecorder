use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use nix::sys::termios::{self, SetArg, Termios};
use witnessed_store::DetachReason;
use witnessed_supervisor::Layout;
use witnessed_supervisor::client::{AttachEvent, AttachSender, Attachment, ConnectError, ExitInfo};
use witnessed_supervisor::window_size;

pub fn terminal_size() -> Option<(u16, u16)> {
    window_size(io::stdout()).or_else(|| window_size(io::stdin())).or_else(|| window_size(io::stderr()))
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone)]
pub struct DetachKey {
    byte: Option<u8>,
    sequences: Vec<Vec<u8>>,
    pub label: String,
}

impl DetachKey {
    pub fn parse(s: &str) -> Result<Self> {
        if s.eq_ignore_ascii_case("none") {
            return Ok(DetachKey { byte: None, sequences: Vec::new(), label: "none".into() });
        }
        let lower = s.to_ascii_lowercase();
        let Some(key) = lower.strip_prefix("ctrl-").or_else(|| lower.strip_prefix("^")) else {
            bail!("detach key {s:?} must look like ctrl-<key> or none");
        };
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            bail!("detach key {s:?} must name a single key");
        };
        if !(c.is_ascii_lowercase() || "@[\\]^_".contains(c)) {
            bail!("detach key {s:?} is not a control key");
        }
        let byte = (c.to_ascii_uppercase() as u8) & 0x1f;
        let code = c as u32;
        let sequences = vec![format!("\x1b[{code};5u").into_bytes(), format!("\x1b[27;5;{code}~").into_bytes()];
        Ok(DetachKey { byte: Some(byte), sequences, label: format!("Ctrl-{c}") })
    }

    fn partial_suffix(&self, buf: &[u8]) -> usize {
        self.sequences
            .iter()
            .filter_map(|seq| (3..seq.len().min(buf.len() + 1)).rev().find(|&k| buf.ends_with(&seq[..k])))
            .max()
            .unwrap_or(0)
    }

    fn matches_at(&self, buf: &[u8]) -> Option<usize> {
        if self.byte.is_some_and(|b| buf.first() == Some(&b)) {
            return Some(1);
        }
        self.sequences.iter().find(|seq| buf.starts_with(seq)).map(Vec::len)
    }

    fn find(&self, buf: &[u8], in_paste: &mut bool) -> Option<(usize, usize)> {
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            if rest.starts_with(PASTE_START) {
                *in_paste = true;
                i += PASTE_START.len();
                continue;
            }
            if rest.starts_with(PASTE_END) {
                *in_paste = false;
                i += PASTE_END.len();
                continue;
            }
            if !*in_paste && let Some(len) = self.matches_at(rest) {
                return Some((i, len));
            }
            i += 1;
        }
        None
    }
}

struct RawMode;

impl RawMode {
    fn enable() -> Result<Option<Self>> {
        let stdin = io::stdin();
        if !stdin.is_terminal() {
            return Ok(None);
        }
        let saved = termios::tcgetattr(stdin.as_fd())?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        *SAVED_TERMIOS.lock().unwrap_or_else(|p| p.into_inner()) = Some(saved);
        restore_on_signals()?;
        termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)?;
        Ok(Some(RawMode))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Default)]
struct AltScreenTracker {
    active: bool,
    pending: Vec<u8>,
}

impl AltScreenTracker {
    fn observe(&mut self, chunk: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let mut i = 0;
        while let Some(p) = data[i..].windows(3).position(|w| w == b"\x1b[?") {
            let start = i + p + 3;
            let Some(end) = data[start..].iter().position(|c| !(c.is_ascii_digit() || *c == b';')) else {
                if data.len() - start < 64 {
                    self.pending = data[i + p..].to_vec();
                }
                return;
            };
            let fin = data[start + end];
            if fin == b'h' || fin == b'l' {
                let alt =
                    data[start..start + end].split(|&c| c == b';').any(|n| matches!(n, b"1049" | b"1047" | b"47"));
                if alt {
                    self.active = fin == b'h';
                }
            }
            i = start + end;
        }
        let keep = data.len().saturating_sub(2).max(i);
        if data[keep..].first() == Some(&0x1b) {
            self.pending = data[keep..].to_vec();
        }
    }
}

pub const RESET_MODES: &str = concat!(
    "\x1b[0m\x1b[?25h",
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l",
    "\x1b[?1l\x1b>\x1b[>4m\x1b[=0;1u\x1b[<u",
    "\x1b7\x1b[r\x1b8",
);

pub const REPLAY_RESET: &str = "\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07";

pub enum Outcome {
    Exited(ExitInfo),
    Detached(DetachReason),
    Disconnected,
    Ended,
}

static SAVED_TERMIOS: Mutex<Option<Termios>> = Mutex::new(None);
static ALT_SCREEN: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    if let Some(saved) = SAVED_TERMIOS.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = termios::tcsetattr(io::stdin().as_fd(), SetArg::TCSANOW, &saved);
        let stdout = io::stdout();
        if ALT_SCREEN.load(Ordering::SeqCst) {
            let _ = nix::unistd::write(stdout.as_fd(), b"\x1b[?1049l");
        }
        let _ = nix::unistd::write(stdout.as_fd(), RESET_MODES.as_bytes());
    }
}

fn restore_on_signals() -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    std::thread::Builder::new().name("client-signals".into()).spawn(move || {
        if let Some(sig) = signals.forever().next() {
            restore_terminal();
            std::process::exit(128 + sig);
        }
    })?;
    Ok(())
}

pub fn attach(layout: &Layout, id: &str, key: &DetachKey) -> Result<Outcome> {
    if std::env::var("WITNESSED_SESSION").is_ok_and(|s| s == id) {
        bail!("refusing to attach session {id} to itself");
    }
    let winch = if io::stdin().is_terminal() {
        Some(signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?)
    } else {
        None
    };
    let (rows, cols) = terminal_size().unwrap_or((24, 80));
    let backpressure = !io::stdout().is_terminal();
    let mut attachment = match Attachment::open(layout, id, rows, cols, backpressure) {
        Ok(a) => a,
        Err(ConnectError::NotRunning) => return Ok(Outcome::Ended),
        Err(e) => return Err(e).with_context(|| format!("attaching to session {id}")),
    };
    let raw = RawMode::enable()?;

    {
        let sender = attachment.sender();
        let key = key.clone();
        std::thread::Builder::new().name("stdin".into()).spawn(move || pump_stdin(&sender, &key))?;
    }
    if let Some(mut signals) = winch {
        let sender = attachment.sender();
        if let Some((r, c)) = terminal_size()
            && (r, c) != (rows, cols)
        {
            let _ = sender.resize(r, c);
        }
        std::thread::Builder::new().name("winch".into()).spawn(move || {
            for _ in signals.forever() {
                if let Some((rows, cols)) = terminal_size()
                    && sender.resize(rows, cols).is_err()
                {
                    break;
                }
            }
        })?;
    }

    let mut stdout = io::stdout().lock();
    let mut alt = AltScreenTracker::default();
    let outcome = loop {
        match attachment.recv() {
            Ok(AttachEvent::Output(data)) => {
                alt.observe(&data);
                ALT_SCREEN.store(alt.active, Ordering::SeqCst);
                if stdout.write_all(&data).and_then(|_| stdout.flush()).is_err() {
                    break Outcome::Disconnected;
                }
            }
            Ok(AttachEvent::Exited(info)) => break Outcome::Exited(info),
            Ok(AttachEvent::Detached(reason)) => break Outcome::Detached(reason),
            Ok(AttachEvent::Closed) => break Outcome::Disconnected,
            Err(e) => {
                drop(stdout);
                drop(raw);
                restore_terminal();
                return Err(e).with_context(|| format!("session {id}"));
            }
        }
    };
    drop(stdout);
    drop(raw);
    restore_terminal();
    Ok(outcome)
}

const END_OF_FILE: u8 = 0x04;

fn pump_stdin(sender: &AttachSender, key: &DetachKey) {
    let is_tty = io::stdin().is_terminal();
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 8192];
    let mut held: Vec<u8> = Vec::new();
    let mut in_paste = false;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => {
                if !held.is_empty() {
                    let _ = sender.input(&held);
                }
                if !is_tty {
                    let _ = sender.input(&[END_OF_FILE]);
                }
                return;
            }
            Ok(n) => n,
        };
        let mut chunk = std::mem::take(&mut held);
        chunk.extend_from_slice(&buf[..n]);
        if let Some((pos, _)) = key.find(&chunk, &mut in_paste) {
            if pos > 0 {
                let _ = sender.input(&chunk[..pos]);
            }
            let _ = sender.detach();
            return;
        }
        let keep = key.partial_suffix(&chunk);
        held = chunk.split_off(chunk.len() - keep);
        if !chunk.is_empty() && sender.input(&chunk).is_err() {
            return;
        }
    }
}

pub fn report_outcome(id: &str, outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Exited(info) => {
            match (info.exit_code, info.signal) {
                (Some(c), _) => eprintln!("[witnessed] session {id}: process exited with code {c}"),
                (None, Some(s)) => eprintln!("[witnessed] session {id}: process killed by signal {s}"),
                _ => eprintln!("[witnessed] session {id}: process exited"),
            }
            info.shell_code()
        }
        Outcome::Detached(reason) => {
            let why = match reason {
                DetachReason::ClientRequest => String::new(),
                r => format!(" ({})", r.as_str().replace('_', " ")),
            };
            eprintln!("[witnessed] detached from session {id}{why}; reattach with: witnessed attach {id}");
            0
        }
        Outcome::Ended => {
            eprintln!("[witnessed] session {id} ended before the client attached; see: witnessed recover {id}");
            1
        }
        Outcome::Disconnected => {
            eprintln!("[witnessed] connection to session {id} closed; check status with: witnessed inspect {id}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(k: &DetachKey, buf: &[u8]) -> Option<(usize, usize)> {
        k.find(buf, &mut false)
    }

    #[test]
    fn parses_detach_keys() {
        let k = DetachKey::parse("ctrl-\\").unwrap();
        assert_eq!(find(&k, b"ab\x1ccd"), Some((2, 1)));
        assert_eq!(find(&k, b"ab\x1b[92;5ucd"), Some((2, 7)));
        assert_eq!(find(&k, b"ab\x1b[27;5;92~cd"), Some((2, 10)));
        let k = DetachKey::parse("ctrl-a").unwrap();
        assert_eq!(find(&k, b"x\x01"), Some((1, 1)));
        assert_eq!(find(&DetachKey::parse("none").unwrap(), b"\x1c"), None);
        assert!(DetachKey::parse("alt-x").is_err());
        assert!(DetachKey::parse("ctrl-1").is_err());
        let k = DetachKey::parse("ctrl-\\").unwrap();
        assert_eq!(k.partial_suffix(b"abc\x1b[92"), 4);
        assert_eq!(k.partial_suffix(b"abc\x1b[27;5"), 6);
        assert_eq!(k.partial_suffix(b"abc\x1b["), 0);
        assert_eq!(k.partial_suffix(b"abc"), 0);
        assert_eq!(k.partial_suffix(b"x\x1b"), 0);
    }

    #[test]
    fn ignores_the_detach_key_inside_a_bracketed_paste() {
        let k = DetachKey::parse("ctrl-\\").unwrap();
        let mut in_paste = false;
        assert_eq!(k.find(b"\x1b[200~a\x1cb", &mut in_paste), None);
        assert!(in_paste);
        assert_eq!(k.find(b"c\x1b[201~d\x1c", &mut in_paste), Some((8, 1)));
        assert!(!in_paste);
    }

    #[test]
    fn tracks_alternate_screen() {
        let mut t = AltScreenTracker::default();
        t.observe(b"a\x1b[?1049hb");
        assert!(t.active);
        t.observe(b"\x1b[?1049h..\x1b[?1049l");
        assert!(!t.active);
        t.observe(b"text\x1b[?10");
        t.observe(b"49hmore");
        assert!(t.active);
        t.observe(b"\x1b[?25;1049l");
        assert!(!t.active);
        t.observe(b"\x1b[?25;1047h");
        assert!(t.active);
    }

    #[test]
    fn reset_covers_modes_programs_enable() {
        for seq in ["\x1b[?1004l", "\x1b[?1005l", "\x1b[?1015l", "\x1b[>4m", "\x1b[=0;1u", "\x1b7\x1b[r\x1b8"] {
            assert!(RESET_MODES.contains(seq), "{seq:?}");
        }
    }
}
