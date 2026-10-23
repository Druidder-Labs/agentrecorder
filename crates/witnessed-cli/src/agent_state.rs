use anyhow::Result;
use serde::Serialize;
use witnessed_store::{AgentHook, Body, Session, Store, kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Working,
    NeedsInput,
    Idle,
    Ended,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsInput => "needs_input",
            Self::Idle => "idle",
            Self::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentState {
    pub state: Phase,
    pub source: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at_ns: i64,
}

pub fn transition(hook: &AgentHook) -> Option<Phase> {
    Some(match hook.hook_event_name.as_str() {
        "SessionStart" | "Stop" | "StopFailure" => Phase::Idle,
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => Phase::Working,
        "PermissionRequest" => Phase::NeedsInput,
        "Notification" if hook.notification_type.as_deref() == Some("permission_prompt") => Phase::NeedsInput,
        "SessionEnd" => Phase::Ended,
        _ => return None,
    })
}

pub fn fold<'a>(events: impl IntoIterator<Item = (i64, &'a AgentHook)>) -> Option<AgentState> {
    let mut state = None;
    for (at_ns, hook) in events {
        if let Some(phase) = transition(hook) {
            state = Some(AgentState {
                state: phase,
                source: format!("{} hooks", hook.agent),
                event: hook.hook_event_name.clone(),
                detail: hook
                    .notification_type
                    .clone()
                    .or_else(|| hook.tool_name.clone())
                    .or_else(|| hook.reason.clone()),
                at_ns,
            });
        }
    }
    state
}

/// The state after the last hook, ended when the process is gone even if no `SessionEnd` hook arrived.
pub fn from_hooks<'a>(
    hooks: impl IntoIterator<Item = (i64, &'a AgentHook)>,
    process_running: bool,
) -> Option<AgentState> {
    let mut state = fold(hooks);
    if let Some(s) = state.as_mut().filter(|s| !process_running && s.state != Phase::Ended) {
        s.state = Phase::Ended;
        s.source = format!("{} and session status", s.source);
        s.detail = Some(format!("process not running; last hook {}", s.event));
    }
    state
}

pub const RESUME_TIMEOUT_MS: i64 = 90_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Confirmed,
    Mismatch,
    Pending,
    Unconfirmed,
}

impl Verdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Mismatch => "mismatch",
            Self::Pending => "pending",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResumeCheck {
    pub expected_session_id: String,
    pub verdict: Verdict,
    pub reason: String,
}

pub fn judge_resume(
    expected: &str,
    first_start: Option<&AgentHook>,
    elapsed_ms: i64,
    running: bool,
    exit_code: Option<i64>,
) -> ResumeCheck {
    let (verdict, reason) = match first_start {
        Some(h) if h.session_id.as_deref() == Some(expected) && h.source.as_deref() == Some("resume") => {
            (Verdict::Confirmed, "the agent reported SessionStart with source resume and this session id".to_string())
        }
        Some(h) => (
            Verdict::Mismatch,
            format!(
                "the agent reported SessionStart with session id {} and source {}",
                h.session_id.as_deref().unwrap_or("none"),
                h.source.as_deref().unwrap_or("none")
            ),
        ),
        None if !running => (
            Verdict::Unconfirmed,
            match exit_code {
                Some(c) => format!("the process exited with code {c} before the agent reported SessionStart"),
                None => "the process ended before the agent reported SessionStart".to_string(),
            },
        ),
        None if elapsed_ms < RESUME_TIMEOUT_MS => {
            (Verdict::Pending, format!("no SessionStart yet; waiting up to {} s", RESUME_TIMEOUT_MS / 1000))
        }
        None => (Verdict::Unconfirmed, format!("no SessionStart within {} s", RESUME_TIMEOUT_MS / 1000)),
    };
    ResumeCheck { expected_session_id: expected.to_string(), verdict, reason }
}

pub fn resume_check(store: &Store, s: &Session, running: bool, now_ns: i64) -> Result<Option<ResumeCheck>> {
    if crate::adapters::resumed_session(&s.command).is_none() {
        return Ok(None);
    }
    let mut hooks = Vec::new();
    store.for_each_event(&s.id, Some(&[kind::AGENT_HOOK]), |e| {
        if let Some(Body::AgentHook(h)) = e.body() {
            hooks.push(*h);
        }
        Ok(())
    })?;
    Ok(resume_check_in(&hooks, s, running, now_ns))
}

