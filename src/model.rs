use chrono::{DateTime, Utc};
use rocket::serde::json::{Value, json};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_HOSTNAME_LEN: usize = 253;
pub const MAX_USERNAME_LEN: usize = 256;
pub const MIN_TIMEOUT_MINUTES: u32 = 1;
pub const MAX_TIMEOUT_MINUTES: u32 = 60;
/// Largest accepted `total_ram_bytes` (400 GiB): no reporting host has more,
/// and bogus reports (there was a 100 PB one) would wreck the chart scale.
pub const MAX_TOTAL_RAM_BYTES: u64 = 400 << 30;

/// Kind of memory a host reports. `system` and `vram` are the generic
/// defaults; the rest name a specific DDR or GDDR generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    System,
    Vram,
    Ddr3,
    Ddr4,
    Ddr5,
    Lpddr5,
    Gddr5,
    Gddr5x,
    Gddr6,
    Gddr6x,
    Gddr7,
}

impl MemoryType {
    const ALL: [MemoryType; 11] = [
        Self::System,
        Self::Vram,
        Self::Ddr3,
        Self::Ddr4,
        Self::Ddr5,
        Self::Lpddr5,
        Self::Gddr5,
        Self::Gddr5x,
        Self::Gddr6,
        Self::Gddr6x,
        Self::Gddr7,
    ];

    /// The wire/database name, e.g. `"gddr6x"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Vram => "vram",
            Self::Ddr3 => "ddr3",
            Self::Ddr4 => "ddr4",
            Self::Ddr5 => "ddr5",
            Self::Lpddr5 => "lpddr5",
            Self::Gddr5 => "gddr5",
            Self::Gddr5x => "gddr5x",
            Self::Gddr6 => "gddr6",
            Self::Gddr6x => "gddr6x",
            Self::Gddr7 => "gddr7",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// Used when a report doesn't say: `vram` if the hostname ends in "vram"
    /// (case-insensitive), else `system`.
    pub fn default_for(hostname: &str) -> Self {
        if hostname.to_ascii_lowercase().ends_with("vram") {
            Self::Vram
        } else {
            Self::System
        }
    }
}

/// Memory report sent by a host.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(example = "example_report")]
pub struct HostReport {
    /// Free (available) RAM in bytes. Must be <= `total_ram_bytes`.
    pub free_ram_bytes: u64,
    /// Total RAM in bytes, at most 400 GiB (429496729600). Larger reports are
    /// rejected with 422 and not stored.
    #[schemars(range(min = 1, max = 429_496_729_600u64))]
    pub total_ram_bytes: u64,
    /// Optional dashboard color for the user, `#rrggbb`. Sets (or changes) the
    /// user's color for all of their hosts. Without it, a user gets a random
    /// color on their first report, which is kept from then on.
    #[serde(default)]
    #[schemars(regex(pattern = r"^#[0-9A-Fa-f]{6}$"))]
    pub color: Option<String>,
    /// Optional per-host timeout in minutes (1-60), for clients that can't
    /// report often: the host stays on the dashboard until this long after its
    /// last report, instead of the server's `stale_after_secs`. Applies until
    /// the next report; a report without it goes back to the default.
    #[serde(default)]
    #[schemars(range(min = 1, max = 60))]
    pub timeout_minutes: Option<u32>,
    /// Optional kind of memory: `system`, `vram`, `ddr3`-`ddr5`, `lpddr5` or
    /// `gddr5`/`gddr5x`/`gddr6`/`gddr6x`/`gddr7`. Defaults to `vram` if the
    /// hostname ends in "vram" (case-insensitive), else `system`. The
    /// dashboard prices free RAM by it.
    #[serde(default)]
    pub memory_type: Option<MemoryType>,
}

fn example_report() -> Value {
    json!({
        "free_ram_bytes": 8_589_934_592u64,
        "total_ram_bytes": 17_179_869_184u64,
        "color": "#3d8bff",
        "timeout_minutes": 30,
        "memory_type": "ddr5",
    })
}

