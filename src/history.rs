//! SQLite-backed history of every accepted report.
//!
//! The in-memory `Store` holds only the latest state per host; this keeps the
//! full timeline. Rows older than the retention period are pruned by the sweeper.
//! The `users` table holds each user's dashboard color; it is never pruned.

use std::hash::{BuildHasher, RandomState};
use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{HistoryEntry, Host};

pub struct History {
    conn: Mutex<Connection>,
}

/// Fixed-width RFC 3339 (microseconds, `Z`), so text order equals time order.
fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

const SATURATION: f64 = 0.75;
const LIGHTNESS: f64 = 0.55;
/// Random candidates per new user; the one farthest from existing colors wins.
const CANDIDATES: usize = 12;

fn random_hue() -> f64 {
    // RandomState is seeded randomly per instance; hashing the time adds entropy.
    (RandomState::new().hash_one(std::time::SystemTime::now()) % 360) as f64
}

/// A random, saturated mid-lightness color (`#rrggbb`): random hue, fixed
/// saturation and lightness, so it reads well on the dashboard's grey.
pub fn random_color() -> String {
    hsl_to_hex(random_hue(), SATURATION, LIGHTNESS)
}

/// Like `random_color`, but of several random hues picks the one farthest
/// from the `taken` colors, so users don't end up with near-identical colors.
fn distinct_random_color(taken: &[String]) -> String {
    let taken: Vec<f64> = taken.iter().filter_map(|c| hue_of(c)).collect();
    let distance = |h: f64| {
        taken
            .iter()
            .map(|t| {
                let d = (h - t).abs();
                d.min(360.0 - d)
            })
            .fold(f64::INFINITY, f64::min)
    };
    let hue = (0..CANDIDATES)
        .map(|_| random_hue())
        .max_by(|a, b| distance(*a).total_cmp(&distance(*b)))
        .unwrap_or_default();
    hsl_to_hex(hue, SATURATION, LIGHTNESS)
}

