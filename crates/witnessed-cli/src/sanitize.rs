const MAX_SEQ: usize = 4096;

const ALLOWED_PRIVATE_MODES: &[u16] = &[1, 3, 5, 6, 7, 12, 25, 47, 1047, 1048, 1049, 2026];
const ALLOWED_ANSI_MODES: &[u16] = &[4, 20];
const MAX_REPEAT: u16 = 1024;

enum State {
    Ground,
    Esc,
    EscDrop,
    EscPass(u8),
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

pub struct Sanitizer {
    state: State,
    buf: Vec<u8>,
    overflow: bool,
    continuation: u8,
    held_c2: bool,
    str_c2: bool,
    tainted: bool,
    prev: u8,
}

impl Default for Sanitizer {
    fn default() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflow: false,
            continuation: 0,
            held_c2: false,
            str_c2: false,
            tainted: false,
            prev: 0,
        }
    }
}

impl Sanitizer {
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.step(b, out);
        }
    }

    fn push(&mut self, b: u8) {
        if self.buf.len() < MAX_SEQ {
            self.buf.push(b);
        } else {
            self.overflow = true;
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.overflow = false;
        self.str_c2 = false;
        self.tainted = false;
        self.state = State::Ground;
    }

    fn abort_then(&mut self, b: u8, out: &mut Vec<u8>) {
        self.reset();
        self.step(b, out);
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => self.ground(b, out),
            State::Esc => match b {
                b'[' => {
                    self.buf.push(b);
                    self.state = State::Csi;
                }
                b']' => {
                    self.buf.push(b);
                    self.state = State::Osc;
                }
                b'P' | b'_' | b'^' | b'X' => self.state = State::Str,
                b'Z' => self.reset(),
                b'%' | b' ' => self.state = State::EscDrop,
                b'(' | b')' | b'*' | b'+' | b'#' => self.state = State::EscPass(b),
                0x1b => {}
                0x80..=0xff | 0x00..=0x1f => self.abort_then(b, out),
                _ => {
                    out.push(0x1b);
                    out.push(b);
                    self.reset();
                }
            },
            State::Csi => match b {
                0x20..=0x3f => self.push(b),
                0x40..=0x7e => {
                    self.push(b);
                    if !self.overflow && csi_allowed(&self.buf) {
                        out.extend_from_slice(&self.buf);
                    }
                    self.reset();
                }
                _ => self.abort_then(b, out),
            },
            State::Osc => {
                if self.str_c2 {
                    self.str_c2 = false;
                    match b {
                        0x9c => return self.finish_osc(out, b"\x1b\\"),
                        0x80..=0x9f => self.tainted = true,
                        _ => {}
                    }
                    self.push(0xc2);
                    self.push(b);
                    return;
                }
                match b {
                    0x07 => self.finish_osc(out, &[0x07]),
                    0x1b => self.state = State::OscEsc,
                    0x18 | 0x1a => self.reset(),
                    0xc2 => self.str_c2 = true,
                    0x00..=0x1f | 0x7f => self.tainted = true,
                    _ => self.push(b),
                }
            }
            State::OscEsc => {
                if b == b'\\' {
                    self.finish_osc(out, b"\x1b\\");
                } else {
                    self.reset();
                    self.step(0x1b, out);
                    self.step(b, out);
                }
            }
            State::EscDrop => self.reset(),
            State::EscPass(intermediate) => {
                if (0x30..=0x7e).contains(&b) {
                    out.extend_from_slice(&[0x1b, intermediate, b]);
                    self.reset();
                } else {
                    self.abort_then(b, out);
                }
            }
            State::Str => {
                let after_c2 = std::mem::replace(&mut self.str_c2, false);
                let prev = std::mem::replace(&mut self.prev, b);
                match b {
                    0x9c if after_c2 || prev < 0x80 => self.reset(),
                    0x1b => self.state = State::StrEsc,
                    0x18 | 0x1a => self.reset(),
                    0xc2 => self.str_c2 = true,
                    _ => {}
                }
            }
            State::StrEsc => {
                self.state = if b == b'\\' {
                    State::Ground
                } else if b == 0x1b {
                    State::StrEsc
                } else {
                    State::Str
                };
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.held_c2 {
            self.held_c2 = false;
            if (0x80..=0x9f).contains(&b) {
                return;
            }
            out.push(0xc2);
        }
        if self.continuation > 0 && (0x80..=0xbf).contains(&b) {
            self.continuation -= 1;
            out.push(b);
            return;
        }
        self.continuation = 0;
        match b {
            0x1b => {
                self.buf.clear();
                self.buf.push(b);
                self.state = State::Esc;
            }
            0xc2 => self.held_c2 = true,
            0x80..=0x9f => {}
            0xc3..=0xdf => {
                self.continuation = 1;
                out.push(b);
            }
            0xe0..=0xef => {
                self.continuation = 2;
                out.push(b);
            }
            0xf0..=0xf7 => {
                self.continuation = 3;
                out.push(b);
            }
            _ => out.push(b),
        }
    }

    fn finish_osc(&mut self, out: &mut Vec<u8>, terminator: &[u8]) {
        if !self.overflow && !self.tainted && osc_allowed(&self.buf[2..]) {
            out.extend_from_slice(&self.buf);
            out.extend_from_slice(terminator);
        }
        self.reset();
    }
}

fn csi_allowed(seq: &[u8]) -> bool {
    let body = &seq[2..];
    let Some((&fin, rest)) = body.split_last() else { return false };
    let private = rest.first().copied().filter(|c| b"<=>?".contains(c));
    let params = if private.is_some() { &rest[1..] } else { rest };
    let intermediates: Vec<u8> = params.iter().copied().filter(|c| (0x20..=0x2f).contains(c)).collect();
    if params.iter().any(|c| b"<=>?".contains(c)) {
        return false;
    }
    let numbers: Vec<Option<u16>> = params
        .split(|&c| c == b';')
        .map(|p| if p.is_empty() { Some(0) } else { std::str::from_utf8(p).ok().and_then(|s| s.parse().ok()) })
        .collect();
    let all_in = |allowed: &[u16]| numbers.iter().all(|n| n.is_some_and(|v| allowed.contains(&v)));
    match (private, intermediates.as_slice()) {
        (None, []) if fin == b'h' || fin == b'l' => all_in(ALLOWED_ANSI_MODES),
        (None, []) if fin == b'b' => numbers.iter().all(|n| n.is_some_and(|v| v <= MAX_REPEAT)),
        (None, []) => b"@ABCDEFGHIJKLMPSTXZ`adefgmrsu".contains(&fin),
        (Some(b'?'), []) if fin == b'h' || fin == b'l' => all_in(ALLOWED_PRIVATE_MODES),
        (Some(b'?'), []) => b"JK".contains(&fin),
        (None, [b' ']) => fin == b'q',
        (None, [b'!']) => fin == b'p',
        _ => false,
    }
}

fn osc_allowed(body: &[u8]) -> bool {
    let num_end = body.iter().position(|&c| c == b';').unwrap_or(body.len());
    let num = std::str::from_utf8(&body[..num_end]).ok().and_then(|s| s.parse::<u32>().ok());
    let rest = &body[num_end.min(body.len())..];
    match num {
        Some(0..=2 | 133) => true,
        Some(8) => {
            let uri = rest.splitn(3, |&c| c == b';').nth(2).unwrap_or(b"");
            uri.is_empty() || [&b"https://"[..], b"http://", b"mailto:"].iter().any(|p| uri.starts_with(p))
        }
        Some(4 | 10..=19 | 104 | 110..=119) => !rest.contains(&b'?'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut s = Sanitizer::default();
        let mut out = Vec::new();
        for c in chunks {
            s.filter(c, &mut out);
        }
        out
    }

    #[test]
    fn keeps_text_colors_and_cursor_movement() {
        let input = b"hi \x1b[31mred\x1b[0m \x1b[2J\x1b[H\x1b]0;title\x07\xe2\x9c\x93";
        assert_eq!(run(&[input]), input.to_vec());
    }

    #[test]
    fn drops_clipboard_queries_and_device_strings() {
        assert_eq!(run(&[b"a\x1b]52;c;ZXZpbA==\x07b"]), b"ab");
        assert_eq!(run(&[b"a\x1b[6nb\x1b[cc\x1b[>0q"]), b"abc");
        assert_eq!(run(&[b"a\x1b]11;?\x1b\\b"]), b"ab");
        assert_eq!(run(&[b"a\x1bPq#0;2;0;0;0\x1b\\b\x1b_Gf=100;AAAA\x1b\\c"]), b"abc");
        assert_eq!(run(&[b"\x1b[21t\x1b[?u"]), b"");
    }

    #[test]
    fn drops_c1_controls_decid_and_unlisted_osc() {
        assert_eq!(run(&[b"a\xc2\x9d52;c;aGk=\x07b"]), b"a52;c;aGk=\x07b");
        assert_eq!(run(&[b"a\xc2", b"\x9b6nb"]), b"a6nb");
        assert_eq!(run(&[b"a\x9b6n\x9d"]), b"a6n");
        assert_eq!(run(&[b"a\x1bZb"]), b"ab");
        assert_eq!(run(&[b"a\x1b]1337;Copy=:aGk=\x07b\x1b]5522;type=write\x07c"]), b"abc");
        assert_eq!(
            run(&[b"\xc2\xa9 \xe2\x9c\x93 \xf0\x9f\x98\x80 \xc3\xa9"]),
            b"\xc2\xa9 \xe2\x9c\x93 \xf0\x9f\x98\x80 \xc3\xa9"
        );
        assert_eq!(
            run(&[b"\x1b]8;;https://x\x07l\x1b]8;;\x07\x1b]133;A\x07"]),
            b"\x1b]8;;https://x\x07l\x1b]8;;\x07\x1b]133;A\x07"
        );
    }

    #[test]
    fn keeps_rendering_sequences() {
        let input = b"\x1b[38;5;196m\x1b[1;24r\x1b[?25l\x1b[?1049h\x1b[2 q\x1b[!p\x1b[3@\x1b[?2J\x1b[K\x1b[5;10H";
        assert_eq!(run(&[input]), input.to_vec());
    }

    #[test]
    fn drops_reply_generating_and_c1_smuggling_sequences() {
        assert_eq!(run(&[b"a\x1b[1$w\x1b[1;1;1;1;1;1*y\x1b[?4m\x1b[#R\x1b[?1;1Sb"]), b"ab");
        assert_eq!(run(&[b"a\x1b[>1u\x1b[=1c\x1b[0x\x1b[18tb"]), b"ab");
        assert_eq!(run(&[b"a\x1b[\xc2\x9d52;p;aGk=\x07b"]), b"a52;p;aGk=\x07b");
        assert_eq!(run(&[b"a\x1b[\xc2\x90$qm\x1b\\b"]), b"a$qm\x1b\\b");
        assert_eq!(run(&[b"a\x1b]0;x\xc2\x9d52;c;aGk=\x07b"]), b"ab");
        assert_eq!(run(&[b"a\x1b]0;t\x18b"]), b"ab");
    }

    #[test]
    fn c1_string_terminator_ends_strings() {
        assert_eq!(run(&[b"T\x1bP\xc2\x9cvisible"]), b"Tvisible");
        assert_eq!(run(&[b"\x1b]0;title\xc2\x9cafter"]), b"\x1b]0;title\x1b\\after");
        assert_eq!(run(&[b"\x1b]0;caf\xc3\xa9\x07"]), b"\x1b]0;caf\xc3\xa9\x07");
    }

    #[test]
    fn third_review_findings_are_filtered() {
        assert_eq!(run(&[b"a\x1b\x9b6nb"]), b"a6nb");
        assert_eq!(run(&[b"a\x1b\x9d52;c;aGk=\x07b"]), b"a52;c;aGk=\x07b");
        assert_eq!(run(&[b"x\x1b[65535b"]), b"x");
        assert_eq!(run(&[b"x\x1b[3b"]), b"x\x1b[3b");
        assert_eq!(run(&[b"\x1b[2h\x1b[12l\x1b[4h"]), b"\x1b[4h");
        assert_eq!(run(&[b"\x1b[?1004h\x1b[?1000;1006h\x1b[?2004h\x1b[?25;1049h"]), b"\x1b[?25;1049h");
        assert_eq!(
            run(&[b"\x1b]8;;file:///etc/passwd\x07l\x1b]8;;https://x\x07m\x1b]8;;\x07"]),
            b"l\x1b]8;;https://x\x07m\x1b]8;;\x07"
        );
        assert_eq!(run(&[b"\x1b]7;file://host/tmp\x07a"]), b"a");
        assert_eq!(run(&[b"a\x1b%@b\x1b Gc\x1b(Bd\x1b#8e"]), b"abc\x1b(Bd\x1b#8e");
        assert_eq!(run(&[b"\x1bPq\x9cVISIBLE"]), b"VISIBLE");
        assert_eq!(run(&[b"\x1b]0;\xc3\x9cber\x07"]), b"\x1b]0;\xc3\x9cber\x07");
    }

    #[test]
    fn handles_sequences_split_across_chunks() {
        assert_eq!(run(&[b"a\x1b]5", b"2;c;x", b"\x07b\x1b", b"[31mc"]), b"ab\x1b[31mc");
    }
}
