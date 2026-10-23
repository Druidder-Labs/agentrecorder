#![allow(clippy::missing_errors_doc, reason = "every fallible function fails with the `Error` variants of this crate")]

mod events;
mod time;

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;

pub use events::{
    AgentHook, Body, ChangedPath, DetachReason, GIT_MAX_PATHS, GitCommit, GitPhase, HookUsage, Notice,
    NotificationSource, SnapshotReason, TerminationReason, ToolSummary, kind,
};
pub use time::{format_rfc3339, format_rfc3339_seconds, now_unix_ns, parse_rfc3339};

pub const EVENT_SCHEMA_VERSION: u32 = 1;

const DB_USER_VERSION: i64 = 6;
const PAGE_ROWS: usize = 256;
const WAL_SIZE_LIMIT: i64 = 4 << 20;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const INIT_RETRY_FOR: Duration = Duration::from_secs(10);
const INIT_RETRY_STEP: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletion {
    Scrubbed,
    WalPending,
}

#[derive(Debug)]
pub enum Error {
    Busy,
    Corrupt { path: PathBuf },
    NewerSchema { found: i64, supported: i64, binary: &'static str },
    InvalidId(String),
    NotFound(String),
    Ambiguous { prefix: String, ids: Vec<String> },
    NoSessions,
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
}

impl Error {
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Busy)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Busy => f.write_str("database is locked"),
            Error::Corrupt { path } => write!(
                f,
                "event store {} is corrupt; move it aside to start a new one (existing recordings stay in the moved file)",
                path.display()
            ),
            Error::NewerSchema { found, supported, binary } => write!(
                f,
                "written by a newer witnessed (store schema {found}; this binary is {binary} and reads up to schema {supported}); \
                 update this binary with install.sh, then run the command again; the store was not changed"
            ),
            Error::InvalidId(id) => write!(f, "invalid session id {id:?}"),
            Error::NotFound(id) => write!(f, "no session matches {id:?}; run `witnessed sessions` to list sessions"),
            Error::Ambiguous { prefix, ids } => write!(f, "{prefix:?} is ambiguous: {}", ids.join(", ")),
            Error::NoSessions => f.write_str("no sessions yet; record one with: witnessed run -- claude"),
            Error::Sqlite(e) => fmt::Display::fmt(e, f),
            Error::Json(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Sqlite(e) => Some(e),
            Error::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::SqliteFailure(f, _)
                if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) =>
            {
                Error::Busy
            }
            e => Error::Sqlite(e),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

type Res<T> = std::result::Result<T, Error>;

fn sqlite_code(e: &anyhow::Error) -> Option<rusqlite::ErrorCode> {
    e.chain().find_map(|c| match c.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(f, _)) => Some(f.code),
        _ => None,
    })
}

