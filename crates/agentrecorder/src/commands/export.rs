use std::io::Write;

use agentrecorder_store::{Body, Event, Session, Store, kind};
use agentrecorder_supervisor::Layout;
use anyhow::{Result, bail};
use serde_json::{Value, json};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Follow, open_store};
use crate::render::initial_size;

const CAST_KINDS: &[&str] = &[
    kind::TERMINAL_OUTPUT,
    kind::TERMINAL_RESIZED,
    kind::RUNTIME_WARNING,
    kind::SESSION_ATTACHED,
    kind::SESSION_DETACHED,
    kind::SESSION_STOP_REQUESTED,
    kind::TERMINAL_NOTIFICATION,
    kind::PROCESS_EXITED,
    kind::USER_MARK,
];

#[derive(clap::Args)]
pub struct ExportArgs {
    #[arg(help = "Session id or a unique prefix of it")]
    pub session: String,
    #[arg(
        long,
        default_value = "jsonl",
        help = "jsonl (all events), asciicast or asciicast-v2, or asciicast-v3 (output, resizes, markers, exit status)"
    )]
    pub format: String,
    #[arg(short, long, help = "Keep writing new events while the session runs; stops when it ends or on Ctrl-C")]
    pub follow: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Cast {
    V2,
    V3,
}

pub fn export(layout: &Layout, args: ExportArgs) -> Result<i32> {
    let store = open_store(layout)?;
    let s = store.resolve(&args.session)?;
    let interrupted = Arc::new(AtomicBool::new(false));
    if args.follow {
        for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            signal_hook::flag::register(sig, interrupted.clone())?;
        }
    }
    let follow = Follow { layout, follow: args.follow, interrupted: &interrupted };
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    match args.format.as_str() {
        "jsonl" => {
            follow.each(&store, &s.id, None, |e| {
                serde_json::to_writer(&mut out, &e.to_json())?;
                out.write_all(b"\n")?;
                if args.follow {
                    out.flush()?;
                }
                Ok(())
            })?;
        }
        "asciicast" | "asciicast-v2" => write_cast(&store, &s, Cast::V2, &follow, &mut out)?,
        "asciicast-v3" => write_cast(&store, &s, Cast::V3, &follow, &mut out)?,
        f => bail!("unknown format {f:?}; use jsonl, asciicast-v2 or asciicast-v3"),
    }
    out.flush()?;
    Ok(if interrupted.load(Ordering::Relaxed) { 130 } else { 0 })
}

