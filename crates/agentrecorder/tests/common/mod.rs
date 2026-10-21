#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_agentrecorder");
pub const DETACH: &[u8] = b"\x1c";
pub const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Env {
    pub home: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        let home = tempfile::Builder::new().prefix("agt").tempdir_in("/tmp").expect("tempdir");
        Env { home }
    }

    pub fn path(&self) -> &Path {
        self.home.path()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("AGENTRECORDER_HOME", self.home.path())
            .env("PS1", "$ ")
            .env("BASH_SILENCE_DEPRECATION_WARNING", "1")
            .env("TERM", "xterm-256color")
            .env_remove("AGENTRECORDER_SESSION")
            .env_remove("AGENTRECORDER_DETACH_KEY");
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running agentrecorder")
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "agentrecorder {args:?} failed: {}\nstdout: {}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8(out.stdout).expect("utf8 stdout")
    }

    pub fn start_detached(&self, cmd: &[&str]) -> String {
        let mut args = vec!["run", "-d", "--"];
        args.extend_from_slice(cmd);
        self.ok(&args).trim().to_string()
    }

    pub fn sessions(&self) -> Vec<Value> {
        serde_json::from_str(&self.ok(&["sessions", "--json"])).expect("sessions json")
    }

    pub fn status(&self, id: &str) -> String {
        self.sessions()
            .into_iter()
            .find(|s| s["session"] == id)
            .map(|s| s["status"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    pub fn inspect(&self, id: &str) -> Value {
        serde_json::from_str(&self.ok(&["inspect", id, "--json"])).expect("inspect json")
    }

    pub fn wait_status(&self, id: &str, want: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let st = self.status(id);
            if st == want {
                return;
            }
            assert!(Instant::now() < deadline, "session {id} status {st:?}, wanted {want:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn events(&self, id: &str) -> Vec<Value> {
        self.ok(&["export", id]).lines().map(|l| serde_json::from_str(l).expect("event json")).collect()
    }

    pub fn db(&self) -> PathBuf {
        self.home.path().join("state.db")
    }

    pub fn spawn_pty(&self, args: &[&str], rows: u16, cols: u16) -> PtyClient {
        let cmd = self.command(args);
        let mut argv = vec![BIN.to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let envs: Vec<(String, String)> =
            cmd.get_envs().filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string()))).collect();
        let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let spawned = spawn_in_pty(&argv, rows, cols, &env_refs);
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut reader = spawned.master.try_clone().expect("clone master");
        let sink = out.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        PtyClient { master: spawned.master, child: spawned.child, out, mark: 0 }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let Ok(out) = self.command(&["sessions", "--json"]).output() else { return };
        let Ok(list) = serde_json::from_slice::<Vec<Value>>(&out.stdout) else { return };
        for s in list {
            if matches!(s["status"].as_str(), Some("running" | "detached" | "starting")) {
                let _ = self.command(&["stop", s["session"].as_str().unwrap(), "--timeout", "1"]).output();
            }
        }
    }
}

pub struct PtyClient {
    pub master: File,
    pub child: Child,
    out: Arc<Mutex<Vec<u8>>>,
    mark: usize,
}

impl PtyClient {
    pub fn send(&mut self, data: &[u8]) {
        self.master.write_all(data).expect("writing to pty");
    }

    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    pub fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            {
                let buf = self.out.lock().unwrap();
                let hay = String::from_utf8_lossy(&buf[self.mark.min(buf.len())..]).into_owned();
                if let Some(p) = hay.find(needle) {
                    self.mark += hay[..p + needle.len()].len();
                    return;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; output so far:\n{}", self.output());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(st) = self.child.try_wait().expect("try_wait") {
                return st;
            }
            assert!(Instant::now() < deadline, "client did not exit; output:\n{}", self.output());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        set_window_size(&self.master, rows, cols);
        send_signal(self.child.id() as i32, libc::SIGWINCH);
    }
}

pub fn session_id_from(output: &str) -> String {
    let p = output.find("session ").expect("session id in output") + "session ".len();
    output[p..p + 6].to_string()
}

pub fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn pid_alive(pid: i64) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

pub fn send_signal(pid: i32, sig: libc::c_int) {
    if let Ok(sig) = nix::sys::signal::Signal::try_from(sig) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), sig);
    }
}

pub struct Spawned {
    pub master: File,
    pub child: Child,
}

fn spawn_in_pty(argv: &[String], rows: u16, cols: u16, env: &[(&str, &str)]) -> Spawned {
    use std::os::fd::OwnedFd;
    use std::os::unix::process::CommandExt;
    let ws = nix::pty::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = nix::pty::openpty(Some(&ws), None).expect("openpty");
    let stdio = |fd: &OwnedFd| std::process::Stdio::from(fd.try_clone().expect("dup slave"));
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_remove("AGENTRECORDER_SESSION")
        .stdin(stdio(&pty.slave))
        .stdout(stdio(&pty.slave))
        .stderr(stdio(&pty.slave));
    for (k, v) in env {
        cmd.env(k, v);
    }
    // SAFETY: the closure captures nothing and calls async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawning pty client");
    drop(pty.slave);
    Spawned { master: File::from(pty.master), child }
}

fn set_window_size(master: &File, rows: u16, cols: u16) {
    use std::os::fd::AsRawFd;
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ reads a winsize struct that outlives the call.
    assert_eq!(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) }, 0, "resize");
}
