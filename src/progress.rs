//! Live progress UI built on `indicatif`.
//!
//! Layout (one bar per parallel copy task, i.e. per destination, plus a
//! grand total bar):
//!
//! ```text
//!   dest:/mnt/hdda  files 12/34 · photos/2026/cat.jpg [██████████▌          ] 1.2GB/4.5GB (26.7%) 412.3MiB/s ETA 00:08
//!   dest:/mnt/hddb  files  1/ 7 · videos/old.mov      [████▉               ] 800MB/9.1GB ( 8.8%) 405.9MiB/s ETA 00:31
//!   TOTAL                                        [█████                   ] 2.0GB/13.6GB (14.7%) 818.2MiB/s ETA 00:19
//! ```
//!
//! Every task bar therefore shows the *full* progress of that parallel copy
//! task (all of its files, not just the current one) while the message still
//! names the file currently being processed — exactly as requested.
//!
//! Styling is exactly as specified:
//! * `.tick_chars(".sSs.")`
//! * `.progress_chars("█▉▊▋▌▍▎▏ ")`

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Bar style for task and total bars.
fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix} {msg} [{bar:40.1}] {bytes}/{total_bytes} ({percent}%) {per_sec} ETA {eta}",
    )
    .expect("valid template")
    .progress_chars("█▉▊▋▌▍▎▏ ")
    .tick_chars(".sSs.")
}

/// Spinner style for the scanning phase (uses the requested tick chars).
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix} {msg}")
        .expect("valid template")
        .tick_chars(".sSs.")
}

/// A handle for one parallel copy task's bar.
///
/// Cheap to clone and `Send + Sync` (indicatif bars share an internal
/// Arc), so workers may hold clones freely.
#[derive(Clone)]
pub struct TaskBar {
    pub bar: ProgressBar,
    pub bytes_done: Arc<AtomicU64>,
    pub files_done: Arc<AtomicU64>,
    pub files_total: Arc<AtomicU64>,
    pub errors: Arc<AtomicU64>,
    pub label: String,
}

impl TaskBar {
    /// Update the displayed "current file" message.
    pub fn set_current_file(&self, rel: &str) {
        let fd = self.files_done.load(Ordering::Relaxed);
        let ft = self.files_total.load(Ordering::Relaxed);
        let msg = if rel.is_empty() {
            format!("files {fd}/{ft}")
        } else {
            format!("files {fd}/{ft} · {rel}")
        };
        self.bar.set_message(msg);
    }

    /// Account `n` bytes copied by this task.
    pub fn inc_bytes(&self, n: u64) {
        self.bytes_done.fetch_add(n, Ordering::Relaxed);
        self.bar.inc(n);
    }

    /// Mark one file as finished.
    pub fn finish_file(&self) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
        self.set_current_file("");
    }

    /// Mark one file as failed.
    pub fn count_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// The whole progress surface.
pub struct ProgressUi {
    multi: MultiProgress,
    hidden: bool,
    total_bar: Option<ProgressBar>,
    tasks: Vec<TaskBar>,
    finished: bool,
}

impl ProgressUi {
    /// Create the UI; automatically hidden when stdout is not a TTY
    /// (logs instead of a live redraw).
    pub fn new() -> Self {
        let multi = MultiProgress::new();
        let hidden = !std::io::stdout().is_terminal();
        if hidden {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        Self {
            multi,
            hidden,
            total_bar: None,
            tasks: Vec::new(),
            finished: false,
        }
    }

    /// A spinner for long-running single-threaded phases (scanning, ...).
    pub fn spinner(&self, prefix: &str, msg: &str) -> ProgressBar {
        let bar = ProgressBar::new(u64::MAX);
        bar.set_style(spinner_style());
        bar.set_prefix(prefix.to_string());
        bar.set_message(msg.to_string());
        if !self.hidden {
            self.multi.add(bar.clone());
        }
        bar
    }

    /// Register a parallel copy task. `total_bytes` is the sum of all file
    /// sizes in the task (set later via [`TaskBar::bar`] if needed).
    pub fn add_task(&mut self, label: &str, total_bytes: u64, total_files: u64) -> TaskBar {
        let bar = ProgressBar::new(total_bytes);
        bar.set_style(bar_style());
        bar.set_prefix(format!("{label}:"));
        let tb = TaskBar {
            bar: bar.clone(),
            bytes_done: Arc::new(AtomicU64::new(0)),
            files_done: Arc::new(AtomicU64::new(0)),
            files_total: Arc::new(AtomicU64::new(total_files)),
            errors: Arc::new(AtomicU64::new(0)),
            label: label.to_string(),
        };
        tb.set_current_file("");
        if !self.hidden {
            self.multi.add(bar.clone());
        }
        self.tasks.push(tb.clone());
        tb
    }

    /// Register the grand total bar.
    pub fn add_total(&mut self, label: &str, total_bytes: u64) -> ProgressBar {
        let bar = ProgressBar::new(total_bytes);
        bar.set_style(bar_style());
        bar.set_prefix(label.to_string());
        if !self.hidden {
            self.multi.add(bar.clone());
        }
        self.total_bar = Some(bar.clone());
        bar
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Finish and clear all bars (or print a newline when hidden).
    pub fn finish_all(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        for t in &self.tasks {
            let _ = t.bar.finish();
        }
        if let Some(b) = &self.total_bar {
            let _ = b.finish();
        }
        if self.hidden {
            println!();
        } else {
            let _ = self.multi.clear();
        }
    }
}
