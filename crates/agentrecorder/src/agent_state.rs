use agentrecorder_store::{AgentHook, Body, Store, kind};
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Working,
    NeedsInput,
    Idle,
    Ended,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Working => "working",
            Phase::NeedsInput => "needs_input",
            Phase::Idle => "idle",
            Phase::Ended => "ended",
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

pub fn from_store(store: &Store, session_id: &str, process_running: bool) -> Result<Option<AgentState>> {
    let mut hooks = Vec::new();
    store.for_each_event(session_id, Some(&[kind::AGENT_HOOK]), |e| {
        if let Some(Body::AgentHook(h)) = e.body() {
            hooks.push((e.ts_ns, h));
        }
        Ok(())
    })?;
    let mut state = fold(hooks.iter().map(|(t, h)| (*t, h.as_ref())));
    if let Some(s) = state.as_mut().filter(|s| !process_running && s.state != Phase::Ended) {
        s.state = Phase::Ended;
        s.source = format!("{} and session status", s.source);
        s.detail = Some(format!("process not running; last hook {}", s.event));
    }
    Ok(state)
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
}
