use std::path::Path;
use std::time::Instant;

use rusqlite::{Connection, params};

struct Layout {
    name: &'static str,
    schema: &'static str,
    side_table: bool,
}

const LAYOUTS: &[Layout] = &[
    Layout {
        name: "without_rowid_pk",
        schema: "CREATE TABLE events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts_ns INTEGER NOT NULL,
            mono_ns INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT, data BLOB, data_len INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, seq)) WITHOUT ROWID;
            CREATE INDEX events_by_kind ON events (session_id, kind, seq, data_len);",
        side_table: false,
    },
    Layout {
        name: "rowid_unique_index",
        schema: "CREATE TABLE events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts_ns INTEGER NOT NULL,
            mono_ns INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT, data BLOB, data_len INTEGER NOT NULL DEFAULT 0);
            CREATE UNIQUE INDEX events_by_seq ON events (session_id, seq);
            CREATE INDEX events_by_kind ON events (session_id, kind, seq, data_len);",
        side_table: false,
    },
    Layout {
        name: "blobs_in_side_table",
        schema: "CREATE TABLE events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts_ns INTEGER NOT NULL,
            mono_ns INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT, data_id INTEGER, data_len INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, seq)) WITHOUT ROWID;
            CREATE INDEX events_by_kind ON events (session_id, kind, seq, data_len);
            CREATE TABLE event_data (id INTEGER PRIMARY KEY, data BLOB NOT NULL);",
        side_table: true,
    },
];

struct Workload {
    name: &'static str,
    events: i64,
    blob: usize,
    small_every: i64,
}

const WORKLOADS: &[Workload] = &[
    Workload { name: "bulk_64KiB_output", events: 3_200, blob: 64 << 10, small_every: 50 },
    Workload { name: "chatty_200B_output", events: 200_000, blob: 200, small_every: 100 },
];

const SESSIONS: [&str; 3] = ["aaaaaa", "bbbbbb", "cccccc"];
const BATCH: usize = 512;
const PAGE: i64 = 256;

fn open(path: &Path, l: &Layout) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "secure_delete", "ON")?;
    c.pragma_update(None, "journal_size_limit", 4 << 20)?;
    c.execute_batch(l.schema)?;
    Ok(c)
}

fn insert(c: &mut Connection, l: &Layout, w: &Workload, session: &str) -> rusqlite::Result<()> {
    let blob = vec![b'x'; w.blob];
    let blob_len = i64::try_from(w.blob).expect("blob fits i64");
    let mut seq = 0i64;
    while seq < w.events {
        let tx = c.transaction()?;
        {
            let mut ev = if l.side_table {
                tx.prepare_cached("INSERT INTO events (session_id, seq, ts_ns, mono_ns, kind, payload, data_id, data_len) VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)")?
            } else {
                tx.prepare_cached("INSERT INTO events (session_id, seq, ts_ns, mono_ns, kind, payload, data, data_len) VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)")?
            };
            let mut data = tx.prepare_cached("INSERT INTO event_data (data) VALUES (?1)").ok();
            for _ in 0..BATCH {
                if seq >= w.events {
                    break;
                }
                let small = seq % w.small_every == 0;
                let (kind, payload, len) = if small {
                    ("terminal.resized", Some(r#"{"rows":24,"cols":80}"#), 0)
                } else {
                    ("terminal.output", None, blob_len)
                };
                if l.side_table {
                    let id = if small {
                        None
                    } else {
                        data.as_mut().expect("side table").execute(params![blob])?;
                        Some(tx.last_insert_rowid())
                    };
                    ev.execute(params![session, seq, seq, kind, payload, id, len])?;
                } else {
                    let d = (!small).then_some(&blob);
                    ev.execute(params![session, seq, seq, kind, payload, d, len])?;
                }
                seq += 1;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

fn read_all(c: &Connection, l: &Layout, session: &str) -> rusqlite::Result<u64> {
    let sql = if l.side_table {
        "SELECT e.seq, e.kind, e.payload, d.data FROM events e LEFT JOIN event_data d ON d.id = e.data_id
         WHERE e.session_id = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3"
    } else {
        "SELECT seq, kind, payload, data FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
    };
    let mut stmt = c.prepare(sql)?;
    let (mut last, mut bytes) = (-1i64, 0u64);
    loop {
        let mut rows = stmt.query(params![session, last, PAGE])?;
        let mut n = 0;
        while let Some(r) = rows.next()? {
            last = r.get(0)?;
            let d: Option<Vec<u8>> = r.get(3)?;
            bytes += d.map_or(0, |d| d.len() as u64);
            n += 1;
        }
        if n < PAGE {
            return Ok(bytes);
        }
    }
}

fn counts(c: &Connection, session: &str) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT SUM(n) FROM (SELECT kind, COUNT(*) AS n, SUM(data_len) FROM events WHERE session_id = ?1 GROUP BY kind)",
        [session],
        |r| r.get(0),
    )
}

fn resized_scan(c: &Connection, session: &str) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT COUNT(payload) FROM events WHERE session_id = ?1 AND kind = 'terminal.resized'",
        [session],
        |r| r.get(0),
    )
}

fn delete(c: &Connection, l: &Layout, session: &str) -> rusqlite::Result<()> {
    if l.side_table {
        c.execute("DELETE FROM event_data WHERE id IN (SELECT data_id FROM events WHERE session_id = ?1)", [session])?;
    }
    c.execute("DELETE FROM events WHERE session_id = ?1", [session])?;
    let mut vacuum = c.prepare("PRAGMA incremental_vacuum")?;
    let mut freed = vacuum.query([])?;
    while freed.next()?.is_some() {}
    drop(freed);
    drop(vacuum);
    c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn secs(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1000.0).round() / 1000.0
}

fn main() -> anyhow::Result<()> {
    let mut out = serde_json::Map::new();
    for w in WORKLOADS {
        let mut per = serde_json::Map::new();
        for l in LAYOUTS {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("events.db");
            let mut c = open(&path, l)?;
            let t = Instant::now();
            for s in SESSIONS {
                insert(&mut c, l, w, s)?;
            }
            let insert_s = secs(t);
            c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            let db_bytes = size(&path);
            let t = Instant::now();
            let read_bytes = read_all(&c, l, SESSIONS[1])?;
            let read_s = secs(t);
            let t = Instant::now();
            let n = counts(&c, SESSIONS[1])?;
            let counts_s = secs(t);
            let t = Instant::now();
            let resized = resized_scan(&c, SESSIONS[1])?;
            let scan_s = secs(t);
            let t = Instant::now();
            delete(&c, l, SESSIONS[1])?;
            let delete_s = secs(t);
            let after_delete = size(&path);
            per.insert(
                l.name.into(),
                serde_json::json!({
                    "insert_s": insert_s, "db_bytes": db_bytes, "read_one_session_s": read_s,
                    "read_bytes": read_bytes, "counts_s": counts_s, "events": n,
                    "kind_scan_s": scan_s, "resized_rows": resized,
                    "delete_one_session_s": delete_s, "db_bytes_after_delete": after_delete,
                }),
            );
        }
        out.insert(w.name.into(), per.into());
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
