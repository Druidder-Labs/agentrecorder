use std::num::NonZeroU16;

use crate::modes::{ModeTracker, Observed};

pub struct ScreenModel {
    parser: vt100::Parser,
    modes: ModeTracker,
}

fn nonzero(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).expect("non-zero")
}

impl ScreenModel {
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self { parser: vt100::Parser::new(nonzero(rows), nonzero(cols), scrollback), modes: ModeTracker::default() }
    }

    pub fn process(&mut self, data: &[u8]) -> Vec<Observed> {
        let mut notes = Vec::new();
        self.modes.scan(data, &mut notes);
        self.parser.process(data);
        notes
    }

    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (rows.get(), cols.get())
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.modes.modes.title.as_deref()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(nonzero(rows), nonzero(cols));
    }

    #[must_use]
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    #[must_use]
    pub fn state(&self) -> String {
        let mut out = String::new();
        if self.alternate_screen() {
            out.push_str("\x1b[?1049h");
        }
        out.push_str(&self.modes.modes.prefix());
        out.push_str(&self.parser.screen().state_formatted());
        out.push_str(&self.modes.modes.suffix());
        out
    }

    pub fn state_with_history(&mut self, max_lines: usize) -> String {
        let mut out = String::new();
        let screen = self.parser.screen_mut();
        if !screen.alternate_screen() {
            let (rows, cols) = screen.size();
            let rows = rows.get() as usize;
            screen.set_scrollback(usize::MAX);
            let mut offset = screen.scrollback().min(max_lines);
            let mut lines = Vec::with_capacity(offset);
            while offset > 0 {
                screen.set_scrollback(offset);
                let take = offset.min(rows);
                lines.extend(screen.rows_formatted(0, cols.get()).take(take));
                offset -= take;
            }
            screen.set_scrollback(0);
            if !lines.is_empty() {
                out.push_str("\x1b[0m\x1b[H\x1b[2J");
                for l in lines {
                    out.push_str(&l);
                    out.push_str("\x1b[0m\r\n");
                }
                out.push_str(&"\r\n".repeat(rows));
            }
        }
        out.push_str(&self.state());
        out
    }

    pub fn text(&self) -> String {
        let text = self.parser.screen().contents();
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        let end = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |p| p + 1);
        lines[..end].join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_restores_screen_and_history() {
        let mut m = ScreenModel::new(5, 20, 100);
        for i in 0..12 {
            m.process(format!("line-{i}\r\n").as_bytes());
        }
        let snap = m.state_with_history(1000);
        let mut fresh = ScreenModel::new(5, 20, 100);
        fresh.process(snap.as_bytes());
        assert_eq!(fresh.text(), m.text());
        assert!(snap.contains("line-0"), "history missing: {snap:?}");
    }

    #[test]
    fn snapshot_restores_modes_vt100_does_not_track() {
        let mut m = ScreenModel::new(10, 20, 0);
        m.process(b"\x1b[?1004h\x1b[>1u\x1b[2;8r\x1b[5;5Hcursor");
        let snap = m.state();
        assert!(snap.starts_with("\x1b[2;8r"), "{snap:?}");
        assert!(snap.contains("\x1b[?1004h") && snap.contains("\x1b[>1u"));
        let mut fresh = ScreenModel::new(10, 20, 0);
        fresh.process(snap.as_bytes());
        assert_eq!(fresh.text(), m.text());
        assert_eq!(fresh.parser.screen().cursor_position(), m.parser.screen().cursor_position());
    }

    #[test]
    fn snapshot_of_alternate_screen_skips_history_and_enters_it() {
        let mut m = ScreenModel::new(5, 20, 100);
        m.process(b"shell-history\r\n\x1b[?1049hfullscreen");
        let snap = m.state_with_history(1000);
        assert!(!snap.contains("shell-history"));
        assert!(snap.starts_with("\x1b[?1049h"));
        assert!(snap.contains("fullscreen"));
    }
}
