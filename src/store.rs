use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, TimeDelta, Utc};

use crate::model::{Dashboard, Host, RankedHost};

/// In-memory store holding the latest report per (username, hostname).
///
/// Hosts whose `last_seen` is older than their timeout are treated as gone:
/// reads ignore them immediately, and `prune` deletes them from memory. The
/// timeout is the host's own `timeout_minutes` if it reported one, otherwise
/// `stale_after`.
pub struct Store {
    hosts: RwLock<HashMap<Key, Host>>,
    stale_after: TimeDelta,
}

/// `(username, hostname)`: the same machine reported by two users is two entries.
type Key = (String, String);

fn key(username: &str, hostname: &str) -> Key {
    (username.to_owned(), hostname.to_owned())
}

impl Store {
    pub fn new(stale_after: TimeDelta) -> Self {
        Self {
            hosts: RwLock::default(),
            stale_after,
        }
    }

    /// Default timeout of hosts that didn't report their own.
    pub fn stale_after(&self) -> TimeDelta {
        self.stale_after
    }

    /// Number of stored entries, including stale ones not yet pruned.
    #[cfg(test)]
    pub fn stored_len(&self) -> usize {
        self.hosts.read().unwrap().len()
    }

    fn is_fresh(&self, host: &Host, now: DateTime<Utc>) -> bool {
        let timeout = host
            .timeout_minutes
            .map(|m| TimeDelta::minutes(m.into()))
            .unwrap_or(self.stale_after);
        now - host.last_seen <= timeout
    }

    /// Inserts or replaces a host. Returns `true` if the host was new
    /// (or had already gone stale), i.e. it (re)appears on the dashboard.
    pub fn upsert(&self, host: Host) -> bool {
        let now = host.last_seen;
        let mut hosts = self.hosts.write().unwrap();
        match hosts.insert(key(&host.username, &host.hostname), host) {
            None => true,
            Some(prev) => !self.is_fresh(&prev, now),
        }
    }

    pub fn get(&self, username: &str, hostname: &str, now: DateTime<Utc>) -> Option<Host> {
        let hosts = self.hosts.read().unwrap();
        hosts
            .get(&key(username, hostname))
            .filter(|h| self.is_fresh(h, now))
            .cloned()
    }

    /// Removes a host. Returns `false` if it didn't exist or was already stale.
    pub fn remove(&self, username: &str, hostname: &str, now: DateTime<Utc>) -> bool {
        let mut hosts = self.hosts.write().unwrap();
        hosts
            .remove(&key(username, hostname))
            .is_some_and(|h| self.is_fresh(&h, now))
    }

    /// Sets the color on every stored host of `username`.
    pub fn recolor(&self, username: &str, color: &str) {
        let mut hosts = self.hosts.write().unwrap();
        for host in hosts.values_mut().filter(|h| h.username == username) {
            host.color = color.to_owned();
        }
    }

    /// Deletes all stale hosts. Returns how many were removed.
    pub fn prune(&self, now: DateTime<Utc>) -> usize {
        let mut hosts = self.hosts.write().unwrap();
        let before = hosts.len();
        hosts.retain(|_, h| self.is_fresh(h, now));
        before - hosts.len()
    }

    /// Fresh hosts sorted by free RAM descending, ties broken by hostname, then username, ascending.
    pub fn dashboard(&self, limit: Option<usize>, now: DateTime<Utc>) -> Dashboard {
        let mut hosts: Vec<Host> = self
            .hosts
            .read()
            .unwrap()
            .values()
            .filter(|h| self.is_fresh(h, now))
            .cloned()
            .collect();
        let host_count = hosts.len();

        hosts.sort_by(|a, b| {
            b.free_ram_bytes
                .cmp(&a.free_ram_bytes)
                .then_with(|| a.hostname.cmp(&b.hostname))
                .then_with(|| a.username.cmp(&b.username))
        });

        let ranked = hosts
            .into_iter()
            .take(limit.unwrap_or(usize::MAX))
            .enumerate()
            .map(|(i, host)| RankedHost {
                rank: i + 1,
                free_ram_percent: round2(
                    host.free_ram_bytes as f64 / host.total_ram_bytes as f64 * 100.0,
                ),
                host,
            })
            .collect();

        Dashboard {
            generated_at: now,
            stale_after_secs: self.stale_after.num_seconds() as u64,
            host_count,
            hosts: ranked,
        }
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, free: u64, last_seen: DateTime<Utc>) -> Host {
        Host {
            hostname: name.into(),
            username: "u".into(),
            free_ram_bytes: free,
            total_ram_bytes: 100,
            color: "#000000".into(),
            timeout_minutes: None,
            memory_type: crate::model::MemoryType::System,
            last_seen,
        }
    }

