use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use witnessed_store::{Body, Notice};

use super::shared::Shared;
use crate::pty;

const MEMBER_POLL: Duration = Duration::from_millis(50);

impl Shared {
    fn signal_all(&self, sig: Signal) {
        if self.exited.load(Ordering::Acquire) {
            return;
        }
        for pid in pty::session_members(self.child_pid) {
            if pid != self.child_pid {
                let _ = kill(Pid::from_raw(pid), sig);
            }
        }
        if self.reaped.load(Ordering::Acquire) {
            return;
        }
        if let Some(pg) = pty::foreground_pgrp(&self.master)
            && pg != self.child_pid
        {
            let _ = killpg(Pid::from_raw(pg), sig);
        }
        let _ = killpg(Pid::from_raw(self.child_pid), sig);
    }

    pub(super) fn stop(self: &Arc<Self>, signal: Signal, grace: Duration, origin: &str) {
        if self.exited.load(Ordering::Acquire) {
            return;
        }
        let first = {
            let mut stop = self.stop.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let first = stop.is_none();
            if first {
                *stop = Some(Instant::now() + grace);
            }
            first
        };
        if let Some(c) = &self.term().client {
            c.backpressure.store(false, Ordering::Release);
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
                if shared.exited.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if !shared.exited.load(Ordering::Acquire) {
                shared.recorder.event(escalation_warning());
                shared.signal_all(Signal::SIGKILL);
            }
        });
    }

    pub(super) fn reap_session_after_stop(&self) {
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
}

fn escalation_warning() -> Body {
    Body::RuntimeWarning(
        Notice::new("stop_escalated", "processes did not exit within the grace period; sent SIGKILL")
            .with("signal", Signal::SIGKILL as i32),
    )
}

pub(super) fn install_signal_handlers(shared: &Arc<Shared>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP, SIGUSR1])?;
    let shared = shared.clone();
    std::thread::Builder::new().name("signals".into()).spawn(move || {
        for sig in signals.forever() {
            if sig == SIGUSR1 {
                if let Err(e) = shared.bind() {
                    eprintln!("witnessed supervisor: recreating the socket failed: {e:#}");
                }
                continue;
            }
            shared.stop(Signal::SIGHUP, Duration::from_secs(3), &format!("supervisor_signal_{sig}"));
        }
    })?;
    Ok(())
}