/// Hue in degrees of a `#rrggbb` color; `None` for greys (no hue) or bad input.
fn hue_of(hex: &str) -> Option<f64> {
    let channel = |i: usize| Some(u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()? as f64 / 255.0);
    let (r, g, b) = (channel(1)?, channel(3)?, channel(5)?);
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    if d < 0.05 {
        return None;
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    Some(h * 60.0)
}

fn hsl_to_hex(h: f64, s: f64, l: f64) -> String {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let byte = |v: f64| ((v + m) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
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
             DROP INDEX IF EXISTS reports_host_time;
             CREATE INDEX IF NOT EXISTS reports_user_host_time
                 ON reports (username, hostname, reported_at);
             CREATE INDEX IF NOT EXISTS reports_time ON reports (reported_at);
             CREATE TABLE IF NOT EXISTS users (
                 username TEXT PRIMARY KEY,
                 color    TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Returns the user's color. `requested` (already validated `#rrggbb`)
    /// replaces the stored color; otherwise a new user gets a random color.
    pub fn user_color(&self, username: &str, requested: Option<&str>) -> rusqlite::Result<String> {
        let conn = self.conn.lock().unwrap();
        if let Some(color) = requested {
            conn.execute(
                "INSERT INTO users (username, color) VALUES (?1, ?2)
                 ON CONFLICT (username) DO UPDATE SET color = excluded.color",
                params![username, color],
            )?;
            return Ok(color.to_owned());
        }
        let existing = conn
            .query_row(
                "SELECT color FROM users WHERE username = ?1",
                [username],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(color) = existing {
            return Ok(color);
        }
        let taken = conn
            .prepare_cached("SELECT color FROM users")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        let color = distinct_random_color(&taken);
        conn.execute(
            "INSERT INTO users (username, color) VALUES (?1, ?2)",
            params![username, color],
        )?;
        Ok(color)
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

    /// Reports of one user on one host, newest first, optionally only those
    /// at/after `since`.
    pub fn for_host(
        &self,
        username: &str,
        hostname: &str,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> rusqlite::Result<Vec<HistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT free_ram_bytes, total_ram_bytes, reported_at FROM reports
             WHERE username = ?1 AND hostname = ?2 AND reported_at >= ?3
             ORDER BY reported_at DESC, id DESC
             LIMIT ?4",
        )?;
        let since = since.map(ts).unwrap_or_default();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        stmt.query_map(params![username, hostname, since, limit], |row| {
            let reported_at: String = row.get(2)?;
            Ok(HistoryEntry {
                free_ram_bytes: row.get(0)?,
                total_ram_bytes: row.get(1)?,
                reported_at: DateTime::parse_from_rfc3339(&reported_at)
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
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
            color: "#000000".into(),
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

        let all = h.for_host("u", "a", None, 100).unwrap();
        let frees: Vec<_> = all.iter().map(|e| e.free_ram_bytes).collect();
        assert_eq!(frees, [2, 1, 0], "newest first, other hosts excluded");
        assert_eq!(all[0].reported_at, t0 + s(2));

        assert_eq!(h.for_host("u", "a", None, 1).unwrap().len(), 1);
        assert_eq!(h.for_host("u", "a", Some(t0 + s(1)), 100).unwrap().len(), 2);
        assert!(h.for_host("u", "nope", None, 100).unwrap().is_empty());
        let other_user = Host {
            username: "v".into(),
            ..host("a", 99, t0)
        };
        h.record(&other_user).unwrap();
        assert_eq!(h.for_host("u", "a", None, 100).unwrap().len(), 3);
        assert_eq!(h.for_host("v", "a", None, 100).unwrap().len(), 1);

        assert_eq!(h.prune(t0 + s(1)).unwrap(), 3); // u/a@t0, u/b@t0, v/a@t0
        assert_eq!(h.for_host("u", "a", None, 100).unwrap().len(), 2);
    }

    #[test]
    fn user_colors_are_random_once_then_kept() {
        let h = History::open(":memory:").unwrap();
        let first = h.user_color("alice", None).unwrap();
        assert!(crate::model::normalize_color(&first).is_ok(), "{first}");
        assert_eq!(h.user_color("alice", None).unwrap(), first, "kept");

        assert_eq!(h.user_color("alice", Some("#123abc")).unwrap(), "#123abc");
        assert_eq!(h.user_color("alice", None).unwrap(), "#123abc");
        assert_eq!(h.user_color("bob", Some("#00ff00")).unwrap(), "#00ff00");
    }

    #[test]
    fn new_colors_avoid_taken_hues() {
        assert_eq!(hue_of("#ff0000"), Some(0.0));
        assert_eq!(hue_of("#00ff00"), Some(120.0));
        assert_eq!(hue_of("#808080"), None);
        // With red, green and blue taken, the pick is far from all of them.
        let taken = ["#ff0000", "#00ff00", "#0000ff"].map(String::from);
        for _ in 0..20 {
            let hue = hue_of(&distinct_random_color(&taken)).unwrap();
            let gap = [0.0, 120.0, 240.0, 360.0]
                .iter()
                .map(|t: &f64| (hue - t).abs())
                .fold(f64::INFINITY, f64::min);
            assert!(gap > 15.0, "hue {hue} too close to a taken one");
        }
    }

    #[test]
    fn hsl_conversion() {
        assert_eq!(hsl_to_hex(0.0, 1.0, 0.5), "#ff0000");
        assert_eq!(hsl_to_hex(120.0, 1.0, 0.5), "#00ff00");
        assert_eq!(hsl_to_hex(240.0, 1.0, 0.5), "#0000ff");
        assert_eq!(hsl_to_hex(359.0, 0.0, 0.5), "#808080");
    }

    #[test]
    fn survives_reopen() {
        let dir = std::env::temp_dir().join(format!("wuppp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let path = path.to_str().unwrap();

        let db = History::open(path).unwrap();
        db.record(&host("a", 1, Utc::now())).unwrap();
        let color = db.user_color("u", None).unwrap();
        drop(db);
        assert_eq!(
            History::open(path).unwrap().user_color("u", None).unwrap(),
            color
        );
        assert_eq!(
            History::open(path)
                .unwrap()
                .for_host("u", "a", None, 10)
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
