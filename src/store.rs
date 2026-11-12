use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, TimeDelta, Utc};

use crate::model::{Dashboard, Host, RankedHost};

/// In-memory store holding the latest report per hostname.
///
/// Hosts whose `last_seen` is older than `stale_after` are treated as gone:
/// reads ignore them immediately, and `prune` deletes them from memory.
pub struct Store {
    hosts: RwLock<HashMap<String, Host>>,
    stale_after: TimeDelta,
}

impl Store {
    pub fn new(stale_after: TimeDelta) -> Self {
        Self {
            hosts: RwLock::default(),
            stale_after,
        }
    }

    /// Number of stored entries, including stale ones not yet pruned.
    #[cfg(test)]
    pub fn stored_len(&self) -> usize {
        self.hosts.read().unwrap().len()
    }

    fn is_fresh(&self, host: &Host, now: DateTime<Utc>) -> bool {
        now - host.last_seen <= self.stale_after
    }

    /// Inserts or replaces a host. Returns `true` if the host was new
    /// (or had already gone stale), i.e. it (re)appears on the dashboard.
    pub fn upsert(&self, host: Host) -> bool {
        let now = host.last_seen;
        let mut hosts = self.hosts.write().unwrap();
        match hosts.insert(host.hostname.clone(), host) {
            None => true,
            Some(prev) => !self.is_fresh(&prev, now),
        }
    }

    pub fn get(&self, hostname: &str, now: DateTime<Utc>) -> Option<Host> {
        let hosts = self.hosts.read().unwrap();
        hosts
            .get(hostname)
            .filter(|h| self.is_fresh(h, now))
            .cloned()
    }

    /// Removes a host. Returns `false` if it didn't exist or was already stale.
    pub fn remove(&self, hostname: &str, now: DateTime<Utc>) -> bool {
        let mut hosts = self.hosts.write().unwrap();
        hosts
            .remove(hostname)
            .is_some_and(|h| self.is_fresh(&h, now))
    }

    /// Deletes all stale hosts. Returns how many were removed.
    pub fn prune(&self, now: DateTime<Utc>) -> usize {
        let mut hosts = self.hosts.write().unwrap();
        let before = hosts.len();
        hosts.retain(|_, h| self.is_fresh(h, now));
        before - hosts.len()
    }

    /// Fresh hosts sorted by free RAM descending, ties broken by hostname ascending.
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
        assert!(store.get("old", now).is_none());
        assert!(!store.remove("old", now));

        store.upsert(host("old", 90, t0));
        assert_eq!(store.prune(now), 1);
        assert_eq!(store.prune(now), 0);
        assert!(store.get("new", now).is_some());
    }

    #[test]
    fn reporting_after_going_stale_counts_as_created() {
        let t0 = Utc::now();
        let store = Store::new(secs(60));
        assert!(store.upsert(host("h", 1, t0)));
        assert!(!store.upsert(host("h", 1, t0 + secs(30))));
        assert!(store.upsert(host("h", 1, t0 + secs(100))));
    }
}
