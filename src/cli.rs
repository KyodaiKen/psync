//! rsync-compatible command line (clap).
//!
//! Note on `-h`: like rsync, `-h` is *human-readable sizes*; use `--help`
//! for usage. Remote-only rsync options (`--daemon`, `--rsh`, `--port`,
//! `--sockopts`, `--rsync-path`, `--files-from`) are accepted so that
//! existing scripts/aliases parse, but psync is local-only and rejects them
//! with a clear error instead of silently misbehaving.

use std::path::PathBuf;

use clap::{ArgAction, Parser};

use crate::error::{AppError, Result};
use crate::fsutil::parse_size;

/// Parse a KiB-denominated size (rsync `--max-size`/`--bwlimit` are in KB/KiB).
fn parse_kbytes(input: &str) -> Result<u64> {
    let s = input.trim().to_ascii_uppercase();
    let s = s.trim_end_matches("B");
    if let Some(num) = s.strip_prefix('K') {
        return Ok(num.parse::<u64>().map_err(|_| {
            AppError::Usage(format!("cannot parse KiB size '{input}'"))
        })?);
    }
    if s.is_empty() {
        return Err(AppError::Usage(format!("cannot parse KiB size '{input}'")));
    }
    if let Ok(v) = s.parse::<u64>() {
        // Plain number: treat as KiB (rsync semantics for -M/--bwlimit).
        return Ok(v);
    }
    // Be forgiving: accept plain human sizes and convert to KiB.
    let bytes = parse_size(input)?;
    Ok(bytes.div_ceil(1024))
}

fn parse_nonneg(input: &str) -> Result<u32> {
    input
        .trim()
        .parse::<u32>()
        .map_err(|_| AppError::Usage(format!("cannot parse non-negative integer '{input}'")))
}

#[derive(Parser, Debug)]
#[command(
    name = "psync",
    version = "0.1.0",
    about = "psync — rsync-compatible local synchronizer with parallel transfer and live progress bars",
    disable_help_flag = true,
    disable_version_flag = true,
)]
pub struct Cli {
    /// `--help` — usage. (Handled in `run`; `-h` is human-readable sizes.)
    #[arg(long = "help")]
    pub help: bool,

    /// `--version` / `-V` (handled in `run`).
    #[arg(short = 'V', long = "version")]
    pub version: bool,

    // --- transfer modifiers ------------------------------------------------
    /// Archive mode: -rlptgoD.
    #[arg(short = 'a', long = "archive")]
    pub archive: bool,
    /// Recursive.
    #[arg(short = 'r', long = "recursive")]
    pub recursive: bool,
    /// Copy symlinks as symlinks.
    #[arg(short = 'l', long = "links")]
    pub links: bool,
    /// Preserve permissions.
    #[arg(short = 'p', long = "perms")]
    pub perms: bool,
    /// Preserve modification times.
    #[arg(short = 't', long = "times")]
    pub times: bool,
    /// Preserve group.
    #[arg(short = 'g', long = "group")]
    pub group: bool,
    /// Preserve owner (requires root to take effect).
    #[arg(short = 'o', long = "owner")]
    pub owner: bool,
    /// Preserve devices and FIFOs (requires root).
    #[arg(short = 'D', long = "devices")]
    pub devices: bool,

    /// Compare by checksum instead of mtime+size.
    #[arg(short = 'c', long = "checksum")]
    pub checksum: bool,
    /// Compare by size only.
    #[arg(long = "size-only")]
    pub size_only: bool,
    /// Tolerance for mtime comparison (seconds).
    #[arg(long = "modify-window", default_value_t = 0)]
    pub modify_window: i64,
    /// Skip files that are newer at the destination.
    #[arg(short = 'u', long = "update")]
    pub update: bool,
    /// Transfer all files (ignore size/time checks).
    #[arg(long = "ignore-times")]
    pub ignore_times: bool,
    /// Whole-file transfer: no-op locally (always whole-file).
    #[arg(short = 'W', long = "whole-file")]
    pub whole_file: bool,
    /// Follow symlinks (copy the target's contents).
    #[arg(short = 'L', long = "copy-links")]
    pub copy_links: bool,

    // --- output ------------------------------------------------------------
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    pub verbose: u8,
    /// Human-readable numbers in the stats output.
    #[arg(short = 'h', help = "human-readable sizes in the stats output")]
    pub human: bool,
    /// Keep partially transferred files.
    #[arg(short = 'P', long = "partial")]
    pub partial: bool,
    /// Show per-file progress (bars are always on for parallel work).
    #[arg(long = "progress")]
    pub progress: bool,
    /// Print a summary at the end.
    #[arg(long = "stats")]
    pub stats: bool,
    /// Output a `*aius`-style itemized line per changed file.
    #[arg(long = "itemize-changes")]
    pub itemize: bool,
    /// Custom out-format (supported tokens: %i %n %b).
    #[arg(long = "out-format", value_name = "FMT")]
    pub out_format: Option<String>,
    /// Write itemized lines to a log file.
    #[arg(long = "log-file", value_name = "FILE")]
    pub log_file: Option<PathBuf>,
    /// Format for log-file lines (defaults to the itemize format).
    #[arg(long = "log-format", value_name = "FMT")]
    pub log_format: Option<String>,
    /// Just list files, no transfer.
    #[arg(long = "list-only")]
    pub list_only: bool,

