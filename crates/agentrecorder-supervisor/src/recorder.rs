use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agentrecorder_store::{Body, NewEvent, Notice, Store, now_unix_ns};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};

pub const QUEUE_CAPACITY: usize = 4096;
pub const QUEUE_BYTES: u64 = 16 << 20;
const BATCH_BYTES: usize = 4 << 20;
const BATCH_MAX: usize = 512;
const BATCH_WINDOW: Duration = Duration::from_millis(20);
const COALESCE_WINDOW_NS: i64 = 5_000_000;
const COALESCE_MAX_BYTES: usize = 64 << 10;
const LIFECYCLE_HEADROOM: usize = 512;
const STORE_RETRY_FOR: Duration = Duration::from_secs(10);
const EXIT_RETRY_FOR: Duration = Duration::from_secs(60);
const STORE_RETRY_STEP: Duration = Duration::from_millis(100);

pub enum Meta {
    Started { child_pid: i64, at_ns: i64 },
    Attached(bool),
    Size(u16, u16),
    Exited { at_ns: i64, exit_code: Option<i32>, signal: Option<i32> },
}

enum Msg {
    Event(NewEvent),
    Meta(Meta),
    Flush(Sender<()>),
    Shutdown,
}

#[derive(Default)]
pub struct Counters {
    pub recorded_events: AtomicU64,
    pub dropped_events: AtomicU64,
    pub dropped_bytes: AtomicU64,
    pub output_disabled: AtomicBool,
    pending_dropped_events: AtomicU64,
    pending_dropped_bytes: AtomicU64,
    queued_bytes: AtomicU64,
}

#[derive(Clone)]
pub struct Recorder {
    tx: Sender<Msg>,
    start: Instant,
    pub counters: Arc<Counters>,
    record_output: bool,
    record_input: bool,
}

pub struct RecorderThread {
    handle: JoinHandle<()>,
}

pub struct Options {
    pub record_output: bool,
    pub record_input: bool,
    pub max_record_bytes: u64,
}

