//! YAML configuration for backup mode — a superset of the pybackupsync
//! format, so existing `*.yaml` configs (e.g. `hdd.yaml`, `ssd.yaml`) keep
//! working unchanged.
//!
//! Additions over the Python original:
//! * `uuid` is optional — when omitted, `mount_point` is used as a plain
//!   directory path (handy for local directories and testing).
//! * `compare` per destination: `size` (default, Python parity),
//!   `size-time`, `time-size`, or `checksum`.
//!
//! Unknown fields are rejected with a precise error instead of being
//! silently ignored.

use serde::Deserialize;

use crate::error::{AppError, Result};

/// How to decide whether a destination file is up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompareMode {
    /// Size only (pybackupsync default).
    Size,
    /// Size first, then mtime (rsync default order).
    #[serde(alias = "size-time", alias = "size_time")]
    SizeTime,
    /// Mtime first, then size.
    #[serde(alias = "time-size", alias = "time_size")]
    TimeSize,
    /// SHA-256 checksums (slowest, most exact).
    Checksum,
}

impl CompareMode {
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "size" => Some(CompareMode::Size),
            "size_time" | "sizetime" => Some(CompareMode::SizeTime),
            "time_size" | "timesize" => Some(CompareMode::TimeSize),
            "checksum" => Some(CompareMode::Checksum),
            _ => None,
        }
    }
}

impl std::fmt::Display for CompareMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CompareMode::Size => "size",
            CompareMode::SizeTime => "size-time",
            CompareMode::TimeSize => "time-size",
            CompareMode::Checksum => "checksum",
        };
        f.write_str(s)
    }
}

/// Deletion policies used to free space (pybackupsync parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionPolicy {
    /// Delete the oldest files (by ctime) until enough space is freed.
    Oldest,
    /// Keep one file per YYYY-month (the newest), delete the rest oldest
    /// first; then fall back to oldest single-file deletion.
    MonthDupes,
}

impl DeletionPolicy {
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "oldest" => Some(DeletionPolicy::Oldest),
            "month_dupes" | "month-dupes" | "monthdupes" => Some(DeletionPolicy::MonthDupes),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Parallel copy workers (pybackupsync: `max_parallel_processes`).
    #[serde(default = "default_jobs")]
    pub max_parallel_processes: usize,
    pub source: SourceCfg,
    pub destinations: Vec<DestCfg>,
}

fn default_jobs() -> usize {
    2
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCfg {
    /// UUID of the source partition. Optional: when omitted, `mount_point`
    /// is treated as an existing directory.
    #[serde(default)]
    pub uuid: Option<String>,
    pub mount_point: String,
    /// Relative path below the mount point.
    #[serde(default)]
    pub path: String,
    /// Glob patterns excluded from the source scan.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestCfg {
    /// UUID of the destination partition. Optional (plain directory mode).
    #[serde(default)]
    pub uuid: Option<String>,
    pub mount_point: String,
    /// Relative path where files are stored below the mount point.
    #[serde(default)]
    pub path: String,
    /// Lower number = scheduled first (pybackupsync parity).
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// `oldest` or `month_dupes`.
    #[serde(default = "default_policy")]
    pub deletion_policy: DeletionPolicy,
    /// Patterns protected from deletion; `["*"]` disables deletion.
    #[serde(default)]
    pub deletion_exclude_patterns: Vec<String>,
    /// How to compare existing destination files.
    #[serde(default = "default_compare")]
    pub compare: CompareMode,
}

fn default_priority() -> u32 {
    99
}

fn default_policy() -> DeletionPolicy {
    DeletionPolicy::Oldest
}

fn default_compare() -> CompareMode {
    CompareMode::Size
}

/// Load and validate a YAML config.
pub fn load(path: &std::path::Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("cannot read '{}': {e}", path.display())))?;
    let cfg: Config = serde_yaml::from_str(&raw)
        .map_err(|e| AppError::Config(format!("'{}': {e}", path.display())))?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<()> {
    if cfg.destinations.is_empty() {
        return Err(AppError::Config("config must define at least one destination".into()));
    }
    if cfg.max_parallel_processes == 0 {
        return Err(AppError::Config("max_parallel_processes must be > 0".into()));
    }
    if cfg.source.mount_point.trim().is_empty() {
        return Err(AppError::Config("source.mount_point is required".into()));
    }
    if !cfg.source.uuid.is_some() && !std::path::Path::new(&cfg.source.mount_point).is_dir() {
        return Err(AppError::Config(format!(
            "source has no uuid and mount_point '{}' does not exist",
            cfg.source.mount_point
        )));
    }
    for d in &cfg.destinations {
        if d.mount_point.trim().is_empty() {
            return Err(AppError::Config("destination.mount_point is required".into()));
        }
    }
    Ok(())
}
