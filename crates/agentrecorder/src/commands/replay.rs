use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agentrecorder_store::{Body, kind};
use agentrecorder_supervisor::Layout;
use anyhow::{Result, bail};

use super::{Follow, open_store};
use crate::attach::{REPLAY_RESET, RESET_MODES};
use crate::fmt;
use crate::render::{apply, initial_size, playback_start};
use crate::sanitize::Sanitizer;

#[derive(clap::Args)]
pub struct ReplayArgs {
    #[arg(help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(long, default_value_t = 1.0, help = "Playback speed multiplier")]
    pub speed: f64,
    #[arg(long, help = "Start at an offset: seconds, MM:SS or HH:MM:SS")]
    pub from: Option<String>,
    #[arg(
        long,
        conflicts_with = "from",
        value_name = "N|LABEL",
        help = "Start at a marker: its number (1 is the first) or its exact label"
    )]
    pub from_mark: Option<String>,
    #[arg(long, default_value_t = 3.0, help = "Cap idle gaps at this many seconds (0 keeps original gaps)")]
    pub max_idle: f64,
    #[arg(long, help = "Write all output immediately, without timing")]
    pub instant: bool,
    #[arg(long, help = "Do not filter terminal queries, clipboard writes and device control strings")]
    pub raw: bool,
    #[arg(short, long, help = "Keep playing new output while the session runs; stops when it ends or on Ctrl-C")]
    pub follow: bool,
}

pub fn replay(layout: &Layout, args: ReplayArgs) -> Result<i32> {
    if !(args.speed.is_finite() && args.speed > 0.0) {
        bail!("--speed must be a positive number");
    }
    if !(args.max_idle.is_finite() && args.max_idle >= 0.0) {
        bail!("--max-idle must be zero or positive");
    }
    let store = open_store(layout)?;
    let s = store.resolve(&args.session)?;
    if !s.record_output {
        bail!("session {} was started with --no-record; there is no output to replay", s.id);
    }
    let from_ns = match &args.from_mark {
        Some(m) => mark_offset(&store, &s.id, m)?,
        None => args.from.as_deref().map(fmt::parse_offset).transpose()?.unwrap_or(0),
    };
    let max_idle_ns = (args.max_idle > 0.0).then_some((args.max_idle * 1e9) as i64);

    let interrupted = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, interrupted.clone())?;
    }

    let (rows, cols) = initial_size(&store, &s)?;
    let tty = std::io::stdout().is_terminal();
    if tty {
        eprintln!(
            "[agentrecorder] replaying session {} recorded at {cols}x{rows}; speed {}x; observational only, nothing is re-executed",
            s.id, args.speed
        );
    }
    let mut out = std::io::stdout().lock();
    let mut sanitizer = Sanitizer::default();
    let (mut seek, start_after) = if from_ns > 0 {
        let (model, after) = playback_start(&store, &s, from_ns)?;
        (Some(model), after)
    } else {
        (None, -1)
    };
    let mut clock = Clock::new(args.speed, max_idle_ns);
    let mut frames = 0u64;
    let mut resync_pending = false;
    let mut buf = Vec::with_capacity(64 << 10);
    let kinds = [
        kind::TERMINAL_OUTPUT,
        kind::TERMINAL_RESIZED,
        kind::TERMINAL_SNAPSHOT,
        kind::RUNTIME_WARNING,
        kind::USER_MARK,
    ];
    let mut marks_seen = marks_before(&store, &s.id, from_ns)?;

    let follow = Follow { layout, follow: args.follow, interrupted: &interrupted };
    follow
        .each_from(&store, &s.id, start_after, Some(&kinds), |e| {
            if interrupted.load(Ordering::Relaxed) {
                bail!("interrupted");
            }
            if let Some(model) = seek.as_mut() {
                if e.mono_ns < from_ns {
                    apply(model, &e);
                    return Ok(());
                }
                let snapshot = model.state();
                write_filtered(&mut out, &mut sanitizer, &mut buf, snapshot.as_bytes(), args.raw)?;
                seek = None;
                clock.start_at(from_ns);
            }
            match e.body() {
                Some(Body::RuntimeWarning(n)) => {
                    resync_pending |= n.code == "events_dropped";
                    return Ok(());
                }
                Some(Body::TerminalResized { rows, cols }) => {
                    if tty {
                        eprintln!("\r\n[agentrecorder] the recorded terminal was resized to {cols}x{rows}\r");
                    }
                    return Ok(());
                }
                Some(Body::UserMark { label }) => {
                    marks_seen += 1;
                    if tty {
                        let shown = if label.is_empty() { String::new() } else { format!(": {label}") };
                        eprintln!("\r\n[agentrecorder] mark {marks_seen}{shown}\r");
                    }
                    return Ok(());
                }
                Some(Body::TerminalSnapshot { .. }) if !resync_pending => return Ok(()),
                _ => {}
            }
            let Some(data) = &e.data else { return Ok(()) };
            if !args.instant {
                sleep_until(clock.deadline(e.mono_ns), &interrupted);
            }
            write_filtered(&mut out, &mut sanitizer, &mut buf, data, args.raw)?;
            if e.kind == kind::TERMINAL_SNAPSHOT {
                resync_pending = false;
            } else {
                frames += 1;
            }
            Ok(())
        })
        .or_else(|e| if interrupted.load(Ordering::Relaxed) { Ok(()) } else { Err(e) })?;

    if let Some(model) = seek.take() {
        let snapshot = model.state();
        write_filtered(&mut out, &mut sanitizer, &mut buf, snapshot.as_bytes(), args.raw)?;
    }
    if tty && !args.raw {
        out.write_all(b"\x1b[?1049l")?;
        out.write_all(RESET_MODES.as_bytes())?;
        out.write_all(REPLAY_RESET.as_bytes())?;
        out.flush()?;
        if std::io::stdin().is_terminal() {
            let _ = nix::sys::termios::tcflush(std::io::stdin(), nix::sys::termios::FlushArg::TCIFLUSH);
        }
        eprintln!("\r\n[agentrecorder] replay of session {} finished ({frames} output events)", s.id);
    }
    if interrupted.load(Ordering::Relaxed) {
        return Ok(130);
    }
    Ok(0)
}

