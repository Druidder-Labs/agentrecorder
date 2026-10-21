mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

const BASH: &[&str] = &["bash", "--norc", "--noprofile"];

fn run_bash(env: &Env) -> (PtyClient, String) {
    let mut args = vec!["run", "--"];
    args.extend_from_slice(BASH);
    let mut c = env.spawn_pty(&args, 24, 80);
    c.expect("started");
    let id = session_id_from(&c.output());
    c.expect("$ ");
    (c, id)
}

#[test]
fn mvp_workflow_run_detach_reattach_inspect_stop_replay() {
    let env = Env::new();
    let marker = env.path().join("still-running");

    let (mut c, id) = run_bash(&env);
    c.send(b"echo hello-$((40+2))\r");
    c.expect("hello-42");
    c.send(format!("sleep 1 && touch {}\r", marker.display()).as_bytes());
    c.send(DETACH);
    c.expect("detached from session");
    assert!(c.wait().success());
    assert_eq!(env.status(&id), "detached");

    wait_for("command to finish while detached", || marker.exists());
    let info = env.inspect(&id);
    let pid = info["session"]["child_pid"].as_i64().unwrap();
    assert!(pid_alive(pid));
    assert_eq!(info["status"], "detached");
    assert!(info["process_tree"]["rss_bytes"].as_u64().unwrap() > 0);

    let mut c2 = env.spawn_pty(&["attach", &id], 24, 80);
    c2.expect("hello-42");
    c2.send(b"echo again-$((1+1))\r");
    c2.expect("again-2");
    assert_eq!(env.status(&id), "running");
    c2.send(DETACH);
    c2.expect("detached from session");
    assert!(c2.wait().success());

    let text = env.ok(&["inspect", &id]);
    assert!(text.contains("EXECUTION PROFILE"));
    assert!(text.contains(&format!("PID:             {pid}")));
    assert!(text.contains("Commands:        unavailable"));

    let stopped = env.ok(&["stop", &id, "--timeout", "2"]);
    assert!(stopped.contains("stopped"), "{stopped}");
    assert_eq!(env.status(&id), "exited");
    assert!(!pid_alive(pid));

    let replay = env.ok(&["replay", &id, "--instant"]);
    assert!(replay.contains("hello-42"));
    assert!(replay.contains("again-2"));

    let kinds: Vec<String> = env.events(&id).iter().map(|e| e["event_type"].as_str().unwrap().to_string()).collect();
    for k in [
        "session.created",
        "process.started",
        "session.attached",
        "session.detached",
        "session.stop_requested",
        "process.exited",
        "session.terminated",
        "terminal.output",
    ] {
        assert!(kinds.iter().any(|x| x == k), "missing {k} in {kinds:?}");
    }
    assert!(kinds.iter().all(|k| k != "terminal.input"));
}

#[test]
fn exit_codes_and_signals_are_recorded() {
    let env = Env::new();
    let mut c = env.spawn_pty(&["run", "--", "sh", "-c", "echo bye; exit 7"], 24, 80);
    c.expect("exited with code 7");
    assert_eq!(c.wait().code(), Some(7));

    let id = env.start_detached(&["sh", "-c", "kill -TERM $$"]);
    env.wait_status(&id, "exited");
    let s = env.sessions().into_iter().find(|s| s["session"] == id).unwrap();
    assert_eq!(s["exit_signal"], 15);
    assert!(s["exit_code"].is_null());
    let sup = env.inspect(&id)["session"]["supervisor_pid"].as_i64().unwrap();
    wait_for("supervisor to exit after its process", || !pid_alive(sup));
}

#[test]
fn missing_command_fails_clearly() {
    let env = Env::new();
    let out = env.run(&["run", "-d", "--", "/nonexistent/agentrecorder-test-binary"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("failed to start"), "{err}");
    let s = &env.sessions()[0];
    assert_eq!(s["status"], "exited");
    assert_eq!(s["exit_code"], 127);
}

#[test]
fn interactive_input_and_resize_reach_the_process() {
    let env = Env::new();
    let (mut c, id) = run_bash(&env);
    c.send(b"stty size\r");
    c.expect("24 80");
    c.resize(50, 132);
    wait_for("resize event", || env.events(&id).iter().any(|e| e["event_type"] == "terminal.resized"));
    c.send(b"stty size\r");
    c.expect("50 132");
    c.send(b"read -r v; echo got-$v\r");
    c.expect("echo got-$v");
    std::thread::sleep(std::time::Duration::from_millis(300));
    c.send(b"typed\r");
    c.expect("got-typed");
    c.send(b"exit 3\r");
    assert_eq!(c.wait().code(), Some(3));
    let resized = env.events(&id).into_iter().find(|e| e["event_type"] == "terminal.resized").unwrap();
    assert_eq!(resized["payload"]["rows"], 50);
    assert_eq!(resized["payload"]["cols"], 132);
}

#[test]
fn large_output_is_recorded_completely_or_loss_is_reported() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "seq 1 300000"]);
    env.wait_status(&id, "exited");
    let events = env.events(&id);
    let dropped = events.iter().any(|e| e["payload"]["code"] == "events_dropped");
    let seqs: Vec<i64> = events.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    let replay = env.ok(&["replay", &id, "--instant", "--raw"]);
    let normalized = if cfg!(target_os = "macos") { replay.replace("\r\r\n", "\r\n") } else { replay };
    let want: String = (1..=300000u32).map(|n| format!("{n}\r\n")).collect();
    assert!(
        normalized == want || dropped,
        "recorded {} of {} bytes without a drop warning",
        normalized.len(),
        want.len()
    );
}