/// `judge_resume` on the first `SessionStart` among the session's hooks; `None` when the command resumed nothing.
pub fn resume_check_in<'a>(
    hooks: impl IntoIterator<Item = &'a AgentHook>,
    s: &Session,
    running: bool,
    now_ns: i64,
) -> Option<ResumeCheck> {
    let expected = crate::adapters::resumed_session(&s.command)?;
    let first = hooks.into_iter().find(|h| h.hook_event_name == "SessionStart");
    let start = s.started_at_ns.unwrap_or(s.created_at_ns);
    let elapsed_ms = (now_ns - start) / 1_000_000;
    Some(judge_resume(&expected, first, elapsed_ms, running, s.exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(name: &str, notification_type: Option<&str>) -> AgentHook {
        AgentHook {
            agent: "claude".into(),
            hook_event_name: name.into(),
            notification_type: notification_type.map(str::to_string),
            ..AgentHook::default()
        }
    }

    fn state_after(names: &[(&str, Option<&str>)]) -> Option<Phase> {
        let hooks: Vec<AgentHook> = names.iter().map(|(n, t)| hook(n, *t)).collect();
        fold(hooks.iter().enumerate().map(|(i, h)| (i as i64, h))).map(|s| s.state)
    }

    #[test]
    fn follows_a_turn_with_an_approved_permission() {
        let mut seq: Vec<(&str, Option<&str>)> = vec![("SessionStart", None)];
        assert_eq!(state_after(&seq), Some(Phase::Idle));
        seq.push(("UserPromptSubmit", None));
        assert_eq!(state_after(&seq), Some(Phase::Working));
        seq.push(("PreToolUse", None));
        assert_eq!(state_after(&seq), Some(Phase::Working));
        seq.push(("PermissionRequest", None));
        seq.push(("Notification", Some("permission_prompt")));
        assert_eq!(state_after(&seq), Some(Phase::NeedsInput));
        seq.push(("PostToolUse", None));
        assert_eq!(state_after(&seq), Some(Phase::Working));
        seq.push(("Stop", None));
        assert_eq!(state_after(&seq), Some(Phase::Idle));
        seq.push(("Notification", Some("something_else")));
        assert_eq!(state_after(&seq), Some(Phase::Idle));
        seq.push(("SessionEnd", None));
        assert_eq!(state_after(&seq), Some(Phase::Ended));
    }

    #[test]
    fn no_hooks_means_no_state_and_unknown_events_change_nothing() {
        assert_eq!(state_after(&[]), None);
        assert_eq!(state_after(&[("PreToolUse", None), ("Notification", None)]), None);
    }

    #[test]
    fn names_the_source_event_and_time() {
        let h = hook("Notification", Some("permission_prompt"));
        let s = fold([(42, &h)]).unwrap();
        assert_eq!(s.source, "claude hooks");
        assert_eq!(s.event, "Notification");
        assert_eq!(s.detail.as_deref(), Some("permission_prompt"));
        assert_eq!(s.at_ns, 42);
    }

    #[test]
    fn judges_resume_confirmation() {
        let id = "4b41da33-24de-41bb-92ef-77589bb77d13";
        let start = |sid: &str, source: &str| AgentHook {
            hook_event_name: "SessionStart".into(),
            session_id: Some(sid.into()),
            source: Some(source.into()),
            ..hook("SessionStart", None)
        };
        let v = |h: Option<&AgentHook>, ms, running, code| judge_resume(id, h, ms, running, code).verdict;
        assert_eq!(v(Some(&start(id, "resume")), 1_000, true, None), Verdict::Confirmed);
        assert_eq!(v(Some(&start(id, "startup")), 1_000, true, None), Verdict::Mismatch);
        assert_eq!(v(Some(&start("other", "resume")), 1_000, false, Some(0)), Verdict::Mismatch);
        assert_eq!(v(None, 1_000, true, None), Verdict::Pending);
        assert_eq!(v(None, RESUME_TIMEOUT_MS, true, None), Verdict::Unconfirmed);
        let gone = judge_resume(id, None, 500, false, Some(1));
        assert_eq!(gone.verdict, Verdict::Unconfirmed);
        assert!(gone.reason.contains("code 1"), "{}", gone.reason);
    }
}
