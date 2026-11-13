//! SQLite-backed history of every accepted report.
//!
//! The in-memory `Store` holds only the latest state per host; this keeps the
//! full timeline. Rows older than the retention period are pruned by the sweeper.
//! The `users` table holds each user's dashboard color; it is never pruned.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{HistoryEntry, Host, MAX_TIMEOUT_MINUTES};

pub struct History {
    conn: Mutex<Connection>,
}

/// Fixed-width RFC 3339 (microseconds, `Z`), so text order equals time order.
fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Parses a stored timestamp; `col` is the column index for the error.
fn parse_ts(s: &str, col: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
        })
}

/// Per sample time: summed `(free, total)` RAM, `None` if nothing was live.
pub type Samples = Vec<Option<(u64, u64)>>;

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
                 reported_at     TEXT    NOT NULL,
                 timeout_minutes INTEGER
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
        // Databases created before per-host timeouts lack the column.
        let has_timeout = conn
            .prepare("SELECT 1 FROM pragma_table_info('reports') WHERE name = 'timeout_minutes'")?
            .exists([])?;
        if !has_timeout {
            conn.execute("ALTER TABLE reports ADD COLUMN timeout_minutes INTEGER", [])?;
        }
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
            "INSERT INTO reports
                 (hostname, username, free_ram_bytes, total_ram_bytes, reported_at, timeout_minutes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                host.hostname,
                host.username,
                host.free_ram_bytes,
                host.total_ram_bytes,
                ts(host.last_seen),
                host.timeout_minutes,
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
                reported_at: parse_ts(&reported_at, 2)?,
            })
        })?
        .collect()
    }

    /// Per user, the summed free/total RAM of the user's hosts that were live
    /// at each of the `samples` (ascending). A host is live at `t` if its latest
    /// report at or before `t` is no older than that report's `timeout_minutes`,
    /// or `default_timeout` if it had none (the same rule as the live store).
    pub fn timeline(
        &self,
        samples: &[DateTime<Utc>],
        default_timeout: TimeDelta,
    ) -> rusqlite::Result<BTreeMap<String, Samples>> {
        let (Some(&first), Some(&last)) = (samples.first(), samples.last()) else {
            return Ok(BTreeMap::new());
        };
        let lookback = default_timeout.max(TimeDelta::minutes(MAX_TIMEOUT_MINUTES.into()));
        let conn = self.conn.lock().unwrap();
        let hosts = conn
            .prepare_cached(
                "SELECT DISTINCT username, hostname FROM reports
                 WHERE reported_at >= ?1 AND reported_at <= ?2",
            )?
            .query_map(params![ts(first - lookback), ts(last)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut latest = conn.prepare_cached(
            "SELECT free_ram_bytes, total_ram_bytes, reported_at, timeout_minutes FROM reports
             WHERE username = ?1 AND hostname = ?2 AND reported_at <= ?3
             ORDER BY reported_at DESC, id DESC
             LIMIT 1",
        )?;
        let mut out = BTreeMap::new();
        for (username, hostname) in hosts {
            let series: &mut Samples = out
                .entry(username.clone())
                .or_insert_with(|| vec![None; samples.len()]);
            for (slot, &t) in series.iter_mut().zip(samples) {
                let row = latest
                    .query_row(params![username, hostname, ts(t)], |row| {
                        Ok((
                            row.get::<_, u64>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<u32>>(3)?,
                        ))
                    })
                    .optional()?;
                let Some((free, total, at, timeout)) = row else {
                    continue;
                };
                let timeout = timeout.map_or(default_timeout, |m| TimeDelta::minutes(m.into()));
                if t - parse_ts(&at, 2)? <= timeout {
                    let (f, tt) = slot.get_or_insert((0, 0));
                    *f = f.saturating_add(free);
                    *tt = tt.saturating_add(total);
                }
            }
        }
        Ok(out)
    }

    /// The newest report of every (username, hostname) that reported at or
    /// after `since`, as live hosts with the user's stored color (empty if the
    /// user has none). Used to refill the in-memory store at startup.
    pub fn latest_reports(&self, since: DateTime<Utc>) -> rusqlite::Result<Vec<Host>> {
        let conn = self.conn.lock().unwrap();
        // SQLite takes the bare columns from the row that has the MAX().
        let mut stmt = conn.prepare(
            "SELECT r.username, r.hostname, r.free_ram_bytes, r.total_ram_bytes,
                    MAX(r.reported_at), r.timeout_minutes, COALESCE(u.color, '')
             FROM reports r LEFT JOIN users u ON u.username = r.username
             WHERE r.reported_at >= ?1
             GROUP BY r.username, r.hostname",
        )?;
        stmt.query_map([ts(since)], |row| {
            let reported_at: String = row.get(4)?;
            Ok(Host {
                username: row.get(0)?,
                hostname: row.get(1)?,
                free_ram_bytes: row.get(2)?,
                total_ram_bytes: row.get(3)?,
                last_seen: parse_ts(&reported_at, 4)?,
                timeout_minutes: row.get(5)?,
                color: row.get(6)?,
            })
        })?
        .collect()
    }

    /// Every stored user color, by username.
    pub fn colors(&self) -> rusqlite::Result<HashMap<String, String>> {
        self.conn
            .lock()
            .unwrap()
            .prepare_cached("SELECT username, color FROM users")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
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
            timeout_minutes: None,
            last_seen: at,
        }
    }

    #[test]
    fn latest_reports_per_user_and_host() {
        let t0 = Utc::now().trunc_subsecs(6);
        let s = TimeDelta::seconds;
        let h = History::open(":memory:").unwrap();
        let at = |user: &str, name: &str, free, t, timeout| Host {
            username: user.into(),
            timeout_minutes: timeout,
            ..host(name, free, t)
        };
        h.record(&at("u", "a", 1, t0, None)).unwrap();
        h.record(&at("u", "a", 2, t0 + s(20), Some(30))).unwrap();
        h.record(&at("u", "a", 3, t0 + s(10), None)).unwrap(); // older, recorded later
        h.record(&at("v", "a", 4, t0 + s(5), None)).unwrap();
        h.record(&at("u", "old", 5, t0 - s(100), None)).unwrap();
        h.user_color("u", Some("#123456")).unwrap();

        let mut got = h.latest_reports(t0).unwrap();
        got.sort_by(|x, y| x.username.cmp(&y.username));
        assert_eq!(
            got.len(),
            2,
            "one per user/host, 'old' is before the cutoff"
        );
        let u = &got[0];
        assert_eq!((u.username.as_str(), u.hostname.as_str()), ("u", "a"));
        assert_eq!(u.free_ram_bytes, 2, "the newest report wins");
        assert_eq!(u.last_seen, t0 + s(20));
        assert_eq!(u.timeout_minutes, Some(30));
        assert_eq!(u.color, "#123456");
        assert_eq!(got[1].username, "v");
        assert_eq!(got[1].color, "", "no stored color yet");
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
    fn timeline_sums_live_hosts_per_user() {
        let t0 = Utc::now().trunc_subsecs(6);
        let m = TimeDelta::minutes;
        let h = History::open(":memory:").unwrap();
        h.record(&host("a", 10, t0)).unwrap();
        h.record(&host("b", 20, t0 + m(1))).unwrap();
        h.record(&host("a", 30, t0 + m(2))).unwrap();
        // Another user whose host keeps itself alive for 30 minutes.
        let v = Host {
            username: "v".into(),
            timeout_minutes: Some(30),
            ..host("c", 5, t0)
        };
        h.record(&v).unwrap();

        let samples = [t0 - m(1), t0, t0 + m(1), t0 + m(2), t0 + m(8), t0 + m(20)];
        let tl = h.timeline(&samples, m(5)).unwrap();
        assert_eq!(
            tl["u"],
            [
                None,            // before any report
                Some((10, 100)), // a
                Some((30, 200)), // a + b
                Some((50, 200)), // a's newer report + b
                None,            // both older than 5 minutes
                None,
            ]
        );
        let v = [None, Some((5, 100)), Some((5, 100))];
        assert_eq!(tl["v"][..3], v);
        assert_eq!(tl["v"][5], Some((5, 100)), "own 30 minute timeout");
        assert!(h.timeline(&[], m(5)).unwrap().is_empty());
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