#[must_use]
pub fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Error>().is_some_and(Error::is_transient))
        || e.downcast_ref::<Error>().is_some_and(Error::is_transient)
        || matches!(sqlite_code(e), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

fn is_corrupt(e: &Error) -> bool {
    matches!(
        e,
        Error::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Running,
    Exited,
    Lost,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Exited => "exited",
            Status::Lost => "lost",
        }
    }

    fn parse(s: &str) -> Option<Status> {
        match s {
            "starting" => Some(Status::Starting),
            "running" => Some(Status::Running),
            "exited" => Some(Status::Exited),
            "lost" => Some(Status::Lost),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LostReason {
    HostRestarted,
    SupervisorTerminated,
}

impl LostReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LostReason::HostRestarted => "host_restarted",
            LostReason::SupervisorTerminated => "supervisor_terminated",
        }
    }

    fn parse(s: &str) -> Option<LostReason> {
        match s {
            "host_restarted" => Some(LostReason::HostRestarted),
            "supervisor_terminated" => Some(LostReason::SupervisorTerminated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub status: Status,
    pub created_at_ns: i64,
    pub started_at_ns: Option<i64>,
    pub ended_at_ns: Option<i64>,
    pub supervisor_pid: Option<i64>,
    pub child_pid: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    pub rows: i64,
    pub cols: i64,
    pub attached: bool,
    pub record_output: bool,
    pub record_input: bool,
    pub boot_time: Option<i64>,
    pub lost_reason: Option<LostReason>,
    pub restarted_from: Option<String>,
    pub agent_session_id: Option<String>,
    pub anchor_token_digest: Option<String>,
}

pub struct NewSession<'a> {
    pub id: &'a str,
    pub command: &'a [String],
    pub cwd: Option<&'a str>,
    pub created_at_ns: i64,
    pub supervisor_pid: i64,
    pub rows: u16,
    pub cols: u16,
    pub record_output: bool,
    pub record_input: bool,
    pub boot_time: Option<i64>,
    pub restarted_from: Option<&'a str>,
    pub agent_session_id: Option<&'a str>,
    pub anchor_token_digest: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub ts_ns: i64,
    pub mono_ns: i64,
    pub body: Body,
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub session_id: String,
    pub seq: i64,
    pub ts_ns: i64,
    pub mono_ns: i64,
    pub kind: String,
    pub payload: Value,
    pub data: Option<Vec<u8>>,
}

impl Event {
    #[must_use]
    pub fn body(&self) -> Option<Body> {
        Body::decode(&self.kind, &self.payload)
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        use base64::Engine;
        let mut v = serde_json::json!({
            "schema_version": EVENT_SCHEMA_VERSION,
            "event_id": format!("{}-{}", self.session_id, self.seq),
            "session_id": self.session_id,
            "seq": self.seq,
            "timestamp": format_rfc3339(self.ts_ns),
            "mono_ns": self.mono_ns,
            "event_type": self.kind,
            "payload": self.payload,
        });
        if let Some(data) = &self.data {
            v["data_b64"] = Value::String(base64::engine::general_purpose::STANDARD.encode(data));
        }
        v
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KindCount {
    pub kind: String,
    pub count: i64,
    pub bytes: i64,
}

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Annotation {
    pub moment: String,
    pub verdict: String,
    pub note: Option<String>,
    pub updated_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiffRow {
    pub base: String,
    pub diff: Vec<u8>,
}

macro_rules! session_cols {
    () => {
        "id, command, cwd, status, created_at_ns, started_at_ns, ended_at_ns,
    supervisor_pid, child_pid, exit_code, exit_signal, rows, cols, attached, record_output,
    record_input, boot_time, lost_reason, restarted_from, agent_session_id, anchor_token_digest"
    };
}

const SQL_SESSION_BY_ID: &str = concat!("SELECT ", session_cols!(), " FROM sessions WHERE id = ?1");
const SQL_SESSIONS: &str = concat!("SELECT ", session_cols!(), " FROM sessions ORDER BY created_at_ns");
const SQL_LAST_SESSION: &str =
    concat!("SELECT ", session_cols!(), " FROM sessions ORDER BY created_at_ns DESC LIMIT 1");
const SQL_SESSIONS_BY_PREFIX: &str =
    concat!("SELECT ", session_cols!(), " FROM sessions WHERE id >= ?1 AND id < ?1 || X'7F' ORDER BY id");
const SQL_INSERT_SESSION: &str =
    "INSERT INTO sessions (id, command, cwd, status, created_at_ns, supervisor_pid, rows, cols,
        record_output, record_input, boot_time, restarted_from, agent_session_id, anchor_token_digest)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";
const SQL_MARK_STARTED: &str = "UPDATE sessions SET status = 'running', child_pid = ?2, started_at_ns = ?3
     WHERE id = ?1 AND status = 'starting'";
const SQL_SET_ATTACHED: &str = "UPDATE sessions SET attached = ?2 WHERE id = ?1";
const SQL_SET_SIZE: &str = "UPDATE sessions SET rows = ?2, cols = ?3 WHERE id = ?1";
const SQL_MARK_EXITED: &str =
    "UPDATE sessions SET status = 'exited', ended_at_ns = ?2, exit_code = ?3, exit_signal = ?4,
        attached = 0, lost_reason = NULL WHERE id = ?1 AND status IN ('starting', 'running', 'lost')";
const SQL_MARK_LOST: &str = "UPDATE sessions SET status = 'lost', lost_reason = ?2, attached = 0
     WHERE id = ?1 AND status IN ('starting', 'running')";
const SQL_INSERT_EVENT: &str = "INSERT INTO events (session_id, seq, ts_ns, mono_ns, kind, payload, data, data_len)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
const SQL_EVENT_TIMES: &str = "SELECT seq, mono_ns FROM events WHERE session_id = ?1 AND kind = ?2 ORDER BY seq";
const SQL_LAST_SEQ_AT_OR_BEFORE: &str =
    "SELECT MAX(seq) FROM events WHERE session_id = ?1 AND kind = ?2 AND mono_ns <= ?3";
const SQL_EVENT_COUNTS: &str = "SELECT kind, COUNT(*), COALESCE(SUM(data_len), 0) FROM events
     WHERE session_id = ?1 GROUP BY kind ORDER BY kind";
const SQL_PUT_SESSION_DIFF: &str =
    "INSERT OR REPLACE INTO session_diffs (session_id, base, diff, created_at_ns) VALUES (?1, ?2, ?3, ?4)";
const SQL_SESSION_DIFF: &str = "SELECT base, diff FROM session_diffs WHERE session_id = ?1";
const SQL_SET_ANNOTATION: &str = "INSERT OR REPLACE INTO annotations (session_id, moment, verdict, note, updated_at_ns)
     VALUES (?1, ?2, ?3, ?4, ?5)";
const SQL_CLEAR_ANNOTATION: &str = "DELETE FROM annotations WHERE session_id = ?1 AND moment = ?2";
const SQL_ANNOTATIONS: &str =
    "SELECT moment, verdict, note, updated_at_ns FROM annotations WHERE session_id = ?1 ORDER BY moment";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(Error::from)
            .with_context(|| format!("opening event store {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(Error::from)?;
        let mut store = Store { conn };
        let deadline = std::time::Instant::now() + INIT_RETRY_FOR;
        loop {
            match store.init() {
                Ok(()) => return Ok(store),
                Err(e) if e.is_transient() && std::time::Instant::now() < deadline => {
                    std::thread::sleep(INIT_RETRY_STEP);
                }
                Err(e) if is_corrupt(&e) => {
                    return Err(e).context(Error::Corrupt { path: path.to_path_buf() });
                }
                Err(e) => return Err(e).with_context(|| format!("opening event store {}", path.display())),
            }
        }
    }

    fn init(&mut self) -> Res<()> {
        let version: i64 = self.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            self.conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        }
        self.conn.pragma_update(None, "journal_mode", "WAL")?;
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.conn.pragma_update(None, "secure_delete", "ON")?;
        self.conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
        if version == DB_USER_VERSION {
            return Ok(());
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > DB_USER_VERSION {
            return Err(Error::NewerSchema {
                found: version,
                supported: DB_USER_VERSION,
                binary: env!("CARGO_PKG_VERSION"),
            });
        }
        if version == 0 {
            tx.execute_batch(SCHEMA)?;
            version = SCHEMA_VERSION;
        }
        for (to, sql) in MIGRATIONS.iter().filter(|(to, _)| *to > version) {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", to)?;
        }
        tx.pragma_update(None, "user_version", DB_USER_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    pub fn create_session(&self, s: &NewSession<'_>) -> Result<()> {
        let command = serde_json::to_string(s.command).map_err(Error::from)?;
        self.conn
            .prepare_cached(SQL_INSERT_SESSION)
            .and_then(|mut stmt| {
                stmt.execute(params![
                    s.id,
                    command,
                    s.cwd,
                    Status::Starting.as_str(),
                    s.created_at_ns,
                    s.supervisor_pid,
                    s.rows,
                    s.cols,
                    s.record_output,
                    s.record_input,
                    s.boot_time,
                    s.restarted_from,
                    s.agent_session_id,
                    s.anchor_token_digest,
                ])
            })
            .map_err(Error::from)?;
        Ok(())
    }

    fn execute_cached(&self, sql: &str, args: impl rusqlite::Params) -> Res<usize> {
        Ok(self.conn.prepare_cached(sql)?.execute(args)?)
    }

    pub fn mark_started(&self, id: &str, child_pid: i64, started_at_ns: i64) -> Result<()> {
        self.execute_cached(SQL_MARK_STARTED, params![id, child_pid, started_at_ns])?;
        Ok(())
    }

    pub fn set_attached(&self, id: &str, attached: bool) -> Result<()> {
        self.execute_cached(SQL_SET_ATTACHED, params![id, attached])?;
        Ok(())
    }

    pub fn set_size(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.execute_cached(SQL_SET_SIZE, params![id, rows, cols])?;
        Ok(())
    }

    pub fn mark_exited(
        &self,
        id: &str,
        ended_at_ns: i64,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) -> Result<()> {
        self.execute_cached(SQL_MARK_EXITED, params![id, ended_at_ns, exit_code, exit_signal])?;
        Ok(())
    }

    pub fn mark_lost(&self, id: &str, reason: LostReason) -> Result<bool> {
        Ok(self.execute_cached(SQL_MARK_LOST, params![id, reason.as_str()])? > 0)
    }

    pub fn append_events(&mut self, session_id: &str, first_seq: i64, events: &[NewEvent]) -> Result<i64> {
        Ok(self.append(session_id, first_seq, events)?)
    }

    fn append(&mut self, session_id: &str, first_seq: i64, events: &[NewEvent]) -> Res<i64> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut seq = first_seq;
        {
            let mut stmt = tx.prepare_cached(SQL_INSERT_EVENT)?;
            for e in events {
                let len = i64::try_from(e.data.as_ref().map_or(0, Vec::len)).unwrap_or(i64::MAX);
                stmt.execute(params![
                    session_id,
                    seq,
                    e.ts_ns,
                    e.mono_ns,
                    e.body.kind(),
                    e.body.payload(),
                    e.data,
                    len
                ])?;
                seq += 1;
            }
        }
        tx.commit()?;
        Ok(seq)
    }

    pub fn resolve(&self, id_or_prefix: &str) -> Result<Session> {
        Ok(self.lookup(id_or_prefix)?)
    }

    fn lookup(&self, id_or_prefix: &str) -> Res<Session> {
        if id_or_prefix == "last" || id_or_prefix == "latest" {
            let mut stmt = self.conn.prepare_cached(SQL_LAST_SESSION)?;
            return match stmt.query_map([], row_to_session)?.next() {
                Some(s) => Ok(s?),
                None => Err(Error::NoSessions),
            };
        }
        if id_or_prefix.is_empty() || !id_or_prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Error::InvalidId(id_or_prefix.to_string()));
        }
        let mut stmt = self.conn.prepare_cached(SQL_SESSIONS_BY_PREFIX)?;
        let mut found: Vec<Session> =
            stmt.query_map([id_or_prefix], row_to_session)?.collect::<rusqlite::Result<_>>()?;
        if found.is_empty() {
            return Err(Error::NotFound(id_or_prefix.to_string()));
        }
        let pick = if found.len() == 1 { Some(0) } else { found.iter().position(|s| s.id == id_or_prefix) };
        match pick {
            Some(i) => Ok(found.swap_remove(i)),
            None => Err(Error::Ambiguous {
                prefix: id_or_prefix.to_string(),
                ids: found.into_iter().map(|s| s.id).collect(),
            }),
        }
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        let row = self
            .conn
            .prepare_cached(SQL_SESSION_BY_ID)
            .and_then(|mut stmt| stmt.query_row([id], row_to_session).optional())
            .map_err(Error::from)?;
        Ok(row)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let rows = self
            .conn
            .prepare_cached(SQL_SESSIONS)
            .and_then(|mut stmt| stmt.query_map([], row_to_session)?.collect::<rusqlite::Result<_>>())
            .map_err(Error::from)?;
        Ok(rows)
    }

    pub fn id_exists(&self, id: &str) -> Result<bool> {
        Ok(self.session(id)?.is_some())
    }

    pub fn for_each_event(
        &self,
        session_id: &str,
        kinds: Option<&[&str]>,
        f: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        self.for_each_event_from(session_id, -1, kinds, f)
    }

    pub fn for_each_event_from(
        &self,
        session_id: &str,
        after_seq: i64,
        kinds: Option<&[&str]>,
        mut f: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        for e in self.events(session_id, after_seq, kinds) {
            f(e?)?;
        }
        Ok(())
    }

    /// The events of a session after `after_seq`, oldest first, read 256 rows per short read transaction so a
    /// writer is never blocked and a checkpoint can run between pages; a page fetched after an append includes
    /// the appended rows, so a reader sees a contiguous prefix of the session at every point.
    pub fn events(
        &self,
        session_id: &str,
        after_seq: i64,
        kinds: Option<&[&str]>,
    ) -> impl Iterator<Item = Result<Event>> + '_ {
        let mut sql = String::from(
            "SELECT session_id, seq, ts_ns, mono_ns, kind, payload, data FROM events
             WHERE session_id = ?1 AND seq > ?2",
        );
        if let Some(ks) = kinds {
            let marks: Vec<String> = (0..ks.len()).map(|i| format!("?{}", i + 3)).collect();
            let _ = write!(sql, " AND kind IN ({})", marks.join(", "));
        }
        let _ = write!(sql, " ORDER BY seq LIMIT {PAGE_ROWS}");
        Events {
            store: self,
            sql,
            session_id: session_id.to_string(),
            kinds: kinds.map_or_else(Vec::new, |ks| ks.iter().map(ToString::to_string).collect()),
            last: after_seq,
            page: Vec::new().into_iter(),
            done: false,
        }
    }

    fn page(&self, sql: &str, session_id: &str, last: i64, kinds: &[String]) -> Res<Vec<Event>> {
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&session_id, &last];
        args.extend(kinds.iter().map(|k| k as &dyn rusqlite::ToSql));
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(args.as_slice(), |row| {
            let payload: Option<String> = row.get(5)?;
            Ok(Event {
                session_id: row.get(0)?,
                seq: row.get(1)?,
                ts_ns: row.get(2)?,
                mono_ns: row.get(3)?,
                kind: row.get(4)?,
                payload: match payload {
                    Some(p) => serde_json::from_str(&p).unwrap_or(Value::String(p)),
                    None => Value::Null,
                },
                data: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<Event>>>()?)
    }

    /// Sequence numbers and monotonic times of the events of one kind, without their payloads or data.
    pub fn event_times(&self, session_id: &str, kind: &str) -> Result<Vec<(i64, i64)>> {
        let rows = self
            .conn
            .prepare_cached(SQL_EVENT_TIMES)
            .and_then(|mut stmt| {
                stmt.query_map(params![session_id, kind], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()
            })
            .map_err(Error::from)?;
        Ok(rows)
    }

    pub fn last_seq_at_or_before(&self, session_id: &str, kind: &str, mono_ns: i64) -> Result<Option<i64>> {
        let seq = self
            .conn
            .prepare_cached(SQL_LAST_SEQ_AT_OR_BEFORE)
            .and_then(|mut stmt| stmt.query_row(params![session_id, kind, mono_ns], |r| r.get(0)))
            .map_err(Error::from)?;
        Ok(seq)
    }

    pub fn event_counts(&self, session_id: &str) -> Result<Vec<KindCount>> {
        let rows = self
            .conn
            .prepare_cached(SQL_EVENT_COUNTS)
            .and_then(|mut stmt| {
                stmt.query_map([session_id], |r| Ok(KindCount { kind: r.get(0)?, count: r.get(1)?, bytes: r.get(2)? }))?
                    .collect::<rusqlite::Result<_>>()
            })
            .map_err(Error::from)?;
        Ok(rows)
    }

    pub fn delete_session(&mut self, id: &str) -> Result<Deletion> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(Error::from)?;
        tx.execute("DELETE FROM events WHERE session_id = ?1", [id]).map_err(Error::from)?;
        tx.execute("DELETE FROM annotations WHERE session_id = ?1", [id]).map_err(Error::from)?;
        tx.execute("DELETE FROM session_diffs WHERE session_id = ?1", [id]).map_err(Error::from)?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [id]).map_err(Error::from)?;
        tx.commit().map_err(Error::from)?;
        self.checkpoint()
    }

    pub fn delete_orphan_events(&mut self) -> Result<usize> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(Error::from)?;
        let n = tx
            .execute("DELETE FROM events WHERE session_id NOT IN (SELECT id FROM sessions)", [])
            .map_err(Error::from)?;
        tx.execute("DELETE FROM annotations WHERE session_id NOT IN (SELECT id FROM sessions)", [])
            .map_err(Error::from)?;
        tx.execute("DELETE FROM session_diffs WHERE session_id NOT IN (SELECT id FROM sessions)", [])
            .map_err(Error::from)?;
        tx.commit().map_err(Error::from)?;
        Ok(n)
    }

    /// Stores the diff of the work tree at session end, so outcomes can be measured after the work tree changes.
    pub fn put_session_diff(&self, session_id: &str, base: &str, diff: &[u8], at_ns: i64) -> Result<()> {
        self.execute_cached(SQL_PUT_SESSION_DIFF, params![session_id, base, diff, at_ns])?;
        Ok(())
    }

    pub fn session_diff(&self, session_id: &str) -> Result<Option<SessionDiffRow>> {
        let row = self
            .conn
            .prepare_cached(SQL_SESSION_DIFF)
            .and_then(|mut stmt| {
                stmt.query_row([session_id], |r| Ok(SessionDiffRow { base: r.get(0)?, diff: r.get(1)? })).optional()
            })
            .map_err(Error::from)?;
        Ok(row)
    }

    pub fn set_annotation(
        &self,
        session_id: &str,
        moment: &str,
        verdict: &str,
        note: Option<&str>,
        at_ns: i64,
    ) -> Result<()> {
        self.execute_cached(SQL_SET_ANNOTATION, params![session_id, moment, verdict, note, at_ns])?;
        Ok(())
    }

    pub fn clear_annotation(&self, session_id: &str, moment: &str) -> Result<bool> {
        Ok(self.execute_cached(SQL_CLEAR_ANNOTATION, [session_id, moment])? > 0)
    }

    pub fn annotations(&self, session_id: &str) -> Result<Vec<Annotation>> {
        let rows = self
            .conn
            .prepare_cached(SQL_ANNOTATIONS)
            .and_then(|mut stmt| {
                stmt.query_map([session_id], |r| {
                    Ok(Annotation { moment: r.get(0)?, verdict: r.get(1)?, note: r.get(2)?, updated_at_ns: r.get(3)? })
                })?
                .collect::<rusqlite::Result<_>>()
            })
            .map_err(Error::from)?;
        Ok(rows)
    }

    pub fn checkpoint(&self) -> Result<Deletion> {
        let busy = self.truncate_wal()?;
        Ok(if busy == 0 { Deletion::Scrubbed } else { Deletion::WalPending })
    }

    fn truncate_wal(&self) -> Res<i64> {
        let mut vacuum = self.conn.prepare("PRAGMA incremental_vacuum")?;
        let mut freed = vacuum.query([])?;
        while freed.next()?.is_some() {}
        drop(freed);
        drop(vacuum);
        Ok(self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?)
    }
}

struct Events<'a> {
    store: &'a Store,
    sql: String,
    session_id: String,
    kinds: Vec<String>,
    last: i64,
    page: std::vec::IntoIter<Event>,
    done: bool,
}

impl Iterator for Events<'_> {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(e) = self.page.next() {
                self.last = e.seq;
                return Some(Ok(e));
            }
            if self.done {
                return None;
            }
            match self.store.page(&self.sql, &self.session_id, self.last, &self.kinds) {
                Ok(page) => {
                    self.done = page.len() < PAGE_ROWS;
                    self.page = page.into_iter();
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e.into()));
                }
            }
        }
    }
}