impl Recorder {
    pub fn spawn(
        store: Option<Store>,
        session_id: String,
        start: Instant,
        opts: Options,
    ) -> (Recorder, RecorderThread) {
        let (tx, rx) = bounded(QUEUE_CAPACITY);
        let counters = Arc::new(Counters::default());
        let worker = Worker {
            store,
            session_id,
            next_seq: 0,
            rx,
            counters: counters.clone(),
            start,
            max_record_bytes: opts.max_record_bytes,
            recorded_bytes: 0,
            terminated: false,
        };
        let handle = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || worker.run())
            .expect("spawning recorder thread");
        let rec = Recorder { tx, start, counters, record_output: opts.record_output, record_input: opts.record_input };
        (rec, RecorderThread { handle })
    }

    pub fn mono_ns(&self) -> i64 {
        self.start.elapsed().as_nanos() as i64
    }

    fn new_event(&self, body: Body, data: Option<Vec<u8>>) -> NewEvent {
        NewEvent { ts_ns: now_unix_ns(), mono_ns: self.mono_ns(), body, data }
    }

    pub fn output(&self, data: &[u8]) {
        if !self.record_output || self.counters.output_disabled.load(Ordering::Relaxed) {
            return;
        }
        let ev = self.new_event(Body::TerminalOutput, Some(data.to_vec()));
        self.send_bulk(ev, data.len());
    }

    pub fn input(&self, data: &[u8]) {
        if !self.record_input {
            return;
        }
        let ev = self.new_event(Body::TerminalInput, Some(data.to_vec()));
        self.send_bulk(ev, data.len());
    }

    fn send_bulk(&self, ev: NewEvent, bytes: usize) {
        let queued = &self.counters.queued_bytes;
        if self.tx.len() >= QUEUE_CAPACITY - LIFECYCLE_HEADROOM
            || queued.load(Ordering::Relaxed) + bytes as u64 > QUEUE_BYTES
        {
            self.count_drop(bytes);
            return;
        }
        queued.fetch_add(bytes as u64, Ordering::Relaxed);
        if self.tx.try_send(Msg::Event(ev)).is_err() {
            queued.fetch_sub(bytes as u64, Ordering::Relaxed);
            self.count_drop(bytes);
        }
    }

    pub fn event(&self, body: Body) {
        let ev = self.new_event(body, None);
        if self.tx.try_send(Msg::Event(ev)).is_err() {
            self.count_drop(0);
        }
    }

    pub fn event_with_data(&self, body: Body, data: Vec<u8>) {
        let ev = self.new_event(body, Some(data));
        if self.tx.try_send(Msg::Event(ev)).is_err() {
            self.count_drop(0);
        }
    }

    pub fn records_output(&self) -> bool {
        self.record_output && !self.counters.output_disabled.load(Ordering::Relaxed)
    }

    pub fn meta(&self, m: Meta) {
        if self.tx.try_send(Msg::Meta(m)).is_err() {
            self.count_drop(0);
        }
    }

    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack_tx, ack_rx) = bounded(1);
        self.tx.send_timeout(Msg::Flush(ack_tx), timeout).is_ok() && ack_rx.recv_timeout(timeout).is_ok()
    }

    fn count_drop(&self, bytes: usize) {
        let c = &self.counters;
        c.dropped_events.fetch_add(1, Ordering::Relaxed);
        c.dropped_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        c.pending_dropped_events.fetch_add(1, Ordering::Relaxed);
        c.pending_dropped_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

impl RecorderThread {
    pub fn finish(self, recorder: Recorder) {
        let _ = recorder.tx.send_timeout(Msg::Shutdown, Duration::from_secs(5));
        drop(recorder);
        let _ = self.handle.join();
    }
}

struct Worker {
    store: Option<Store>,
    session_id: String,
    next_seq: i64,
    rx: Receiver<Msg>,
    counters: Arc<Counters>,
    start: Instant,
    max_record_bytes: u64,
    recorded_bytes: u64,
    terminated: bool,
}

impl Worker {
    fn run(mut self) {
        let mut batch: Vec<NewEvent> = Vec::with_capacity(BATCH_MAX);
        while let Ok(first) = self.rx.recv() {
            let deadline = Instant::now() + BATCH_WINDOW;
            let mut next = Some(first);
            let mut disconnected = false;
            while let Some(msg) = next.take() {
                match msg {
                    Msg::Event(ev) => {
                        if matches!(ev.body, Body::TerminalOutput | Body::TerminalInput) {
                            let len = ev.data.as_ref().map_or(0, Vec::len) as u64;
                            self.counters.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                        }
                        self.push(&mut batch, ev);
                    }
                    Msg::Meta(m) => {
                        self.flush(&mut batch);
                        self.apply_meta(m);
                    }
                    Msg::Flush(ack) => {
                        self.report_drops(&mut batch);
                        self.flush(&mut batch);
                        let _ = ack.send(());
                    }
                    Msg::Shutdown => {
                        disconnected = true;
                        break;
                    }
                }
                if batch.len() >= BATCH_MAX
                    || batch.iter().map(|e| e.data.as_ref().map_or(0, Vec::len)).sum::<usize>() >= BATCH_BYTES
                {
                    break;
                }
                match self.rx.recv_deadline(deadline) {
                    Ok(m) => next = Some(m),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => disconnected = true,
                }
            }
            self.report_drops(&mut batch);
            self.flush(&mut batch);
            if disconnected {
                break;
            }
        }
        self.report_drops(&mut batch);
        self.flush(&mut batch);
    }

    fn push(&mut self, batch: &mut Vec<NewEvent>, ev: NewEvent) {
        if self.terminated {
            return;
        }
        if matches!(ev.body, Body::SessionTerminated { .. }) {
            self.report_drops(batch);
            self.terminated = true;
        }
        if ev.body == Body::TerminalOutput {
            let len = ev.data.as_ref().map_or(0, Vec::len) as u64;
            if self.recorded_bytes + len > self.max_record_bytes {
                if !self.counters.output_disabled.swap(true, Ordering::Relaxed) {
                    batch.push(
                        self.warning(
                            Notice::new(
                                "recording_limit_reached",
                                "terminal output recording stopped at the configured byte limit",
                            )
                            .with("limit_bytes", self.max_record_bytes),
                        ),
                    );
                }
                return;
            }
            self.recorded_bytes += len;
            if let Some(last) = batch.last_mut()
                && last.body == Body::TerminalOutput
                && ev.mono_ns - last.mono_ns < COALESCE_WINDOW_NS
            {
                let last_data = last.data.as_mut().expect("output events carry data");
                let new_data = ev.data.as_ref().expect("output events carry data");
                if last_data.len() + new_data.len() <= COALESCE_MAX_BYTES {
                    last_data.extend_from_slice(new_data);
                    return;
                }
            }
        }
        batch.push(ev);
    }

    fn warning(&self, notice: Notice) -> NewEvent {
        NewEvent {
            ts_ns: now_unix_ns(),
            mono_ns: self.start.elapsed().as_nanos() as i64,
            body: Body::RuntimeWarning(notice),
            data: None,
        }
    }

    fn report_drops(&mut self, batch: &mut Vec<NewEvent>) {
        if self.terminated {
            return;
        }
        let events = self.counters.pending_dropped_events.swap(0, Ordering::Relaxed);
        let bytes = self.counters.pending_dropped_bytes.swap(0, Ordering::Relaxed);
        if events > 0 {
            batch.push(
                self.warning(
                    Notice::new(
                        "events_dropped",
                        "event queue was full; events were dropped instead of blocking the process",
                    )
                    .with("dropped_events", events)
                    .with("dropped_bytes", bytes),
                ),
            );
        }
    }

    fn flush(&mut self, batch: &mut Vec<NewEvent>) {
        if batch.is_empty() {
            return;
        }
        let n = batch.len() as u64;
        let (id, seq) = (self.session_id.clone(), self.next_seq);
        if let Some(store) = self.store.as_mut() {
            match with_retry(STORE_RETRY_FOR, || store.append_events(&id, seq, batch)) {
                Ok(next) => {
                    self.next_seq = next;
                    self.counters.recorded_events.fetch_add(n, Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("agentrecorder supervisor: failed to persist {n} events: {e:#}");
                    self.counters.dropped_events.fetch_add(n, Ordering::Relaxed);
                }
            }
        }
        batch.clear();
    }

    fn apply_meta(&mut self, m: Meta) {
        let Some(store) = self.store.as_ref() else { return };
        let id = self.session_id.as_str();
        let window = if matches!(m, Meta::Exited { .. }) { EXIT_RETRY_FOR } else { STORE_RETRY_FOR };
        let res = with_retry(window, || match m {
            Meta::Started { child_pid, at_ns } => store.mark_started(id, child_pid, at_ns),
            Meta::Attached(a) => store.set_attached(id, a),
            Meta::Size(r, c) => store.set_size(id, r, c),
            Meta::Exited { at_ns, exit_code, signal } => store.mark_exited(id, at_ns, exit_code, signal),
        });
        if let Err(e) = res {
            self.counters.dropped_events.fetch_add(1, Ordering::Relaxed);
            eprintln!("agentrecorder supervisor: failed to update session metadata: {e:#}");
        }
    }
}

fn with_retry<T>(window: Duration, mut f: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let deadline = Instant::now() + window;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if !agentrecorder_store::is_transient(&e) || Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(STORE_RETRY_STEP),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentrecorder_store::{NewSession, kind};

    fn open(dir: &std::path::Path) -> Store {
        let store = Store::open(&dir.join("s.db")).unwrap();
        let cmd = vec!["x".to_string()];
        store
            .create_session(&NewSession {
                id: "abc123",
                command: &cmd,
                cwd: None,
                created_at_ns: 0,
                supervisor_pid: 0,
                rows: 24,
                cols: 80,
                record_output: true,
                record_input: false,
                boot_time: None,
                restarted_from: None,
            })
            .unwrap();
        store
    }

    fn collect(dir: &std::path::Path) -> Vec<agentrecorder_store::Event> {
        let store = Store::open(&dir.join("s.db")).unwrap();
        let mut out = vec![];
        store
            .for_each_event("abc123", None, |e| {
                out.push(e);
                Ok(())
            })
            .unwrap();
        out
    }

    #[test]
    fn coalesces_output_and_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let (rec, th) = Recorder::spawn(
            Some(open(dir.path())),
            "abc123".into(),
            Instant::now(),
            Options { record_output: true, record_input: false, max_record_bytes: u64::MAX },
        );
        rec.event(Body::ProcessStarted { pid: 1, command: vec![] });
        for i in 0..100u8 {
            rec.output(&[i]);
        }
        rec.input(b"secret");
        th.finish(rec);
        let events = collect(dir.path());
        assert_eq!(events[0].kind, kind::PROCESS_STARTED);
        let bytes: Vec<u8> =
            events.iter().filter(|e| e.kind == kind::TERMINAL_OUTPUT).flat_map(|e| e.data.clone().unwrap()).collect();
        assert_eq!(bytes, (0..100u8).collect::<Vec<_>>());
        assert!(events.iter().all(|e| e.kind != kind::TERMINAL_INPUT));
        assert!(events.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
    }

    #[test]
    fn stops_recording_at_byte_limit_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (rec, th) = Recorder::spawn(
            Some(open(dir.path())),
            "abc123".into(),
            Instant::now(),
            Options { record_output: true, record_input: false, max_record_bytes: 10 },
        );
        rec.output(b"12345");
        rec.output(b"67890");
        rec.output(b"overflow");
        th.finish(rec);
        let events = collect(dir.path());
        let recorded: usize = events.iter().filter_map(|e| e.data.as_ref()).map(Vec::len).sum();
        assert_eq!(recorded, 10);
        assert!(events.iter().any(|e| e.payload["code"] == "recording_limit_reached"));
    }

    #[test]
    fn lifecycle_events_survive_a_stalled_store_and_terminated_stays_last() {
        use agentrecorder_store::TerminationReason;
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let blocker = rusqlite::Connection::open(dir.path().join("s.db")).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let (rec, th) = Recorder::spawn(
            Some(store),
            "abc123".into(),
            Instant::now(),
            Options { record_output: true, record_input: false, max_record_bytes: u64::MAX },
        );
        for _ in 0..600 {
            rec.output(&[b'x'; 64 << 10]);
        }
        rec.event(Body::ProcessExited { pid: 1, exit_code: Some(0), signal: None, duration_ms: 1 });
        rec.event(Body::SessionTerminated {
            reason: TerminationReason::ProcessExited,
            exit_code: Some(0),
            signal: None,
            dropped_events: 0,
        });
        let dropped = rec.counters.dropped_events.load(Ordering::Relaxed);
        assert!(dropped > 0);
        std::thread::sleep(Duration::from_millis(300));
        blocker.execute_batch("COMMIT").unwrap();
        drop(blocker);
        th.finish(rec);
        let events = collect(dir.path());
        assert!(events.iter().any(|e| e.kind == kind::PROCESS_EXITED));
        assert_eq!(events.last().unwrap().kind, kind::SESSION_TERMINATED);
        assert!(events.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
        let reported: u64 = events
            .iter()
            .filter(|e| e.payload["code"] == "events_dropped")
            .map(|e| e.payload["dropped_events"].as_u64().unwrap())
            .sum();
        assert_eq!(reported, dropped);
    }

    #[test]
    fn events_with_data_that_are_not_output_do_not_disturb_the_byte_budget() {
        use agentrecorder_store::SnapshotReason;
        let dir = tempfile::tempdir().unwrap();
        let (rec, th) = Recorder::spawn(
            Some(open(dir.path())),
            "abc123".into(),
            Instant::now(),
            Options { record_output: true, record_input: false, max_record_bytes: u64::MAX },
        );
        rec.event_with_data(
            Body::TerminalSnapshot { rows: 24, cols: 80, reason: SnapshotReason::Interval },
            vec![b's'; 4096],
        );
        rec.flush(Duration::from_secs(5));
        for _ in 0..64 {
            rec.output(&[b'x'; 1024]);
            rec.flush(Duration::from_secs(5));
        }
        assert_eq!(rec.counters.dropped_events.load(Ordering::Relaxed), 0);
        th.finish(rec);
    }

    #[test]
    fn full_queue_drops_output_without_blocking_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let (rec, th) = Recorder::spawn(
            Some(store),
            "abc123".into(),
            Instant::now(),
            Options { record_output: true, record_input: false, max_record_bytes: u64::MAX },
        );
        let t = Instant::now();
        for _ in 0..(QUEUE_CAPACITY * 20) {
            rec.output(&[b'x'; 1024]);
        }
        assert!(t.elapsed() < Duration::from_secs(5));
        let dropped = rec.counters.dropped_events.load(Ordering::Relaxed);
        th.finish(rec);
        if dropped > 0 {
            let events = collect(dir.path());
            let reported: u64 = events
                .iter()
                .filter(|e| e.payload["code"] == "events_dropped")
                .map(|e| e.payload["dropped_events"].as_u64().unwrap())
                .sum();
            assert_eq!(reported, dropped);
        }
    }
}