fn recorded_term(store: &Store, s: &Session) -> Result<Option<String>> {
    let mut term = None;
    store.for_each_event(&s.id, Some(&[kind::SESSION_CREATED]), |e| {
        if let Some(Body::SessionCreated { term: t, .. }) = e.body() {
            term = t;
        }
        Ok(())
    })?;
    Ok(term)
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_controls(line: String) -> String {
    if !line.chars().any(|c| c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c)) {
        return line;
    }
    let mut out = String::with_capacity(line.len() + 16);
    for c in line.chars() {
        if c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn marker(e: &Event) -> Option<String> {
    Some(match e.body()? {
        Body::RuntimeWarning(n) => format!("warning: {}", n.code),
        Body::SessionAttached { .. } => "attached".into(),
        Body::SessionDetached { reason, .. } => format!("detached ({})", reason.as_str()),
        Body::StopRequested { signal, .. } => format!("stop requested (signal {signal})"),
        Body::TerminalNotification { text, .. } => format!("notification: {text}"),
        Body::UserMark { label } if label.is_empty() => "mark".into(),
        Body::UserMark { label } => format!("mark: {label}"),
        _ => return None,
    })
}

struct Timeline {
    cast: Cast,
    last_ns: i64,
    emitted_us: i64,
}

impl Timeline {
    fn stamp(&mut self, mono_ns: i64) -> Value {
        let mono_ns = mono_ns.max(self.last_ns);
        let value = match self.cast {
            Cast::V2 => json!(mono_ns as f64 / 1e9),
            Cast::V3 => {
                let target_us = (mono_ns / 1_000).div_euclid(1_000) * 1_000;
                let interval_us = (target_us - self.emitted_us).max(0);
                self.emitted_us += interval_us;
                json!(interval_us as f64 / 1e6)
            }
        };
        self.last_ns = mono_ns;
        value
    }
}

fn write_cast(store: &Store, s: &Session, cast: Cast, follow: &Follow<'_>, out: &mut impl Write) -> Result<()> {
    let (rows, cols) = initial_size(store, s)?;
    let term = recorded_term(store, s)?;
    let mut header = match cast {
        Cast::V2 => json!({"version": 2, "width": cols, "height": rows}),
        Cast::V3 => json!({"version": 3, "term": {"cols": cols, "rows": rows}}),
    };
    header["timestamp"] = json!(s.created_at_ns / 1_000_000_000);
    header["command"] = json!(shell_join(&s.command));
    if let Some(t) = term {
        match cast {
            Cast::V2 => header["env"] = json!({"TERM": t}),
            Cast::V3 => header["term"]["type"] = json!(t),
        }
    }
    writeln!(out, "{header}")?;
    let flush = follow.follow;
    let line = |out: &mut dyn Write, v: Value| -> Result<()> {
        writeln!(out, "{}", escape_controls(v.to_string()))?;
        if flush {
            out.flush()?;
        }
        Ok(())
    };
    let mut timeline = Timeline { cast, last_ns: 0, emitted_us: 0 };
    let mut pending: Vec<u8> = Vec::new();
    follow.each(store, &s.id, Some(CAST_KINDS), |e| {
        match e.body() {
            Some(Body::TerminalResized { rows, cols }) => {
                line(out, json!([timeline.stamp(e.mono_ns), "r", format!("{cols}x{rows}")]))?
            }
            Some(Body::TerminalOutput) => {
                if let Some(d) = &e.data {
                    pending.extend_from_slice(d);
                    let text = take_utf8(&mut pending);
                    if !text.is_empty() {
                        line(out, json!([timeline.stamp(e.mono_ns), "o", text]))?;
                    }
                }
            }
            Some(Body::ProcessExited { exit_code, signal, .. }) if cast == Cast::V3 => {
                let code = exit_code.unwrap_or_else(|| 128 + signal.unwrap_or(0));
                line(out, json!([timeline.stamp(e.mono_ns), "x", code.to_string()]))?;
            }
            _ => {
                if let Some(label) = marker(&e) {
                    line(out, json!([timeline.stamp(e.mono_ns), "m", label]))?;
                }
            }
        }
        Ok(())
    })?;
    if !pending.is_empty() {
        let t = timeline.stamp(timeline.last_ns);
        line(out, json!([t, "o", String::from_utf8_lossy(&pending)]))?;
    }
    Ok(())
}

fn take_utf8(pending: &mut Vec<u8>) -> String {
    let mut text = String::new();
    let mut rest: &[u8] = pending;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                text.push_str(s);
                rest = &[];
                break;
            }
            Err(e) => {
                let (ok, bad) = rest.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(ok).expect("validated"));
                match e.error_len() {
                    Some(n) => {
                        text.push('\u{fffd}');
                        rest = &bad[n..];
                    }
                    None => {
                        rest = bad;
                        break;
                    }
                }
            }
        }
    }
    let keep = rest.to_vec();
    *pending = keep;
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_del_and_c1_and_quotes_commands() {
        let line = json!([0.1, "o", "a\u{7f}\u{9b}b\u{1b}"]).to_string();
        let escaped = escape_controls(line);
        assert!(escaped.contains("\\u007f") && escaped.contains("\\u009b") && escaped.contains("\\u001b"));
        assert_eq!(shell_join(&["sh".into(), "-c".into(), "echo it's".into()]), "sh -c 'echo it'\\''s'");
    }

    #[test]
    fn v3_intervals_round_to_milliseconds_without_drift() {
        let mut t = Timeline { cast: Cast::V3, last_ns: 0, emitted_us: 0 };
        let mut total = 0.0;
        for i in 1..=1000 {
            total += t.stamp(i * 1_500_700).as_f64().unwrap();
        }
        assert!((total - 1.5007).abs() < 0.001, "{total}");
    }

    #[test]
    fn carries_split_utf8_sequences() {
        let mut p = b"a\xe2\x9c".to_vec();
        assert_eq!(take_utf8(&mut p), "a");
        p.extend_from_slice(b"\x93b\xffc");
        assert_eq!(take_utf8(&mut p), "\u{2713}b\u{fffd}c");
        assert!(p.is_empty());
    }
}
