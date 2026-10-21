use agentrecorder_store::{LostReason, Session, Status, Store, now_unix_ns};
use agentrecorder_supervisor::Layout;
use agentrecorder_supervisor::client::{self, ConnectError, SessionStatus};
use anyhow::Result;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const STARTUP_GRACE_NS: i64 = 10_000_000_000;
const BOOT_TIME_TOLERANCE_S: i64 = 5;
const REBIND_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug)]
pub enum State {
    Starting,
    Running(SessionStatus),
    Exited,
    Lost,
    Unreachable(String),
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running(s) if s.attached => "running",
            State::Running(_) => "detached",
            State::Exited => "exited",
            State::Lost => "lost",
            State::Unreachable(_) => "unreachable",
        }
    }

    pub fn live(&self) -> Option<&SessionStatus> {
        match self {
            State::Running(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, State::Exited | State::Lost)
    }
}

pub fn open_and_resolve(layout: &Layout, id: &str) -> Result<(Store, Session, State)> {
    let store = crate::commands::open_store(layout)?;
    let mut s = store.resolve(id)?;
    let st = resolve(layout, &store, &mut s);
    Ok((store, s, st))
}

pub fn resolve(layout: &Layout, store: &Store, s: &mut Session) -> State {
    if let Some(done) = finished_state(s) {
        return done;
    }
    match client::status(layout, &s.id) {
        Ok(st) => return State::Running(st),
        Err(ConnectError::NotRunning) => {}
        Err(e) => return State::Unreachable(e.to_string()),
    }
    if let Ok(Some(fresh)) = store.session(&s.id) {
        *s = fresh;
        if let Some(done) = finished_state(s) {
            return done;
        }
    }
    if s.status == Status::Starting && now_unix_ns() - s.created_at_ns < STARTUP_GRACE_NS {
        return State::Starting;
    }
    let restarted = host_restarted(s);
    if !restarted && supervisor_alive(s) {
        if let Some(st) = ask_supervisor_to_rebind(layout, s) {
            return State::Running(st);
        }
        return State::Unreachable("the socket is missing but the supervisor process still exists".into());
    }
    let reason = if restarted { LostReason::HostRestarted } else { LostReason::SupervisorTerminated };
    if store.mark_lost(&s.id, reason).unwrap_or(false) {
        s.status = Status::Lost;
        s.lost_reason = Some(reason);
        s.attached = false;
    }
    match store.session(&s.id) {
        Ok(Some(fresh)) => {
            *s = fresh;
            finished_state(s).unwrap_or(State::Lost)
        }
        _ => State::Lost,
    }
}

fn finished_state(s: &Session) -> Option<State> {
    match s.status {
        Status::Exited => Some(State::Exited),
        Status::Lost => Some(State::Lost),
        Status::Starting | Status::Running => None,
    }
}

fn ask_supervisor_to_rebind(layout: &Layout, s: &Session) -> Option<SessionStatus> {
    let pid = nix::unistd::Pid::from_raw(s.supervisor_pid? as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGUSR1).ok()?;
    let deadline = std::time::Instant::now() + REBIND_WAIT;
    while std::time::Instant::now() < deadline {
        if let Ok(st) = client::status(layout, &s.id) {
            return Some(st);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

pub fn supervisor_alive(s: &Session) -> bool {
    let Some(pid) = s.supervisor_pid else { return false };
    let pid = Pid::from_u32(pid as u32);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.process(pid).is_some_and(|p| {
        let cmd: Vec<&str> = p.cmd().iter().filter_map(|a| a.to_str()).collect();
        cmd.contains(&"__supervise") && cmd.windows(2).any(|w| w[0] == "--id" && w[1] == s.id)
    })
}

pub fn host_restarted(s: &Session) -> bool {
    let now = sysinfo::System::boot_time() as i64;
    s.boot_time.is_some_and(|b| (b - now).abs() > BOOT_TIME_TOLERANCE_S)
}

pub fn duration_ms(s: &Session) -> Option<i64> {
    let start = s.started_at_ns?;
    let end = match s.ended_at_ns {
        Some(e) => e,
        None if s.status.is_live() => now_unix_ns(),
        None => return None,
    };
    Some((end - start) / 1_000_000)
}

pub fn program(s: &Session) -> String {
    let p = crate::adapters::agent_index(&s.command)
        .and_then(|i| s.command.get(i))
        .or(s.command.first())
        .map(String::as_str)
        .unwrap_or("?");
    std::path::Path::new(p).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string())
}
