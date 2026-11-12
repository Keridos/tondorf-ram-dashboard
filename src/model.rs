use chrono::{DateTime, Utc};
use rocket::serde::json::{Value, json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_HOSTNAME_LEN: usize = 253;
pub const MAX_USERNAME_LEN: usize = 256;

/// Memory report sent by a host.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(example = "example_report")]
pub struct HostReport {
    /// Name of the user on the host.
    #[schemars(length(min = 1, max = 256))]
    pub username: String,
    /// Free (available) RAM in bytes. Must be <= `total_ram_bytes`.
    pub free_ram_bytes: u64,
    /// Total RAM in bytes (at most 2^63-1).
    #[schemars(range(min = 1))]
    pub total_ram_bytes: u64,
}

fn example_report() -> Value {
    json!({
        "username": "alice",
        "free_ram_bytes": 8_589_934_592u64,
        "total_ram_bytes": 17_179_869_184u64,
    })
}

impl HostReport {
    pub fn validate(&self) -> Result<(), String> {
        let username = self.username.trim();
        if username.is_empty() {
            return Err("username must not be empty".into());
        }
        if username.len() > MAX_USERNAME_LEN {
            return Err(format!("username must be at most {MAX_USERNAME_LEN} bytes"));
        }
        if self.total_ram_bytes == 0 {
            return Err("total_ram_bytes must be greater than 0".into());
        }
        if self.free_ram_bytes > self.total_ram_bytes {
            return Err("free_ram_bytes must not exceed total_ram_bytes".into());
        }
        // SQLite integers are signed 64-bit.
        if self.total_ram_bytes > i64::MAX as u64 {
            return Err(format!("total_ram_bytes must be at most {}", i64::MAX));
        }
        Ok(())
    }
}

/// Validates a hostname and returns its normalized (lowercase) form.
pub fn normalize_hostname(raw: &str) -> Result<String, String> {
    if raw.is_empty() || raw.len() > MAX_HOSTNAME_LEN {
        return Err(format!("hostname must be 1-{MAX_HOSTNAME_LEN} characters"));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err("hostname may only contain [A-Za-z0-9.-_]".into());
    }
    Ok(raw.to_ascii_lowercase())
}

/// Latest stored state of a host.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Host {
    /// Lowercased hostname.
    pub hostname: String,
    pub username: String,
    pub free_ram_bytes: u64,
    pub total_ram_bytes: u64,
    /// Server time of the latest report.
    pub last_seen: DateTime<Utc>,
}

/// A host with its position on the dashboard.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RankedHost {
    /// 1-based position, ordered by free RAM descending.
    pub rank: usize,
    #[serde(flatten)]
    pub host: Host,
    /// `free_ram_bytes / total_ram_bytes * 100`, rounded to 2 decimals.
    pub free_ram_percent: f64,
}

/// Hosts ranked by free RAM.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Dashboard {
    pub generated_at: DateTime<Utc>,
    /// Hosts are dropped this many seconds after their `last_seen`.
    pub stale_after_secs: u64,
    /// Number of non-stale hosts (before `limit` is applied).
    pub host_count: usize,
    pub hosts: Vec<RankedHost>,
}

/// Error response body.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ErrorBody {
    pub error: String,
}

/// One past report of a host.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HistoryEntry {
    pub username: String,
    pub free_ram_bytes: u64,
    pub total_ram_bytes: u64,
    /// Server time the report arrived.
    pub reported_at: DateTime<Utc>,
}

/// Stored reports of a host, newest first.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HostHistory {
    /// Lowercased hostname.
    pub hostname: String,
    pub entries: Vec<HistoryEntry>,
}
