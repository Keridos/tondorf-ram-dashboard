//! SQLite-backed history of every accepted report.
//!
//! The in-memory `Store` holds only the latest state per host; this keeps the
//! full timeline. Rows older than the retention period are pruned by the sweeper.

use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, params};

use crate::model::{HistoryEntry, Host};

pub struct History {
    conn: Mutex<Connection>,
}

/// Fixed-width RFC 3339 (microseconds, `Z`), so text order equals time order.
fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

impl History {
    /// Opens (or creates) the database. `":memory:"` gives a throwaway database.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // journal_mode returns a row, so it can't go through execute().
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS reports (
                 id              INTEGER PRIMARY KEY,
                 hostname        TEXT    NOT NULL,
                 username        TEXT    NOT NULL,
                 free_ram_bytes  INTEGER NOT NULL,
                 total_ram_bytes INTEGER NOT NULL,
                 reported_at     TEXT    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS reports_host_time ON reports (hostname, reported_at);
             CREATE INDEX IF NOT EXISTS reports_time ON reports (reported_at);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(&self, host: &Host) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO reports (hostname, username, free_ram_bytes, total_ram_bytes, reported_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                host.hostname,
                host.username,
                host.free_ram_bytes,
                host.total_ram_bytes,
                ts(host.last_seen),
            ],
        )?;
        Ok(())
    }

    /// Reports for one host, newest first, optionally only those at/after `since`.
    pub fn for_host(
        &self,
        hostname: &str,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> rusqlite::Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT username, free_ram_bytes, total_ram_bytes, reported_at FROM reports
             WHERE hostname = ?1 AND reported_at >= ?2
             ORDER BY reported_at DESC, id DESC
             LIMIT ?3",
        )?;
        let since = since.map(ts).unwrap_or_default();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        stmt.query_map(params![hostname, since, limit], |row| {
            let reported_at: String = row.get(3)?;
            Ok(HistoryEntry {
                username: row.get(0)?,
                free_ram_bytes: row.get(1)?,
                total_ram_bytes: row.get(2)?,
                reported_at: DateTime::parse_from_rfc3339(&reported_at)
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?
                    .with_timezone(&Utc),
            })
        })?
        .collect()
    }

    /// Deletes reports older than `cutoff`. Returns how many were removed.
    pub fn prune(&self, cutoff: DateTime<Utc>) -> rusqlite::Result<usize> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM reports WHERE reported_at < ?1", [ts(cutoff)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{SubsecRound, TimeDelta};

    fn host(name: &str, free: u64, at: DateTime<Utc>) -> Host {
        Host {
            hostname: name.into(),
            username: "u".into(),
            free_ram_bytes: free,
            total_ram_bytes: 100,
            last_seen: at,
        }
    }

    #[test]
    fn records_queries_and_prunes() {
        let t0 = Utc::now().trunc_subsecs(6);
        let s = TimeDelta::seconds;
        let h = History::open(":memory:").unwrap();
        for i in 0..3 {
            h.record(&host("a", i, t0 + s(i as i64))).unwrap();
        }
        h.record(&host("b", 50, t0)).unwrap();

        let all = h.for_host("a", None, 100).unwrap();
        let frees: Vec<_> = all.iter().map(|e| e.free_ram_bytes).collect();
        assert_eq!(frees, [2, 1, 0], "newest first, other hosts excluded");
        assert_eq!(all[0].reported_at, t0 + s(2));

        assert_eq!(h.for_host("a", None, 1).unwrap().len(), 1);
        assert_eq!(h.for_host("a", Some(t0 + s(1)), 100).unwrap().len(), 2);
        assert!(h.for_host("nope", None, 100).unwrap().is_empty());

        assert_eq!(h.prune(t0 + s(1)).unwrap(), 2); // a@t0 and b@t0
        assert_eq!(h.for_host("a", None, 100).unwrap().len(), 2);
    }

    #[test]
    fn survives_reopen() {
        let dir = std::env::temp_dir().join(format!("wuppp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let path = path.to_str().unwrap();

        History::open(path)
            .unwrap()
            .record(&host("a", 1, Utc::now()))
            .unwrap();
        assert_eq!(
            History::open(path)
                .unwrap()
                .for_host("a", None, 10)
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