impl HostReport {
    pub fn validate(&self) -> Result<(), String> {
        if self.total_ram_bytes == 0 {
            return Err("total_ram_bytes must be greater than 0".into());
        }
        if self.free_ram_bytes > self.total_ram_bytes {
            return Err("free_ram_bytes must not exceed total_ram_bytes".into());
        }
        if self.total_ram_bytes > MAX_TOTAL_RAM_BYTES {
            return Err(format!(
                "total_ram_bytes must be at most {MAX_TOTAL_RAM_BYTES} (400 GiB)"
            ));
        }
        if let Some(color) = &self.color {
            normalize_color(color)?;
        }
        if let Some(minutes) = self.timeout_minutes
            && !(MIN_TIMEOUT_MINUTES..=MAX_TIMEOUT_MINUTES).contains(&minutes)
        {
            return Err(format!(
                "timeout_minutes must be {MIN_TIMEOUT_MINUTES}-{MAX_TIMEOUT_MINUTES}"
            ));
        }
        Ok(())
    }
}

/// Validates a (percent-decoded) username and returns it trimmed.
pub fn normalize_username(raw: &str) -> Result<String, String> {
    let username = raw.trim();
    if username.is_empty() {
        return Err("username must not be empty".into());
    }
    if username.len() > MAX_USERNAME_LEN {
        return Err(format!("username must be at most {MAX_USERNAME_LEN} bytes"));
    }
    Ok(username.to_owned())
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

/// Validates a `#rrggbb` color and returns it lowercased.
pub fn normalize_color(raw: &str) -> Result<String, String> {
    match raw.strip_prefix('#') {
        Some(hex) if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
            Ok(raw.to_ascii_lowercase())
        }
        _ => Err("color must be a hex code like #3d8bff".into()),
    }
}

/// Latest stored state of a host.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Host {
    /// Lowercased hostname.
    pub hostname: String,
    pub username: String,
    pub free_ram_bytes: u64,
    pub total_ram_bytes: u64,
    /// The user's dashboard color, `#rrggbb` (same for all of a user's hosts).
    pub color: String,
    /// Per-host timeout from the latest report, in minutes; `null` means the
    /// server default (`stale_after_secs`).
    pub timeout_minutes: Option<u32>,
    /// Kind of memory: from the report, or the default for the hostname.
    pub memory_type: MemoryType,
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
    pub free_ram_bytes: u64,
    pub total_ram_bytes: u64,
    /// Server time the report arrived.
    pub reported_at: DateTime<Utc>,
}

/// Stored reports of one user on one host, newest first.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HostHistory {
    pub username: String,
    /// Lowercased hostname.
    pub hostname: String,
    pub entries: Vec<HistoryEntry>,
}

/// Free and total RAM per sample time, aligned with `Timeline::samples`;
/// `null` where no host was live.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Series {
    pub free_ram_bytes: Vec<Option<u64>>,
    pub total_ram_bytes: Vec<Option<u64>>,
}

impl Series {
    pub fn from_samples(samples: &[Option<(u64, u64)>]) -> Self {
        Self {
            free_ram_bytes: samples.iter().map(|s| s.map(|(f, _)| f)).collect(),
            total_ram_bytes: samples.iter().map(|s| s.map(|(_, t)| t)).collect(),
        }
    }
}

/// The summed RAM of one user's live hosts over time.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UserSeries {
    pub username: String,
    /// The user's dashboard color, `#rrggbb`.
    pub color: String,
    #[serde(flatten)]
    pub series: Series,
}

/// Free vs total RAM of all hosts, sampled from the history.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Timeline {
    pub generated_at: DateTime<Utc>,
    /// How far back the samples go, in seconds.
    pub span_secs: u64,
    /// Sample times, oldest first (exactly `span_secs` ago) to newest (now),
    /// evenly spaced in `ln(age + 900 s)`: dense near now, sparse in the past. Each series has one value per sample.
    pub samples: Vec<DateTime<Utc>>,
    /// Sum over all live hosts.
    pub total: Series,
    /// Per user, sorted by username.
    pub users: Vec<UserSeries>,
}
