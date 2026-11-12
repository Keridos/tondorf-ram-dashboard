use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_HOSTNAME_LEN: usize = 253;
pub const MAX_USERNAME_LEN: usize = 256;

/// Body of `PUT /api/v1/hosts/<hostname>`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostReport {
    pub username: String,
    pub free_ram_bytes: u64,
    pub total_ram_bytes: u64,
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
#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub hostname: String,
    pub username: String,
    pub free_ram_bytes: u64,
    pub total_ram_bytes: u64,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct RankedHost {
    pub rank: usize,
    #[serde(flatten)]
    pub host: Host,
    pub free_ram_percent: f64,
}

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub generated_at: DateTime<Utc>,
    pub stale_after_secs: u64,
    pub host_count: usize,
    pub hosts: Vec<RankedHost>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}
