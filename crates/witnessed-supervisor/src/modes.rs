use witnessed_store::NotificationSource;

const MAX_SEQ: usize = 4096;
const MAX_TITLE: usize = 256;
const MAX_NOTIFICATION: usize = 512;
const MAX_KITTY_STACK: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Notification(NotificationSource, String),
    CommandFinished { exit_code: Option<i32> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub struct Modes {
    pub scroll_region: Option<(u16, u16)>,
    pub focus_events: bool,
    pub modify_other_keys: Option<u16>,
    pub kitty_keyboard: Vec<u16>,
    pub title: Option<String>,
}

impl Modes {
    pub fn prefix(&self) -> String {
        match self.scroll_region {
            Some((top, bottom)) => format!("\x1b[{top};{bottom}r"),
            None => String::new(),
        }
    }

    pub fn suffix(&self) -> String {
        let mut out = String::new();
        if self.focus_events {
            out.push_str("\x1b[?1004h");
        }
        for flags in &self.kitty_keyboard {
            out.push_str(&format!("\x1b[>{flags}u"));
        }
        if let Some(level) = self.modify_other_keys {
            out.push_str(&format!("\x1b[>4;{level}m"));
        }
        if let Some(title) = &self.title {
            out.push_str(&format!("\x1b]2;{title}\x07"));
        }
        out
    }
}

pub struct ModeTracker {
    state: State,
    buf: Vec<u8>,
    command_running: bool,
    pub modes: Modes,
}

impl Default for ModeTracker {
    fn default() -> Self {
        Self { state: State::Ground, buf: Vec::new(), command_running: false, modes: Modes::default() }
    }
}

fn params(body: &[u8]) -> Vec<Option<u16>> {
    if body.is_empty() {
        return Vec::new();
    }
    body.split(|&c| c == b';').map(|p| std::str::from_utf8(p).ok().and_then(|s| s.parse().ok())).collect()
}

impl ModeTracker {
    pub fn scan(&mut self, data: &[u8], notes: &mut Vec<Observed>) {
        for &b in data {
            match self.state {
                State::Ground => match b {
                    0x1b => self.state = State::Esc,
                    0x07 => notes.push(Observed::Notification(NotificationSource::Bell, String::new())),
                    _ => {}
                },
                State::Esc => {
                    self.buf.clear();
                    self.state = match b {
                        b'[' => State::Csi,
                        b']' => State::Osc,
                        b'c' => {
                            self.modes = Modes::default();
                            self.command_running = false;
                            State::Ground
                        }
                        0x1b => State::Esc,
                        _ => State::Ground,
                    };
                }
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.csi(b);
                        self.state = State::Ground;
                    } else if b == 0x1b {
                        self.state = State::Esc;
                    } else if self.buf.len() < MAX_SEQ {
                        self.buf.push(b);
                    }
                }
                State::Osc => match b {
                    0x07 => {
                        self.osc(notes);
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::OscEsc,
                    0x18 | 0x1a => self.state = State::Ground,
                    _ if self.buf.len() < MAX_SEQ => self.buf.push(b),
                    _ => {}
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.osc(notes);
                        self.state = State::Ground;
                    } else {
                        self.buf.clear();
                        self.state = if b == b'[' {
                            State::Csi
                        } else if b == b']' {
                            State::Osc
                        } else {
                            State::Ground
                        };
                    }
                }
            }
        }
    }

    fn csi(&mut self, fin: u8) {
        let buf = std::mem::take(&mut self.buf);
        let (marker, body) = match buf.first() {
            Some(m @ (b'?' | b'>' | b'<' | b'=')) => (Some(*m), &buf[1..]),
            _ => (None, &buf[..]),
        };
        if body.iter().any(|c| (0x20..=0x2f).contains(c)) {
            return;
        }
        let p = params(body);
        let m = &mut self.modes;
        match (marker, fin) {
            (None, b'r') => {
                m.scroll_region = match (p.first().copied().flatten(), p.get(1).copied().flatten()) {
                    (Some(t), Some(b)) if t > 1 || b > 1 => Some((t.max(1), b)),
                    _ => None,
                };
            }
            (Some(b'?'), b'h' | b'l') => {
                if p.contains(&Some(1004)) {
                    m.focus_events = fin == b'h';
                }
            }
            (Some(b'>'), b'm') => {
                if p.first() == Some(&Some(4)) || p.is_empty() {
                    m.modify_other_keys = p.get(1).copied().flatten().filter(|v| *v > 0);
                }
            }
            (Some(b'>'), b'u') => {
                if m.kitty_keyboard.len() < MAX_KITTY_STACK {
                    m.kitty_keyboard.push(p.first().copied().flatten().unwrap_or(0));
                }
            }
            (Some(b'<'), b'u') => {
                let n = p.first().copied().flatten().unwrap_or(1) as usize;
                let keep = m.kitty_keyboard.len().saturating_sub(n);
                m.kitty_keyboard.truncate(keep);
            }
            (Some(b'='), b'u') => {
                let flags = p.first().copied().flatten().unwrap_or(0);
                let mode = p.get(1).copied().flatten().unwrap_or(1);
                match m.kitty_keyboard.last_mut() {
                    Some(top) => {
                        *top = match mode {
                            2 => *top | flags,
                            3 => *top & !flags,
                            _ => flags,
                        }
                    }
                    None if flags > 0 && mode != 3 => m.kitty_keyboard.push(flags),
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn osc(&mut self, notes: &mut Vec<Observed>) {
        let buf = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&buf);
        let (num, rest) = text.split_once(';').unwrap_or((&text, ""));
        let clean = |s: &str, max: usize| -> String { s.chars().filter(|c| !c.is_control()).take(max).collect() };
        match num {
            "0" | "2" => self.modes.title = Some(clean(rest, MAX_TITLE)),
            "9" if !rest.starts_with("4;") => {
                notes.push(Observed::Notification(NotificationSource::Osc9, clean(rest, MAX_NOTIFICATION)));
            }
            "777" => {
                if let Some(body) = rest.strip_prefix("notify;") {
                    notes.push(Observed::Notification(
                        NotificationSource::Osc777,
                        clean(&body.replace(';', ": "), MAX_NOTIFICATION),
                    ));
                }
            }
            "133" => match rest.split(';').next() {
                Some("C") => self.command_running = true,
                Some("D") if std::mem::take(&mut self.command_running) => {
                    let exit_code = rest.split(';').nth(1).and_then(|c| c.parse().ok());
                    notes.push(Observed::CommandFinished { exit_code });
                }
                _ => {}
            },
            "99" => {
                let payload = rest.split_once(';').map_or(rest, |(_, p)| p);
                notes.push(Observed::Notification(NotificationSource::Osc99, clean(payload, MAX_NOTIFICATION)));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> (Modes, Vec<Observed>) {
        let mut t = ModeTracker::default();
        let mut notes = Vec::new();
        for c in chunks {
            t.scan(c, &mut notes);
        }
        (t.modes, notes)
    }

    #[test]
    fn tracks_modes_vt100_does_not_keep() {
        let (m, _) = scan(&[b"\x1b[?1004h\x1b[>1u\x1b[2;10r\x1b[>4;2m\x1b]0;agent\x07"]);
        assert_eq!(m.scroll_region, Some((2, 10)));
        assert!(m.focus_events);
        assert_eq!(m.kitty_keyboard, vec![1]);
        assert_eq!(m.modify_other_keys, Some(2));
        assert_eq!(m.title.as_deref(), Some("agent"));
        assert!(m.suffix().contains("\x1b[?1004h"));
        assert!(m.suffix().contains("\x1b[>1u"));
        assert_eq!(m.prefix(), "\x1b[2;10r");
    }

    #[test]
    fn resets_and_pops() {
        let (m, _) = scan(&[b"\x1b[?1004h\x1b[>1u\x1b[>3u\x1b[<u\x1b[r\x1b[?25;1004l"]);
        assert!(!m.focus_events);
        assert_eq!(m.kitty_keyboard, vec![1]);
        assert_eq!(m.scroll_region, None);
        let (m, _) = scan(&[b"\x1b[?1004h\x1bc"]);
        assert_eq!(m, Modes::default());
    }

    #[test]
    fn detects_notifications_across_chunks_and_ignores_progress() {
        let (_, notes) =
            scan(&[b"a\x07b\x1b]9;build ", b"done\x07\x1b]9;4;1;50\x07\x1b]777;notify;Claude;needs input\x1b\\"]);
        let got: Vec<_> = notes
            .iter()
            .filter_map(|n| match n {
                Observed::Notification(s, t) => Some((*s, t.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (NotificationSource::Bell, ""),
                (NotificationSource::Osc9, "build done"),
                (NotificationSource::Osc777, "Claude: needs input")
            ]
        );
    }

    #[test]
    fn title_is_sanitized_and_capped() {
        let (m, _) = scan(&[format!("\x1b]2;a\x01b{}\x07", "x".repeat(1000)).as_bytes()]);
        let t = m.title.unwrap();
        assert!(t.starts_with("ab") && t.len() == MAX_TITLE);
    }

    #[test]
    fn counts_finished_commands_from_osc_133_marks() {
        let (_, sig) = scan(&[
            b"\x1b]133;A\x07$ \x1b]133;D\x07",
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07",
            b"\x1b]133;C\x1b\\boom\x1b]133;D;",
            b"2;aid=7\x1b\\\x1b]133;C\x07\x1b]133;D\x07\x1b]133;D;1\x07",
        ]);
        assert_eq!(
            sig,
            vec![
                Observed::CommandFinished { exit_code: Some(0) },
                Observed::CommandFinished { exit_code: Some(2) },
                Observed::CommandFinished { exit_code: None },
            ]
        );
    }
}