#[test]
fn client_crash_leaves_session_running_and_reattachable() {
    let env = Env::new();
    let (mut c, id) = run_bash(&env);
    c.send(b"echo before-crash\r");
    c.expect("before-crash");
    send_signal(c.child.id() as i32, libc::SIGKILL);
    c.wait();
    env.wait_status(&id, "detached");
    let mut c2 = env.spawn_pty(&["attach", &id], 24, 80);
    c2.expect("before-crash");
    c2.send(b"echo after-crash\r");
    c2.expect("after-crash");
    c2.send(b"exit\r");
    assert!(c2.wait().success());
    let reasons: Vec<String> = env
        .events(&id)
        .iter()
        .filter(|e| e["event_type"] == "session.detached")
        .map(|e| e["payload"]["reason"].as_str().unwrap().to_string())
        .collect();
    assert!(reasons.contains(&"client_disconnected".to_string()), "{reasons:?}");
}

#[test]
fn detach_command_and_second_attach_take_over_clients() {
    let env = Env::new();
    let (mut c, id) = run_bash(&env);
    let detached = env.ok(&["detach", &id]);
    assert!(detached.contains("detached the client"), "{detached}");
    c.expect("detached from session");
    assert!(c.wait().success());

    let mut a = env.spawn_pty(&["attach", &id], 24, 80);
    a.expect("$ ");
    let mut b = env.spawn_pty(&["attach", &id], 24, 80);
    b.expect("$ ");
    a.expect("attached elsewhere");
    assert!(a.wait().success());
    b.send(b"exit\r");
    assert!(b.wait().success());
}

#[test]
fn stop_can_escalate_while_a_gentler_stop_is_pending() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "trap '' HUP TERM; echo armed; sleep 300"]);
    wait_for("trap installed", || env.ok(&["replay", &id, "--instant", "--raw"]).contains("armed"));
    let mut gentle = env.command(&["stop", &id, "--signal", "HUP", "--timeout", "60"]).spawn().unwrap();
    wait_for("stop request recorded", || env.events(&id).iter().any(|e| e["event_type"] == "session.stop_requested"));
    let t = Instant::now();
    let out = env.ok(&["stop", &id, "--signal", "KILL", "--timeout", "1"]);
    assert!(out.contains("signal 9"), "{out}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    let _ = gentle.kill();
    let _ = gentle.wait();
}

#[test]
fn closed_stdin_keeps_the_client_attached_until_exit() {
    let env = Env::new();
    let out =
        env.command(&["run", "--", "sh", "-c", "sleep 1; exit 7"]).stdin(std::process::Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(7), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn detach_works_while_the_process_is_not_reading_input() {
    let env = Env::new();
    let mut c = env.spawn_pty(&["run", "--", "sh", "-c", "stty raw -echo; echo blocked; sleep 30"], 24, 80);
    c.expect("blocked");
    c.send(&vec![b'x'; 256 * 1024]);
    c.send(DETACH);
    c.expect("detached from session");
    assert!(c.wait().success());
}

#[test]
fn stop_reaches_background_jobs_of_an_interactive_shell() {
    let env = Env::new();
    let (mut c, id) = run_bash(&env);
    c.send(b"sleep 1777 & echo bgpid=$!\r");
    c.expect("bgpid=");
    c.expect("\n");
    let out = c.output();
    let tail = &out[out.rfind("bgpid=").unwrap() + 6..];
    let bg: i64 = tail.split(|ch: char| !ch.is_ascii_digit()).next().unwrap().parse().unwrap();
    assert!(pid_alive(bg));
    c.send(DETACH);
    c.wait();
    env.ok(&["stop", &id, "--signal", "TERM", "--timeout", "1"]);
    wait_for("background job to end", || !pid_alive(bg));
}

#[test]
fn reattach_to_a_full_screen_program_restores_and_leaves_the_alternate_screen() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "printf '\\033[?1049hTUI-SCREEN'; sleep 30"]);
    wait_for("tui output", || env.ok(&["replay", &id, "--instant", "--raw"]).contains("TUI-SCREEN"));
    let mut c = env.spawn_pty(&["attach", &id], 24, 80);
    c.expect("\x1b[?1049h");
    c.expect("TUI-SCREEN");
    c.send(DETACH);
    c.expect("\x1b[?1049l");
    assert!(c.wait().success());
}

#[test]
fn stop_escalates_to_sigkill_after_the_grace_period() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "trap '' HUP TERM; echo armed; sleep 300"]);
    wait_for("trap installed", || env.ok(&["replay", &id, "--instant", "--raw"]).contains("armed"));
    let child = env.inspect(&id)["session"]["child_pid"].as_i64().unwrap();
    let t = Instant::now();
    let out = env.ok(&["stop", &id, "--signal", "HUP", "--timeout", "1"]);
    assert!(out.contains("signal 9"), "{out}");
    assert!(t.elapsed() < Duration::from_secs(7), "{:?}", t.elapsed());
    let events = env.events(&id);
    assert!(events.iter().any(|e| e["payload"]["code"] == "stop_escalated" && e["payload"]["signal"] == 9));
    let last = events.last().unwrap();
    assert_eq!(last["event_type"], "session.terminated");
    assert_eq!(last["payload"]["reason"], "stopped");
    assert!(!pid_alive(child));
}

