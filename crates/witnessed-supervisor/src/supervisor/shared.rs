use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crossbeam_channel::Sender;
use witnessed_store::{Body, DetachReason, Notice, NotificationSource};

use super::delivery::Client;
use crate::protocol::ExitInfo;
use crate::pty;
use crate::recorder::{Meta, Recorder};
use crate::screen::ScreenModel;

const NOTIFICATION_INTERVAL_MS: u64 = 1000;

pub(super) struct Term {
    pub(super) screen: ScreenModel,
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) client: Option<Client>,
    pub(super) next_client_id: u64,
    pub(super) bytes_since_snapshot: u64,
    pub(super) drops_at_snapshot: u64,
    pub(super) held: Option<Vec<u8>>,
}

#[derive(Default)]
pub(super) struct NotificationLog {
    pub(super) total: u64,
    last_at_ms: [Option<u64>; 4],
}

/// A one-way flag that waiters sleep on instead of polling.
#[derive(Default)]
pub(super) struct Latch {
    set: Mutex<bool>,
    cv: Condvar,
}

impl Latch {
    pub(super) fn set(&self) {
        *self.set.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    pub(super) fn wait_until(&self, deadline: Instant) -> bool {
        let mut set = self.set.lock().unwrap_or_else(PoisonError::into_inner);
        while !*set {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            set = self.cv.wait_timeout(set, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }
}

/// Live connection threads; `run` waits for zero before the process ends.
#[derive(Default)]
pub(super) struct Gauge {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Gauge {
    pub(super) fn inc(&self) {
        *self.count.lock().unwrap_or_else(PoisonError::into_inner) += 1;
    }

    pub(super) fn dec(&self) {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        *count -= 1;
        if *count == 0 {
            self.cv.notify_all();
        }
    }

    pub(super) fn wait_zero_until(&self, deadline: Instant) -> bool {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        while *count > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            count = self.cv.wait_timeout(count, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }
}

pub(super) struct Shared {
    pub(super) session_id: String,
    pub(super) socket_path: PathBuf,
    pub(super) term: Mutex<Term>,
    pub(super) master: File,
    pub(super) child_pid: i32,
    pub(super) recorder: Recorder,
    pub(super) exited: AtomicBool,
    pub(super) recorded: Latch,
    pub(super) connections: Gauge,
    pub(super) reaped: AtomicBool,
    pub(super) exit: Mutex<Option<ExitInfo>>,
    pub(super) output_bytes: AtomicU64,
    pub(super) input_bytes: AtomicU64,
    pub(super) last_output_ms: AtomicU64,
    pub(super) notifications: Mutex<NotificationLog>,
    pub(super) started: Instant,
    pub(super) stop: Mutex<Option<Instant>>,
    pub(super) input: Sender<Vec<u8>>,
}

impl Shared {
    pub(super) fn term(&self) -> MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub(super) fn stop_deadline(&self) -> Option<Instant> {
        *self.stop.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn resize(&self, term: &mut Term, rows: u16, cols: u16) {
        let rows = rows.clamp(1, pty::MAX_ROWS);
        let cols = cols.clamp(1, pty::MAX_COLS);
        if (rows, cols) == (term.rows, term.cols) {
            return;
        }
        if let Err(e) = pty::resize(&self.master, rows, cols) {
            self.recorder.best_effort(Body::RuntimeError(Notice::new("resize_failed", e.to_string())));
            return;
        }
        term.screen.resize(rows, cols);
        term.rows = rows;
        term.cols = cols;
        self.recorder.best_effort(Body::TerminalResized { rows, cols });
        self.recorder.meta(Meta::Size(rows, cols));
    }

    pub(super) fn detach_client(&self, term: &mut Term, reason: DetachReason) {
        if let Some(c) = term.client.take() {
            let id = c.id;
            c.close_detached(reason);
            self.recorder.event(Body::SessionDetached { client: id, reason });
            self.recorder.meta(Meta::Attached(false));
        }
    }

    pub(super) fn note(&self, source: NotificationSource, text: String) {
        let now = self.uptime_ms();
        let slot = source as usize;
        let mut log = self.notifications.lock().unwrap_or_else(PoisonError::into_inner);
        log.total += 1;
        if log.last_at_ms[slot].is_some_and(|t| now.saturating_sub(t) < NOTIFICATION_INTERVAL_MS) {
            return;
        }
        log.last_at_ms[slot] = Some(now);
        drop(log);
        self.recorder.best_effort(Body::TerminalNotification { source, text });
    }
}
