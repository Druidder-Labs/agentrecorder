mod events;
mod time;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;

pub use events::{
    AgentHook, Body, ChangedPath, DetachReason, GIT_MAX_PATHS, GitPhase, HookUsage, Notice, NotificationSource,
    SnapshotReason, TerminationReason, kind,
};
pub use time::{format_rfc3339, now_unix_ns, parse_rfc3339};

pub const EVENT_SCHEMA_VERSION: u32 = 1;

const DB_USER_VERSION: i64 = 5;
const PAGE_ROWS: i64 = 256;
const WAL_SIZE_LIMIT: i64 = 4 << 20;
const INIT_RETRY_FOR: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletion {
    Scrubbed,
    WalPending,
}

fn sqlite_code(e: &anyhow::Error) -> Option<rusqlite::ErrorCode> {
    e.chain().find_map(|c| match c.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(f, _)) => Some(f.code),
        _ => None,
    })
}

pub fn is_transient(e: &anyhow::Error) -> bool {
    is_busy(e)
}

fn is_busy(e: &anyhow::Error) -> bool {
    matches!(sqlite_code(e), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

fn is_corrupt(e: &anyhow::Error) -> bool {
    matches!(sqlite_code(e), Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt))
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
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Exited => "exited",
            Status::Lost => "lost",
        }
    }

    fn parse(s: &str) -> Status {
        match s {
            "starting" => Status::Starting,
            "running" => Status::Running,
            "exited" => Status::Exited,
            _ => Status::Lost,
        }
    }

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
    pub fn body(&self) -> Option<Body> {
        Body::decode(&self.kind, &self.payload)
    }

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

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening event store {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mut store = Store { conn };
        let deadline = std::time::Instant::now() + INIT_RETRY_FOR;
        loop {
            match store.init() {
                Ok(()) => return Ok(store),
                Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) if is_corrupt(&e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "event store {} is corrupt; move it aside to start a new one \
                             (existing recordings stay in the moved file)",
                            path.display()
                        )
                    });
                }
                Err(e) => return Err(e).with_context(|| format!("opening event store {}", path.display())),
            }
        }
    }

    fn init(&mut self) -> Result<()> {
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
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.execute_batch(MIGRATE_2_TO_3)?;
                tx.execute_batch(MIGRATE_3_TO_4)?;
                tx.execute_batch(MIGRATE_4_TO_5)?;
            }
            1 => {
                tx.execute_batch(MIGRATE_1_TO_2)?;
                tx.execute_batch(MIGRATE_2_TO_3)?;
                tx.execute_batch(MIGRATE_3_TO_4)?;
                tx.execute_batch(MIGRATE_4_TO_5)?;
            }
            2 => {
                tx.execute_batch(MIGRATE_2_TO_3)?;
                tx.execute_batch(MIGRATE_3_TO_4)?;
                tx.execute_batch(MIGRATE_4_TO_5)?;
            }
            3 => {
                tx.execute_batch(MIGRATE_3_TO_4)?;
                tx.execute_batch(MIGRATE_4_TO_5)?;
            }
            4 => tx.execute_batch(MIGRATE_4_TO_5)?,
            DB_USER_VERSION => {}
            v => bail!("event store schema version {v} is newer than this binary supports"),
        }
        tx.pragma_update(None, "user_version", DB_USER_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    pub fn create_session(&self, s: &NewSession<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, command, cwd, status, created_at_ns, supervisor_pid, rows, cols,
                record_output, record_input, boot_time, restarted_from, agent_session_id, anchor_token_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                s.id,
                serde_json::to_string(s.command)?,
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
            ],
        )?;
        Ok(())
    }

    pub fn mark_started(&self, id: &str, child_pid: i64, started_at_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = 'running', child_pid = ?2, started_at_ns = ?3
             WHERE id = ?1 AND status = 'starting'",
            params![id, child_pid, started_at_ns],
        )?;
        Ok(())
    }

    pub fn set_attached(&self, id: &str, attached: bool) -> Result<()> {
        self.conn.execute("UPDATE sessions SET attached = ?2 WHERE id = ?1", params![id, attached])?;
        Ok(())
    }

    pub fn set_size(&self, id: &str, rows: u16, cols: u16) -> Result<()> {
        self.conn.execute("UPDATE sessions SET rows = ?2, cols = ?3 WHERE id = ?1", params![id, rows, cols])?;
        Ok(())
    }

    pub fn mark_exited(
        &self,
        id: &str,
        ended_at_ns: i64,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = 'exited', ended_at_ns = ?2, exit_code = ?3, exit_signal = ?4,
                attached = 0, lost_reason = NULL WHERE id = ?1 AND status IN ('starting', 'running', 'lost')",
            params![id, ended_at_ns, exit_code, exit_signal],
        )?;
        Ok(())
    }

    pub fn mark_lost(&self, id: &str, reason: LostReason) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE sessions SET status = 'lost', lost_reason = ?2, attached = 0
             WHERE id = ?1 AND status IN ('starting', 'running')",
            params![id, reason.as_str()],
        )?;
        Ok(n > 0)
    }

    pub fn append_events(&mut self, session_id: &str, first_seq: i64, events: &[NewEvent]) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let mut seq = first_seq;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO events (session_id, seq, ts_ns, mono_ns, kind, payload, data, data_len)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for e in events {
                let len = e.data.as_ref().map_or(0, Vec::len) as i64;
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
        if id_or_prefix == "last" || id_or_prefix == "latest" {
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY created_at_ns DESC LIMIT 1"))?;
            return match stmt.query_map([], row_to_session)?.next() {
                Some(s) => Ok(s?),
                None => bail!("no sessions recorded yet; start one with `agentrecorder run -- <command>`"),
            };
        }
        if id_or_prefix.is_empty() || !id_or_prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("invalid session id {id_or_prefix:?}");
        }
        let mut stmt =
            self.conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions WHERE id LIKE ?1 || '%' ORDER BY id"))?;
        let found: Vec<Session> = stmt.query_map([id_or_prefix], row_to_session)?.collect::<Result<_, _>>()?;
        match found.len() {
            0 => bail!("no session matches {id_or_prefix:?}; run `agentrecorder sessions` to list sessions"),
            1 => Ok(found.into_iter().next().expect("one element")),
            _ => {
                if let Some(exact) = found.iter().find(|s| s.id == id_or_prefix) {
                    return Ok(exact.clone());
                }
                let ids: Vec<_> = found.iter().map(|s| s.id.as_str()).collect();
                bail!("{id_or_prefix:?} is ambiguous: {}", ids.join(", "))
            }
        }
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"), [id], row_to_session)
            .optional()?)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY created_at_ns"))?;
        let rows = stmt.query_map([], row_to_session)?.collect::<Result<_, _>>()?;
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
        let mut sql = String::from(
            "SELECT session_id, seq, ts_ns, mono_ns, kind, payload, data FROM events
             WHERE session_id = ?1 AND seq > ?2",
        );
        if let Some(ks) = kinds {
            let marks: Vec<String> = (0..ks.len()).map(|i| format!("?{}", i + 3)).collect();
            sql.push_str(&format!(" AND kind IN ({})", marks.join(", ")));
        }
        sql.push_str(&format!(" ORDER BY seq LIMIT {PAGE_ROWS}"));
        let mut last = after_seq;
        loop {
            let page = {
                let mut args: Vec<&dyn rusqlite::ToSql> = vec![&session_id, &last];
                if let Some(ks) = kinds {
                    args.extend(ks.iter().map(|k| k as &dyn rusqlite::ToSql));
                }
                let mut stmt = self.conn.prepare_cached(&sql)?;
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
                rows.collect::<rusqlite::Result<Vec<Event>>>()?
            };
            let Some(tail) = page.last() else { return Ok(()) };
            last = tail.seq;
            let full = page.len() as i64 == PAGE_ROWS;
            for e in page {
                f(e)?;
            }
            if !full {
                return Ok(());
            }
        }
    }

    pub fn last_seq_at_or_before(&self, session_id: &str, kind: &str, mono_ns: i64) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(seq) FROM events WHERE session_id = ?1 AND kind = ?2 AND mono_ns <= ?3",
            params![session_id, kind, mono_ns],
            |r| r.get(0),
        )?)
    }

    pub fn event_counts(&self, session_id: &str) -> Result<Vec<KindCount>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(data_len), 0) FROM events
             WHERE session_id = ?1 GROUP BY kind ORDER BY kind",
        )?;
        let rows = stmt
            .query_map([session_id], |r| Ok(KindCount { kind: r.get(0)?, count: r.get(1)?, bytes: r.get(2)? }))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn delete_session(&mut self, id: &str) -> Result<Deletion> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM events WHERE session_id = ?1", [id])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        tx.commit()?;
        self.checkpoint()
    }

    pub fn delete_orphan_events(&mut self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM events WHERE session_id NOT IN (SELECT id FROM sessions)", [])?)
    }

    pub fn checkpoint(&self) -> Result<Deletion> {
        let mut vacuum = self.conn.prepare("PRAGMA incremental_vacuum")?;
        let mut freed = vacuum.query([])?;
        while freed.next()?.is_some() {}
        drop(freed);
        drop(vacuum);
        let busy: i64 = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        Ok(if busy == 0 { Deletion::Scrubbed } else { Deletion::WalPending })
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

const SESSION_COLS: &str = "id, command, cwd, status, created_at_ns, started_at_ns, ended_at_ns,
    supervisor_pid, child_pid, exit_code, exit_signal, rows, cols, attached, record_output,
    record_input, boot_time, lost_reason, restarted_from, agent_session_id, anchor_token_digest";

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let command: String = r.get(1)?;
    Ok(Session {
        id: r.get(0)?,
        command: serde_json::from_str(&command).unwrap_or_else(|_| vec![command]),
        cwd: r.get(2)?,
        status: Status::parse(&r.get::<_, String>(3)?),
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
                if e.seq % PAGE_ROWS == PAGE_ROWS - 1 {
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
}