#[test]
fn stop_reaches_jobs_in_other_process_groups() {
    let env = Env::new();
    let id = env.start_detached(&["bash", "-c", "set -m; sleep 1001 & echo bg=$!; sleep 1002 & echo bg=$!; wait"]);
    let mut pids = Vec::new();
    wait_for("background pids", || {
        let out = env.ok(&["replay", &id, "--instant", "--raw"]);
        pids = out
            .split("bg=")
            .skip(1)
            .filter_map(|t| t.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok())
            .collect();
        pids.len() == 2
    });
    assert!(pids.iter().all(|p| pid_alive(*p)));
    env.ok(&["stop", &id, "--timeout", "2"]);
    wait_for("background jobs to end", || pids.iter().all(|p| !pid_alive(*p)));
}

#[test]
fn missing_socket_is_recreated_and_a_frozen_supervisor_is_unreachable_not_lost() {
    let env = Env::new();
    let id = env.start_detached(&["sleep", "300"]);
    let sock = env.path().join("run").join(format!("{id}.sock"));
    std::fs::remove_file(&sock).unwrap();
    assert_eq!(env.status(&id), "detached");
    assert!(sock.exists());

    let sup = env.inspect(&id)["session"]["supervisor_pid"].as_i64().unwrap() as i32;
    std::fs::remove_file(&sock).unwrap();
    send_signal(sup, libc::SIGSTOP);
    assert_eq!(env.status(&id), "unreachable");
    assert!(!env.run(&["rm", &id]).status.success());
    send_signal(sup, libc::SIGCONT);
    let out = env.ok(&["stop", &id, "--timeout", "2"]);
    assert!(out.contains("stopped"), "{out}");
    assert_eq!(env.status(&id), "exited");
}

#[test]
fn piped_client_waits_for_a_slow_reader_and_returns_the_exit_code() {
    let env = Env::new();
    let script = format!(
        "{BIN} run -- sh -c 'yes | head -c 3000000; exit 3' </dev/null | (sleep 2; cat >/dev/null); echo \"rc=${{PIPESTATUS[0]}}\""
    );
    let out = env.command(&[]).get_envs().fold(Command::new("bash"), |mut c, (k, v)| {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
        c
    });
    let mut cmd = out;
    let out = cmd.args(["-c", &script]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rc=3"), "{stdout} {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn end_of_piped_input_reaches_an_interactive_program() {
    let env = Env::new();
    let mut child = env
        .command(&["run", "--", "bash", "--norc", "--noprofile"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"echo hi-$((2+3))\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("client did not exit after its stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi-5"));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn killing_the_client_restores_the_terminal() {
    use nix::sys::termios::{LocalFlags, tcgetattr};
    let env = Env::new();
    let (mut c, _id) = run_bash(&env);
    let raw = tcgetattr(&c.master).unwrap();
    assert!(!raw.local_flags.contains(LocalFlags::ICANON));
    send_signal(c.child.id() as i32, libc::SIGTERM);
    c.wait();
    let restored = tcgetattr(&c.master).unwrap();
    assert!(restored.local_flags.contains(LocalFlags::ICANON | LocalFlags::ECHO));
}

#[test]
fn retention_deletes_only_old_finished_sessions() {
    let env = Env::new();
    let old = env.start_detached(&["true"]);
    env.wait_status(&old, "exited");
    let live = env.start_detached(&["sleep", "30"]);
    let month_ago = agentrecorder_store::now_unix_ns() - 31 * 86_400 * 1_000_000_000;
    let conn = rusqlite::Connection::open(env.db()).unwrap();
    conn.execute(
        "UPDATE sessions SET created_at_ns = ?1, ended_at_ns = ?1 WHERE id = ?2",
        rusqlite::params![month_ago, old],
    )
    .unwrap();
    conn.execute("UPDATE sessions SET created_at_ns = ?1 WHERE id = ?2", rusqlite::params![month_ago, live]).unwrap();
    drop(conn);
    let ids = |env: &Env| env.sessions().iter().map(|s| s["session"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    env.command(&["run", "-d", "--", "true"]).env("AGENTRECORDER_RETENTION_DAYS", "0").output().unwrap();
    assert!(ids(&env).contains(&old));
    env.ok(&["run", "-d", "--", "true"]);
    let after = ids(&env);
    assert!(!after.contains(&old), "{after:?}");
    assert!(after.contains(&live));
    assert!(!env.path().join("logs").join(format!("{old}.log")).exists());
}

#[test]
fn replay_from_skips_the_earlier_timeline() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo early; sleep 2; echo late"]);
    env.wait_status(&id, "exited");
    let t = Instant::now();
    let out = env.ok(&["replay", &id, "--from", "00:00:01.9"]);
    assert!(out.contains("late"));
    assert!(t.elapsed() < Duration::from_millis(1200), "{:?}", t.elapsed());
    let t = Instant::now();
    env.ok(&["replay", &id, "--max-idle", "0.2"]);
    assert!(t.elapsed() < Duration::from_millis(1200), "{:?}", t.elapsed());
}

#[test]
fn declining_the_restart_prompt_starts_nothing() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "exit 2"]);
    env.wait_status(&id, "exited");
    let mut c = env.spawn_pty(&["recover", &id, "--restart"], 24, 80);
    c.expect("Continue? [y/N]");
    c.send(b"n\r");
    c.expect("not restarted");
    assert_eq!(c.wait().code(), Some(1));
    assert_eq!(env.sessions().len(), 1);
}

#[test]
fn a_stalled_interactive_client_is_detached_and_the_process_keeps_running() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    let env = Env::new();
    let id = env.start_detached(&[
        "sh",
        "-c",
        "sleep 1; i=0; while [ $i -lt 100000 ]; do echo line-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done; echo finished; sleep 30",
    ]);
    let mut sock = UnixStream::connect(env.path().join("run").join(format!("{id}.sock"))).unwrap();
    let hello = br#"{"version":1,"mode":"attach","rows":24,"cols":80,"client_pid":1}"#;
    let mut frame = vec![1u8];
    frame.extend_from_slice(&(hello.len() as u32).to_be_bytes());
    frame.extend_from_slice(hello);
    sock.write_all(&frame).unwrap();
    wait_for("process to finish its output", || env.ok(&["replay", &id, "--instant", "--raw"]).contains("finished"));
    let reasons: Vec<String> = env
        .events(&id)
        .iter()
        .filter(|e| e["event_type"] == "session.detached")
        .map(|e| e["payload"]["reason"].as_str().unwrap().to_string())
        .collect();
    assert!(reasons.contains(&"client_too_slow".to_string()), "{reasons:?}");
    assert_eq!(env.status(&id), "detached");
    drop(sock);
}

fn raw_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![kind];
    f.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

fn bash_with_env(env: &Env, script: &str) -> Command {
    let mut cmd = Command::new("bash");
    for (k, v) in env.command(&[]).get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.args(["-c", script]);
    cmd
}

#[test]
fn a_stalled_pipe_client_does_not_freeze_the_session_or_block_stop() {
    let env = Env::new();
    let id = env.start_detached(&[
        "sh",
        "-c",
        "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
    ]);
    let mut pipeline = bash_with_env(&env, &format!("{BIN} attach {id} </dev/null | sleep 60")).spawn().unwrap();
    wait_for("pipe client to attach", || env.status(&id) == "running");
    let bytes = |env: &Env| env.inspect(&id)["supervisor"]["output_bytes"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    env.ok(&["detach", &id]);
    let before = bytes(&env);
    wait_for("output to flow again after detach", || bytes(&env) > before + 100_000);
    let t = Instant::now();
    env.ok(&["stop", &id, "--signal", "KILL", "--timeout", "1"]);
    assert!(t.elapsed() < Duration::from_secs(5));
    let _ = pipeline.kill();
    let _ = pipeline.wait();
}

#[test]
fn the_child_gets_default_signals_the_users_umask_and_no_inherited_fds() {
    use std::os::unix::process::CommandExt;
    let env = Env::new();
    let script =
        "trap -p INT; echo umask=$(umask); if [ -e /dev/fd/7 ]; then echo fd7=open; else echo fd7=closed; fi; stty -a";
    let mut cmd = env.command(&["run", "-d", "--", "bash", "-c", script]);
    let file = std::fs::File::open("/dev/null").unwrap();
    let raw = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: the closure captures a Copy fd number and calls async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::umask(0o022);
            libc::dup2(raw, 7);
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    env.wait_status(&id, "exited");
    let text = env.ok(&["replay", &id, "--instant", "--raw"]);
    assert!(!text.contains("SIGINT"), "{text}");
    assert!(text.contains("umask=0022"), "{text}");
    assert!(text.contains("fd7=closed"), "{text}");
    assert!(text.contains("iutf8") && !text.contains("-iutf8"), "{text}");
}

#[test]
fn reattach_restores_focus_reporting_and_detach_turns_it_off() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "printf '\\033[?1004hfocus-on'; sleep 30"]);
    wait_for("output", || env.ok(&["replay", &id, "--instant", "--raw"]).contains("focus-on"));
    let mut c = env.spawn_pty(&["attach", &id], 24, 80);
    c.expect("\x1b[?1004h");
    c.send(DETACH);
    c.expect("\x1b[?1004l");
    assert!(c.wait().success());
}