    fn secs(s: i64) -> TimeDelta {
        TimeDelta::seconds(s)
    }

    #[test]
    fn stale_hosts_are_hidden_and_pruned() {
        let t0 = Utc::now();
        let store = Store::new(secs(60));
        store.upsert(host("old", 90, t0));
        store.upsert(host("new", 10, t0 + secs(50)));

        // At t0+60 both are still fresh (boundary is inclusive).
        let now = t0 + secs(60);
        assert_eq!(store.dashboard(None, now).host_count, 2);

        // At t0+61 "old" is stale: hidden from all reads, then pruned.
        let now = t0 + secs(61);
        let dash = store.dashboard(None, now);
        assert_eq!(dash.host_count, 1);
        assert_eq!(dash.hosts[0].host.hostname, "new");
        assert_eq!(dash.hosts[0].rank, 1);
        assert!(store.get("u", "old", now).is_none());
        assert!(!store.remove("u", "old", now));

        store.upsert(host("old", 90, t0));
        assert_eq!(store.prune(now), 1);
        assert_eq!(store.prune(now), 0);
        assert!(store.get("u", "new", now).is_some());
    }

    #[test]
    fn per_host_timeout_overrides_the_default() {
        let t0 = Utc::now();
        let store = Store::new(secs(60));
        let slow = Host {
            timeout_minutes: Some(10),
            ..host("slow", 1, t0)
        };
        store.upsert(slow.clone());
        store.upsert(host("fast", 1, t0));

        // Past the 60 s default, only the host with its own timeout is left.
        let now = t0 + secs(61);
        assert!(store.get("u", "fast", now).is_none());
        assert!(store.get("u", "slow", now).is_some());
        assert_eq!(store.prune(now), 1);
        assert_eq!(store.dashboard(None, now).host_count, 1);

        // 10 minutes is inclusive, like the default.
        assert!(store.get("u", "slow", t0 + secs(600)).is_some());
        let now = t0 + secs(601);
        assert!(store.get("u", "slow", now).is_none());
        assert_eq!(store.prune(now), 1);

        // A report within its own timeout is an update, not a re-appearance.
        assert!(store.upsert(slow.clone()));
        assert!(!store.upsert(Host {
            last_seen: t0 + secs(300),
            ..slow
        }));
    }

    #[test]
    fn reporting_after_going_stale_counts_as_created() {
        let t0 = Utc::now();
        let store = Store::new(secs(60));
        assert!(store.upsert(host("h", 1, t0)));
        assert!(!store.upsert(host("h", 1, t0 + secs(30))));
        assert!(store.upsert(host("h", 1, t0 + secs(100))));
    }

    #[test]
    fn same_hostname_different_users_are_separate() {
        let t0 = Utc::now();
        let store = Store::new(secs(60));
        let as_user = |user: &str, free| Host {
            username: user.into(),
            ..host("h", free, t0)
        };
        assert!(store.upsert(as_user("bob", 10)));
        assert!(store.upsert(as_user("alice", 10)));
        assert!(store.upsert(as_user("carol", 20)));

        let dash = store.dashboard(None, t0);
        let users: Vec<_> = dash
            .hosts
            .iter()
            .map(|h| h.host.username.as_str())
            .collect();
        assert_eq!(users, ["carol", "alice", "bob"], "free desc, then username");

        assert!(store.remove("alice", "h", t0));
        assert!(store.get("alice", "h", t0).is_none());
        assert!(store.get("bob", "h", t0).is_some());
        assert!(store.get("dave", "h", t0).is_none());

        store.upsert(host("h2", 5, t0));
        store.recolor("u", "#abcdef");
        assert_eq!(store.get("u", "h2", t0).unwrap().color, "#abcdef");
        assert_eq!(store.get("bob", "h", t0).unwrap().color, "#000000");
    }
}