const SCHEMA: &str = "
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    command         TEXT NOT NULL,
    cwd             TEXT,
    status          TEXT NOT NULL,
    created_at_ns   INTEGER NOT NULL,
    started_at_ns   INTEGER,
    ended_at_ns     INTEGER,
    supervisor_pid  INTEGER,
    child_pid       INTEGER,
    exit_code       INTEGER,
    exit_signal     INTEGER,
    rows            INTEGER NOT NULL,
    cols            INTEGER NOT NULL,
    attached        INTEGER NOT NULL DEFAULT 0,
    record_output   INTEGER NOT NULL,
    record_input    INTEGER NOT NULL,
    boot_time       INTEGER,
    lost_reason     TEXT,
    restarted_from  TEXT
);
CREATE TABLE events (
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    ts_ns       INTEGER NOT NULL,
    mono_ns     INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    payload     TEXT,
    data        BLOB,
    data_len    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;
";

/// `SCHEMA` creates the tables as of this version; `MIGRATIONS` carries every later step.
const SCHEMA_VERSION: i64 = 2;

const MIGRATIONS: &[(i64, &str)] =
    &[(2, MIGRATE_1_TO_2), (3, MIGRATE_2_TO_3), (4, MIGRATE_3_TO_4), (5, MIGRATE_4_TO_5), (6, MIGRATE_5_TO_6)];

const _: () = assert!(MIGRATIONS[MIGRATIONS.len() - 1].0 == DB_USER_VERSION);

const MIGRATE_1_TO_2: &str = "
ALTER TABLE events ADD COLUMN data_len INTEGER NOT NULL DEFAULT 0;
UPDATE events SET data_len = COALESCE(LENGTH(data), 0);
";

const MIGRATE_2_TO_3: &str = "
CREATE INDEX IF NOT EXISTS events_by_kind ON events (session_id, kind, seq, data_len);
";

const MIGRATE_3_TO_4: &str = "
ALTER TABLE sessions ADD COLUMN agent_session_id TEXT;
";

const MIGRATE_4_TO_5: &str = "
ALTER TABLE sessions ADD COLUMN anchor_token_digest TEXT;
";

const MIGRATE_5_TO_6: &str = "
CREATE TABLE IF NOT EXISTS annotations (
    session_id     TEXT NOT NULL,
    moment         TEXT NOT NULL,
    verdict        TEXT NOT NULL,
    note           TEXT,
    updated_at_ns  INTEGER NOT NULL,
    PRIMARY KEY (session_id, moment)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS session_diffs (
    session_id     TEXT PRIMARY KEY,
    base           TEXT NOT NULL,
    diff           BLOB NOT NULL,
    created_at_ns  INTEGER NOT NULL
);
";

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let command: String = r.get(1)?;
    Ok(Session {
        id: r.get(0)?,
        command: serde_json::from_str(&command).unwrap_or_else(|_| vec![command]),
        cwd: r.get(2)?,
        status: {
            let raw: String = r.get(3)?;
            Status::parse(&raw).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    format!("unknown status {raw:?}").into(),
                )
            })?
        },
        created_at_ns: r.get(4)?,
        started_at_ns: r.get(5)?,
        ended_at_ns: r.get(6)?,
        supervisor_pid: r.get(7)?,
        child_pid: r.get(8)?,
        exit_code: r.get(9)?,
        exit_signal: r.get(10)?,
        rows: r.get(11)?,
        cols: r.get(12)?,
        attached: r.get(13)?,
        record_output: r.get(14)?,
        record_input: r.get(15)?,
        boot_time: r.get(16)?,
        lost_reason: r.get::<_, Option<String>>(17)?.as_deref().and_then(LostReason::parse),
        restarted_from: r.get(18)?,
        agent_session_id: r.get(19)?,
        anchor_token_digest: r.get(20)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_session(store: &Store, id: &str) {
        let cmd = vec!["bash".to_string()];
        store
            .create_session(&NewSession {
                id,
                command: &cmd,
                cwd: Some("/tmp"),
                created_at_ns: 1,
                supervisor_pid: 1,
                rows: 24,
                cols: 80,
                record_output: true,
                record_input: false,
                boot_time: None,
                restarted_from: None,
                agent_session_id: None,
                anchor_token_digest: None,
            })
            .unwrap();
    }

    fn ev(mono: i64, data: &[u8]) -> NewEvent {
        NewEvent { ts_ns: mono, mono_ns: mono, body: Body::TerminalOutput, data: Some(data.to_vec()) }
    }

    #[test]
    fn events_keep_sequence_order_and_binary_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        let next = store.append_events("abc123", 0, &[ev(5, b"\x00\xff"), ev(3, b"b")]).unwrap();
        assert_eq!(next, 2);
        store.append_events("abc123", next, &[ev(9, b"c")]).unwrap();
        let mut got = vec![];
        store
            .for_each_event("abc123", None, |e| {
                got.push((e.seq, e.data.unwrap()));
                Ok(())
            })
            .unwrap();
        assert_eq!(got, vec![(0, b"\x00\xff".to_vec()), (1, b"b".to_vec()), (2, b"c".to_vec())]);
        let counts = store.event_counts("abc123").unwrap();
        assert_eq!(counts[0].count, 3);
        assert_eq!(counts[0].bytes, 4);
    }

    #[test]
    fn duplicate_sequence_is_rejected_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        store.append_events("abc123", 0, &[ev(1, b"a")]).unwrap();
        assert!(store.append_events("abc123", 1, &[ev(2, b"b"), ev(3, b"c")]).is_ok());
        assert!(store.append_events("abc123", 2, &[ev(4, b"d")]).is_err());
        let mut n = 0;
        store
            .for_each_event("abc123", None, |_| {
                n += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn resolves_unique_prefix_and_rejects_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        new_session(&store, "abd456");
        assert_eq!(store.resolve("abc").unwrap().id, "abc123");
        assert!(store.resolve("ab").is_err());
        assert!(store.resolve("zz").is_err());
        assert!(store.resolve("a%").is_err());
    }

    #[test]
    fn corrupt_file_reports_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        let err = Store::open(&path).err().expect("corrupt store must not open");
        assert!(format!("{err:#}").contains("corrupt"), "{err:#}");
    }

    #[test]
    fn delete_removes_events_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        store.append_events("abc123", 0, &[ev(1, b"secret-token")]).unwrap();
        assert_eq!(store.delete_session("abc123").unwrap(), Deletion::Scrubbed);
        assert!(store.session("abc123").unwrap().is_none());
        drop(store);
        let raw = std::fs::read(dir.path().join("s.db")).unwrap();
        assert!(!raw.windows(12).any(|w| w == b"secret-token"));
    }

    #[test]
    fn annotations_and_session_diffs_round_trip_and_leave_with_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        store.set_annotation("abc123", "m-7", "again", None, 1).unwrap();
        store.set_annotation("abc123", "m-7", "differently", Some("should have run the tests"), 2).unwrap();
        store.set_annotation("abc123", "m-2", "unsure", None, 3).unwrap();
        let got: Vec<(String, String, Option<String>)> =
            store.annotations("abc123").unwrap().into_iter().map(|a| (a.moment, a.verdict, a.note)).collect();
        assert_eq!(
            got,
            [
                ("m-2".into(), "unsure".into(), None),
                ("m-7".into(), "differently".into(), Some("should have run the tests".into()))
            ]
        );
        assert!(store.clear_annotation("abc123", "m-2").unwrap());
        assert!(!store.clear_annotation("abc123", "m-2").unwrap());
        assert!(store.session_diff("abc123").unwrap().is_none());
        store.put_session_diff("abc123", "deadbeef", b"diff --git a/x b/x\nsecret-diff", 4).unwrap();
        let row = store.session_diff("abc123").unwrap().unwrap();
        assert_eq!((row.base.as_str(), row.diff.len()), ("deadbeef", 30));
        store.delete_session("abc123").unwrap();
        assert_eq!(store.annotations("abc123").unwrap(), []);
        assert!(store.session_diff("abc123").unwrap().is_none());
        drop(store);
        let raw = std::fs::read(dir.path().join("s.db")).unwrap();
        assert!(!raw.windows(11).any(|w| w == b"secret-diff"));
    }

    #[test]
    fn migrates_from_every_schema_version() {
        for from in 1..DB_USER_VERSION {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.db");
            {
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch(&SCHEMA.replace("    data_len    INTEGER NOT NULL DEFAULT 0,\n", "")).unwrap();
                for (to, sql) in MIGRATIONS.iter().filter(|(to, _)| *to <= from) {
                    conn.execute_batch(sql).unwrap();
                    conn.pragma_update(None, "user_version", to).unwrap();
                }
                conn.pragma_update(None, "user_version", from).unwrap();
            }
            let store = Store::open(&path).unwrap();
            let v: i64 = store.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
            assert_eq!(v, DB_USER_VERSION, "from {from}");
            new_session(&store, "abc123");
            store.set_annotation("abc123", "m", "again", None, 1).unwrap();
            store.put_session_diff("abc123", "base", b"d", 1).unwrap();
            assert_eq!(store.session("abc123").unwrap().unwrap().anchor_token_digest, None);
        }
    }

    #[test]
    fn migrates_version_1_stores_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&SCHEMA.replace("    data_len    INTEGER NOT NULL DEFAULT 0,\n", "")).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, command, status, created_at_ns, rows, cols, record_output, record_input)
                 VALUES ('abc123', '[\"x\"]', 'exited', 0, 24, 80, 1, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (session_id, seq, ts_ns, mono_ns, kind, data)
                 VALUES ('abc123', 0, 0, 0, 'terminal.output', X'00010203')",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.event_counts("abc123").unwrap()[0].bytes, 4);
        let s = store.session("abc123").unwrap().unwrap();
        assert_eq!((s.agent_session_id, s.anchor_token_digest), (None, None));
        drop(store);
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, DB_USER_VERSION);
    }

    #[test]
    fn a_store_from_a_newer_binary_names_the_versions_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        drop(Store::open(&path).unwrap());
        Connection::open(&path).unwrap().pragma_update(None, "user_version", DB_USER_VERSION + 1).unwrap();
        let err = match Store::open(&path) {
            Ok(_) => panic!("a newer store opened"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains(&format!("opening event store {}", path.display())), "{err}");
        assert!(err.contains(&format!("store schema {}", DB_USER_VERSION + 1)), "{err}");
        assert!(err.contains(&format!("this binary is {}", env!("CARGO_PKG_VERSION"))), "{err}");
        assert!(err.contains("update this binary with install.sh"), "{err}");
        let v: i64 = Connection::open(&path).unwrap().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, DB_USER_VERSION + 1);
    }

    #[test]
    fn concurrent_first_open_initializes_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&path).map(|_| ()).map_err(|e| format!("{e:#}"))
                })
            })
            .collect();
        let failures: Vec<String> = handles.into_iter().filter_map(|h| h.join().unwrap().err()).collect();
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn exit_after_a_premature_lost_mark_clears_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        store.mark_started("abc123", 1, 1).unwrap();
        assert!(store.mark_lost("abc123", LostReason::SupervisorTerminated).unwrap());
        store.mark_exited("abc123", 2, Some(0), None).unwrap();
        let s = store.session("abc123").unwrap().unwrap();
        assert_eq!((s.status, s.lost_reason), (Status::Exited, None));
    }

    #[test]
    fn delete_under_a_reader_reports_pending_wal_and_a_later_checkpoint_scrubs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut store = Store::open(&path).unwrap();
        new_session(&store, "abc123");
        store.append_events("abc123", 0, &[ev(1, b"SECRET-XYZ-123")]).unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN; SELECT count(*) FROM events;").unwrap();
        assert_eq!(store.delete_session("abc123").unwrap(), Deletion::WalPending);
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        assert_eq!(store.checkpoint().unwrap(), Deletion::Scrubbed);
        for f in ["s.db", "s.db-wal"] {
            let bytes = std::fs::read(dir.path().join(f)).unwrap_or_default();
            assert!(!bytes.windows(14).any(|w| w == b"SECRET-XYZ-123"), "{f}");
        }
    }

    #[test]
    fn deleting_a_session_returns_its_pages_to_the_file_system() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut store = Store::open(&path).unwrap();
        new_session(&store, "abc123");
        let chunk = vec![b'x'; 16 * 1024];
        let events: Vec<NewEvent> = (0..400).map(|i| ev(i, &chunk)).collect();
        store.append_events("abc123", 0, &events).unwrap();
        store.checkpoint().unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        assert_eq!(store.delete_session("abc123").unwrap(), Deletion::Scrubbed);
        let free: i64 = store.conn.pragma_query_value(None, "freelist_count", |r| r.get(0)).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(free, 0);
        assert!(after < before / 10, "before {before} after {after}");
    }

    #[test]
    fn orphan_events_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        store.append_events("gone00", 0, &[ev(1, b"x")]).unwrap();
        assert_eq!(store.delete_orphan_events().unwrap(), 1);
    }

    #[test]
    fn paged_iteration_does_not_hold_a_read_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut store = Store::open(&path).unwrap();
        new_session(&store, "abc123");
        let events: Vec<NewEvent> = (0..1000).map(|i| ev(i, b"x")).collect();
        store.append_events("abc123", 0, &events).unwrap();
        let other = Store::open(&path).unwrap();
        let mut seen = 0;
        let mut checkpoint_busy = Vec::new();
        store
            .for_each_event("abc123", None, |e| {
                seen += 1;
                if e.seq % 256 == 255 {
                    other.conn.execute("UPDATE sessions SET rows = rows WHERE id = 'abc123'", []).unwrap();
                    checkpoint_busy.push(other.checkpoint().unwrap());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, 1000);
        assert!(checkpoint_busy.iter().all(|d| *d == Deletion::Scrubbed), "{checkpoint_busy:?}");
    }

    #[test]
    fn iteration_can_start_after_a_sequence_number() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        let events: Vec<NewEvent> = (0..600).map(|i| ev(i * 10, b"x")).collect();
        store.append_events("abc123", 0, &events).unwrap();
        assert_eq!(store.last_seq_at_or_before("abc123", kind::TERMINAL_OUTPUT, 2995).unwrap(), Some(299));
        let mut seqs = vec![];
        store
            .for_each_event_from("abc123", 590, None, |e| {
                seqs.push(e.seq);
                Ok(())
            })
            .unwrap();
        assert_eq!(seqs, (591..600).collect::<Vec<_>>());
    }

    #[test]
    fn new_stores_use_incremental_vacuum_and_the_kind_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        let mode: i64 = store.conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0)).unwrap();
        assert_eq!(mode, 2);
        let plan: String = store
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT seq FROM events WHERE session_id = 'a' AND kind = 'b' ORDER BY seq",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("events_by_kind"), "{plan}");
    }

    #[test]
    fn mark_lost_only_affects_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        new_session(&store, "abc123");
        store.mark_exited("abc123", 10, Some(0), None).unwrap();
        assert!(!store.mark_lost("abc123", LostReason::SupervisorTerminated).unwrap());
        assert_eq!(store.session("abc123").unwrap().unwrap().status, Status::Exited);
        store.mark_started("abc123", 9, 9).unwrap();
        assert_eq!(store.session("abc123").unwrap().unwrap().child_pid, None);
    }

    #[test]
    fn every_error_variant_displays_its_message() {
        let path = PathBuf::from("/tmp/state.db");
        let cases: Vec<(Error, String)> = vec![
            (Error::Busy, "database is locked".into()),
            (
                Error::Corrupt { path: path.clone() },
                "event store /tmp/state.db is corrupt; move it aside to start a new one \
                 (existing recordings stay in the moved file)"
                    .into(),
            ),
            (
                Error::NewerSchema { found: 9, supported: 6, binary: "0.4.0" },
                "written by a newer witnessed (store schema 9; this binary is 0.4.0 and reads up to schema 6); \
                 update this binary with install.sh, then run the command again; the store was not changed"
                    .into(),
            ),
            (Error::InvalidId("a%".into()), "invalid session id \"a%\"".into()),
            (
                Error::NotFound("zz".into()),
                "no session matches \"zz\"; run `witnessed sessions` to list sessions".into(),
            ),
            (
                Error::Ambiguous { prefix: "ab".into(), ids: vec!["abc123".into(), "abd456".into()] },
                "\"ab\" is ambiguous: abc123, abd456".into(),
            ),
            (Error::NoSessions, "no sessions yet; record one with: witnessed run -- claude".into()),
            (Error::Sqlite(rusqlite::Error::QueryReturnedNoRows), rusqlite::Error::QueryReturnedNoRows.to_string()),
            (
                Error::Json(serde_json::from_str::<Value>("{").unwrap_err()),
                serde_json::from_str::<Value>("{").unwrap_err().to_string(),
            ),
        ];
        for (e, want) in cases {
            assert_eq!(e.to_string(), want);
            assert_eq!(e.is_transient(), matches!(e, Error::Busy), "{e}");
        }
    }

    #[test]
    fn resolve_failures_downcast_to_the_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        let e = store.resolve("last").unwrap_err();
        assert!(matches!(e.downcast_ref::<Error>(), Some(Error::NoSessions)), "{e:#}");
        new_session(&store, "abc123");
        new_session(&store, "abd456");
        let e = store.resolve("ab").unwrap_err();
        match e.downcast_ref::<Error>() {
            Some(Error::Ambiguous { prefix, ids }) => assert_eq!((prefix.as_str(), ids.len()), ("ab", 2)),
            other => panic!("{other:?}"),
        }
        assert_eq!(e.to_string(), "\"ab\" is ambiguous: abc123, abd456");
        let e = store.resolve("zz").unwrap_err();
        assert!(matches!(e.downcast_ref::<Error>(), Some(Error::NotFound(id)) if id == "zz"), "{e:#}");
        let e = store.resolve("a%").unwrap_err();
        assert!(matches!(e.downcast_ref::<Error>(), Some(Error::InvalidId(id)) if id == "a%"), "{e:#}");
        assert!(!is_transient(&e));
    }

    #[test]
    fn open_failures_downcast_to_corrupt_and_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        let e = Store::open(&path).err().expect("corrupt store must not open");
        assert!(matches!(e.downcast_ref::<Error>(), Some(Error::Corrupt { path: p }) if *p == path), "{e:#}");
        let path = dir.path().join("n.db");
        drop(Store::open(&path).unwrap());
        Connection::open(&path).unwrap().pragma_update(None, "user_version", DB_USER_VERSION + 1).unwrap();
        let e = Store::open(&path).err().expect("a newer store must not open");
        assert!(
            matches!(
                e.downcast_ref::<Error>(),
                Some(Error::NewerSchema { found, supported, .. }) if *found == DB_USER_VERSION + 1 && *supported == DB_USER_VERSION
            ),
            "{e:#}"
        );
    }

    #[test]
    fn a_write_against_an_exclusive_lock_is_a_transient_busy_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut store = Store::open(&path).unwrap();
        new_session(&store, "abc123");
        store.conn.busy_timeout(Duration::from_millis(20)).unwrap();
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let e = store.append_events("abc123", 0, &[ev(1, b"x")]).unwrap_err();
        assert!(matches!(e.downcast_ref::<Error>(), Some(Error::Busy)), "{e:#}");
        assert!(is_transient(&e));
        let e = store.set_attached("abc123", true).unwrap_err();
        assert!(is_transient(&e), "{e:#}");
        holder.execute_batch("COMMIT").unwrap();
        assert_eq!(store.append_events("abc123", 0, &[ev(1, b"x")]).unwrap(), 1);
    }

    #[test]
    fn a_reader_paging_under_a_writer_sees_a_contiguous_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut store = Store::open(&path).unwrap();
        new_session(&store, "abc123");
        let first: Vec<NewEvent> = (0..300).map(|i| ev(i, b"x")).collect();
        let mut next = store.append_events("abc123", 0, &first).unwrap();
        let mut writer = Store::open(&path).unwrap();
        let mut seqs = Vec::new();
        for e in store.events("abc123", -1, None) {
            let e = e.unwrap();
            seqs.push(e.seq);
            if e.seq % 256 == 255 && next < 1500 {
                let more: Vec<NewEvent> = (0..256).map(|i| ev(next + i, b"y")).collect();
                next = writer.append_events("abc123", next, &more).unwrap();
            }
        }
        assert_eq!(seqs, (0..next).collect::<Vec<_>>());
        assert!(next > 300, "{next}");
        let filtered: Vec<i64> =
            store.events("abc123", 1000, Some(&[kind::TERMINAL_OUTPUT])).map(|e| e.unwrap().seq).collect();
        assert_eq!(filtered, (1001..next).collect::<Vec<_>>());
        assert_eq!(store.events("abc123", 0, Some(&[kind::USER_MARK])).count(), 0);
    }
}
