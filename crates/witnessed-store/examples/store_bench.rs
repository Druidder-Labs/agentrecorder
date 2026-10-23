use std::time::Instant;

use witnessed_store::{Body, NewEvent, NewSession, Store};

fn main() -> anyhow::Result<()> {
    let n: usize = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(200_000);
    let size: usize = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(1024);
    let batch: usize = 512;
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(&dir.path().join("bench.db"))?;
    let cmd = vec!["bench".to_string()];
    store.create_session(&NewSession {
        id: "bench1",
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
        agent_session_id: None,
        anchor_token_digest: None,
    })?;
    let payload = vec![b'x'; size];
    let events: Vec<NewEvent> = (0..batch)
        .map(|i| NewEvent {
            ts_ns: i as i64,
            mono_ns: i as i64,
            body: Body::TerminalOutput,
            data: Some(payload.clone()),
        })
        .collect();
    let start = Instant::now();
    let mut seq = 0;
    let mut written = 0;
    while written < n {
        let take = batch.min(n - written);
        seq = store.append_events("bench1", seq, &events[..take])?;
        written += take;
    }
    let write = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let mut read_bytes = 0usize;
    store.for_each_event("bench1", None, |e| {
        read_bytes += e.data.map_or(0, |d| d.len());
        Ok(())
    })?;
    let read = start.elapsed().as_secs_f64();
    let mb = (n * size) as f64 / (1 << 20) as f64;
    println!(
        "{{\"events\": {n}, \"event_bytes\": {size}, \"batch\": {batch}, \"write_s\": {write:.3}, \"write_events_per_s\": {:.0}, \"write_mib_per_s\": {:.1}, \"read_s\": {read:.3}, \"read_mib_per_s\": {:.1}}}",
        n as f64 / write,
        mb / write,
        (read_bytes as f64 / (1 << 20) as f64) / read
    );
    Ok(())
}