#[test]
fn notifications_idle_time_and_git_state_are_measured() {
    let env = Env::new();
    let repo = env.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| Command::new("git").current_dir(&repo).args(args).output().map(|o| o.status.success());
    if git(&["init", "-q"]).ok() != Some(true) {
        return;
    }
    git(&["-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]).unwrap();
    let id = env
        .ok(&[
            "run",
            "-d",
            "--cwd",
            repo.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "printf '\\033]9;build done\\007'; printf '\\007'; touch new-file; sleep 2",
        ])
        .trim()
        .to_string();
    wait_for("notification", || {
        env.inspect(&id)["notifications"]
            .as_array()
            .is_some_and(|n| n.iter().any(|x| x["payload"]["text"] == "build done"))
    });
    let info = env.inspect(&id);
    assert!(info["supervisor"]["output_idle_ms"].as_u64().is_some());
    assert!(info["notifications"].as_array().unwrap().iter().any(|x| x["payload"]["source"] == "bell"));
    env.wait_status(&id, "exited");
    let git_events = env.inspect(&id)["git"].as_array().unwrap().clone();
    let exit = git_events.iter().find(|g| g["payload"]["phase"] == "exit").expect("exit snapshot");
    assert!(exit["payload"]["dirty_paths"].as_u64().unwrap() >= 1);
    assert!(git_events.iter().any(|g| g["payload"]["phase"] == "start" && g["payload"]["head"].is_string()));
}

#[test]
fn large_recordings_carry_screen_snapshots_used_for_seek_and_recover() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "head -c 9000000 /dev/zero | tr '\\000' a; echo; echo tail-marker"]);
    env.wait_status(&id, "exited");
    let events = env.events(&id);
    assert!(events.iter().any(|e| e["event_type"] == "terminal.snapshot" && e["payload"]["reason"] == "interval"));
    assert!(events.iter().all(|e| e["payload"]["code"] != "events_dropped"));
    let raw = env.ok(&["replay", &id, "--instant", "--raw"]);
    let normalized = raw.replace("\r\r\n", "\r\n");
    assert_eq!(normalized.len(), 9_000_000 + "\r\ntail-marker\r\n".len());
    let rec = env.ok(&["recover", &id]);
    assert!(rec.contains("tail-marker"), "{rec}");
    let late = events.last().unwrap()["mono_ns"].as_i64().unwrap() as f64 / 1e9;
    let out = env.ok(&["replay", &id, "--instant", "--from", &format!("{late:.3}")]);
    assert!(out.contains("tail-marker"));
}

