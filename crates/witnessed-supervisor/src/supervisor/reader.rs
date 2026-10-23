use std::fs::File;
use std::io::{ErrorKind, Read};
use std::sync::atomic::Ordering;

use witnessed_store::{Body, DetachReason, Notice, SnapshotReason};

use super::delivery::{deliver, offer_interactive};
use super::shared::{Shared, Term};
use crate::modes::Observed;
use crate::protocol::Frame;

const SNAPSHOT_EVERY_BYTES: u64 = 8 << 20;
const MAX_HELD_OUTPUT: usize = 4 << 20;

pub(super) fn read_pty(shared: &Shared, mut master: File) {
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let data = &buf[..n];
        shared.output_bytes.fetch_add(n as u64, Ordering::Relaxed);
        shared.last_output_ms.store(shared.uptime_ms(), Ordering::Relaxed);
        shared.recorder.output(data);
        let (target, notes) = {
            let mut term = shared.term();
            let notes = term.screen.process(data);
            hold(shared, &mut term, data);
            maybe_snapshot(shared, &mut term, n as u64);
            let target = match term.client.as_ref() {
                Some(c) if c.backpressure.load(Ordering::Acquire) => Some((c.id, c.tx.clone(), c.backpressure.clone())),
                Some(c) => {
                    offer_interactive(shared, &term, c, data);
                    None
                }
                None => None,
            };
            (target, notes)
        };
        for signal in notes {
            match signal {
                Observed::Notification(source, text) => shared.note(source, text),
                Observed::CommandFinished { exit_code } => {
                    shared.recorder.best_effort(Body::ShellCommandFinished { exit_code });
                }
            }
        }
        let Some((id, tx, backpressure)) = target else { continue };
        let delivered = deliver(&tx, Frame::Output(data.to_vec()), &backpressure);
        drop(tx);
        if !delivered {
            let mut term = shared.term();
            if term.client.as_ref().map(|c| c.id) == Some(id) {
                shared.detach_client(&mut term, DetachReason::ClientTooSlow);
            }
        }
    }
}

fn hold(shared: &Shared, term: &mut Term, data: &[u8]) {
    let Some(held) = term.held.as_mut() else { return };
    if held.len() + data.len() <= MAX_HELD_OUTPUT {
        held.extend_from_slice(data);
        return;
    }
    term.held = None;
    shared.recorder.event(Body::RuntimeWarning(
        Notice::new(
            "pre_attach_output_dropped",
            "more output arrived before the first client attached than witnessed holds for it; the client got a screen snapshot instead",
        )
        .with("limit_bytes", MAX_HELD_OUTPUT),
    ));
}

fn maybe_snapshot(shared: &Shared, term: &mut Term, bytes: u64) {
    if !shared.recorder.records_output() {
        return;
    }
    term.bytes_since_snapshot += bytes;
    let drops = shared.recorder.counters.dropped_events.load(Ordering::Relaxed);
    let reason = if drops > term.drops_at_snapshot {
        SnapshotReason::AfterDrop
    } else if term.bytes_since_snapshot >= SNAPSHOT_EVERY_BYTES {
        SnapshotReason::Interval
    } else {
        return;
    };
    let (rows, cols) = term.screen.size();
    let state = term.screen.state().into_bytes();
    shared.recorder.event_with_data(Body::TerminalSnapshot { rows, cols, reason }, state);
    term.bytes_since_snapshot = 0;
    term.drops_at_snapshot = drops;
}
