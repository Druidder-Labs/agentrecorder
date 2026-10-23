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
    let out = env.run(&["run", "-d", "--", "/nonexistent/witnessed-test-binary"]);
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
    let id = env.start_detached(&["sh", "-c", "trap '' HUP TERM; echo armed; exec sleep 300"]);
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
    let id = env.start_detached(&["sh", "-c", "trap '' HUP TERM; echo armed; exec sleep 300"]);
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
    let month_ago = witnessed_store::now_unix_ns() - 31 * 86_400 * 1_000_000_000;
    let conn = rusqlite::Connection::open(env.db()).unwrap();
    conn.execute(
        "UPDATE sessions SET created_at_ns = ?1, ended_at_ns = ?1 WHERE id = ?2",
        rusqlite::params![month_ago, old],
    )
    .unwrap();
    conn.execute("UPDATE sessions SET created_at_ns = ?1 WHERE id = ?2", rusqlite::params![month_ago, live]).unwrap();
    drop(conn);
    let ids = |env: &Env| env.sessions().iter().map(|s| s["session"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    env.command(&["run", "-d", "--", "true"]).env("WITNESSED_RETENTION_DAYS", "0").output().unwrap();
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
    let reasons = || -> Vec<String> {
        env.events(&id)
            .iter()
            .filter(|e| e["event_type"] == "session.detached")
            .map(|e| e["payload"]["reason"].as_str().unwrap().to_string())
            .collect()
    };
    wait_for("client_too_slow detach", || reasons().contains(&"client_too_slow".to_string()));
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
    std::fs::write(repo.join("tracked.txt"), "one\n").unwrap();
    std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
    git(&["add", "tracked.txt", "gone.txt"]).unwrap();
    git(&["-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "-m", "init"]).unwrap();
    std::fs::write(repo.join("pre-dirty.txt"), "before\n").unwrap();
    let id = env
        .ok(&[
            "run",
            "-d",
            "--cwd",
            repo.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "printf '\\033]9;build done\\007'; printf '\\007'; sleep 2; touch new-file; echo two >> tracked.txt; rm gone.txt; git -c user.email=a@b -c user.name=t commit -q --allow-empty -m 'feat: made during the session'; sleep 1",
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
    let info = env.inspect(&id);
    let git_events = info["git"].as_array().unwrap().clone();
    let exit = git_events.iter().find(|g| g["payload"]["phase"] == "exit").expect("exit snapshot");
    assert!(exit["payload"]["dirty_paths"].as_u64().unwrap() >= 1);
    let start = git_events.iter().find(|g| g["payload"]["phase"] == "start").expect("start snapshot");
    assert!(start["payload"]["head"].is_string());
    assert_eq!(start["payload"]["dirty"], serde_json::json!(["pre-dirty.txt"]));
    let changed: Vec<(String, String)> = exit["payload"]["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["status"].as_str().unwrap().into(), c["path"].as_str().unwrap().into()))
        .collect();
    let want = [("D", "gone.txt"), ("?", "new-file"), ("?", "pre-dirty.txt"), ("M", "tracked.txt")];
    assert_eq!(changed, want.map(|(s, p)| (s.to_string(), p.to_string())));
    assert_eq!(exit["payload"]["changed_paths"], 4);
    assert_eq!(exit["payload"]["new_commits"], 1);
    assert_eq!(exit["payload"]["commits"][0]["subject"], "feat: made during the session");
    assert_eq!(exit["payload"]["commits"][0]["hash"].as_str().unwrap().len(), 40);
    assert!(start["payload"]["commits"].is_null() && start["payload"]["new_commits"].is_null());
    assert_eq!(exit["payload"]["diff_sha256"].as_str().unwrap().len(), 64);
    assert!(start["payload"]["diff_sha256"].is_null());

    let with_diff = env.path().join("with-diff");
    let out = env.ok(&["bundle", &id, "--diff", "-o", with_diff.to_str().unwrap()]);
    assert!(out.contains("code changes: 4 files") && out.contains("matches the session end"), "{out}");
    let patch = std::fs::read_to_string(with_diff.join("diff.patch")).unwrap();
    assert!(patch.contains("+two") && patch.contains("deleted file mode") && patch.contains("b/new-file"), "{patch}");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(with_diff.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["diff"]["status"], "matches the session end");
    assert_eq!(manifest["diff"]["files"], 4);
    let checked = env.ok(&["verify", with_diff.to_str().unwrap()]);
    assert!(checked.contains("but no anchor covers that record"), "{checked}");
    assert!(checked.contains("diff matches the recorded hash, but the session end is not anchored"), "{checked}");
    let anchors = env.path().join("anchors.jsonl");
    std::fs::write(&anchors, env.ok(&["anchor", &id])).unwrap();
    let checked = env.ok(&["verify", with_diff.to_str().unwrap(), "--anchors", anchors.to_str().unwrap()]);
    assert!(checked.contains("diff:     matches the hash recorded when the session ended"), "{checked}");
    assert!(checked.contains("anchored, diff matches the session end"), "{checked}");
    std::fs::write(repo.join("tracked.txt"), "changed after the session\n").unwrap();
    let later = env.path().join("later");
    let out = env.ok(&["bundle", &id, "--diff", "-o", later.to_str().unwrap()]);
    assert!(out.contains("differs from the session end"), "{out}");
    let checked = env.ok(&["verify", later.to_str().unwrap()]);
    assert!(checked.contains("diff differs from the session end"), "{checked}");
    let mut patch = std::fs::read_to_string(with_diff.join("diff.patch")).unwrap();
    patch.push_str("+edited\n");
    std::fs::write(with_diff.join("diff.patch"), patch).unwrap();
    let tampered = env.command(&["verify", with_diff.to_str().unwrap()]).output().unwrap();
    assert!(!tampered.status.success());
    assert!(String::from_utf8_lossy(&tampered.stdout).contains("diff.patch: sha256 differs from manifest.json"));
    let files = &info["files_changed"];
    assert_eq!((files["count"].as_u64(), files["already_dirty_at_start"].as_u64()), (Some(4), Some(1)), "{files}");
    assert_eq!(files["since_head"].as_str().unwrap().len(), 12);
    assert_eq!(files["paths"][2]["dirty_at_start"], true);
    assert_eq!(files["paths"][3]["dirty_at_start"], false);
    let text = env.ok(&["inspect", &id]);
    assert!(text.contains("Files changed:   4 since HEAD "), "{text}");
    assert!(text.contains(" at start, 1 already modified when the session started\n"), "{text}");
    assert!(text.contains("  M tracked.txt\n") && text.contains("  ? pre-dirty.txt (modified at start)\n"), "{text}");
    let plain = env.start_detached(&["sh", "-c", "true"]);
    env.wait_status(&plain, "exited");
    assert!(
        env.inspect(&plain)["files_changed"].is_null() || env.inspect(&plain)["files_changed"]["count"].is_number()
    );
}

#[test]
fn the_session_end_diff_is_stored_only_when_output_is_recorded() {
    let env = Env::new();
    let repo = env.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| Command::new("git").current_dir(&repo).args(args).output().map(|o| o.status.success());
    if git(&["init", "-q"]).ok() != Some(true) {
        return;
    }
    std::fs::write(repo.join("f"), "one\n").unwrap();
    git(&["add", "f"]).unwrap();
    git(&["-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "-m", "init"]).unwrap();
    let head =
        String::from_utf8(Command::new("git").current_dir(&repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
    let cwd = repo.to_str().unwrap();
    let id = env.ok(&["run", "-d", "--cwd", cwd, "--", "sh", "-c", "echo x >> f"]).trim().to_string();
    let quiet = env.ok(&["run", "-d", "--no-record", "--cwd", cwd, "--", "sh", "-c", "echo y >> f"]).trim().to_string();
    env.wait_status(&id, "exited");
    env.wait_status(&quiet, "exited");
    let store = witnessed_store::Store::open(&env.db()).unwrap();
    let row = store.session_diff(&id).unwrap().expect("session diff stored");
    assert_eq!(row.base, head);
    assert!(String::from_utf8_lossy(&row.diff).contains("+x"));
    let exit = env.inspect(&id)["git"].as_array().unwrap().iter().find(|g| g["payload"]["phase"] == "exit").cloned();
    assert_eq!(exit.unwrap()["payload"]["diff_bytes"].as_u64(), Some(row.diff.len() as u64));
    assert!(store.session_diff(&quiet).unwrap().is_none());
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
    let out = env.command(&["run", "-d", "--", "true"]).env("WITNESSED_HOME", &long).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("socket path"));
    let list = env.command(&["sessions", "--json"]).env("WITNESSED_HOME", &long).output().unwrap();
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
        (0..5).map(|_| env.start_detached(&["sh", "-c", "echo $WITNESSED_SESSION; sleep 30"])).collect();
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
    for cmd in ["attach", "inspect", "recover", "replay", "review", "stop"] {
        let out = env.run(&[cmd, "ffffff"]);
        assert!(!out.status.success(), "{cmd}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("no session matches"), "{cmd}");
    }
}

#[test]
fn review_of_a_session_without_an_agent_explains_why_and_refuses_annotations() {
    let env = Env::new();
    let id = env.start_detached(&["true"]);
    env.wait_status(&id, "exited");
    let text = env.ok(&["review"]);
    assert!(text.starts_with(&format!("Session {id}  true  ")), "{text}");
    assert!(text.contains("Decision moments: unavailable ("), "{text}");
    let json: serde_json::Value = serde_json::from_str(&env.ok(&["review", &id, "--json"])).unwrap();
    assert_eq!(json["session"], id.as_str());
    assert_eq!(json["decisions"]["available"], false);
    assert!(json["annotations"].as_array().unwrap().is_empty());
    let out = env.run(&["review", &id, "--mark", "m-1", "--verdict", "again"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("cannot annotate session {id}: its decision moments are unavailable (")), "{err}");
    let out = env.run(&["review", &id, "--mark", "m-1", "--verdict", "maybe"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("possible values: again, differently, unsure"));
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
    let cwd = env.path().display().to_string();
    let mut c = env.spawn_pty(&["run", "--cwd", &cwd, "--", "sh", "-c", "read -r x; echo len-${#x}"], 24, 80);
    c.expect("started");
    let id = session_id_from(&c.output());
    c.send(b"typed-secret-123\r");
    c.expect("len-16");
    c.wait();
    assert!(env.events(&id).iter().all(|e| e["event_type"] != "terminal.input"));

    let mut c =
        env.spawn_pty(&["run", "--cwd", &cwd, "--record-input", "--", "sh", "-c", "read -r x; echo len-${#x}"], 24, 80);
    c.expect("started");
    let id2 = session_id_from(&c.output());
    c.send(b"abc\r");
    c.expect("len-3");
    c.wait();
    assert!(env.events(&id2).iter().any(|e| e["event_type"] == "terminal.input"));

    let id3 = env
        .command(&["run", "-d", "--cwd", &cwd, "--", "sleep", "30"])
        .env("WITNESSED_TEST_SECRET", "env-secret-value-987")
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
    let sup = env.inspect(&id)["session"]["supervisor_pid"].as_i64().unwrap();
    wait_for("supervisor to close the store", || !pid_alive(sup));
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
    let text = env.ok(&["inspect", &id]);
    assert!(text.contains(&format!("Agent session:   {agent}")));
    assert!(text.contains("Command:         env A=1 ") && text.contains("--model haiku"), "{text}");
    assert!(!text.contains("--session-id") && !text.contains("hook --agent claude"), "{text}");
    let described = env.ok(&["recover", &id]);
    let line = described.lines().find(|l| l.trim_start().starts_with("Command:")).unwrap();
    assert!(line.contains("--settings <witnessed hooks>") && !line.contains("hook --agent"), "{line}");

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
import datetime, json, os, subprocess, sys
args = sys.argv[1:]
sid = args[args.index("--session-id") + 1]
hooks = json.loads(args[args.index("--settings") + 1])["hooks"]
transcript = os.path.join(os.path.dirname(os.path.abspath(sys.argv[0])), sid + ".jsonl")
def fire(name, **fields):
    cmd = hooks[name][0]["hooks"][0]["command"]
    payload = dict(hook_event_name=name, session_id=sid, transcript_path=transcript, **fields)
    r = subprocess.run(["sh", "-c", cmd], input=json.dumps(payload).encode(), capture_output=True)
    print("HOOK %s exit=%d stdout=%r" % (name, r.returncode, r.stdout.decode()), flush=True)
def line(ts, req, inp, out, read, create):
    usage = {"input_tokens": inp, "output_tokens": out, "cache_read_input_tokens": read, "cache_creation_input_tokens": create}
    return json.dumps({"type": "assistant", "timestamp": ts, "requestId": req, "message": {"model": "claude-test-1", "usage": usage}})
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
with open(transcript, "w") as f:
    f.write(line(now, "req_1", 2, 100, 500, 50) + "\n")
    f.write(line(now, "req_1", 2, 100, 500, 50) + "\n")
    f.write(line("2000-01-01T00:00:00.000Z", "req_0", 9, 9, 9, 9) + "\n")
    f.write(json.dumps({"type": "user", "timestamp": now, "origin": {"kind": "human"}, "message": {"content": "the-secret-prompt\n\x1b]52;c;evil\x07"}}) + "\n")
    f.write(json.dumps({"type": "user", "timestamp": now, "message": {"content": [{"type": "tool_result", "content": "ok"}]}}) + "\n")
    f.write(line(now, "req_2", 3, 10, 600, 0) + "\n")
    f.write(json.dumps({"type": "assistant", "timestamp": now, "uuid": "a1", "message": {"content": [{"type": "text", "text": "twelve chars"}]}}) + "\n")
    f.write(json.dumps({"type": "user", "timestamp": now, "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]}}) + "\n")
    f.write(json.dumps({"type": "user", "timestamp": now, "isCompactSummary": True, "message": {"content": "summary"}}) + "\n")
fire("SessionStart", source="startup", model="haiku")
fire("UserPromptSubmit", prompt="the-secret-prompt", permission_mode="default")
fire("Notification", notification_type="permission_prompt", message="Claude needs your permission")
fire("PostToolUse", tool_name="Bash", tool_input={"command": "the-secret-command"})
fire("PostToolUse", tool_name="Edit", tool_input={"file_path": "src/a.py", "old_string": "the-secret-old", "new_string": "x\ny"})
fire("PostToolUseFailure", tool_name="Bash", tool_input={"command": "python3 -m unittest"}, error="exit 1")
fire("PostToolUse", tool_name="Bash")
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
    let info = env.inspect(&id);
    let agent = info["session"]["agent_session_id"].as_str().unwrap().to_string();
    let hooks: Vec<_> = env.events(&id).into_iter().filter(|e| e["event_type"] == "agent.hook").collect();
    let names: Vec<_> = hooks.iter().map(|e| e["payload"]["hook_event_name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "SessionStart",
            "UserPromptSubmit",
            "Notification",
            "PostToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "PostToolUse",
            "Stop"
        ]
    );
    for e in &hooks {
        assert_eq!(e["payload"]["agent"], "claude");
        assert_eq!(e["payload"]["session_id"], agent.as_str());
    }
    assert_eq!(hooks[0]["payload"]["source"], "startup");
    assert_eq!(hooks[2]["payload"]["notification_type"], "permission_prompt");
    assert_eq!(hooks[2]["payload"]["message"], "Claude needs your permission");
    assert_eq!(hooks[3]["payload"]["tool_name"], "Bash");
    assert!(hooks[3]["payload"]["tool_input"].is_null());
    assert!(hooks[0]["payload"]["tool"].is_null());
    assert!(hooks[3]["payload"]["usage"].is_null());
    let stop_usage = &hooks[7]["payload"]["usage"];
    assert_eq!(
        (
            stop_usage["api_calls"].as_u64(),
            stop_usage["input"].as_u64(),
            stop_usage["output"].as_u64(),
            stop_usage["cache_read"].as_u64(),
            stop_usage["cache_creation"].as_u64(),
            stop_usage["outside_session"].as_u64()
        ),
        (Some(2), Some(5), Some(110), Some(1100), Some(50), Some(1)),
        "{stop_usage}"
    );
    assert_eq!(stop_usage["models"], serde_json::json!(["claude-test-1"]));
    let all = env.ok(&["export", &id, "--format", "jsonl"]);
    for secret in ["the-secret-prompt", "the-secret-reply", "the-secret-old"] {
        assert!(!all.contains(secret), "{secret}");
    }
    assert_eq!(hooks[3]["payload"]["tool"]["command"], "the-secret-command");
    assert_eq!(
        hooks[4]["payload"]["tool"],
        serde_json::json!({"path": "src/a.py", "lines_added": 2, "lines_removed": 1})
    );
    assert_eq!(hooks[5]["payload"]["tool"], serde_json::json!({"command": "python3 -m unittest", "error": "exit 1"}));
    let db = std::fs::read(env.db()).unwrap();
    assert!(!db.windows(17).any(|w| w == b"the-secret-prompt"));

    let tools = &info["tool_calls"];
    assert_eq!((tools["count"].as_u64(), tools["failed"].as_u64()), (Some(4), Some(1)), "{tools}");
    assert_eq!(tools["source"], "claude hooks");
    let by: Vec<(String, u64, u64)> = tools["by_tool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["tool"].as_str().unwrap().into(), c["count"].as_u64().unwrap(), c["failed"].as_u64().unwrap()))
        .collect();
    assert_eq!(by, [("Bash".to_string(), 3, 1), ("Edit".to_string(), 1, 0)]);
    let tokens = &info["tokens"];
    assert_eq!(tokens["source"], "claude transcript", "{tokens}");
    assert_eq!(
        tokens["transcript_path"].as_str().unwrap(),
        env.path().join("hookbin").join(format!("{agent}.jsonl")).to_str().unwrap()
    );
    let n = |k: &str| tokens[k].as_u64().unwrap();
    assert_eq!(
        (
            n("api_calls"),
            n("input"),
            n("output"),
            n("cache_read"),
            n("cache_creation"),
            n("total"),
            n("outside_session")
        ),
        (2, 5, 110, 1100, 50, 1265, 1)
    );
    assert_eq!(tokens["models"], serde_json::json!(["claude-test-1"]));
    let unavailable = info["unavailable"].as_array().unwrap();
    assert!(!unavailable.iter().any(|u| u == "tokens" || u == "tool_calls"), "{unavailable:?}");
    assert!(unavailable.iter().any(|u| u == "cost"));
    let text = env.ok(&["inspect", &id]);
    assert!(text.contains("Tool calls:      4 (1 failed; claude hooks: Bash 3, Edit 1)"), "{text}");
    assert!(
        text.contains("Tokens:          input 5, output 110, cache read 1100, cache write 50 (2 API calls, claude-test-1; claude transcript; 1 line outside the session skipped)"),
        "{text}"
    );
    assert!(text.contains("Cost:            unavailable (no price table)"), "{text}");
    let turns = tokens["interactions"].as_array().unwrap();
    let got: Vec<(String, u64, u64)> = turns
        .iter()
        .map(|t| (t["origin"].as_str().unwrap().into(), t["api_calls"].as_u64().unwrap(), t["total"].as_u64().unwrap()))
        .collect();
    assert_eq!(got, [("none".to_string(), 1, 652), ("human".to_string(), 1, 613)]);
    assert!(turns.iter().all(|t| t.get("text").is_none()), "{turns:?}");
    assert!(!text.contains("the-secret-prompt"), "{text}");
    assert!(text.contains("Interactions:    2 (1 human, 1 none; tokens per prompt from the claude transcript; prompt text hidden, --prompts shows it)"), "{text}");
    let shown = env.ok(&["inspect", &id, "--prompts"]);
    assert!(
        shown.contains(
            "human             613 tokens, 1 API calls, output 10, 30 chars  \"the-secret-prompt ]52;c;evil\""
        ),
        "{shown}"
    );
    assert!(!shown.contains('\x1b') && !shown.contains('\x07'), "{shown}");
    let full: serde_json::Value = serde_json::from_str(&env.ok(&["inspect", &id, "--json", "--prompts"])).unwrap();
    assert_eq!(full["tokens"]["interactions"][1]["text"], "the-secret-prompt\n\u{1b}]52;c;evil\u{7}");
    let db = std::fs::read(env.db()).unwrap();
    assert!(!db.windows(17).any(|w| w == b"the-secret-prompt"));

    let plain = env.path().join("plain");
    let out = env.ok(&["bundle", &id, "-o", plain.to_str().unwrap()]);
    assert!(out.contains("prompts: 1, with times and tokens; text not included (--prompts adds it)"), "{out}");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(plain.join("manifest.json")).unwrap()).unwrap();
    let story = &manifest["prompts"];
    assert_eq!((story["available"].as_bool(), story["text"].as_bool()), (Some(true), Some(false)), "{story}");
    assert_eq!(story["source"], "claude transcript");
    assert_eq!((story["interrupts"].as_array().unwrap().len(), story["compactions"].as_array().unwrap().len()), (1, 1));
    assert_eq!(story["interactions"][1]["reply_chars"], 12);
    assert_eq!(story["interactions"][1]["model"], "claude-test-1");
    let listed: Vec<(u64, String, u64, bool)> = story["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["index"].as_u64().unwrap(),
                t["origin"].as_str().unwrap().into(),
                t["total"].as_u64().unwrap(),
                t["t_ms"].is_u64(),
            )
        })
        .collect();
    assert_eq!(listed, [(1, "none".to_string(), 652, true), (2, "human".to_string(), 613, true)]);
    for f in ["manifest.json", "index.html", "events.jsonl", "session.cast"] {
        assert!(!std::fs::read_to_string(plain.join(f)).unwrap().contains("the-secret-prompt"), "{f}");
    }
    assert!(std::fs::read_to_string(plain.join("index.html")).unwrap().contains("Thinking: from the previous answer"));

    let with_text = env.path().join("with-text.zip");
    let out = env.ok(&[
        "bundle",
        &id,
        "--zip",
        "--prompts",
        "--redact-literal",
        "the-secret",
        "-o",
        with_text.to_str().unwrap(),
    ]);
    assert!(out.contains("prompts: 1, with their text after redaction; read them before sending"), "{out}");
    let dry = env.ok(&["bundle", &id, "--prompts", "--redact-literal", "the-secret", "--dry-run"]);
    assert!(dry.contains("1 redaction matches in prompt text and the diff: the-***"), "{dry}");
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&with_text).unwrap()).unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("with-text/manifest.json").unwrap(), &mut text).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(manifest["prompts"]["text"], true);
    assert_eq!(manifest["prompts"]["interactions"][1]["text"], "**********-prompt\n\u{1b}]52;c;evil\u{7}");
    assert!(!text.contains("the-secret"), "{text}");

    let quiet = env.ok(&["run", "-d", "--no-record", "--", &claude]).trim().to_string();
    env.wait_status(&quiet, "exited");
    let quiet_hooks: Vec<_> = env.events(&quiet).into_iter().filter(|e| e["event_type"] == "agent.hook").collect();
    assert_eq!(quiet_hooks.len(), 8);
    assert!(quiet_hooks.iter().all(|e| e["payload"]["tool"].is_null()), "{quiet_hooks:?}");

    std::fs::remove_file(tokens["transcript_path"].as_str().unwrap()).unwrap();
    let gone = env.inspect(&id);
    assert!(gone["tokens"].is_null());
    assert!(gone["unavailable"].as_array().unwrap().iter().any(|u| u == "tokens"));
    assert!(
        env.ok(&["inspect", &id]).contains("Tokens:          unavailable (opening "),
        "{}",
        env.ok(&["inspect", &id])
    );
}

#[test]
fn the_hook_command_never_fails_the_agent() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "sleep 30"]);
    let send = |session: Option<&str>, input: &str| {
        let mut c = env.command(&["hook", "--agent", "claude"]);
        if let Some(s) = session {
            c.env("WITNESSED_SESSION", s);
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

fn fake_claude_that_stays(env: &Env) -> String {
    let dir = env.path().join("statebin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("claude");
    let script = r#"#!/usr/bin/env python3
import json, os, subprocess, sys, time
args = sys.argv[1:]
hooks = json.loads(args[args.index("--settings") + 1])["hooks"]
events = [a for a in args if a[:1].isupper()]
for spec in events:
    name, _, ntype = spec.partition(":")
    payload = {"hook_event_name": name, "session_id": "s"}
    if ntype:
        payload["notification_type"] = ntype
    subprocess.run(["sh", "-c", hooks[name][0]["hooks"][0]["command"]], input=json.dumps(payload).encode())
print("FIRED", " ".join(events), flush=True)
time.sleep(60)
"#;
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn agent_state_follows_hook_events_and_ends_with_the_process() {
    let env = Env::new();
    let claude = fake_claude_that_stays(&env);
    let cases: [(&[&str], &str, &str); 4] = [
        (&["SessionStart"], "idle", "SessionStart"),
        (&["SessionStart", "UserPromptSubmit"], "working", "UserPromptSubmit"),
        (&["UserPromptSubmit", "PermissionRequest", "Notification:permission_prompt"], "needs_input", "Notification"),
        (&["UserPromptSubmit", "PermissionRequest", "PostToolUse"], "working", "PostToolUse"),
    ];
    for (events, want, event) in cases {
        let mut cmd = vec![claude.as_str()];
        cmd.extend_from_slice(events);
        let id = env.start_detached(&cmd);
        wait_for("hooks fired", || env.ok(&["replay", &id, "--instant"]).contains("FIRED"));
        let info = env.inspect(&id);
        assert_eq!(info["agent_state"]["state"], want, "{events:?}: {}", info["agent_state"]);
        assert_eq!(info["agent_state"]["event"], event);
        assert_eq!(info["agent_state"]["source"], "claude hooks");
        let text = env.ok(&["inspect", &id]);
        assert!(text.contains(&format!("Agent state:     {want} (claude hooks: {event}")), "{text}");
        env.ok(&["stop", &id]);
        let after = env.inspect(&id);
        assert_eq!(after["agent_state"]["state"], "ended", "{}", after["agent_state"]);
        assert_eq!(after["agent_state"]["source"], "claude hooks and session status");
    }
    let plain = env.start_detached(&["sh", "-c", "echo no-agent"]);
    env.wait_status(&plain, "exited");
    assert!(env.inspect(&plain)["agent_state"].is_null());
    assert!(env.ok(&["inspect", &plain]).contains("Agent state:     unavailable (not an agent session"));
}

fn fake_claude_with_conversations(env: &Env) -> (String, std::path::PathBuf) {
    let dir = env.path().join("resumebin");
    let known = env.path().join("conversations");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&known).unwrap();
    let path = dir.join("claude");
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, subprocess, sys, time
known = {known:?}
args = sys.argv[1:]
hooks = json.loads(args[args.index("--settings") + 1])["hooks"]
def fire(name, **payload):
    payload["hook_event_name"] = name
    subprocess.run(["sh", "-c", hooks[name][0]["hooks"][0]["command"]], input=json.dumps(payload).encode())
if "--session-id" in args:
    sid = args[args.index("--session-id") + 1]
    open(os.path.join(known, sid), "w").write("ok")
    fire("SessionStart", session_id=sid, source="startup")
    sys.exit(0)
sid = args[args.index("--resume") + 1]
path = os.path.join(known, sid)
if not os.path.exists(path):
    print("No conversation found with session ID: " + sid, flush=True)
    fire("SessionEnd", session_id=sid, reason="other")
    sys.exit(1)
if open(path).read() == "wrong":
    fire("SessionStart", session_id="11111111-1111-4111-8111-111111111111", source="startup")
else:
    fire("SessionStart", session_id=sid, source="resume")
time.sleep(30)
"#,
        known = known.to_string_lossy()
    );
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path.to_string_lossy().into_owned(), known)
}

#[test]
fn resume_is_confirmed_only_when_the_agent_reports_the_expected_session() {
    let env = Env::new();
    let (claude, known) = fake_claude_with_conversations(&env);
    let original = env.start_detached(&[&claude, "--model", "haiku"]);
    env.wait_status(&original, "exited");
    let agent = env.inspect(&original)["session"]["agent_session_id"].as_str().unwrap().to_string();
    assert!(env.inspect(&original)["resume"].is_null());

    let resume = |want: &str, reason: &str| {
        let out = env.run(&["recover", &original, "--resume", "--yes", "--detached"]);
        assert!(out.status.success(), "{out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(stderr.contains(&format!("[witnessed] resume {want}: ")), "{stderr}");
        assert!(stderr.contains(reason), "{stderr}");
        let id = String::from_utf8(out.stdout).unwrap().trim().lines().last().unwrap().to_string();
        let info = env.inspect(&id);
        assert_eq!(info["resume"]["verdict"], want, "{}", info["resume"]);
        assert_eq!(info["resume"]["expected_session_id"], agent.as_str());
        assert!(env.ok(&["inspect", &id]).contains(&format!("Resume:          {want} (")));
        let _ = env.run(&["stop", &id]);
    };
    resume("confirmed", "source resume and this session id");
    std::fs::write(known.join(&agent), "wrong").unwrap();
    resume("mismatch", "session id 11111111-1111-4111-8111-111111111111 and source startup");
    std::fs::remove_file(known.join(&agent)).unwrap();
    resume("unconfirmed", "exited with code 1 before the agent reported SessionStart");
}

#[test]
fn osc_133_marks_fill_commands_and_failures() {
    let env = Env::new();
    let script = r"printf '\033]133;A\007$ \033]133;D\007'; \
        printf '\033]133;C\007ok\r\n\033]133;D;0\007'; \
        printf '\033]133;C\007bad\r\n\033]133;D;3\007'; \
        printf '\033]133;C\033\\\033]133;D\007'; \
        printf '\033]133;D;9\007'";
    let id = env.start_detached(&["sh", "-c", script]);
    env.wait_status(&id, "exited");
    let info = env.inspect(&id);
    assert_eq!(info["commands"]["count"], 3, "{}", info["commands"]);
    assert_eq!(info["commands"]["failed"], 1);
    assert_eq!(info["commands"]["exit_unknown"], 1);
    assert!(!info["unavailable"].as_array().unwrap().iter().any(|v| v == "commands"));
    let text = env.ok(&["inspect", &id]);
    assert!(text.contains("Commands:        3 (OSC 133 shell integration marks)"), "{text}");
    assert!(text.contains("Failures:        1 (non-zero exit code, 1 without an exit code)"), "{text}");
    let exits: Vec<_> = env
        .events(&id)
        .into_iter()
        .filter(|e| e["event_type"] == "shell.command_finished")
        .map(|e| e["payload"]["exit_code"].clone())
        .collect();
    assert_eq!(exits, [serde_json::json!(0), serde_json::json!(3), serde_json::Value::Null]);

    let plain = env.start_detached(&["sh", "-c", "echo no-marks"]);
    env.wait_status(&plain, "exited");
    assert!(env.inspect(&plain)["commands"].is_null());
}

fn read_streaming(
    mut child: std::process::Child,
    start: Instant,
) -> (Vec<(Duration, String)>, std::process::ExitStatus) {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(child.stdout.take().unwrap());
    let mut lines = Vec::new();
    for line in reader.lines() {
        lines.push((start.elapsed(), line.unwrap()));
    }
    (lines, child.wait().unwrap())
}

#[test]
fn follow_streams_a_running_session_and_ends_with_it() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo line-one; sleep 1.5; echo line-two; sleep 1.5; echo line-three"]);
    let spawn = |args: &[&str]| env.command(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let started = Instant::now();
    let replay = spawn(&["replay", &id, "--follow", "--instant"]);
    let export = spawn(&["export", &id, "--follow"]);
    let replay = std::thread::spawn(move || read_streaming(replay, started));
    let (exported, export_status) = read_streaming(export, started);
    let (replayed, replay_status) = replay.join().unwrap();
    assert!(replay_status.success() && export_status.success());
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    let when = |lines: &[(Duration, String)], needle: &str| {
        lines.iter().find(|(_, l)| l.contains(needle)).map(|(t, _)| *t).unwrap_or_else(|| panic!("{needle}: {lines:?}"))
    };
    for (lines, first, third) in [(&replayed, "line-one", "line-three"), (&exported, "bGluZS1vbmU", "bGluZS10aHJlZQ")] {
        assert!(when(lines, first) < Duration::from_millis(1500), "{lines:?}");
        assert!(when(lines, third) >= Duration::from_millis(2500), "{lines:?}");
    }
    let last = exported.last().unwrap().1.clone();
    assert!(last.contains("\"event_type\":\"session.terminated\""), "{last}");
    assert_eq!(env.status(&id), "exited");
}

#[test]
fn follow_stops_on_interrupt_and_returns_immediately_for_finished_sessions() {
    let env = Env::new();
    let done = env.start_detached(&["sh", "-c", "echo finished-output"]);
    env.wait_status(&done, "exited");
    let t = Instant::now();
    assert!(env.ok(&["replay", &done, "--follow", "--instant"]).contains("finished-output"));
    assert!(t.elapsed() < Duration::from_secs(2));

    let id = env.start_detached(&["sh", "-c", "echo running; sleep 30"]);
    let child = env.command(&["export", &id, "--follow"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(800));
    send_signal(child.id() as i32, libc::SIGINT);
    let (lines, status) = read_streaming(child, Instant::now());
    assert_eq!(status.code(), Some(130));
    assert!(lines.iter().any(|(_, l)| l.contains("session.created")), "{lines:?}");
    env.ok(&["stop", &id]);
}

#[test]
fn markers_are_recorded_listed_exported_and_replayed_from() {
    let env = Env::new();
    let unmarked = env.start_detached(&["sh", "-c", "echo nothing"]);
    let id = env.start_detached(&[
        "sh",
        "-c",
        "echo step-a; sleep 0.7; echo step-b; sleep 0.7; echo step-c; sleep 0.7; echo step-d; sleep 1.2; echo final-line",
    ]);
    let seen = |needle: &str| {
        wait_for(needle, || env.ok(&["replay", &id, "--instant"]).contains(needle));
    };
    seen("step-b");
    assert!(env.ok(&["mark", &id, "first"]).contains(": first"));
    seen("step-c");
    env.ok(&["mark", &id]);
    seen("step-d");
    env.ok(&["mark", &id, "build\x1b done"]);
    env.wait_status(&id, "exited");

    let markers = env.inspect(&id)["markers"].as_array().unwrap().clone();
    let labels: Vec<_> = markers.iter().map(|m| m["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["first", "", "build done"]);
    let offsets: Vec<_> = markers.iter().map(|m| m["offset_ms"].as_i64().unwrap()).collect();
    assert!(offsets.windows(2).all(|w| w[0] < w[1]), "{offsets:?}");
    let text = env.ok(&["inspect", &id]);
    assert!(text.contains("Markers:         3") && text.contains(" build done"), "{text}");

    let cast = env.ok(&["export", &id, "--format", "asciicast-v3"]);
    for m in [r#""m","mark: first""#, r#""m","mark""#, r#""m","mark: build done""#] {
        assert!(cast.contains(m), "{m}: {cast}");
    }

    let timed = |args: &[&str]| {
        let t = Instant::now();
        let out = env.ok(args);
        (t.elapsed(), out)
    };
    let (full, _) = timed(&["replay", &id]);
    let (from_last, out) = timed(&["replay", &id, "--from-mark", "build done"]);
    let (from_third, _) = timed(&["replay", &id, "--from-mark", "3"]);
    assert!(out.contains("final-line"), "{out}");
    assert!(full >= Duration::from_millis(3000), "{full:?}");
    assert!(from_last < Duration::from_millis(2500) && from_third < Duration::from_millis(2500), "{from_last:?}");

    for (args, err) in [
        (vec!["replay", id.as_str(), "--from-mark", "nope"], "no marker \"nope\""),
        (vec!["replay", unmarked.as_str(), "--from-mark", "1"], "has no markers"),
        (vec!["mark", id.as_str(), "late"], "markers can only be added while it runs"),
    ] {
        let o = env.run(&args);
        assert!(!o.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains(err), "{args:?}: {o:?}");
    }
}

#[test]
fn a_slow_interactive_client_is_resynced_instead_of_detached() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;
    let env = Env::new();
    let id = env.start_detached(&[
        "sh",
        "-c",
        "sleep 1; i=0; while [ $i -lt 60000 ]; do echo line-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done; echo all-output-done; sleep 30",
    ]);
    let mut sock = UnixStream::connect(env.path().join("run").join(format!("{id}.sock"))).unwrap();
    let hello = br#"{"version":1,"mode":"attach","rows":24,"cols":80,"client_pid":1}"#;
    sock.write_all(&raw_frame(1, hello)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut seen = Vec::new();
    let mut frames = 0u32;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(120) {
        let mut header = [0u8; 5];
        sock.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).unwrap();
        assert_eq!(header[0], 101, "expected output frames only, got kind {}", header[0]);
        seen.extend_from_slice(&payload);
        frames += 1;
        if seen.windows(15).any(|w| w == b"all-output-done") {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(
        seen.windows(15).any(|w| w == b"all-output-done"),
        "the redrawn screen never reached the client: {frames} frames, {} bytes in {:.1} s",
        seen.len(),
        started.elapsed().as_secs_f64()
    );
    let events = env.events(&id);
    let discarded: Vec<_> = events
        .iter()
        .filter(|e| e["event_type"] == "runtime.warning" && e["payload"]["code"] == "client_output_discarded")
        .collect();
    assert!(!discarded.is_empty(), "no discard was needed; the client was not slow enough");
    assert!(discarded.iter().all(|e| e["payload"]["bytes"].as_u64().unwrap() > 0));
    assert!(!events.iter().any(|e| e["event_type"] == "session.detached"), "the slow client was detached");
    assert_eq!(env.status(&id), "running");
    sock.write_all(&raw_frame(4, b"")).unwrap();
    wait_for("detach", || env.status(&id) == "detached");
    let reasons: Vec<_> = env
        .events(&id)
        .into_iter()
        .filter(|e| e["event_type"] == "session.detached")
        .map(|e| e["payload"]["reason"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(reasons, ["client_request"]);
    env.ok(&["stop", &id]);
}

#[test]
fn a_pipe_client_gets_output_produced_before_it_attached() {
    let env = Env::new();
    let file = env.path().join("lines.txt");
    let line = format!("{}\n", "b".repeat(98));
    std::fs::write(&file, line.repeat(200_000)).unwrap();
    let want = std::fs::read(&file).unwrap();
    for round in 0..2 {
        let out = env.command(&["run", "--", "cat", file.to_str().unwrap()]).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let mut got: Vec<u8> = out.stdout.iter().copied().filter(|b| *b != b'\r').collect();
        if let Some(at) = got.windows(4).position(|w| w == b"^D\x08\x08") {
            got.drain(at..at + 4);
        }
        assert_eq!(got.len(), want.len(), "round {round}: {} bytes missing", want.len() as i64 - got.len() as i64);
        assert!(got == want, "round {round}: content differs");
    }
    for round in 0..3 {
        let out = env.command(&["run", "--", "seq", "1", "30000"]).stdin(Stdio::null()).output().unwrap();
        let lines = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(lines, 30000, "round {round}");
    }
}

#[test]
fn bundles_redact_verify_and_detect_changes_against_anchors() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo 'token sk-SECRET42'; sleep 0.3; echo after; exit 2"]);
    env.wait_status(&id, "exited");
    let anchors = env.path().join("anchors.jsonl");
    std::fs::write(&anchors, env.ok(&["anchor", &id])).unwrap();
    let dir = env.path().join("bundle");
    let dir_s = dir.to_str().unwrap();
    env.ok(&["bundle", &id, "-o", dir_s, "--redact", "sk-[A-Z0-9]+"]);
    for f in ["index.html", "manifest.json", "events.jsonl", "chain.jsonl", "session.cast"] {
        let body = std::fs::read_to_string(dir.join(f)).unwrap();
        assert!(!body.contains("SECRET42"), "{f} leaks the redacted value");
    }
    let cast = std::fs::read_to_string(dir.join("session.cast")).unwrap();
    assert!(cast.contains("after") && cast.contains("token *********"), "{cast}");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "witnessed-bundle");
    assert!(manifest["fates"]["redacted"].as_u64().unwrap() >= 1);
    let report = env.ok(&["verify", dir_s, "--anchors", anchors.to_str().unwrap()]);
    assert!(report.contains("result:   ok"), "{report}");

    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    std::fs::write(dir.join("events.jsonl"), events.replace("\"exit_code\":2", "\"exit_code\":0")).unwrap();
    let out = env.run(&["verify", dir_s]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("content differs from its recorded hash"));

    let conn = rusqlite::Connection::open(env.db()).unwrap();
    conn.execute(
        "UPDATE events SET payload = replace(payload, '\"exit_code\":2', '\"exit_code\":0') WHERE session_id = ?1",
        [&id],
    )
    .unwrap();
    drop(conn);
    let rebuilt = env.path().join("rebuilt");
    env.ok(&["bundle", &id, "-o", rebuilt.to_str().unwrap()]);
    let out = env.run(&["verify", rebuilt.to_str().unwrap(), "--anchors", anchors.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stdout).contains("chain differs"));
}

#[test]
fn view_writes_a_private_dashboard_that_rm_deletes() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo hello"]);
    env.wait_status(&id, "exited");
    let path = env.ok(&["view", &id, "--no-open"]).trim().to_string();
    let html = std::fs::read_to_string(&path).unwrap();
    assert!(html.contains("\"manifest\"") && !html.contains("__WITNESSED_DATA__"));
    assert!(path.starts_with(env.path().to_str().unwrap()), "{path}");
    let live = env.start_detached(&["sleep", "30"]);
    let out = env.run(&["bundle", &live, "-o", env.path().join("b").to_str().unwrap()]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("needs a finished session"));
    env.ok(&["stop", &live]);
    env.ok(&["rm", &id]);
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn anchor_streams_heads_while_the_session_runs_and_ends_with_it() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "for i in 1 2 3 4 5 6; do echo $i; sleep 0.4; done"]);
    let out = env.command(&["anchor", &id, "--interval", "1"]).output().unwrap();
    assert!(out.status.success());
    let lines: Vec<serde_json::Value> =
        String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.len() >= 2, "{lines:?}");
    let last_seq = env.events(&id).last().unwrap()["seq"].clone();
    assert_eq!(lines.last().unwrap()["seq"], last_seq);
    assert!(lines.windows(2).all(|w| w[0]["seq"].as_i64() < w[1]["seq"].as_i64()));
}

#[test]
fn last_resolves_the_most_recent_session_and_no_args_prints_help() {
    let env = Env::new();
    let out = env.run(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("witnessed run -- claude"));
    let out = env.run(&["inspect", "last"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("no sessions yet"));
    let first = env.start_detached(&["true"]);
    env.wait_status(&first, "exited");
    let second = env.start_detached(&["true"]);
    env.wait_status(&second, "exited");
    assert_eq!(env.inspect("last")["session"]["id"], second);
    assert_eq!(env.inspect("latest")["session"]["id"], second);
    env.ok(&["rm", "last"]);
    assert_eq!(env.inspect("last")["session"]["id"], first);
}

#[test]
fn sessions_table_shows_when_where_and_the_full_command() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo table test"]);
    env.wait_status(&id, "exited");
    let out = env.ok(&["sessions"]);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("SESSION   STARTED    STATUS        DURATION  EXIT       DIRECTORY"), "{}", lines[0]);
    assert!(lines[0].ends_with("COMMAND"));
    assert!(
        lines[1].starts_with(&id) && lines[1].contains("s ago") && lines[1].ends_with("sh -c 'echo table test'"),
        "{}",
        lines[1]
    );
}

#[test]
fn run_anchor_writes_chain_heads_to_a_file() {
    let env = Env::new();
    let file = env.path().join("anchors.jsonl");
    let out = env.ok(&["run", "-d", "--anchor", file.to_str().unwrap(), "--", "sh", "-c", "echo a; sleep 0.3; echo b"]);
    let id = out.trim().to_string();
    env.wait_status(&id, "exited");
    wait_for("anchor file to be written", || std::fs::read_to_string(&file).is_ok_and(|s| s.contains("\"seq\"")));
    std::thread::sleep(Duration::from_millis(300));
    let text = std::fs::read_to_string(&file).unwrap();
    let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(last["session_id"], id);
    let dir = env.path().join("b");
    env.ok(&["bundle", &id, "-o", dir.to_str().unwrap()]);
    let report = env.ok(&["verify", dir.to_str().unwrap(), "--anchors", file.to_str().unwrap(), "--json"]);
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["anchored_through_seq"], last["seq"]);
}

#[test]
fn bundle_dry_run_presets_default_dir_and_warnings() {
    let env = Env::new();
    let id =
        env.start_detached(&["sh", "-c", "echo token ghp_abcdefghijklmnopqrstuvwxyz0123456789; echo pass=hunter2"]);
    env.wait_status(&id, "exited");
    let out = env.ok(&[
        "bundle",
        &id,
        "--dry-run",
        "--redact-secrets",
        "--redact-literal",
        "hunter2",
        "--cut",
        "01:00-02:00",
    ]);
    assert!(out.contains("nothing written"), "{out}");
    assert!(
        out.contains("6 redaction matches")
            && out.contains("ghp_************ (40 bytes)")
            && out.contains("hunt*** (7 bytes)"),
        "{out}"
    );
    assert!(out.contains("terminal.output ghp_"), "{out}");
    assert!(out.contains("starts after the session ended"), "{out}");
    assert!(!out.contains("hunter2") && !out.contains("ghp_abcdef"));
    assert!(
        std::fs::read_dir(env.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("witnessed-"))
    );

    let out = env
        .command(&["bundle", &id, "--redact-literal", "hunter2", "--json"])
        .env("HOME", env.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["path"], env.path().join(format!("witnessed-{id}")).display().to_string());
    let dir = env.path().join(format!("witnessed-{id}"));
    assert!(dir.join("index.html").is_file());
    let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
    assert!(!html.contains("hunter2") && !html.contains("cdn.jsdelivr.net") && html.contains("AsciinemaPlayer"));

    let out = env.run(&["bundle", &id, "-o", dir.to_str().unwrap()]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("--force"));
    env.ok(&["bundle", &id, "-o", dir.to_str().unwrap(), "--force"]);
    let out = env.run(&["verify", env.path().to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a witnessed bundle"));
}

#[test]
fn completions_are_generated_for_each_shell() {
    let env = Env::new();
    for shell in ["bash", "zsh", "fish"] {
        let out = env.ok(&["completions", shell]);
        assert!(out.contains("bundle") && out.contains("witnessed"), "{shell}");
    }
}

#[test]
fn bundle_zip_holds_the_same_files_and_verify_and_view_read_it() {
    let env = Env::new();
    let id = env.start_detached(&["sh", "-c", "echo zipped; exit 3"]);
    env.wait_status(&id, "exited");
    let dir = env.path().join("bundle");
    env.ok(&["bundle", &id, "-o", dir.to_str().unwrap()]);
    let zip_path = env.path().join("review.zip");
    let zip_s = zip_path.to_str().unwrap();
    let out = env.ok(&["bundle", &id, "--zip", "-o", zip_s, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["zip"].as_bool(), v["path"].as_str()), (Some(true), Some(zip_s)));
    assert!(zip_path.is_file() && !env.path().join("review").exists());

    let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let mut names: Vec<String> = archive.file_names().map(|n| n.unwrap().to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        ["chain.jsonl", "events.jsonl", "index.html", "manifest.json", "session.cast"].map(|f| format!("review/{f}"))
    );
    for f in ["manifest.json", "events.jsonl", "chain.jsonl", "session.cast"] {
        let mut body = String::new();
        std::io::Read::read_to_string(&mut archive.by_name(&format!("review/{f}")).unwrap(), &mut body).unwrap();
        let on_disk = std::fs::read_to_string(dir.join(f)).unwrap();
        if f == "manifest.json" {
            let strip = |t: &str| t.lines().filter(|l| !l.contains("\"created_at\"")).collect::<Vec<_>>().join("\n");
            assert_eq!(strip(&body), strip(&on_disk), "{f} differs between zip and directory");
        } else {
            assert_eq!(body, on_disk, "{f} differs between zip and directory");
        }
    }

    let report = env.ok(&["verify", zip_s, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(v["ok"], true);
    let from_dir: serde_json::Value =
        serde_json::from_str(&env.ok(&["verify", dir.to_str().unwrap(), "--json"])).unwrap();
    assert_eq!(v["head"], from_dir["head"]);

    let path = env.ok(&["view", zip_s, "--no-open"]).trim().to_string();
    assert!(path.starts_with(env.path().to_str().unwrap()) && path.ends_with("index.html"), "{path}");
    assert!(std::fs::read_to_string(&path).unwrap().contains("\"manifest\""));

    let out = env.run(&["bundle", &id, "--zip", "-o", zip_s]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("pass --force"));
    env.ok(&["bundle", &id, "--zip", "-o", zip_s, "--force"]);
    let out = env.run(&["bundle", &id, "--zip", "-o", dir.to_str().unwrap(), "--force"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("is a directory"));
    let out = env.run(&["verify", env.path().join("missing.zip").to_str().unwrap()]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("does not exist"));
    let html = std::fs::read_to_string(&path).unwrap();
    assert!(html.contains("Verification") && html.contains("\"bundle_name\":\"review.zip\""), "{path}");
    let out = env.run(&["verify", zip_s, "--anchors", env.path().join("none.jsonl").to_str().unwrap()]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("anchors file"));
}

#[test]
fn anchor_token_binds_the_chain_and_verify_checks_it() {
    use sha2::Digest;
    let env = Env::new();
    let file = env.path().join("anchors.jsonl");
    let out = env.ok(&[
        "run",
        "-d",
        "--anchor",
        file.to_str().unwrap(),
        "--anchor-token",
        "invite-42",
        "--",
        "sh",
        "-c",
        "echo a; sleep 0.3; echo b",
    ]);
    let id = out.trim().to_string();
    let note = env
        .command(&["run", "-d", "--anchor", "a2.jsonl", "--anchor-token", "t", "--", "true"])
        .current_dir(env.path())
        .output()
        .unwrap();
    let note = String::from_utf8_lossy(&note.stderr).into_owned();
    assert!(note.contains("anchors -> a2.jsonl; chain bound to a token"), "{note}");
    env.wait_status(&id, "exited");
    wait_for("anchor file to be written", || std::fs::read_to_string(&file).is_ok_and(|s| s.contains("\"seq\"")));
    std::thread::sleep(Duration::from_millis(300));
    let digest: String = sha2::Sha256::digest(b"invite-42").iter().map(|b| format!("{b:02x}")).collect();
    let text = std::fs::read_to_string(&file).unwrap();
    let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(last["genesis"], digest);
    assert!(!text.contains("invite-42"));
    let info = env.inspect(&id);
    assert!(!info.to_string().contains("invite-42"));
    assert_eq!(info["session"]["anchor_token_digest"], digest);

    let dir = env.path().join("b");
    let dir_s = dir.to_str().unwrap();
    env.ok(&["bundle", &id, "-o", dir_s]);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        (manifest["chain"]["token_bound"].as_bool(), manifest["chain"]["genesis"].as_str()),
        (Some(true), Some(digest.as_str()))
    );
    for f in ["manifest.json", "events.jsonl", "chain.jsonl", "session.cast", "index.html"] {
        assert!(!std::fs::read_to_string(dir.join(f)).unwrap().contains("invite-42"), "{f} carries the token");
    }

    let report = env.ok(&["verify", dir_s, "--anchors", file.to_str().unwrap(), "--token", "invite-42", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(
        (v["ok"].as_bool(), v["token_bound"].as_bool(), v["token_checked"].as_bool()),
        (Some(true), Some(true), Some(true))
    );
    assert_eq!(v["proves"], serde_json::json!(["consistent", "anchored", "token matched"]));
    assert_eq!(v["anchored_through_seq"], last["seq"]);
    let report = env.ok(&["verify", dir_s, "--anchors", file.to_str().unwrap()]);
    assert!(report.contains("pass --token to check it") && report.contains("match the anchors"), "{report}");
    assert!(report.contains("result:   ok: consistent, anchored; token not checked"), "{report}");
    let report = env.ok(&["verify", dir_s, "--anchors", file.to_str().unwrap(), "--token", "invite-42"]);
    assert!(report.contains("result:   ok: consistent, anchored, token matched"), "{report}");
    let report = env.ok(&["verify", dir_s]);
    assert!(report.contains("result:   ok: consistent; not anchored, token not checked"), "{report}");
    let out = env.run(&["verify", dir_s, "--token", "invite-43"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("token:    does not match") && text.contains("not the chain genesis"), "{text}");

    let plain = env.start_detached(&["sh", "-c", "echo a; sleep 0.3; echo b"]);
    env.wait_status(&plain, "exited");
    let plain_dir = env.path().join("plain");
    env.ok(&["bundle", &plain, "-o", plain_dir.to_str().unwrap()]);
    let out = env.run(&["verify", plain_dir.to_str().unwrap(), "--token", "invite-42"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not bound to a token"));
    let plain_anchors = env.path().join("plain-anchors.jsonl");
    std::fs::write(&plain_anchors, env.ok(&["anchor", &plain])).unwrap();
    let swapped = std::fs::read_to_string(&plain_anchors).unwrap().replace(&plain, &id);
    std::fs::write(&plain_anchors, swapped).unwrap();
    let out = env.run(&["verify", dir_s, "--anchors", plain_anchors.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("genesis differs"));
}