    // --- selection ---------------------------------------------------------
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    /// Maximum file size in KiB (files larger are skipped).
    #[arg(short = 'M', long = "max-size", value_name = "KIB", value_parser = parse_kbytes)]
    pub max_size: Option<u64>,
    /// Maximum file size (human readable, e.g. 100M).
    #[arg(long = "max-filesize", value_parser = parse_size)]
    pub max_filesize: Option<u64>,
    /// Minimum file size (human readable, e.g. 10K).
    #[arg(long = "min-filesize", value_parser = parse_size)]
    pub min_filesize: Option<u64>,
    /// Recurse into directories at most N levels deep (0 = top level only).
    #[arg(long = "max-depth", value_parser = parse_nonneg)]
    pub max_depth: Option<u32>,

    // --- filters -----------------------------------------------------------
    /// Filter rule: `ACTION PATTERN` (action: `-`, `+`, `!`).
    #[arg(short = 'f', long = "filter", value_name = "SPEC", action = ArgAction::Append)]
    pub filter: Vec<String>,
    #[arg(long = "exclude", value_name = "PATTERN", action = ArgAction::Append)]
    pub exclude: Vec<String>,
    #[arg(long = "include", value_name = "PATTERN", action = ArgAction::Append)]
    pub include: Vec<String>,
    #[arg(long = "exclude-from", value_name = "FILE", action = ArgAction::Append)]
    pub exclude_from: Vec<PathBuf>,
    #[arg(long = "include-from", value_name = "FILE", action = ArgAction::Append)]
    pub include_from: Vec<PathBuf>,
    #[arg(long = "filter-from", value_name = "FILE", action = ArgAction::Append)]
    pub filter_from: Vec<PathBuf>,

    // --- deletion ----------------------------------------------------------
    /// Delete extraneous files from the destination.
    #[arg(long = "delete")]
    pub delete: bool,
    /// Delete before transferring (default when --delete is given).
    #[arg(long = "delete-before")]
    pub delete_before: bool,
    /// Delete after transferring.
    #[arg(long = "delete-after")]
    pub delete_after: bool,
    /// Delete during transferring (psync maps this to before).
    #[arg(long = "delete-during")]
    pub delete_during: bool,
    /// Also delete empty directories.
    #[arg(long = "prune-empty-dirs")]
    pub prune_empty: bool,
    /// Allow deleting non-empty directories and force -n.
    #[arg(long = "force")]
    pub force: bool,

    // --- misc --------------------------------------------------------------
    /// Backup replaced destination files.
    #[arg(short = 'b', long = "backup")]
    pub backup: bool,
    /// Suffix for --backup files.
    #[arg(long = "suffix", value_name = "SUFFIX")]
    pub suffix: Option<String>,
    /// Apply a mode to received files (e.g. a+rX).
    #[arg(long = "chmod", value_name = "MODE")]
    pub chmod: Option<String>,
    /// Do not update directory modification times.
    #[arg(long = "omit-dir-times")]
    pub omit_dir_times: bool,
    #[arg(long = "no-perms")]
    pub no_perms: bool,
    #[arg(long = "no-o")]
    pub no_o: bool,
    #[arg(long = "no-g")]
    pub no_g: bool,
    /// Do not cross filesystem boundaries.
    #[arg(short = 'x', long = "one-file-system")]
    pub one_fs: bool,
    /// Preserve hard link structure.
    #[arg(long = "hard-links")]
    pub hard_links: bool,
    /// Hard-link identical files from DIR instead of copying.
    #[arg(long = "link-dest", value_name = "DIR")]
    pub link_dest: Option<PathBuf>,
    /// Sparse output: accepted for compatibility (local writes are normal).
    #[arg(short = 's', long = "sparse")]
    pub sparse: bool,
    /// Delete files from the source after transfer.
    #[arg(long = "remove-source-files")]
    pub remove_source: bool,
    /// Store temp files in DIR during transfer.
    #[arg(long = "temp-dir", value_name = "DIR")]
    pub temp_dir: Option<PathBuf>,
    #[arg(long = "numeric-ids", help = "accepted for compatibility (local copies always use numeric ids)")]
    pub numeric_ids: bool,
    #[arg(long = "timeout", value_name = "SECS", help = "accepted for compatibility (local transfers are not remote)")]
    pub timeout: Option<u32>,
    #[arg(short = 'z', long = "compress", help = "accepted for compatibility (local transfers are not remote)")]
    pub compress: bool,

    // --- parallelism / bandwidth -------------------------------------------
    /// Number of parallel copy workers (default 2 in rsync mode; in config
    /// mode the config file's `max_parallel_processes` wins unless given).
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,
    /// Total bandwidth limit in KiB/s.
    #[arg(long = "bwlimit", value_name = "KIB_PER_SEC", value_parser = parse_kbytes)]
    pub bwlimit: Option<u64>,
    /// pybackupsync compatibility: limit speed to 1 MiB/s.
    #[arg(long = "slow")]
    pub slow: bool,

