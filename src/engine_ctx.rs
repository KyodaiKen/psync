//! Shared engine state: per-task progress handles, counters, warnings,
//! events and the global bandwidth pacer.
//!
//! Everything mutable is behind atomics or mutexes, so worker threads can
//! update progress and results concurrently without synchronization
//! mistakes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use std::path::{Path, PathBuf};

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
    /// Show the *destination* file path on each progress line (config
    /// mode: tells apart parallel copies to different destinations).
    /// rsync mode shows the classic source-relative name instead.
    pub show_dst_names: bool,
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
    /// Longest common *component-wise* prefix of the destination
    /// labels ("" when they don't share one). Component-wise so that
    /// `.../dest-a` and `.../dest-b` do not merge into a bogus prefix.
    fn common_label_prefix(labels: &[String]) -> String {
        if labels.is_empty() {
            return String::new();
        }
        let mut rest: Vec<Vec<&str>> = labels.iter().map(|l| l.split('/').collect()).collect();
        let mut out: Vec<&str> = Vec::new();
        loop {
            let head = rest[0].first().copied();
            if head.is_none() || rest.iter().any(|c| c.first().copied() != head) {
                break;
            }
            // Never swallow a one-component label whole.
            if rest.iter().any(|c| c.len() <= 1) {
                break;
            }
            out.push(head.unwrap());
            for c in rest.iter_mut() {
                c.remove(0);
            }
        }
        out.join("/")
    }

    /// Progress-line name for a copied file when destination identity
    /// matters (config mode): the destination root with the prefix
    /// shared by all destinations of this run stripped, plus the file's
    /// relative path — e.g. `dest-a/data/archive.tar.gz` vs
    /// `dest-b/mirror/data/archive.tar.gz`. Falls back to the plain
    /// relative path when the root would be empty (single destination),
    /// or to the full destination path if it matches no label.
    pub fn dst_display_name(labels: &[String], dst: &Path, rel: &str) -> String {
        let prefix = Self::common_label_prefix(labels);
        for l in labels {
            if dst.starts_with(l.as_str()) {
                let root = if prefix.is_empty() {
                    l.clone()
                } else {
                    l.strip_prefix(prefix.as_str())
                        .unwrap_or(l.as_str())
                        .trim_start_matches('/')
                        .to_string()
                };
                if root.is_empty() {
                    return rel.to_string();
                }
                return format!("{root}/{rel}");
            }
        }
        dst.display().to_string()
    }

    pub fn new(
        ui: Arc<ProgressUi>,
        labels: Vec<String>,
        pacer: Option<Mutex<Pacer>>,
        dry_run: bool,
        suffix: String,
        show_dst_names: bool,
    ) -> Self {
        Self {
            ui,
            labels,
            pacer,
            dry_run,
            suffix,
            show_dst_names,
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
        // Routed through the UI so the line coexists with the live frame
        // (while it runs) and prints plainly otherwise (dry runs, errors).
        // Warnings get their own bold-amber styling in the live frame.
        self.ui.warn(&format!("warning: {msg}"));
        self.warnings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(msg);
    }

    pub fn add_event(&self, msg: String) {
        let line = if self.dry_run {
            format!("[dry-run] {msg}")
        } else {
            msg.clone()
        };
        self.ui.println(&line);
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
