use anyhow::{Result, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use witnessed_store::{LostReason, Session, Status, Store, now_unix_ns};
use witnessed_supervisor::Layout;
use witnessed_supervisor::client::{self, ConnectError, SessionStatus};

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
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running(s) if s.attached => "running",
            Self::Running(_) => "detached",
            Self::Exited => "exited",
            Self::Lost => "lost",
            Self::Unreachable(_) => "unreachable",
        }
    }

    pub const fn live(&self) -> Option<&SessionStatus> {
        match self {
            Self::Running(s) => Some(s),
            _ => None,
        }
    }

    pub const fn is_finished(&self) -> bool {
        matches!(self, Self::Exited | Self::Lost)
    }
}

/// What a command requires of the resolved session; `Finished` names the thing that needs it in the error.
#[derive(Clone, Copy)]
pub enum Need<'a> {
    Any,
    Finished(&'a str),
    Live,
}

pub fn open_and_resolve(layout: &Layout, id: &str) -> Result<(Store, Session, State)> {
    open_needing(layout, id, Need::Any)
}

pub fn open_needing(layout: &Layout, id: &str, need: Need<'_>) -> Result<(Store, Session, State)> {
    let store = crate::commands::open_store(layout)?;
    let mut s = store.resolve(id)?;
    let st = resolve(layout, &store, &mut s);
    check(&s, &st, need)?;
    Ok((store, s, st))
}

pub fn check(s: &Session, st: &State, need: Need<'_>) -> Result<()> {
    match need {
        Need::Any => {}
        Need::Finished(what) if !st.is_finished() => bail!(
            "session {} is {}; {what} needs a finished session. Stop it with `witnessed stop {}`, or follow it \
             live with `witnessed replay {} --follow`",
            s.id,
            st.label(),
            s.id,
            s.id
        ),
        Need::Live if st.live().is_none() => bail!("session {} is {}", s.id, st.label()),
        Need::Finished(_) | Need::Live => {}
    }
    Ok(())
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
    if !restarted && supervisor_alive(s).is_some() {
        if let Some(st) = ask_supervisor_to_rebind(layout, s) {
            return State::Running(st);
        }
        return State::Unreachable("the socket is missing but the supervisor process still exists".into());
    }
    let reason = if restarted { LostReason::HostRestarted } else { LostReason::SupervisorTerminated };
    match store.mark_lost(&s.id, reason) {
        Ok(true) => {
            s.status = Status::Lost;
            s.lost_reason = Some(reason);
            s.attached = false;
        }
        Ok(false) => {}
        Err(e) => eprintln!("witnessed: warning: could not mark session {} lost: {e:#}", s.id),
    }
    match store.session(&s.id) {
        Ok(Some(fresh)) => {
            *s = fresh;
            finished_state(s).unwrap_or(State::Lost)
        }
        _ => State::Lost,
    }
}

const fn finished_state(s: &Session) -> Option<State> {
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

/// The supervisor's pid when a `__supervise --id <session>` process with it still exists.
pub fn supervisor_alive(s: &Session) -> Option<u32> {
    let pid = u32::try_from(s.supervisor_pid?).ok()?;
    let sys_pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let p = sys.process(sys_pid)?;
    let cmd: Vec<&str> = p.cmd().iter().filter_map(|a| a.to_str()).collect();
    (cmd.contains(&"__supervise") && cmd.windows(2).any(|w| w[0] == "--id" && w[1] == s.id)).then_some(pid)
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
        .map_or("?", String::as_str);
    std::path::Path::new(p).file_name().map_or_else(|| p.to_string(), |f| f.to_string_lossy().into_owned())
}
