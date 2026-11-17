//! Live progress UI built on `indicatif`.
//!
//! Layout — one live bar per active file (at most `-j` at a time, each
//! named, ellipsized to the terminal width), plus one grand total bar:
//!
//! ```text
//! . photos/2026/cat.jpg  [██████████▌        ] 1.2GB/4.5GB (27%) 412MB/s ETA 9s
//! . videos/old.mov       [████▉              ] 800MB/9.1GB (9%) 406MB/s ETA 31s
//! total                  [█████              ] 2.0GB/13.6GB (15%) 818MB/s ETA 19s
//! ```
//!
//! * Redraws and spinner animation run at a steady 250 ms cadence
//!   (`enable_steady_tick`): fluent, low CPU, no erratic spinner.
//! * Values reported from copy chunks are throttled to the same cadence.
//! * Sizes/speeds are human-readable using the unit convention chosen at
//!   startup (`Units::Jedec` default: 1 KB = 1024 B; `Units::Si`: 1 KB = 1000 B).
//! * Names are front-ellipsized ("…long/tail.bin") so the whole line
//!   fits the terminal width.
//! * Styling exactly as specified: `.tick_chars(".sSs.")` and
//!   `.progress_chars("█▉▊▋▌▍▎▏ ")`.
//!
//! Bars for files that are skipped (up to date, checksum identical, hard
//! linked) need no live bar; their bytes are accounted in the total bar
//! instantly. A destination with nothing to copy therefore never shows a
//! stray, never-updating bar.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::fsutil::{format_bytes, format_speed, Units};

/// Steady redraw / spinner animation cadence: fluent and cheap.
const TICK: Duration = Duration::from_millis(250);

/// Terminal width in columns (fallback: 80).
fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
        .max(40)
}

/// Human-readable ETA (indicatif-style "9s", "2m05s", "1h03m").
fn fmt_eta(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Bar style for a bar of the given width.
fn bar_style(width: usize) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{{spinner}} {{prefix}} [{{bar:{width}}}] {{msg}}"
    ))
    .expect("valid template")
    .progress_chars("█▉▊▋▌▍▎▏ ")
    .tick_chars(".sSs.")
}

/// Spinner style for the scanning/deletion phases.
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {prefix} {msg}")
        .expect("valid template")
        .tick_chars(".sSs.")
}

/// Front-ellipsize `name` to at most `max` chars, keeping the tail
/// (the file name).
fn ellipsize(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let tail: String = chars[chars.len() - max..].iter().collect();
    format!("…{tail}")
}

/// Shared UI state (behind one `Arc`).
struct Inner {
    multi: MultiProgress,
    units: Units,
    /// True on narrow terminals: the message drops the speed so the
    /// whole line always fits the width.
    compact: bool,
    total_bar: Mutex<Option<ProgressBar>>,
    total_done: AtomicU64,
}

impl Inner {
    /// Refresh the total bar's value and message.
    fn refresh_total(&self) {
        let Ok(guard) = self.total_bar.lock() else { return };
        let Some(tb) = guard.as_ref() else { return };
        let done = self.total_done.load(Ordering::Relaxed);
        let total = tb.length().unwrap_or(0);
        let units = self.units;
        let compact = self.compact;
        tb.set_position(done);
        tb.set_message(format_total(done, total, units, tb, compact));
    }
}

/// `1.2GB/4.5GB (27%) 412MB/s ETA 9s` (without the speed on narrow
/// terminals — the line must always fit the width).
fn stats_str(done: u64, total: u64, units: Units, bar: &ProgressBar, compact: bool) -> String {
    let pct = if total == 0 {
        0.0
    } else {
        done as f64 * 100.0 / total as f64
    };
    if compact {
        format!(
            "{}/{} ({:.0}%) ETA {}",
            format_bytes(done, units),
            format_bytes(total, units),
            pct,
            fmt_eta(bar.eta())
        )
    } else {
        format!(
            "{}/{} ({:.0}%) {} ETA {}",
            format_bytes(done, units),
            format_bytes(total, units),
            pct,
            format_speed(bar.per_sec(), units),
            fmt_eta(bar.eta())
        )
    }
}

