use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod kind {
    pub const SESSION_CREATED: &str = "session.created";
    pub const SESSION_ATTACHED: &str = "session.attached";
    pub const SESSION_DETACHED: &str = "session.detached";
    pub const SESSION_STOP_REQUESTED: &str = "session.stop_requested";
    pub const SESSION_TERMINATED: &str = "session.terminated";
    pub const PROCESS_STARTED: &str = "process.started";
    pub const PROCESS_EXITED: &str = "process.exited";
    pub const TERMINAL_OUTPUT: &str = "terminal.output";
    pub const TERMINAL_INPUT: &str = "terminal.input";
    pub const TERMINAL_RESIZED: &str = "terminal.resized";
    pub const TERMINAL_SNAPSHOT: &str = "terminal.snapshot";
    pub const TERMINAL_NOTIFICATION: &str = "terminal.notification";
    pub const GIT_SNAPSHOT: &str = "git.snapshot";
    pub const RUNTIME_WARNING: &str = "runtime.warning";
    pub const RUNTIME_ERROR: &str = "runtime.error";
    pub const AGENT_HOOK: &str = "agent.hook";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload")]
pub enum Body {
    #[serde(rename = "session.created")]
    SessionCreated {
        command: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
        record_output: bool,
        record_input: bool,
        supervisor_pid: u32,
        restarted_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_record_bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        term: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_session_id: Option<String>,
    },
    #[serde(rename = "process.started")]
    ProcessStarted { pid: i32, command: Vec<String> },
    #[serde(rename = "session.attached")]
    SessionAttached { client: u64, client_pid: u32, rows: u16, cols: u16 },
    #[serde(rename = "session.detached")]
    SessionDetached { client: u64, reason: DetachReason },
    #[serde(rename = "session.stop_requested")]
    StopRequested { signal: i32, grace_ms: u64, origin: String },
    #[serde(rename = "process.exited")]
    ProcessExited { pid: i32, exit_code: Option<i32>, signal: Option<i32>, duration_ms: i64 },
    #[serde(rename = "session.terminated")]
    SessionTerminated {
        reason: TerminationReason,
        exit_code: Option<i32>,
        signal: Option<i32>,
        #[serde(default)]
        dropped_events: u64,
    },
    #[serde(rename = "terminal.output")]
    TerminalOutput,
    #[serde(rename = "terminal.input")]
    TerminalInput,
    #[serde(rename = "terminal.resized")]
    TerminalResized { rows: u16, cols: u16 },
    #[serde(rename = "terminal.snapshot")]
    TerminalSnapshot { rows: u16, cols: u16, reason: SnapshotReason },
    #[serde(rename = "terminal.notification")]
    TerminalNotification { source: NotificationSource, text: String },
    #[serde(rename = "git.snapshot")]
    GitSnapshot { phase: GitPhase, head: Option<String>, dirty_paths: Option<u64> },
    #[serde(rename = "runtime.warning")]
    RuntimeWarning(Notice),
    #[serde(rename = "runtime.error")]
    RuntimeError(Notice),
    #[serde(rename = "agent.hook")]
    AgentHook(Box<AgentHook>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHook {
    pub agent: String,
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetachReason {
    ClientRequest,
    ClientDisconnected,
    DetachRequested,
    AttachedElsewhere,
    ClientTooSlow,
    ProcessExited,
    PtyWriteFailed,
    ProtocolError,
}

impl DetachReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DetachReason::ClientRequest => "client_request",
            DetachReason::ClientDisconnected => "client_disconnected",
            DetachReason::DetachRequested => "detach_requested",
            DetachReason::AttachedElsewhere => "attached_elsewhere",
            DetachReason::ClientTooSlow => "client_too_slow",
            DetachReason::ProcessExited => "process_exited",
            DetachReason::PtyWriteFailed => "pty_write_failed",
            DetachReason::ProtocolError => "protocol_error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    Interval,
    AfterDrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSource {
    Osc9,
    Osc777,
    Osc99,
    Bell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitPhase {
    Start,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    ProcessExited,
    Stopped,
    SpawnFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub code: String,
    pub message: String,
    #[serde(flatten)]
    pub details: Map<String, Value>,
}

impl Notice {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Notice { code: code.to_string(), message: message.into(), details: Map::new() }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_string(), value.into());
        self
    }
}

impl Body {
    pub fn kind(&self) -> &'static str {
        match self {
            Body::SessionCreated { .. } => kind::SESSION_CREATED,
            Body::ProcessStarted { .. } => kind::PROCESS_STARTED,
            Body::SessionAttached { .. } => kind::SESSION_ATTACHED,
            Body::SessionDetached { .. } => kind::SESSION_DETACHED,
            Body::StopRequested { .. } => kind::SESSION_STOP_REQUESTED,
            Body::ProcessExited { .. } => kind::PROCESS_EXITED,
            Body::SessionTerminated { .. } => kind::SESSION_TERMINATED,
            Body::TerminalOutput => kind::TERMINAL_OUTPUT,
            Body::TerminalInput => kind::TERMINAL_INPUT,
            Body::TerminalResized { .. } => kind::TERMINAL_RESIZED,
            Body::TerminalSnapshot { .. } => kind::TERMINAL_SNAPSHOT,
            Body::TerminalNotification { .. } => kind::TERMINAL_NOTIFICATION,
            Body::GitSnapshot { .. } => kind::GIT_SNAPSHOT,
            Body::RuntimeWarning(_) => kind::RUNTIME_WARNING,
            Body::RuntimeError(_) => kind::RUNTIME_ERROR,
            Body::AgentHook(_) => kind::AGENT_HOOK,
        }
    }

    pub(crate) fn payload(&self) -> Option<String> {
        let v = serde_json::to_value(self).expect("event bodies serialize");
        v.get("payload").filter(|p| !p.is_null()).map(Value::to_string)
    }

    pub(crate) fn decode(kind: &str, payload: &Value) -> Option<Body> {
        let mut obj = Map::new();
        obj.insert("event_type".into(), Value::String(kind.to_string()));
        if !payload.is_null() {
            obj.insert("payload".into(), payload.clone());
        }
        serde_json::from_value(Value::Object(obj)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Body> {
        vec![
            Body::SessionCreated {
                command: vec!["bash".into()],
                cwd: Some("/tmp".into()),
                rows: 24,
                cols: 80,
                record_output: true,
                record_input: false,
                supervisor_pid: 1,
                restarted_from: None,
                max_record_bytes: Some(10),
                term: Some("xterm-256color".into()),
                agent_session_id: Some("4b41da33-24de-41bb-92ef-77589bb77d13".into()),
            },
            Body::ProcessStarted { pid: 2, command: vec!["bash".into()] },
            Body::SessionAttached { client: 1, client_pid: 3, rows: 24, cols: 80 },
            Body::SessionDetached { client: 1, reason: DetachReason::ClientTooSlow },
            Body::StopRequested { signal: 1, grace_ms: 5000, origin: "client".into() },
            Body::ProcessExited { pid: 2, exit_code: Some(0), signal: None, duration_ms: 10 },
            Body::SessionTerminated {
                reason: TerminationReason::Stopped,
                exit_code: None,
                signal: Some(1),
                dropped_events: 0,
            },
            Body::TerminalOutput,
            Body::TerminalInput,
            Body::TerminalResized { rows: 50, cols: 132 },
            Body::TerminalSnapshot { rows: 24, cols: 80, reason: SnapshotReason::AfterDrop },
            Body::TerminalNotification { source: NotificationSource::Osc9, text: "done".into() },
            Body::GitSnapshot { phase: GitPhase::Start, head: Some("abc".into()), dirty_paths: Some(2) },
            Body::RuntimeWarning(Notice::new("events_dropped", "x").with("dropped_events", 3)),
            Body::RuntimeError(Notice::new("spawn_failed", "y")),
            Body::AgentHook(Box::new(AgentHook {
                agent: "claude".into(),
                hook_event_name: "Notification".into(),
                session_id: Some("4b41da33-24de-41bb-92ef-77589bb77d13".into()),
                notification_type: Some("permission_prompt".into()),
                message: Some("Claude needs your permission".into()),
                ..AgentHook::default()
            })),
        ]
    }

    #[test]
    fn every_body_round_trips_through_kind_and_payload() {
        for body in all() {
            let payload = body.payload().map(|p| serde_json::from_str(&p).unwrap()).unwrap_or(Value::Null);
            assert_eq!(Body::decode(body.kind(), &payload).as_ref(), Some(&body), "{}", body.kind());
            let tagged = serde_json::to_value(&body).unwrap();
            assert_eq!(tagged["event_type"], body.kind());
        }
    }

    #[test]
    fn notice_details_are_flat_fields() {
        let body = Body::RuntimeWarning(Notice::new("events_dropped", "m").with("dropped_bytes", 7));
        let p: Value = serde_json::from_str(&body.payload().unwrap()).unwrap();
        assert_eq!(p["dropped_bytes"], 7);
        assert_eq!(p["code"], "events_dropped");
    }

    #[test]
    fn unknown_kinds_do_not_decode() {
        assert!(Body::decode("agent.tool_call", &Value::Null).is_none());
    }

    #[test]
    fn spawn_failure_without_counters_decodes() {
        let p = serde_json::json!({"reason": "spawn_failed", "exit_code": 127, "signal": null});
        assert!(matches!(
            Body::decode(kind::SESSION_TERMINATED, &p),
            Some(Body::SessionTerminated { reason: TerminationReason::SpawnFailed, dropped_events: 0, .. })
        ));
    }
}