#[test]
fn asciicast_v3_export_has_terminal_type_markers_and_exit_status() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo hi; printf '\\033]9;ping\\007'; sleep 0.3; exit 4"]);
    env.wait_status(&id, "exited");
    let out = env.ok(&["export", &id, "--format", "asciicast-v3"]);
    let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["version"], 3);
    assert_eq!(lines[0]["term"]["type"], "xterm-256color");
    assert!(lines[0]["command"].as_str().unwrap().starts_with("sh -c"));
    assert!(lines.iter().any(|l| l[1] == "m" && l[2] == "notification: ping"));
    let last = lines.last().unwrap();
    assert_eq!((last[1].as_str(), last[2].as_str()), (Some("x"), Some("4")));
    assert!(lines[1..].iter().all(|l| l[0].as_f64().unwrap() >= 0.0));
}

#[test]
fn run_is_not_slowed_by_a_concurrent_replay_and_retention() {
    let env = Env::new();
    let done = env.start_detached(&["sh", "-c", "for i in 1 2 3 4 5; do echo $i; sleep 0.2; done"]);
    env.wait_status(&done, "exited");
    let _live = env.start_detached(&["sh", "-c", "while :; do echo live; sleep 0.01; done"]);
    let mut replay = env.command(&["replay", &done, "--speed", "0.05"]).stdout(Stdio::null()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let t = Instant::now();
    let out = env.run(&["run", "-d", "--", "true"]);
    let elapsed = t.elapsed();
    let _ = replay.kill();
    let _ = replay.wait();
    assert!(out.status.success());
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("state.db-wal"));
}

#[test]
fn an_overlong_state_directory_fails_without_leaving_a_session() {
    let env = Env::new();
    let long = env.path().join("x".repeat(110));
    std::fs::create_dir_all(&long).unwrap();
    let out = env.command(&["run", "-d", "--", "true"]).env("AGENTRECORDER_HOME", &long).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("socket path"));
    let list = env.command(&["sessions", "--json"]).env("AGENTRECORDER_HOME", &long).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&list.stdout).trim(), "[]");
}

#[test]
fn a_client_with_another_protocol_version_gets_a_clear_error() {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;
    let env = Env::new();
    let id = env.start_detached(&["sleep", "30"]);
    let mut sock = UnixStream::connect(env.path().join("run").join(format!("{id}.sock"))).unwrap();
    sock.write_all(&raw_frame(1, br#"{"version":99,"mode":"control","rows":0,"cols":0,"client_pid":1}"#)).unwrap();
    let mut reply = Vec::new();
    sock.read_to_end(&mut reply).unwrap();
    let text = String::from_utf8_lossy(&reply);
    assert!(text.contains("protocol version mismatch") && text.contains("speaks 99"), "{text}");
}

#[test]
fn everything_is_persisted_before_the_client_sees_the_exit() {
    let env = Env::new();
    for _ in 0..10 {
        let mut c = env.spawn_pty(&["run", "--record-input", "--", "sh", "-c", "read -r x; echo got-$x"], 24, 80);
        c.expect("started");
        let id = session_id_from(&c.output());
        c.send(b"abc\r");
        c.expect("got-abc");
        c.wait();
        let kinds: Vec<String> =
            env.events(&id).iter().map(|e| e["event_type"].as_str().unwrap().to_string()).collect();
        assert!(kinds.iter().any(|k| k == "terminal.input"), "{kinds:?}");
        assert_eq!(kinds.last().map(String::as_str), Some("session.terminated"), "{kinds:?}");
        assert_eq!(env.status(&id), "exited");
    }
}

#[test]
fn concurrent_sessions_are_independent() {
    let env = Env::new();
    let ids: Vec<String> =
        (0..5).map(|_| env.start_detached(&["sh", "-c", "echo $AGENTRECORDER_SESSION; sleep 30"])).collect();
    let list = env.sessions();
    for id in &ids {
        assert!(list.iter().any(|s| s["session"] == id.as_str() && s["status"] == "detached"));
    }
    env.ok(&["stop", &ids[2], "--signal", "TERM", "--timeout", "2"]);
    assert_eq!(env.status(&ids[2]), "exited");
    assert_eq!(env.status(&ids[3]), "detached");
    for id in &ids {
        wait_for("session id in output", || env.ok(&["replay", id, "--instant", "--raw"]).contains(id.as_str()));
    }
}

fn kill_supervisor(env: &Env, id: &str) -> (i64, i64) {
    let info = env.inspect(id);
    let sup = info["session"]["supervisor_pid"].as_i64().unwrap();
    let child = info["session"]["child_pid"].as_i64().unwrap();
    send_signal(sup as i32, libc::SIGKILL);
    wait_for("supervisor to die", || !pid_alive(sup));
    (sup, child)
}

#[test]
fn supervisor_termination_is_reported_as_lost_with_intact_history() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo recorded-before-loss; sleep 300"]);
    wait_for("output recorded", || env.ok(&["replay", &id, "--instant", "--raw"]).contains("recorded-before-loss"));
    let (_, child) = kill_supervisor(&env, &id);
    assert_eq!(env.status(&id), "lost");
    let rec = env.ok(&["recover", &id]);
    assert!(rec.contains("Scenario C"), "{rec}");
    assert!(rec.contains("supervisor ended without recording an exit"), "{rec}");
    assert!(rec.contains("recorded-before-loss"), "{rec}");
    send_signal(child as i32, libc::SIGKILL);
}

#[test]
fn host_restart_is_detected_from_boot_time() {
    let env = Env::new();
    let id = env.start_detached(&["sleep", "300"]);
    let (_, child) = kill_supervisor(&env, &id);
    send_signal(child as i32, libc::SIGKILL);
    let conn = rusqlite::Connection::open(env.db()).unwrap();
    conn.execute("UPDATE sessions SET boot_time = boot_time - 86400 WHERE id = ?1", [&id]).unwrap();
    drop(conn);
    let rec = env.ok(&["recover", &id]);
    assert!(rec.contains("host restarted"), "{rec}");
    let s = env.sessions().into_iter().find(|s| s["session"] == id.as_str()).unwrap();
    assert_eq!(s["lost_reason"], "host_restarted");
}

#[test]
fn recover_exited_session_shows_history_and_restarts_only_on_request() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo crashed-output; exit 2"]);
    env.wait_status(&id, "exited");
    let rec = env.ok(&["recover", &id]);
    assert!(rec.contains("Scenario B"), "{rec}");
    assert!(rec.contains("crashed-output"), "{rec}");
    assert!(rec.contains("code 2"), "{rec}");
    assert_eq!(env.sessions().len(), 1);

    let refused = env.command(&["recover", &id, "--restart"]).stdin(std::process::Stdio::null()).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("confirmation"));

    let new_id =
        env.ok(&["recover", &id, "--restart", "--yes", "--detached"]).trim().lines().last().unwrap().to_string();
    assert_ne!(new_id, id);
    env.wait_status(&new_id, "exited");
    let info = env.inspect(&new_id);
    assert_eq!(info["session"]["restarted_from"], id.as_str());
}