    // --- backup config mode (pybackupsync compatible) -----------------------
    /// YAML config file (pybackupsync format); switches to backup mode.
    #[arg(short = 'C', long = "config", value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Unmount auto-mounted drives after success (backup mode).
    #[arg(short = 'U', long = "umount", help = "backup mode: unmount auto-mounted drives after success")]
    pub umount: bool,

    // --- remote-only options (accepted, then rejected) ----------------------
    #[arg(short = 'e', long = "rsh", value_name = "CMD", help = "NOT SUPPORTED: psync is local-only")]
    pub rsh: Option<String>,
    #[arg(long = "daemon", help = "NOT SUPPORTED: psync is local-only")]
    pub daemon: bool,
    #[arg(long = "port", value_name = "PORT", help = "NOT SUPPORTED: psync is local-only")]
    pub port: Option<u16>,
    #[arg(long = "sockopts", value_name = "OPTS", help = "NOT SUPPORTED: psync is local-only")]
    pub sockopts: Option<String>,
    #[arg(long = "rsync-path", value_name = "PATH", help = "NOT SUPPORTED: psync is local-only")]
    pub rsync_path: Option<String>,
    #[arg(long = "files-from", value_name = "FILE", help = "NOT IMPLEMENTED in psync")]
    pub files_from: Option<PathBuf>,

    // --- positional: SRC [SRC...] DST --------------------------------------
    #[arg(value_name = "SRC...DST", help = "one or more source paths and the destination (last argument)")]
    pub paths: Vec<PathBuf>,
}

impl Cli {
    /// Effective recursive flag (rsync: -a implies -r; without -r/-a only
    /// top-level non-directory entries are transferred).
    pub fn recursive(&self) -> bool {
        self.archive || self.recursive
    }

    /// Effective links flag (-a implies -l).
    pub fn links(&self) -> bool {
        self.archive || self.links || self.copy_links
    }

    /// Effective perms flag (-a implies -p).
    pub fn perms(&self) -> bool {
        (self.archive || self.perms) && !self.no_perms
    }

    /// Effective times flag (-a implies -t).
    pub fn times(&self) -> bool {
        (self.archive || self.times) && !self.no_perms
    }

    /// Effective group flag (-a implies -g).
    pub fn group(&self) -> bool {
        (self.archive || self.group) && !self.no_g
    }

    /// Effective owner flag (-a implies -o).
    pub fn owner(&self) -> bool {
        (self.archive || self.owner) && !self.no_o
    }

    /// Effective devices flag (-a implies -D).
    pub fn devices(&self) -> bool {
        (self.archive || self.devices) && !self.no_o
    }

    /// Effective bandwidth limit in KiB/s, if any.
    pub fn bwlimit_kib(&self) -> Option<u64> {
        if let Some(k) = self.bwlimit {
            return Some(k);
        }
        if self.slow {
            eprintln!("WARNING: SLOW MODE ENABLED -> 1MByte/s speed limit enabled!");
            return Some(1024);
        }
        None
    }

    /// Validate options that must be rejected with a clear error.
    pub fn validate(&self) -> Result<()> {
        if let Some(e) = &self.rsh {
            return Err(AppError::Unsupported(format!("--rsh {e:?}")));
        }
        if self.daemon {
            return Err(AppError::Unsupported("--daemon".into()));
        }
        if let Some(p) = self.port {
            return Err(AppError::Unsupported(format!("--port {p}")));
        }
        if let Some(s) = &self.sockopts {
            return Err(AppError::Unsupported(format!("--sockopts {s:?}")));
        }
        if let Some(p) = &self.rsync_path {
            return Err(AppError::Unsupported(format!("--rsync-path {p:?}")));
        }
        if let Some(f) = &self.files_from {
            return Err(AppError::NotImplemented(format!("--files-from {}", f.display())));
        }
        if let Some(j) = self.jobs {
            if j == 0 {
                return Err(AppError::Usage("--jobs must be >= 1".into()));
            }
        }
        if self.delete_before && self.delete_after {
            return Err(AppError::Usage("cannot combine --delete-before and --delete-after".into()));
        }
        if self.max_size.is_some() && self.max_filesize.is_some() {
            return Err(AppError::Usage("use either --max-size or --max-filesize, not both".into()));
        }
        if self.min_filesize.is_some() && self.max_filesize.is_some() {
            if let (Some(lo), Some(hi)) = (self.min_filesize, self.max_filesize) {
                if lo > hi {
                    return Err(AppError::Usage("--min-filesize is larger than --max-filesize".into()));
                }
            }
        }
        Ok(())
    }

    /// Maximum file size in bytes, combining -M and --max-filesize.
    pub fn max_file_bytes(&self) -> Option<u64> {
        if let Some(kib) = self.max_size {
            return Some(kib * 1024);
        }
        self.max_filesize
    }
}
