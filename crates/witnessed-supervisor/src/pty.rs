use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::pty::{OpenptyResult, Winsize, openpty};
use nix::sys::termios::{InputFlags, SetArg, tcgetattr, tcsetattr};

pub const MAX_ROWS: u16 = 1000;
pub const MAX_COLS: u16 = 1000;

const RESET_SIGNALS: [libc::c_int; 10] = [
    libc::SIGCHLD,
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGALRM,
    libc::SIGPIPE,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
];

pub struct Spawned {
    pub master: File,
    pub child: Child,
}

fn inheritable_fds() -> Vec<libc::c_int> {
    let dir = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<libc::c_int>().ok())
        .filter(|fd| *fd > 2)
        .filter(|&fd| {
            // SAFETY: F_GETFD only reads descriptor flags; an fd closed meanwhile returns -1.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            flags >= 0 && flags & libc::FD_CLOEXEC == 0
        })
        .collect()
}

fn reset_child_process_state(inheritable: &[libc::c_int]) {
    // SAFETY: runs between fork and exec; only async-signal-safe syscalls are used, nothing allocates.
    unsafe {
        for sig in RESET_SIGNALS {
            libc::signal(sig, libc::SIG_DFL);
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        for &fd in inheritable {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

pub fn spawn(
    argv: &[String],
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
    env: &[(&str, &str)],
    umask: Option<libc::mode_t>,
) -> Result<Spawned> {
    let ws = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let OpenptyResult { master, slave } = openpty(Some(&ws), None).context("openpty")?;
    fcntl(&master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("setting FD_CLOEXEC on pty master")?;
    fcntl(&slave, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("setting FD_CLOEXEC on pty slave")?;
    let mut termios = tcgetattr(&slave).context("reading pty termios")?;
    termios.input_flags |= InputFlags::IUTF8;
    tcsetattr(&slave, SetArg::TCSANOW, &termios).context("enabling IUTF8 on the pty")?;
    let stdio = |fd: &OwnedFd| -> Result<Stdio> { Ok(Stdio::from(fd.try_clone()?)) };
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(stdio(&slave)?).stdout(stdio(&slave)?).stderr(stdio(&slave)?);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let inheritable = inheritable_fds();
    // SAFETY: the closure reads a vector allocated before fork and calls only async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(move || {
            reset_child_process_state(&inheritable);
            if let Some(mask) = umask {
                libc::umask(mask);
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => anyhow::anyhow!("{} is not installed or not on PATH", argv[0]),
        _ => anyhow::Error::new(e).context(format!("spawning {:?}", argv[0])),
    })?;
    drop(cmd);
    drop(slave);
    Ok(Spawned { master: File::from(master), child })
}

pub fn default_sigpipe() {
    // SAFETY: restores the default disposition of a signal; no handler state is shared.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

pub fn new_session_on_spawn(cmd: &mut Command) {
    let inheritable = inheritable_fds();
    // SAFETY: the closure reads a vector allocated before fork and calls only async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(move || {
            reset_child_process_state(&inheritable);
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn resize(master: &File, rows: u16, cols: u16) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCSWINSZ reads a winsize struct that outlives the call.
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn window_size(fd: impl AsRawFd) -> Option<(u16, u16)> {
    // SAFETY: winsize is plain data; zero is a valid bit pattern.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes into the winsize struct passed by pointer.
    let ok = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

pub fn session_members(sid: i32) -> Vec<i32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    sys.processes()
        .iter()
        .filter(|(_, p)| p.session_id().is_some_and(|s| s.as_u32() as i32 == sid))
        .map(|(pid, _)| pid.as_u32() as i32)
        .collect()
}

pub fn foreground_pgrp(master: &File) -> Option<i32> {
    nix::unistd::tcgetpgrp(master).ok().map(|p| p.as_raw()).filter(|p| *p > 0)
}