#[test]
fn missing_session_is_a_clear_error() {
    let env = Env::new();
    env.start_detached(&["true"]);
    for cmd in ["attach", "inspect", "recover", "replay", "stop"] {
        let out = env.run(&[cmd, "ffffff"]);
        assert!(!out.status.success(), "{cmd}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("no session matches"), "{cmd}");
    }
}

#[test]
fn replay_honours_speed_offset_and_filters_dangerous_sequences() {
    let env = Env::new();
    let id = env.start_detached(&[
        "sh",
        "-c",
        "echo first-mark; sleep 1.5; printf 'second-mark\\033]52;c;ZXZpbA==\\007\\033[6n\\n'",
    ]);
    env.wait_status(&id, "exited");

    let t = Instant::now();
    env.ok(&["replay", &id]);
    let normal = t.elapsed();
    let t = Instant::now();
    env.ok(&["replay", &id, "--speed", "5"]);
    let fast = t.elapsed();
    assert!(normal >= Duration::from_millis(1300), "{normal:?}");
    assert!(fast < Duration::from_millis(1000), "{fast:?}");

    let from = env.ok(&["replay", &id, "--from", "1", "--instant"]);
    assert!(from.contains("first-mark") && from.contains("second-mark"), "{from:?}");

    let safe = env.ok(&["replay", &id, "--instant"]);
    assert!(!safe.contains("\x1b]52"), "{safe:?}");
    assert!(!safe.contains("\x1b[6n"), "{safe:?}");
    let raw = env.ok(&["replay", &id, "--instant", "--raw"]);
    assert!(raw.contains("\x1b]52"));
}

#[test]
fn input_is_recorded_only_when_enabled_and_env_is_never_stored() {
    let env = Env::new();
    let mut c = env.spawn_pty(&["run", "--", "sh", "-c", "read -r x; echo len-${#x}"], 24, 80);
    c.expect("started");
    let id = session_id_from(&c.output());
    c.send(b"typed-secret-123\r");
    c.expect("len-16");
    c.wait();
    assert!(env.events(&id).iter().all(|e| e["event_type"] != "terminal.input"));

    let mut c = env.spawn_pty(&["run", "--record-input", "--", "sh", "-c", "read -r x; echo len-${#x}"], 24, 80);
    c.expect("started");
    let id2 = session_id_from(&c.output());
    c.send(b"abc\r");
    c.expect("len-3");
    c.wait();
    assert!(env.events(&id2).iter().any(|e| e["event_type"] == "terminal.input"));

    let id3 = env
        .command(&["run", "-d", "--", "sleep", "30"])
        .env("AGENTRECORDER_TEST_SECRET", "env-secret-value-987")
        .output()
        .unwrap();
    let id3 = String::from_utf8(id3.stdout).unwrap().trim().to_string();
    env.ok(&["stop", &id3, "--timeout", "1"]);
    for f in ["state.db", "state.db-wal"] {
        if let Ok(bytes) = std::fs::read(env.path().join(f)) {
            assert!(!bytes.windows(20).any(|w| w == b"env-secret-value-987"), "secret found in {f}");
        }
    }
}

#[test]
fn state_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let id = env.start_detached(&["sleep", "30"]);
    let mode = |p: std::path::PathBuf| std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(env.path().to_path_buf()), 0o700);
    assert_eq!(mode(env.path().join("run")), 0o700);
    assert_eq!(mode(env.db()), 0o600);
    assert_eq!(mode(env.path().join("run").join(format!("{id}.sock"))), 0o600);
    assert_eq!(mode(env.path().join("logs").join(format!("{id}.log"))), 0o600);
    assert_eq!(mode(env.path().join("logs")), 0o700);
    assert_eq!(mode(env.path().join("state.db-wal")), 0o600);
}

