//! SQLite event log with an ack watermark and count/age retention.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, Params, params};
use serde_json::Value;
use tokio::sync::Notify;

/// Rows removed per batch while pruning.
const PRUNE_BATCH: u64 = 500;
const ACK_KEY: &str = "ack";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    received_ms INTEGER NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_received ON events (received_ms);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT OR IGNORE INTO meta (key, value) VALUES ('ack', 0);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_events: u64,
    pub max_age_ms: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: u64,
    /// Removed rows that were above the ack watermark. Only the cap or the
    /// age limit removes these.
    pub removed_unacked: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: u64,
    pub source: String,
    pub received_ms: u64,
    pub body: Value,
}

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    retention: Retention,
    /// Woken on every insert so long-polling readers can re-query.
    pub notify: Notify,
}

impl Store {
    pub fn open(path: &Path, retention: Retention) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            retention,
            notify: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stores one event and prunes with the configured retention.
    ///
    /// A failed prune is logged and does not fail the insert. The id is
    /// AUTOINCREMENT, so it is never reused, even after the row is pruned.
    pub fn insert(&self, source: &str, body: &Value, now_ms: u64) -> Result<u64, StoreError> {
        let text = serde_json::to_string(body)?;
        let id = {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO events (source, received_ms, body) VALUES (?1, ?2, ?3)",
                params![source, to_i64(now_ms), text],
            )?;
            let id = conn.last_insert_rowid() as u64;
            if let Err(e) = prune_conn(&conn, now_ms, self.retention) {
                tracing::error!(error = %e, "prune after insert failed");
            }
            id
        };
        self.notify.notify_waiters();
        Ok(id)
    }

    /// Events with id greater than `since`, oldest first, at most `limit`.
    pub fn since(&self, since: u64, limit: u64) -> Result<Vec<Event>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT id, source, received_ms, body FROM events WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![to_i64(since), to_i64(limit)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, source, received_ms, body) = row?;
            out.push(Event {
                id: id as u64,
                source,
                received_ms: received_ms as u64,
                body: serde_json::from_str(&body)?,
            });
        }
        Ok(out)
    }

    /// Moves the ack watermark forward. A lower value leaves it unchanged.
    pub fn ack(&self, through: u64) -> Result<(), StoreError> {
        self.lock().execute(
            "UPDATE meta SET value = MAX(value, ?1) WHERE key = ?2",
            params![to_i64(through), ACK_KEY],
        )?;
        Ok(())
    }

    pub fn ack_watermark(&self) -> Result<u64, StoreError> {
        let value = ack_watermark_conn(&self.lock())?;
        Ok(value.max(0) as u64)
    }

    /// Prunes with the given retention rather than the configured one.
    pub fn prune(&self, now_ms: u64, retention: Retention) -> Result<PruneReport, StoreError> {
        prune_conn(&self.lock(), now_ms, retention)
    }

    /// Runs a trivial query to confirm the database answers.
    pub fn health(&self) -> Result<(), StoreError> {
        self.lock().query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn ack_watermark_conn(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![ACK_KEY],
        |row| row.get(0),
    )?)
}

/// Removes rows forced out by the age limit, then by the count cap.
///
/// Rows are removed oldest first. Acked rows form a prefix of the table, so
/// unacked rows go only once no acked row is left to remove.
fn prune_conn(
    conn: &Connection,
    now_ms: u64,
    retention: Retention,
) -> Result<PruneReport, StoreError> {
    let watermark = ack_watermark_conn(conn)?;
    let mut report = PruneReport::default();

    let cutoff = to_i64(now_ms.saturating_sub(retention.max_age_ms));
    loop {
        let ids = select_ids(
            conn,
            "SELECT id FROM events WHERE received_ms < ?1 ORDER BY id LIMIT ?2",
            params![cutoff, PRUNE_BATCH as i64],
        )?;
        if ids.is_empty() {
            break;
        }
        delete_ids(conn, &ids, watermark, &mut report)?;
        if (ids.len() as u64) < PRUNE_BATCH {
            break;
        }
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    let mut excess = (count.max(0) as u64).saturating_sub(retention.max_events);
    while excess > 0 {
        let batch = excess.min(PRUNE_BATCH);
        let ids = select_ids(
            conn,
            "SELECT id FROM events ORDER BY id LIMIT ?1",
            params![batch as i64],
        )?;
        if ids.is_empty() {
            break;
        }
        delete_ids(conn, &ids, watermark, &mut report)?;
        excess = excess.saturating_sub(ids.len() as u64);
    }

    if report.removed_unacked > 0 {
        tracing::warn!(
            removed = report.removed,
            removed_unacked = report.removed_unacked,
            "pruning removed events the consumer has not acked"
        );
    } else if report.removed > 0 {
        tracing::info!(removed = report.removed, "pruned acked events");
    }
    Ok(report)
}

fn select_ids<P: Params>(conn: &Connection, sql: &str, params: P) -> Result<Vec<i64>, StoreError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let ids = stmt
        .query_map(params, |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn delete_ids(
    conn: &Connection,
    ids: &[i64],
    watermark: i64,
    report: &mut PruneReport,
) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    for id in ids {
        let removed = tx.execute("DELETE FROM events WHERE id = ?1", params![id])? as u64;
        report.removed += removed;
        if *id > watermark {
            report.removed_unacked += removed;
        }
    }
    tx.commit()?;
    Ok(())
}
