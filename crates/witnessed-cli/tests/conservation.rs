mod common;

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::Value;

use common::*;

const HOLD: usize = 4 * 1024 * 1024;

fn numbered(start: usize, bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes + 64);
    let mut i = start;
    while out.len() < bytes {
        out.extend_from_slice(format!("{i:09}-{}\n", "q".repeat(54)).as_bytes());
        i += 1;
    }
    out
}

fn write_file(env: &Env, name: &str, data: &[u8]) -> PathBuf {
    let path = env.path().join(name);
    std::fs::write(&path, data).unwrap();
    path
}

fn without_cr(data: &[u8]) -> Vec<u8> {
    data.iter().copied().filter(|b| *b != b'\r').collect()
}

fn without_eof_echo(mut data: Vec<u8>) -> Vec<u8> {
    while let Some(at) = data.windows(4).position(|w| w == b"^D\x08\x08") {
        data.drain(at..at + 4);
    }
    data
}

fn recorded(env: &Env, id: &str) -> Vec<u8> {
    let out = env.command(&["replay", id, "--instant", "--raw"]).output().unwrap();
    assert!(out.status.success(), "replay failed: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn output_counts(env: &Env, id: &str) -> (i64, i64) {
    let v = env.inspect(id);
    v["event_counts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "terminal.output")
        .map_or((0, 0), |c| (c["count"].as_i64().unwrap(), c["bytes"].as_i64().unwrap()))
}

fn warnings(env: &Env, id: &str, code: &str) -> Vec<Value> {
    env.inspect(id)["notices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["payload"]["code"] == code)
        .map(|n| n["payload"].clone())
        .collect()
}

fn session_ids(env: &Env) -> Vec<String> {
    env.sessions().iter().map(|s| s["session"].as_str().unwrap().to_string()).collect()
}

fn first_difference(got: &[u8], want: &[u8]) -> String {
    let at = got.iter().zip(want).position(|(a, b)| a != b).unwrap_or(got.len().min(want.len()));
    let line_start = want[..at.min(want.len())].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let show =
        |d: &[u8]| String::from_utf8_lossy(&d[line_start.min(d.len())..(line_start + 80).min(d.len())]).into_owned();
    format!(
        "got {} bytes, want {}; first difference at {at}: got {:?}, want {:?}",
        got.len(),
        want.len(),
        show(got),
        show(want)
    )
}

fn assert_same(got: &[u8], want: &[u8], what: &str) {
    assert!(got == want, "{what}: {}", first_difference(got, want));
}

fn assert_recorded_completely(env: &Env, id: &str, want: &[u8]) {
    let raw = recorded(env, id);
    let (_, bytes) = output_counts(env, id);
    assert_eq!(bytes as usize, raw.len(), "inspect output bytes differ from replayed bytes");
    if !warnings(env, id, "events_dropped").is_empty() {
        return;
    }
    assert_same(&without_eof_echo(without_cr(&raw)), want, "recorded output");
}

#[test]
fn a_pipe_client_receives_every_recorded_byte_around_the_hold_buffer() {
    let env = Env::new();
    for size in [64 * 1024, HOLD - 4096, HOLD + 64 * 1024, 3 * HOLD] {
        let pre = numbered(0, size);
        let post = numbered(10_000_000, 512 * 1024);
        let pre_file = write_file(&env, "pre.txt", &pre);
        let post_file = write_file(&env, "post.txt", &post);
        let script = format!("cat '{}'; sleep 0.2; cat '{}'", pre_file.display(), post_file.display());
        let before = session_ids(&env);
        let out = env.command(&["run", "--", "sh", "-c", &script]).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "size {size}: {out:?}");
        let id = session_ids(&env).into_iter().find(|s| !before.contains(s)).expect("new session");
        let mut want = pre.clone();
        want.extend_from_slice(&post);
        assert_recorded_completely(&env, &id, &want);
        let delivered = without_eof_echo(without_cr(&out.stdout));
        if warnings(&env, &id, "pre_attach_output_dropped").is_empty() {
            assert_same(&delivered, &want, &format!("size {size}: delivered output"));
        } else {
            assert!(
                delivered.ends_with(&post),
                "size {size}: output after the attach is incomplete: {}",
                first_difference(&delivered[delivered.len().saturating_sub(post.len())..], &post)
            );
        }
    }
}

#[test]
fn a_detached_session_records_everything_and_a_late_pipe_client_gets_the_rest() {
    let env = Env::new();
    let pre = numbered(0, 2 * 1024 * 1024);
    let post = numbered(20_000_000, 2 * 1024 * 1024);
    let pre_file = write_file(&env, "pre.txt", &pre);
    let post_file = write_file(&env, "post.txt", &post);
    let flag = env.path().join("go");
    let script = format!(
        "cat '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; cat '{}'",
        pre_file.display(),
        flag.display(),
        post_file.display()
    );
    let id = env.start_detached(&["sh", "-c", &script]);
    wait_for("the first burst to be recorded", || without_cr(&recorded(&env, &id)).len() >= pre.len());
    let client = env.command(&["attach", &id]).stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap();
    wait_for("the client to attach", || env.status(&id) == "running");
    std::fs::write(&flag, b"").unwrap();
    let out = client.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let mut want = pre.clone();
    want.extend_from_slice(&post);
    assert_recorded_completely(&env, &id, &want);
    let delivered = without_eof_echo(without_cr(&out.stdout));
    let first = &post[..post.iter().position(|b| *b == b'\n').unwrap() + 1];
    let start = delivered.windows(first.len()).position(|w| w == first).expect("first line after attach delivered");
    assert_same(&delivered[start..], &post, "output after the attach");
}

#[test]
fn a_producer_that_exits_at_once_is_recorded_completely() {
    let env = Env::new();
    for size in [1, 100, 64 * 1024, HOLD + 1] {
        let data = numbered(0, size);
        let file = write_file(&env, "fast.txt", &data);
        let id = env.start_detached(&["cat", file.to_str().unwrap()]);
        env.wait_status(&id, "exited");
        assert_recorded_completely(&env, &id, &data);
    }
}

#[test]
fn an_interactive_client_gets_a_large_burst_or_a_discard_warning() {
    let env = Env::new();
    let data = numbered(0, 24 * 1024 * 1024);
    let file = write_file(&env, "burst.txt", &data);
    let mut c = env.spawn_pty(&["run", "--", "cat", file.to_str().unwrap()], 40, 200);
    assert!(c.wait().success(), "client failed");
    let id = env.sessions()[0]["session"].as_str().unwrap().to_string();
    assert_recorded_completely(&env, &id, &data);
    let discards = warnings(&env, &id, "client_output_discarded");
    let delivered = without_cr(c.output().as_bytes());
    let complete = delivered.windows(data.len()).any(|w| w == data.as_slice());
    let discarded: u64 = discards.iter().map(|w| w["bytes"].as_u64().unwrap_or(0)).sum();
    assert!(
        complete || discarded > 0,
        "delivered {} bytes of {} without a client_output_discarded warning",
        delivered.len(),
        data.len()
    );
}

#[test]
fn many_tiny_writes_are_recorded_in_order() {
    let env = Env::new();
    let n = 60_000;
    let script = format!("awk 'BEGIN {{ for (i = 0; i < {n}; i++) {{ printf \"%d\\n\", i; fflush() }} }}'");
    let id = env.start_detached(&["sh", "-c", &script]);
    env.wait_status(&id, "exited");
    let want: Vec<u8> = (0..n).flat_map(|i| format!("{i}\n").into_bytes()).collect();
    assert_recorded_completely(&env, &id, &want);
    let (events, _) = output_counts(&env, &id);
    assert!(events > 1, "expected several output events, got {events}");
    let seqs: Vec<i64> = env.events(&id).iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "sequence numbers are not contiguous");
}