#[test]
fn supervisor_killed_mid_output_leaves_consistent_events() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"]);
    wait_for("output to be recorded", || env.events(&id).iter().any(|e| e["event_type"] == "terminal.output"));
    let (_, child) = kill_supervisor(&env, &id);
    send_signal(child as i32, libc::SIGKILL);
    let events = env.events(&id);
    assert!(events.len() > 2);
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e["seq"].as_u64().unwrap(), i as u64);
    }
    assert_eq!(env.status(&id), "lost");
}

#[test]
fn corrupt_store_is_reported_not_overwritten() {
    let env = Env::new();
    let id = env.start_detached(&["true"]);
    env.wait_status(&id, "exited");
    for f in ["state.db-wal", "state.db-shm"] {
        let _ = std::fs::remove_file(env.path().join(f));
    }
    std::fs::write(env.db(), vec![0x41; 16384]).unwrap();
    let out = env.run(&["sessions"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("corrupt") && err.contains(&env.db().display().to_string()), "{err}");
    let out = env.run(&["run", "-d", "--", "true"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(env.db()).unwrap(), vec![0x41; 16384]);
}

#[test]
fn rm_and_prune_respect_running_sessions() {
    let env = Env::new();
    let live = env.start_detached(&["sleep", "30"]);
    let done = env.start_detached(&["true"]);
    env.wait_status(&done, "exited");
    let out = env.run(&["rm", &live]);
    assert!(!out.status.success());
    env.ok(&["prune", "--older-than", "0s"]);
    let ids: Vec<String> = env.sessions().iter().map(|s| s["session"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, vec![live.clone()]);
    env.ok(&["stop", &live, "--timeout", "1"]);
    env.ok(&["rm", &live]);
    assert!(env.sessions().is_empty());
}

#[test]
fn asciicast_export_is_valid_v2() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "printf 'caf\\303\\251\\n'"]);
    env.wait_status(&id, "exited");
    let out = env.ok(&["export", &id, "--format", "asciicast"]);
    let mut lines = out.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["version"], 2);
    let text: String = lines
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v[1] == "o")
        .map(|v| v[2].as_str().unwrap().to_string())
        .collect();
    assert!(text.contains("caf\u{e9}"));
}

fn fake_claude(env: &Env) -> String {
    let dir = env.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let used = env.path().join("used-ids");
    std::fs::create_dir_all(&used).unwrap();
    let path = dir.join("claude");
    let script = format!(
        r#"#!/bin/sh
echo "ARGS: $*"
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id)
      if [ -e "{used}/$2" ]; then echo "Error: Session ID $2 is already in use."; exit 1; fi
      touch "{used}/$2"; shift ;;
    --resume) echo "RESUMED $2"; shift ;;
  esac
  shift
done
"#,
        used = used.display()
    );
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn is_uuid_v4(s: &str) -> bool {
    s.len() == 36 && s.as_bytes()[14] == b'4' && s.split('-').map(str::len).eq([8, 4, 4, 4, 12])
}

