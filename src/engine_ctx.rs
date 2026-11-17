//! Shared engine state: per-task progress handles, counters, warnings,
//! events and the global bandwidth pacer.
//!
//! Everything mutable is behind atomics or mutexes, so worker threads can
//! update progress and results concurrently without synchronization
//! mistakes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use std::path::PathBuf;

use crate::error::AppError;
use crate::fsutil::Pacer;
use crate::progress::ProgressUi;

/// Error for a single job (file), kept for the final report.
#[derive(Debug)]
pub struct JobError {
    pub rel: String,
    pub error: AppError,
}

/// Clone for report snapshots: the error is kept as its rendered text
/// (`std::io::Error` is not `Clone`, so the structured source is not
/// preserved across the clone — the message is).
impl Clone for JobError {
    fn clone(&self) -> Self {
        JobError {
            rel: self.rel.clone(),
            error: crate::error::AppError::Other(self.error.to_string()),
        }
    }
}

/// Result of one executed job.
#[derive(Debug)]
pub enum JobOutcome {
    /// A file was copied.
    Copied { bytes: u64 },
    /// A file was skipped (up to date, checksum identical, newer at dest).
    Skipped { reason: String },
    /// A file was hard-linked (link-dest / hard-links) or a symlink/node
    /// was created.
    Linked { bytes: u64, link_dest: bool },
    /// The job failed.
    Error(JobError),
    /// Cancelled before starting.
    Cancelled,
    /// Dry-run: nothing was done.
    DryRun { bytes: u64 },
}

/// Context shared by all workers of one run.
pub struct EngineCtx {
    /// The shared progress UI (per-file bars + total bar).
    pub ui: Arc<ProgressUi>,
    /// One label per destination (for reporting/debugging).
    pub labels: Vec<String>,
    /// Shared total bandwidth bucket (`--bwlimit` / `--slow`).
    pub pacer: Option<Mutex<Pacer>>,
    /// True in `--dry-run` mode (no mutations at all).
    pub dry_run: bool,
    /// Suffix for `--backup` files.
    pub suffix: String,
    /// Started at the beginning of the transfer phase.
    pub started: Instant,

    pub copied_bytes: AtomicU64,
    pub copied_files: AtomicU64,
    pub skipped_files: AtomicU64,
    pub linked_files: AtomicU64,
    pub error_files: AtomicU64,
    pub cancelled_jobs: AtomicU64,
    pub deleted_files: AtomicU64,
    pub deleted_bytes: AtomicU64,
    /// Destination paths that were successfully transferred (for --chmod
    /// post-processing and reports).
    pub transferred: Mutex<Vec<PathBuf>>,

    pub errors: Mutex<Vec<JobError>>,
    pub warnings: Mutex<Vec<String>>,
    /// Human-readable events (backups made, fstrim, ...).
    pub events: Mutex<Vec<String>>,
}

impl EngineCtx {
    pub fn new(
        ui: Arc<ProgressUi>,
        labels: Vec<String>,
        pacer: Option<Mutex<Pacer>>,
        dry_run: bool,
        suffix: String,
    ) -> Self {
        Self {
            ui,
            labels,
            pacer,
            dry_run,
            suffix,
            started: Instant::now(),
            copied_bytes: AtomicU64::new(0),
            copied_files: AtomicU64::new(0),
            skipped_files: AtomicU64::new(0),
            linked_files: AtomicU64::new(0),
            error_files: AtomicU64::new(0),
            cancelled_jobs: AtomicU64::new(0),
        deleted_files: AtomicU64::new(0),
        deleted_bytes: AtomicU64::new(0),
        transferred: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
            warnings: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn add_warning(&self, msg: String) {
        eprintln!("warning: {msg}");
        self.warnings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(msg);
    }

    pub fn add_event(&self, msg: String) {
        if !self.dry_run {
            eprintln!("{msg}");
        } else {
            eprintln!("[dry-run] {msg}");
        }
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(msg);
    }

    pub fn record_cancelled(&self) {
        self.cancelled_jobs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_transferred(&self, path: PathBuf) {
        self.transferred
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(path);
    }

    /// Update global counters for a finished job.
    pub fn finish_job(&self, outcome: JobOutcome) {
        match outcome {
            JobOutcome::Copied { bytes } => {
                self.copied_bytes.fetch_add(bytes, Ordering::Relaxed);
                self.copied_files.fetch_add(1, Ordering::Relaxed);
            }
            JobOutcome::Skipped { .. } => {
                self.skipped_files.fetch_add(1, Ordering::Relaxed);
            }
            JobOutcome::Linked { .. } => {
                self.linked_files.fetch_add(1, Ordering::Relaxed);
            }
            JobOutcome::Error(e) => {
                self.error_files.fetch_add(1, Ordering::Relaxed);
                self.errors
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(e);
            }
            JobOutcome::Cancelled => {
                self.record_cancelled();
            }
            JobOutcome::DryRun { .. } => {}
        }
    }

    /// Total errors collected so far.
    pub fn error_count(&self) -> u64 {
        self.error_files.load(Ordering::Relaxed)
    }
}
