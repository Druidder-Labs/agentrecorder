use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, TrySendError, bounded};
use witnessed_store::{Body, DetachReason, Notice};

use super::shared::{Shared, Term};
use crate::protocol::{Frame, write_frame};

pub(super) const CLIENT_QUEUE: usize = 256;
const CLIENT_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_SEND_SLICE: Duration = Duration::from_millis(100);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_RESYNC_POLL: Duration = Duration::from_millis(100);
const CLIENT_CLOSE_GRACE: Duration = Duration::from_secs(1);
const SNAPSHOT_CHUNK: usize = 64 << 10;

pub(super) struct Client {
    pub(super) id: u64,
    pub(super) tx: Sender<Frame>,
    pub(super) backpressure: Arc<AtomicBool>,
    pub(super) discarding: Arc<AtomicBool>,
    pub(super) discarded: Arc<AtomicU64>,
    pub(super) stalled: Arc<AtomicBool>,
    sock: UnixStream,
    last: Arc<Mutex<Option<Frame>>>,
    abort: Arc<AtomicBool>,
    done: Receiver<()>,
}

impl Client {
    pub(super) fn close(self, last: Frame, drop_pending: bool) -> (Receiver<()>, bool) {
        *self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(last);
        self.abort.store(drop_pending, Ordering::Release);
        drop(self.tx);
        (self.done, self.backpressure.load(Ordering::Acquire))
    }

    pub(super) fn close_detached(self, reason: DetachReason) {
        let sock = self.sock.try_clone();
        let (done, _) = self.close(Frame::Detached { reason }, true);
        if let Ok(sock) = sock {
            std::thread::spawn(move || {
                if done.recv_timeout(CLIENT_CLOSE_GRACE).is_err() {
                    let _ = sock.shutdown(std::net::Shutdown::Both);
                }
            });
        }
    }
}

pub(super) fn spawn_client_writer(shared: &Arc<Shared>, mut stream: UnixStream, backpressure: bool) -> Result<Client> {
    let (tx, rx) = bounded::<Frame>(CLIENT_QUEUE);
    let (done_tx, done) = bounded::<()>(1);
    let last: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let abort = Arc::new(AtomicBool::new(false));
    if !backpressure {
        stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    }
    let sock = stream.try_clone()?;
    let discarding = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(AtomicBool::new(false));
    let (w_last, w_abort, w_stalled, w_discarding) = (last.clone(), abort.clone(), stalled.clone(), discarding.clone());
    let w_shared = shared.clone();
    std::thread::Builder::new().name("client-writer".into()).spawn(move || {
        let mut healthy = true;
        loop {
            let frame = match rx.recv_timeout(CLIENT_RESYNC_POLL) {
                Ok(f) => f,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if w_discarding.load(Ordering::Acquire) {
                        let term = w_shared.term();
                        if let Some(c) = term.client.as_ref().filter(|c| Arc::ptr_eq(&c.discarding, &w_discarding)) {
                            resync(&w_shared, &term, c);
                        }
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
            if w_abort.load(Ordering::Acquire) {
                break;
            }
            if let Err(e) = write_frame(&mut stream, &frame) {
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                    w_stalled.store(true, Ordering::Release);
                }
                healthy = false;
                break;
            }
        }
        if healthy && let Some(frame) = w_last.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            let _ = write_frame(&mut stream, &frame);
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let _ = done_tx.send(());
    })?;
    Ok(Client {
        id: 0,
        tx,
        backpressure: Arc::new(AtomicBool::new(backpressure)),
        discarding,
        discarded: Arc::new(AtomicU64::new(0)),
        stalled,
        sock,
        last,
        abort,
        done,
    })
}

pub(super) fn deliver(tx: &Sender<Frame>, mut frame: Frame, backpressure: &AtomicBool) -> bool {
    let mut deadline = (!backpressure.load(Ordering::Acquire)).then(|| Instant::now() + CLIENT_SEND_TIMEOUT);
    loop {
        match tx.send_timeout(frame, CLIENT_SEND_SLICE) {
            Ok(()) => return true,
            Err(SendTimeoutError::Disconnected(_)) => return false,
            Err(SendTimeoutError::Timeout(f)) => {
                frame = f;
                if deadline.is_none() && !backpressure.load(Ordering::Acquire) {
                    deadline = Some(Instant::now() + CLIENT_SEND_TIMEOUT);
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return false;
                }
            }
        }
    }
}

pub(super) fn send_chunks(tx: &Sender<Frame>, data: &[u8]) {
    for chunk in data.chunks(SNAPSHOT_CHUNK) {
        if tx.try_send(Frame::Output(chunk.to_vec())).is_err() {
            break;
        }
    }
}

pub(super) fn offer_interactive(shared: &Shared, term: &Term, c: &Client, data: &[u8]) {
    if c.discarding.load(Ordering::Acquire) {
        c.discarded.fetch_add(data.len() as u64, Ordering::Relaxed);
        resync(shared, term, c);
        return;
    }
    if let Err(TrySendError::Full(_)) = c.tx.try_send(Frame::Output(data.to_vec())) {
        c.discarding.store(true, Ordering::Release);
        c.discarded.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
}

fn resync(shared: &Shared, term: &Term, c: &Client) {
    if !c.discarding.load(Ordering::Acquire) || c.tx.len() > CLIENT_QUEUE / 2 {
        return;
    }
    let mut snapshot = String::new();
    if !term.screen.alternate_screen() {
        snapshot.push_str("\x1b[?1049l");
    }
    snapshot.push_str("\x1b[0m\x1b[H\x1b[2J");
    snapshot.push_str(&term.screen.state());
    let chunks: Vec<&[u8]> = snapshot.as_bytes().chunks(SNAPSHOT_CHUNK).collect();
    if chunks.len() > CLIENT_QUEUE - c.tx.len() {
        return;
    }
    for chunk in chunks {
        if c.tx.try_send(Frame::Output(chunk.to_vec())).is_err() {
            return;
        }
    }
    c.discarding.store(false, Ordering::Release);
    let bytes = c.discarded.swap(0, Ordering::Relaxed);
    shared.recorder.best_effort(Body::RuntimeWarning(
        Notice::new(
            "client_output_discarded",
            "the attached terminal fell behind; queued output was discarded and the screen was redrawn",
        )
        .with("bytes", bytes)
        .with("client", c.id),
    ));
}