#[test]
fn claude_sessions_get_a_pre_assigned_id_used_for_exact_resume_and_fresh_on_restart() {
    let env = Env::new();
    let claude = fake_claude(&env);
    let id = env.start_detached(&["env", "A=1", &claude, "--model", "haiku"]);
    env.wait_status(&id, "exited");
    let info = env.inspect(&id);
    let agent = info["session"]["agent_session_id"].as_str().expect("agent session id").to_string();
    assert!(is_uuid_v4(&agent), "{agent}");
    let argv: Vec<&str> = info["session"]["command"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(argv[..5], ["env", "A=1", claude.as_str(), "--session-id", agent.as_str()]);
    assert_eq!(argv[5], "--settings");
    assert_eq!(argv[7..], ["--model", "haiku"]);
    let created = env.events(&id).into_iter().find(|e| e["event_type"] == "session.created").unwrap();
    assert_eq!(created["payload"]["agent_session_id"], agent.as_str());
    assert!(env.ok(&["replay", &id, "--instant"]).contains(&format!("--session-id {agent} --settings")));
    assert!(env.ok(&["inspect", &id]).contains(&format!("Agent session:   {agent}")));

    let offer = env.ok(&["recover", &id]);
    assert!(offer.contains(&format!("run `env A=1 {claude} --resume {agent}`")), "{offer}");
    assert!(offer.contains(&format!("re-run `env A=1 {claude} --model haiku`")), "{offer}");

    let resumed =
        env.ok(&["recover", &id, "--resume", "--yes", "--detached"]).trim().lines().last().unwrap().to_string();
    env.wait_status(&resumed, "exited");
    assert_eq!(env.inspect(&resumed)["session"]["agent_session_id"], agent.as_str());
    assert!(env.ok(&["replay", &resumed, "--instant"]).contains(&format!("RESUMED {agent}")));

    let restarted =
        env.ok(&["recover", &id, "--restart", "--yes", "--detached"]).trim().lines().last().unwrap().to_string();
    env.wait_status(&restarted, "exited");
    let again = env.inspect(&restarted);
    assert_eq!(again["session"]["exit_code"], 0, "{}", env.ok(&["replay", &restarted, "--instant"]));
    let fresh = again["session"]["agent_session_id"].as_str().unwrap();
    assert!(is_uuid_v4(fresh) && fresh != agent, "{fresh}");
}

#[test]
fn session_ids_are_not_injected_when_disabled_or_already_chosen() {
    let env = Env::new();
    let claude = fake_claude(&env);
    let off = env.ok(&["run", "-d", "--no-agent-integration", "--", &claude, "-p", "hi"]).trim().to_string();
    let continued = env.start_detached(&[&claude, "--continue"]);
    let given = "4b41da33-24de-41bb-92ef-77589bb77d13";
    let chosen = env.start_detached(&[&claude, "--session-id", given]);
    let shell = env.start_detached(&["sh", "-c", "echo plain"]);
    for id in [&off, &continued, &chosen, &shell] {
        env.wait_status(id, "exited");
    }
    for (id, want) in [(&off, None), (&continued, None), (&chosen, Some(given)), (&shell, None)] {
        let s = &env.inspect(id)["session"];
        assert_eq!(s["agent_session_id"].as_str(), want, "{s}");
        assert!(!env.ok(&["replay", id, "--instant"]).contains("--session-id ") || want == Some(given));
    }
    let resume = env.ok(&["recover", &continued]);
    assert!(resume.contains(&format!("run `{claude} --continue`")), "{resume}");
}

fn fake_claude_running_hooks(env: &Env) -> String {
    let dir = env.path().join("hookbin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("claude");
    let script = r#"#!/usr/bin/env python3
import json, subprocess, sys
args = sys.argv[1:]
sid = args[args.index("--session-id") + 1]
hooks = json.loads(args[args.index("--settings") + 1])["hooks"]
def fire(name, **fields):
    cmd = hooks[name][0]["hooks"][0]["command"]
    payload = dict(hook_event_name=name, session_id=sid, transcript_path="/t/" + sid + ".jsonl", **fields)
    r = subprocess.run(["sh", "-c", cmd], input=json.dumps(payload).encode(), capture_output=True)
    print("HOOK %s exit=%d stdout=%r" % (name, r.returncode, r.stdout.decode()), flush=True)
fire("SessionStart", source="startup", model="haiku")
fire("UserPromptSubmit", prompt="the-secret-prompt", permission_mode="default")
fire("Notification", notification_type="permission_prompt", message="Claude needs your permission")
fire("Stop", last_assistant_message="the-secret-reply")
"#;
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn injected_claude_hooks_record_agent_events_without_prompt_text() {
    let env = Env::new();
    let claude = fake_claude_running_hooks(&env);
    let id = env.start_detached(&[&claude]);
    env.wait_status(&id, "exited");
    let out = env.ok(&["replay", &id, "--instant"]);
    for name in ["SessionStart", "UserPromptSubmit", "Notification", "Stop"] {
        assert!(out.contains(&format!("HOOK {name} exit=0 stdout=''")), "{out}");
    }
    let agent = env.inspect(&id)["session"]["agent_session_id"].as_str().unwrap().to_string();
    let hooks: Vec<_> = env.events(&id).into_iter().filter(|e| e["event_type"] == "agent.hook").collect();
    let names: Vec<_> = hooks.iter().map(|e| e["payload"]["hook_event_name"].as_str().unwrap()).collect();
    assert_eq!(names, ["SessionStart", "UserPromptSubmit", "Notification", "Stop"]);
    for e in &hooks {
        assert_eq!(e["payload"]["agent"], "claude");
        assert_eq!(e["payload"]["session_id"], agent.as_str());
    }
    assert_eq!(hooks[0]["payload"]["source"], "startup");
    assert_eq!(hooks[2]["payload"]["notification_type"], "permission_prompt");
    assert_eq!(hooks[2]["payload"]["message"], "Claude needs your permission");
    let all = env.ok(&["export", &id, "--format", "jsonl"]);
    assert!(!all.contains("the-secret-prompt") && !all.contains("the-secret-reply"));
    let db = std::fs::read(env.db()).unwrap();
    assert!(!db.windows(17).any(|w| w == b"the-secret-prompt"));
}

#[test]
fn the_hook_command_never_fails_the_agent() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "sleep 30"]);
    let send = |session: Option<&str>, input: &str| {
        let mut c = env.command(&["hook", "--agent", "claude"]);
        if let Some(s) = session {
            c.env("AGENTRECORDER_SESSION", s);
        }
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let ok = send(Some(&id), r#"{"hook_event_name":"Stop","session_id":"x"}"#);
    assert!(ok.status.success() && ok.stdout.is_empty() && ok.stderr.is_empty(), "{ok:?}");
    for (session, input) in [
        (None, r#"{"hook_event_name":"Stop"}"#),
        (Some("ffffff"), r#"{"hook_event_name":"Stop"}"#),
        (Some("../../x"), r#"{"hook_event_name":"Stop"}"#),
        (Some(id.as_str()), "not json"),
    ] {
        let out = send(session, input);
        assert!(out.status.success(), "{session:?} {input}: {out:?}");
        assert!(out.stdout.is_empty(), "{session:?} {input}: {out:?}");
    }
    wait_for("agent.hook event", || env.events(&id).iter().any(|e| e["event_type"] == "agent.hook"));
    let hooks = env.events(&id).into_iter().filter(|e| e["event_type"] == "agent.hook").count();
    assert_eq!(hooks, 1);
    env.ok(&["stop", &id]);
}