pub fn marks(store: &agentrecorder_store::Store, id: &str) -> Result<Vec<(i64, String)>> {
    let mut out = Vec::new();
    store.for_each_event(id, Some(&[kind::USER_MARK]), |e| {
        if let Some(Body::UserMark { label }) = e.body() {
            out.push((e.mono_ns, label));
        }
        Ok(())
    })?;
    Ok(out)
}

fn marks_before(store: &agentrecorder_store::Store, id: &str, from_ns: i64) -> Result<u64> {
    Ok(marks(store, id)?.iter().filter(|(t, _)| *t < from_ns).count() as u64)
}

fn mark_offset(store: &agentrecorder_store::Store, id: &str, wanted: &str) -> Result<i64> {
    let all = marks(store, id)?;
    if all.is_empty() {
        bail!("session {id} has no markers; add one with `agentrecorder mark {id} LABEL` while it runs");
    }
    let by_label = all.iter().find(|(_, l)| l == wanted);
    let by_index = wanted.parse::<usize>().ok().filter(|n| *n >= 1).and_then(|n| all.get(n - 1));
    match by_label.or(by_index) {
        Some((t, _)) => Ok((*t).max(1)),
        None => bail!("no marker {wanted:?} in session {id}; it has {} (see `agentrecorder inspect {id}`)", all.len()),
    }
}

struct Clock {
    speed: f64,
    max_idle_ns: Option<i64>,
    wall_start: Option<Instant>,
    prev_mono: i64,
    virtual_ns: f64,
}

impl Clock {
    fn new(speed: f64, max_idle_ns: Option<i64>) -> Self {
        Clock { speed, max_idle_ns, wall_start: None, prev_mono: 0, virtual_ns: 0.0 }
    }

    fn start_at(&mut self, mono_ns: i64) {
        self.wall_start = Some(Instant::now());
        self.prev_mono = mono_ns;
        self.virtual_ns = 0.0;
    }

    fn deadline(&mut self, mono_ns: i64) -> Instant {
        if self.wall_start.is_none() {
            self.start_at(mono_ns);
        }
        let mut gap = (mono_ns - self.prev_mono).max(0);
        if let Some(cap) = self.max_idle_ns {
            gap = gap.min(cap);
        }
        self.prev_mono = self.prev_mono.max(mono_ns);
        self.virtual_ns += gap as f64 / self.speed;
        self.wall_start.expect("started") + Duration::from_nanos(self.virtual_ns as u64)
    }
}

fn write_filtered(out: &mut impl Write, s: &mut Sanitizer, buf: &mut Vec<u8>, data: &[u8], raw: bool) -> Result<()> {
    if raw {
        out.write_all(data)?;
    } else {
        buf.clear();
        s.filter(data, buf);
        out.write_all(buf)?;
    }
    out.flush()?;
    Ok(())
}

fn sleep_until(deadline: Instant, flag: &AtomicBool) {
    let step = Duration::from_millis(50);
    loop {
        let now = Instant::now();
        if now >= deadline || flag.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep((deadline - now).min(step));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_schedules_against_a_fixed_start_with_idle_cap_and_speed() {
        let mut c = Clock::new(2.0, Some(1_000_000_000));
        let t0 = c.deadline(5_000_000_000);
        let t1 = c.deadline(6_000_000_000);
        let t2 = c.deadline(20_000_000_000);
        assert_eq!(t1 - t0, Duration::from_millis(500));
        assert_eq!(t2 - t1, Duration::from_millis(500));
        let t3 = c.deadline(19_000_000_000);
        assert_eq!(t3, t2);
    }
}