fn format_total(done: u64, total: u64, units: Units, bar: &ProgressBar, compact: bool) -> String {
    stats_str(done, total, units, bar, compact)
}

/// The whole progress surface. Cheaply shared: workers hold `Arc` clones
/// (all indicatif bars are thread-safe clone handles).
pub struct ProgressUi {
    inner: Arc<Inner>,
    hidden: bool,
    width: usize,
}

impl ProgressUi {
    /// Create the UI (hidden automatically when stdout is not a TTY).
    pub fn new(units: Units) -> Arc<Self> {
        let hidden = !std::io::stdout().is_terminal();
        let multi = MultiProgress::new();
        if hidden {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        let width = term_width();
        let inner = Arc::new(Inner {
            multi,
            units,
            compact: width < 70,
            total_bar: Mutex::new(None),
            total_done: AtomicU64::new(0),
        });
        Arc::new(Self {
            inner,
            hidden,
            width,
        })
    }

    pub fn units(&self) -> Units {
        self.inner.units
    }

    /// The grand total bar (call once with the planned byte count; skip
    /// when nothing will be copied).
    pub fn begin_total(&self, label: &str, total_bytes: u64) {
        if self.hidden || total_bytes == 0 {
            return;
        }
        let (bar, msg) = self.make_bar(label, total_bytes);
        bar.set_message(msg);
        bar.enable_steady_tick(TICK);
        *self.inner.total_bar.lock().unwrap() = Some(bar);
    }

    /// Register a file that will be copied with a live bar.
    pub fn begin_file(&self, name: &str, size: u64) -> FileBar {
        let bar = if self.hidden {
            ProgressBar::hidden()
        } else {
            let (bar, msg) = self.make_bar(name, size);
            bar.set_message(msg);
            bar.enable_steady_tick(TICK);
            self.inner.multi.add(bar.clone());
            bar
        };
        FileBar {
            bar,
            prev: Arc::new(AtomicU64::new(0)),
            start: Arc::new(Instant::now()),
            last: Arc::new(AtomicU64::new(0)),
            inner: self.inner.clone(),
            hidden: self.hidden,
        }
    }

    /// Account for bytes that need no live bar (skips, links): they land
    /// in the total bar immediately.
    pub fn skip_bytes(&self, n: u64) {
        if self.hidden {
            return;
        }
        self.inner.total_done.fetch_add(n, Ordering::Relaxed);
        self.inner.refresh_total();
    }

    /// A spinner for single-threaded phases (scanning, deleting, …).
    pub fn spinner(&self, prefix: &str, msg: &str) -> ProgressBar {
        let bar = ProgressBar::new(u64::MAX);
        bar.set_style(spinner_style());
        bar.set_prefix(prefix.to_string());
        bar.set_message(msg.to_string());
        bar.enable_steady_tick(TICK);
        if !self.hidden {
            self.inner.multi.add(bar.clone());
        }
        bar
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Finish: keep the final total line. File bars remove themselves as
    /// they complete and the phase spinners are cleared at their sites, so
    /// nothing else should still be on screen here.
    pub fn finish_all(&self) {
        if self.hidden {
            return;
        }
        let Ok(guard) = self.inner.total_bar.lock() else {
            let _ = self.inner.multi.clear();
            return;
        };
        if let Some(tb) = guard.as_ref() {
            let done = self.inner.total_done.load(Ordering::Relaxed);
            let total_len = tb.length().unwrap_or(0);
            let units = self.inner.units;
            let compact = self.inner.compact;
            tb.set_position(done);
            tb.set_message(stats_str(done, total_len, units, tb, compact));
            let _ = tb.finish();
        }
    }

    /// Create a width-fitted bar: the name is ellipsized against the
    /// *worst-case* message length (speed/ETA grow over time), and the bar
    /// shrinks on narrow terminals so the line always fits.
    fn make_bar(&self, name: &str, size: u64) -> (ProgressBar, String) {
        let width = self.width;
        let units = self.inner.units;
        let compact = self.inner.compact;

        // Worst-case message length (speed up to 999.9 TB/s, ETA up to 9h99m).
        let total_str = format_bytes(size, units);
        let msg_len = if compact {
            format!("{total_str}/{total_str} (100%) ETA 9h99m").chars().count()
        } else {
            format!("{total_str}/{total_str} (100%) 999.9 TB/s ETA 9h99m").chars().count()
        };

        // Fixed parts of the line: spinner(1) + " " + " [" + "] " = 6.
        let mut bar_w = 40usize.min(width / 3).max(10);
        let mut name_max = width.saturating_sub(6 + bar_w + msg_len);
        if name_max < 8 {
            // Very narrow terminal: shrink the bar, keep a readable name.
            bar_w = width.saturating_sub(6 + msg_len + 8).max(6);
            name_max = 8;
        }
        let name = ellipsize(name, name_max.max(1));

        let bar = ProgressBar::new(size);
        bar.set_style(bar_style(bar_w));
        bar.set_prefix(name);
        let msg = stats_str(0, size, units, &bar, compact);
        (bar, msg)
    }
}

/// Handle for one active file's bar. Cloneable and `Send + Sync` (all
/// fields are shared handles), so the copy engine's chunk closure can
/// move it freely.
#[derive(Clone)]
pub struct FileBar {
    bar: ProgressBar,
    /// Cumulative bytes last reported (for the total-bar delta).
    prev: Arc<AtomicU64>,
    /// Creation time of the bar (relative monotonic clock for the
    /// report throttle).
    start: Arc<Instant>,
    /// Elapsed nanoseconds at the last UI update (throttle).
    last: Arc<AtomicU64>,
    inner: Arc<Inner>,
    hidden: bool,
}

impl FileBar {
    /// Report cumulative copied bytes. Throttled to the 250 ms cadence;
    /// the final value is always reported on [`FileBar::finish`].
    pub fn report(&self, done: u64) {
        if self.hidden {
            return;
        }
        let now = self.start.elapsed().as_nanos() as u64;
        if now < self.last.load(Ordering::Relaxed) + TICK.as_nanos() as u64 {
            return;
        }
        self.last.store(now, Ordering::Relaxed);

        let prev_done = self.prev.swap(done, Ordering::Relaxed);
        let delta = done.saturating_sub(prev_done);
        let units = self.inner.units;
        let compact = self.inner.compact;
        self.bar.set_position(done);
        self.bar
            .set_message(stats_str(done, self.bar.length().unwrap_or(done), units, &self.bar, compact));
        if delta > 0 {
            self.inner.total_done.fetch_add(delta, Ordering::Relaxed);
            self.inner.refresh_total();
        }
    }

    /// Finish the file: report the exact final value (when known — always
    /// passed on a successful copy, so the total bar lands at 100%), then
    /// clear the bar's line.
    pub fn finish(&self, final_bytes: Option<u64>) {
        if self.hidden {
            return;
        }
        let done = final_bytes.unwrap_or_else(|| self.prev.load(Ordering::Relaxed));
        let prev_done = self.prev.swap(done, Ordering::Relaxed);
        let delta = done.saturating_sub(prev_done);
        let units = self.inner.units;
        let compact = self.inner.compact;
        self.bar.set_position(done);
        self.bar
            .set_message(stats_str(done, self.bar.length().unwrap_or(done), units, &self.bar, compact));
        if delta > 0 {
            self.inner.total_done.fetch_add(delta, Ordering::Relaxed);
            self.inner.refresh_total();
        }
        let _ = self.inner.multi.remove(&self.bar);
        // Marks the bar finished: its steady-ticker thread exits.
        let _ = self.bar.finish();
    }
}
