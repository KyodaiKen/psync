//! Live progress UI with a custom single-threaded renderer.
//!
//! Why not `indicatif::MultiProgress`? Three classes of artifacts were
//! observed on real terminals (verified with PTY byte captures):
//! * `finish_and_clear` leaves the finished bar's final line "zombied" on
//!   screen (indicatif 0.17.11 treats removed leading bars as log lines),
//! * the first burst ticks draw the bar on its own default target before
//!   `multi.add()` rebinds it,
//! * lines padded to exactly the terminal width leave the cursor in a
//!   pending-wrap state, so on terminals that resolve the wrap before the
//!   next `\r`, every subsequent frame lands one line lower and the old
//!   frame is left as a stale ghost.
//!
//! So psync renders its own small UI with a strict protocol that is
//! deterministic on any terminal:
//!
//! 1. No line is ever written exactly to the terminal's right edge, so the
//!    cursor can never enter a pending-wrap state.
//! 2. All terminal writes happen on one 250 ms ticker thread (plus the
//!    final frame), so frames can never interleave.
//! 3. Each frame: move up `prev_lines` lines, clear them top-to-bottom,
//!    then rewrite the frame's lines. Unambiguous when every line is
//!    shorter than the terminal width.
//! 4. The first frame enters the terminal's *alternate screen buffer*
//!    (the vim/less-style "next page"); `finish_all` — or the death of the
//!    process itself (SIGKILL included: modern terminals restore the
//!    primary screen when the foreground program exits, and a panic hook
//!    covers in-process panics) — returns to the primary screen, so the
//!    shell prompt, scrollback, and the summary lines always land on the
//!    main page.
//!
//! Look: modern 256-color styling — the bar fill is a smooth blue →
//! cyan → green sweep (each neighbor one cube step apart, so the
//! gradient can't patch or jump on any palette), pinned to bar position
//! so it reveals stably as the bar fills; the grand total bar uses the
//! brighter half of the same path. Muted gray text; disabled
//! automatically by `NO_COLOR` or `TERM=dumb`.
//!
//! Layout: one live line per active file plus one grand-total line. The
//! name (prefix) column and the message column have fixed widths computed
//! once from the whole planned file set, so every job's line lines up
//! perfectly; long names are center-ellipsized to exactly that width.
//! The grand-total bar sits at the *bottom* of the frame.
//!
//! ```text
//! . long-…n.bin [██████▊     ] 512.0 MB/1.1 GB (46%) 142.0 MB/s ETA 4s
//! . long-…ar.gz [███████     ] 543.0 MB/1.1 GB (49%) 149.0 MB/s ETA 3s
//! . total       [██████      ] 1.1 GB/3.4 GB (32%) 291.0 MB/s ETA 8s
//! ```

use std::io::IsTerminal;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::fsutil::{format_bytes, format_speed, Units};

/// Steady redraw / spinner animation cadence: fluent and cheap.
const TICK: Duration = Duration::from_millis(250);

/// Spinner animation characters (as specified).
const TICK_CHARS: [char; 4] = ['.', 's', 'S', 's'];

/// Bar fill characters, 8 sub-cells to blank (as specified).
const BAR_CHARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Blue → cyan → green ramp for the bar fill (256-color cube indices):
/// each step moves exactly one axis by one level, so the gradient is
/// smooth in every palette — and R stays 0, so nothing can render red.
const FILL_RAMP: [u8; 11] = [21, 27, 33, 39, 45, 51, 50, 49, 48, 47, 46];

/// The brighter half of the same path, for the grand-total bar.
const TOTAL_RAMP: [u8; 8] = [33, 39, 45, 51, 50, 49, 48, 47];

/// Muted colors (256-color indices).
const COL_NAME: u8 = 252;
const COL_NAME_GRAND: u8 = 255;
const COL_MSG: u8 = 244;
const COL_BAR_EMPTY: u8 = 236;
const COL_BRACKET: u8 = 238;
const COL_SPINNER: u8 = 87;
const COL_WARNING: u8 = 214;

/// Whether ANSI 256-color output is enabled for this process (a TTY,
/// no `NO_COLOR`, and `TERM` other than `dumb`). Shared by the live UI
/// and the final report so both always agree.
pub fn color_enabled() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").map(|v| v.as_os_str().is_empty()).unwrap_or(true)
        && std::env::var_os("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// ETAs longer than this are capped (keeps the message width bounded).
const MAX_ETA_SECS: u64 = 9 * 3600 + 59 * 60;

/// Tracked process-wide: the panic hook and any death path must be able to
/// return to the primary screen without locking state.
static ALT_ENTERED: AtomicBool = AtomicBool::new(false);

/// Enter the terminal's alternate screen buffer (the vim/less-style
/// "next page"): the whole UI lives there, the primary screen and its
/// scrollback are untouched, and we return to it via
/// `restore_alt_screen` (finish, error, cancel) or automatically when the
/// process dies (SIGKILL included — modern terminals restore the primary
/// screen when the foreground program exits).
fn enter_alt_screen() {
    ALT_ENTERED.store(true, Ordering::SeqCst);
}

/// Return to the primary screen (idempotent, thread-safe).
fn restore_alt_screen() {
    if !ALT_ENTERED.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut err = std::io::stderr();
    let _ = err.write_all(b"\x1b[0m\x1b[?1049l");
    let _ = err.flush();
}

/// Install the panic hook once: restore the primary screen before the
/// panic report prints, so the report lands on the main page.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_alt_screen();
            default(info);
        }));
    });
}

/// Terminal width in columns (fallback: 80).
fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
        .max(40)
}

/// Human-readable ETA like `9s`, `2m05s`, `1h03m` (capped at 9h59m).
fn fmt_eta(d: Duration) -> String {
    let s = d.as_secs().min(MAX_ETA_SECS);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// The longest `format_bytes` string that any value `<= size` can produce
/// (the maximum is either at `size` itself or just below a unit boundary,
/// e.g. "999.9 MB").
fn worst_size_str(size: u64, units: Units) -> String {
    let base = units.base();
    let mut best = (0usize, format_bytes(size, units));
    let mut p = base;
    for _ in 0..5 {
        if p > size as f64 {
            break;
        }
        let cand = format_bytes((p - 1.0) as u64, units);
        if cand.chars().count() > best.0 {
            best = (cand.chars().count(), cand);
        }
        p *= base;
    }
    if format_bytes(size, units).chars().count() > best.0 {
        best = (
            format_bytes(size, units).chars().count(),
            format_bytes(size, units),
        );
    }
    best.1
}

/// The longest message a bar of `size` bytes can show (worst-case done
/// string on both sides, 100%, max speed, max ETA).
fn worst_msg(size: u64, units: Units, compact: bool) -> String {
    let b = worst_size_str(size, units);
    if compact {
        format!("{b}/{b} (100%) ETA 9h59m")
    } else {
        format!("{b}/{b} (100%) 999.9 TB/s ETA 9h59m")
    }
}

/// Fixed per-run layout: every bar line is
/// `tick ' ' name ' ' '[' bar ']' ' ' msg` with `name` exactly `name_w`
/// chars and `msg` exactly `msg_w` chars, so all jobs align perfectly and
/// each line is exactly `name_w + bar_w + msg_w + 7` columns — always
/// strictly inside the terminal width.
#[derive(Clone, Copy, Debug)]
struct Layout {
    width: usize,
    bar_w: usize,
    name_w: usize,
    msg_w: usize,
    compact: bool,
}

fn compute_layout(width: usize, units: Units, hints: &[(String, u64)], total: Option<u64>) -> Layout {
    let longest_name = hints
        .iter()
        .map(|(n, _)| n.chars().count())
        .chain(std::iter::once(5usize)) // "total", "scan", "del"
        .max()
        .unwrap_or(5);
    let sizes: Vec<u64> = hints.iter().map(|(_, s)| *s).chain(total).collect();

    for compact in [width < 70, true] {
        let msg_w = sizes
            .iter()
            .map(|s| worst_msg(*s, units, compact).chars().count())
            .max()
            .unwrap_or_else(|| worst_msg(0, units, compact).chars().count());
        let desired_bar = 40usize.min(width / 3).max(10);
        // 1) The classic split, when it fits.
        if longest_name + desired_bar + 7 + msg_w <= width {
            return Layout {
                width,
                bar_w: desired_bar,
                name_w: longest_name,
                msg_w,
                compact,
            };
        }
        // 2) Long names (e.g. destination paths in config mode) get the
        //    full width; the bar shrinks to a 10-cell floor instead.
        let bar_min = 10usize;
        if longest_name + bar_min + 7 + msg_w <= width {
            let bar_w = (width - 7 - msg_w - longest_name)
                .max(bar_min)
                .min(desired_bar);
            return Layout {
                width,
                bar_w,
                name_w: longest_name,
                msg_w,
                compact,
            };
        }
    }

    // Extremely narrow terminal: shrink the bar to make room.
    let compact = true;
    let msg_w = sizes
        .iter()
        .map(|s| worst_msg(*s, units, compact).chars().count())
        .max()
        .unwrap_or_else(|| worst_msg(0, units, compact).chars().count());
    let bar_w = 6usize;
    let name_w = longest_name
        .min(width.saturating_sub(7 + bar_w + msg_w))
        .max(4);
    Layout {
        width,
        bar_w,
        name_w,
        msg_w,
        compact,
    }
}

/// Display width of one character: wide (CJK, fullwidth, emoji) characters
/// occupy two terminal cells, everything else one. (`char::width` is not
/// yet stabilized on this toolchain.)
fn char_width(c: char) -> usize {
    if ('\u{1100}'..='\u{115F}').contains(&c)
        || ('\u{2E80}'..='\u{33FF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{A000}'..='\u{A4CF}').contains(&c)
        || ('\u{AC00}'..='\u{D7A3}').contains(&c)
        || ('\u{F900}'..='\u{FAFF}').contains(&c)
        || ('\u{FE30}'..='\u{FE4F}').contains(&c)
        || ('\u{FF00}'..='\u{FF60}').contains(&c)
        || ('\u{FFE0}'..='\u{FFDF}').contains(&c)
        || ('\u{1F300}'..='\u{1FAFF}').contains(&c)
    {
        2
    } else {
        1
    }
}

/// Fit `name` into exactly `w` columns: center-ellipsize long names
/// (`mid…dle`), or right-pad shorter ones with spaces — the column always
/// ends at the same position.
fn fit_name(name: &str, w: usize) -> String {
    let chs: Vec<char> = name.chars().collect();
    let total_w: usize = chs.iter().map(|c| char_width(*c)).sum();
    if total_w <= w {
        let mut s = name.to_string();
        s.push_str(&" ".repeat(w - total_w));
        s
    } else if w == 0 {
        String::new()
    } else if w == 1 {
        "…".to_string()
    } else {
        // Reserve one column for the ellipsis; split the rest head/tail.
        let avail = w - 1;
        let half = avail / 2;
        let mut head = String::new();
        let mut hw = 0;
        for &c in &chs {
            let cw = char_width(c);
            if hw + cw > half {
                break;
            }
            head.push(c);
            hw += cw;
        }
        let mut tail = String::new();
        let mut tw = 0;
        for &c in chs.iter().rev() {
            let cw = char_width(c);
            if tw + cw > avail - hw {
                break;
            }
            tail.insert(0, c);
            tw += cw;
        }
        format!("{head}…{tail}")
    }
}

/// One bar's state (total or file).
struct BarState {
    name: String,
    size: u64,
    done: u64,
    start: Instant,
    /// Last value seen by the ticker (for per-tick rate estimation).
    last_done: u64,
    last_tick: Instant,
    /// Smoothed rate in bytes/s.
    rate: f64,
}

impl BarState {
    fn new(name: String, size: u64) -> Self {
        let now = Instant::now();
        Self {
            name,
            size,
            done: 0,
            start: now,
            last_done: 0,
            last_tick: now,
            rate: 0.0,
        }
    }

    /// Update the smoothed rate from the value change since the last tick.
    fn update_rate(&mut self, now: Instant) {
        let dt = now.duration_since(self.last_tick).as_secs_f64().clamp(0.0, 2.0);
        let delta = self.done.saturating_sub(self.last_done) as f64;
        self.last_done = self.done;
        self.last_tick = now;
        if dt <= 0.0 || delta == 0.0 {
            if delta == 0.0 {
                self.rate = 0.0;
            }
            return;
        }
        let inst = delta / dt;
        self.rate = if self.rate <= 0.0 {
            inst
        } else {
            0.3 * inst + 0.7 * self.rate
        };
    }
}

struct State {
    /// Persistent text lines (warnings, notes) shown above the live frame.
    orphans: Vec<(String, bool)>,
    /// Phase spinner (scan/delete), shown alone while it runs.
    spinner: Option<(String, String)>,
    total: Option<BarState>,
    files: Vec<BarState>,
    layout: Layout,
    hints: Vec<(String, u64)>,
    /// Global tick counter (drives the spinner characters).
    tick: usize,
    /// Lines of the previously rendered frame (incl. orphans).
    prev_lines: usize,
    /// True once any spinner/bar has been started (for `println` routing).
    ui_started: bool,
    /// True after the final frame has been rendered.
    finished: bool,
    /// True once the frame has been anchored at the top of the screen
    /// (first non-empty frame clears the screen and draws at line 0, so
    /// frame growth can never scroll the screen and strand top lines).
    anchored: bool,
}

struct Inner {
    state: Mutex<State>,
    wait: Condvar,
    units: Units,
    hidden: bool,
    /// Color output (ANSI 256-color), off for NO_COLOR / TERM=dumb / pipes.
    color: bool,
    /// Set when the UI must stop ticking.
    stop: AtomicBool,
    /// Serializes the actual terminal writes (ticker vs final frame).
    write_lock: Mutex<()>,
    ticker: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Compute the frame lines from the current state.
    fn frame_lines(s: &State, units: Units, color: bool) -> Vec<String> {
        let layout = s.layout;
        let tick = TICK_CHARS[s.tick % TICK_CHARS.len()];
        let mut lines: Vec<String> = Vec::new();
        for (o, warn) in &s.orphans {
            let l: String = o.chars().take(layout.width.saturating_sub(1)).collect();
            lines.push(if color {
                if *warn {
                    format!("\x1b[1;38;5;{COL_WARNING}m{l}\x1b[0m")
                } else {
                    format!("\x1b[38;5;{COL_MSG}m{l}\x1b[0m")
                }
            } else {
                l
            });
        }
        if let Some((prefix, msg)) = &s.spinner {
            lines.push(spinner_line(layout, tick, prefix, msg, color));
        }
        for f in &s.files {
            lines.push(bar_line(layout, tick, f, units, color, false));
        }
        if let Some(t) = &s.total {
            // Grand total bar: last, at the bottom of the frame.
            lines.push(bar_line(layout, tick, t, units, color, true));
        }
        lines
    }
}

/// `.` + name column + message column (phase spinner).
fn spinner_line(layout: Layout, tick: char, name: &str, msg: &str, color: bool) -> String {
    let name = fit_name(name, layout.name_w);
    let msg = pad_to(msg, layout.msg_w);
    if color {
        format!(
            "\x1b[1;38;5;{COL_SPINNER}m{tick}\x1b[0m \x1b[38;5;{COL_NAME}m{name}\x1b[0m \x1b[38;5;{COL_MSG}m{msg}\x1b[0m"
        )
    } else {
        format!("{tick} {name} {msg}")
    }
}

/// `{tick} {name} [{bar}] {msg}` — the fixed-width bar line.
fn bar_line(layout: Layout, tick: char, b: &BarState, units: Units, color: bool, grand: bool) -> String {
    let done = b.done.min(b.size);
    let frac = if b.size == 0 {
        1.0
    } else {
        done as f64 / b.size as f64
    };
    let fill = frac * layout.bar_w as f64;
    let full = fill as usize;
    let rem = ((fill - full as f64) * 8.0) as usize;
    let ramp: &[u8] = if grand { &TOTAL_RAMP[..] } else { &FILL_RAMP[..] };
    let mut bar = String::with_capacity(layout.bar_w * (if color { 12 } else { 1 }));
    for i in 0..layout.bar_w {
        let ch = if i < full {
            '█'
        } else if i == full {
            BAR_CHARS[rem.min(8)]
        } else {
            ' '
        };
        if color {
            // Filled cells ride the blue→green ramp (position-scaled across
            // the filled span); empty cells stay dark.
            let col = if i <= full {
                // A fully filled bar ends exactly on the ramp's final
                // color (pure green) instead of one step short.
                let idx = i * (ramp.len() - 1) / layout.bar_w.max(1);
                let idx = if i == layout.bar_w - 1 { ramp.len() - 1 } else { idx };
                ramp[idx]
            } else {
                COL_BAR_EMPTY
            };
            bar.push_str(&format!("\x1b[38;5;{col}m{ch}"));
        } else {
            bar.push(ch);
        }
    }
    if color {
        bar.push_str("\x1b[0m");
    }
    let pct = if b.size == 0 { 0.0 } else { done as f64 * 100.0 / b.size as f64 };
    let eta = if done >= b.size {
        "0s".to_string()
    } else if b.rate > 0.0 {
        let remaining = b.size.saturating_sub(done) as f64 / b.rate;
        fmt_eta(Duration::from_secs(remaining.min(MAX_ETA_SECS as f64) as u64))
    } else {
        "--".to_string()
    };
    let v = format_speed(b.rate, units);
    let d = format_bytes(done, units);
    let s = format_bytes(b.size, units);
    let msg = if layout.compact {
        format!("{d}/{s} ({pct:.0}%) ETA {eta}")
    } else {
        format!("{d}/{s} ({pct:.0}%) {v} ETA {eta}")
    };
    let name = fit_name(&b.name, layout.name_w);
    let msg = pad_to(&msg, layout.msg_w);
    if color {
        let name_csi = if grand { COL_NAME_GRAND } else { COL_NAME };
        format!(
            "\x1b[1;38;5;{COL_SPINNER}m{tick}\x1b[0m \x1b[{};38;5;{name_csi}m{name}\x1b[0m \x1b[38;5;{COL_BRACKET}m[\x1b[0m{bar}\x1b[38;5;{COL_BRACKET}m]\x1b[0m \x1b[38;5;{COL_MSG}m{msg}\x1b[0m",
            if grand { "1" } else { "" }
        )
    } else {
        format!("{tick} {name} [{bar}] {msg}")
    }
}

fn pad_to(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - chars.len()))
    }
}

/// Render one frame: clear the previous frame's lines (top-to-bottom), then
/// write the new lines. Every line is shorter than the terminal width, so
/// the cursor can never be in a pending-wrap state and the up/clear/down
/// dance is unambiguous on any terminal.
fn render_frame(inner: &Inner, final_frame: bool) {
    if inner.hidden {
        return;
    }
    let lines = {
        let mut s = inner.state();
        if !final_frame {
            let now = Instant::now();
            s.tick += 1;
            if let Some(t) = &mut s.total {
                t.update_rate(now);
            }
            for f in &mut s.files {
                f.update_rate(now);
            }
        }
        Inner::frame_lines(&s, inner.units, inner.color)
    };

    let prev = inner.state().prev_lines;
    let anchored = inner.state().anchored;
    let mut out = String::new();
    if !anchored && !lines.is_empty() {
        // First frame: take over the terminal's *alternate* screen
        // (vim/less-style). The primary screen and its scrollback are left
        // completely untouched; on finish — or when the process dies,
        // however it dies — we (or the terminal) return to it. Clearing +
        // homing the fresh screen also keeps the invariant "frame top at
        // line 0", so growth never scrolls and no line can be stranded.
        out.push_str("\x1b[?1049h");
        enter_alt_screen();
    } else if prev > 0 {
        out.push_str(&format!("\x1b[{prev}A"));
        for i in 0..prev {
            out.push_str("\r\x1b[2K");
            if i + 1 < prev {
                out.push_str("\x1b[1B");
            }
        }
        if prev > 1 {
            out.push_str(&format!("\x1b[{}A", prev - 1));
        }
    }
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    let n = lines.len();

    let _wl = inner.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut err = std::io::stderr();
    if let Err(e) = err.write_all(out.as_bytes()) {
        eprintln!("psync: error writing progress UI: {e}");
    }
    if final_frame {
        let _ = err.write_all(b"\n");
    }
    let _ = err.flush();
    drop(_wl);

    let mut s = inner.state();
    s.prev_lines = n;
    s.anchored = s.anchored || !lines.is_empty();
    if final_frame {
        s.finished = true;
    }
}

/// The shared progress surface. Cheaply shared: workers hold `Arc` clones.
pub struct ProgressUi {
    inner: Arc<Inner>,
}

/// Handle for a phase spinner (scan/delete).
pub struct Spinner {
    inner: Arc<Inner>,
}

impl Spinner {
    /// End the spinner: its line is cleared on the next render (immediate).
    pub fn finish(self) {
        if self.inner.hidden {
            return;
        }
        {
            let mut s = self.inner.state();
            s.spinner = None;
        }
        self.inner.wait.notify_all();
    }
}

/// Handle for one live file bar (cloneable; the bar is shared state).
#[derive(Clone)]
pub struct FileBar {
    inner: Arc<Inner>,
    key: String,
}

impl FileBar {
    /// Report cumulative copied bytes. The value is stored (and the total
    /// bar advances by the delta); the UI renders on its own 250 ms
    /// cadence.
    pub fn report(&self, done: u64) {
        if self.inner.hidden {
            return;
        }
        let mut s = self.inner.state();
        if let Some(f) = s.files.iter_mut().find(|f| f.name == self.key) {
            let old = f.done;
            let new_done = done.max(old);
            if new_done > old {
                f.done = new_done;
                if let Some(t) = &mut s.total {
                    t.done += new_done - old;
                }
            }
        }
    }

    /// Finish the file: store the exact final value (when known — always on
    /// a successful copy, so the total bar lands at 100%), advance the
    /// total by the remaining delta, then drop the bar's line from the
    /// frame.
    pub fn finish(&self, final_bytes: Option<u64>) {
        if self.inner.hidden {
            return;
        }
        {
            let mut s = self.inner.state();
            if let Some(pos) = s.files.iter().position(|f| f.name == self.key) {
                let old = s.files[pos].done;
                if let Some(d) = final_bytes {
                    let new_done = d.max(old);
                    s.files[pos].done = new_done;
                    if new_done > old {
                        if let Some(t) = &mut s.total {
                            t.done += new_done - old;
                        }
                    }
                }
                s.files.remove(pos);
            }
        }
        self.inner.wait.notify_all();
    }
}

impl ProgressUi {
    /// Create the UI (hidden automatically when stdout is not a TTY).
    /// `width_override` forces the terminal width (for testing).
    pub fn new(units: Units, width_override: Option<usize>) -> Arc<Self> {
        let hidden = !std::io::stdout().is_terminal();
        let color = color_enabled();
        let width = width_override.unwrap_or_else(term_width).max(20);
        let state = State {
            orphans: Vec::new(),
            spinner: None,
            total: None,
            files: Vec::new(),
            layout: compute_layout(width, units, &[], None),
            hints: Vec::new(),
            tick: 0,
            prev_lines: 0,
            ui_started: false,
            finished: false,
            anchored: false,
        };
        let inner = Arc::new(Inner {
            state: Mutex::new(state),
            wait: Condvar::new(),
            units,
            hidden,
            color,
            stop: AtomicBool::new(false),
            write_lock: Mutex::new(()),
            ticker: Mutex::new(None),
        });
        let ui = Arc::new(ProgressUi {
            inner: Arc::clone(&inner),
        });
        if !hidden {
            install_panic_hook();
            let inner_clone = inner.clone();
            let handle = thread::spawn(move || {
                loop {
                    if inner_clone.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    render_frame(&inner_clone, false);
                    let _guard = match inner_clone
                        .wait
                        .wait_timeout(inner_clone.state(), TICK)
                    {
                        Ok((guard, _result)) => guard, // early wake or timeout: render again
                        Err(p) => p.into_inner().0,
                    };
                }
            });
            *inner.ticker.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        }
        ui
    }

    pub fn units(&self) -> Units {
        self.inner.units
    }

    pub fn is_hidden(&self) -> bool {
        self.inner.hidden
    }

    /// The grand total bar (call once with the planned byte count; skip
    /// when nothing will be copied — no stray bar).
    pub fn begin_total(&self, label: &str, total_bytes: u64) {
        if self.inner.hidden || total_bytes == 0 {
            return;
        }
        {
            let mut s = self.inner.state();
            s.total = Some(BarState::new(label.to_string(), total_bytes));
            s.layout = compute_layout(s.layout.width, self.inner.units, &s.hints, Some(total_bytes));
            s.ui_started = true;
        }
        self.inner.wait.notify_all();
    }

    /// Tell the UI the full planned file set (name + size) so the name and
    /// message columns can be fixed for the whole run: every job's line
    /// then has identical column widths, with the ellipsis fitting exactly.
    pub fn set_file_hints(&self, hints: &[(String, u64)]) {
        if self.inner.hidden {
            return;
        }
        {
            let mut s = self.inner.state();
            s.hints = hints.to_vec();
            s.layout = compute_layout(
                s.layout.width,
                self.inner.units,
                &s.hints,
                s.total.as_ref().map(|t| t.size),
            );
        }
        self.inner.wait.notify_all();
    }

    /// Register a file that will be copied with a live bar.
    pub fn begin_file(&self, name: &str, size: u64) -> FileBar {
        if self.inner.hidden {
            return FileBar {
                inner: self.inner.clone(),
                key: name.to_string(),
            };
        }
        {
            let mut s = self.inner.state();
            if !s.files.iter().any(|f| f.name == name) {
                s.files.push(BarState::new(name.to_string(), size));
                s.ui_started = true;
            }
        }
        self.inner.wait.notify_all();
        FileBar {
            inner: self.inner.clone(),
            key: name.to_string(),
        }
    }

    /// Account for bytes that need no live bar (skips, links): they land in
    /// the total bar immediately.
    pub fn skip_bytes(&self, n: u64) {
        if self.inner.hidden {
            return;
        }
        let mut s = self.inner.state();
        if let Some(t) = &mut s.total {
            t.done += n;
            t.last_done = t.done;
        }
    }

    /// A phase spinner (scanning/deleting). Call `finish()` when the phase
    /// ends; the spinner's line is cleared, not left behind.
    pub fn spinner(&self, prefix: &str, msg: &str) -> Spinner {
        if !self.inner.hidden {
            let mut s = self.inner.state();
            s.spinner = Some((prefix.to_string(), msg.to_string()));
            s.ui_started = true;
        }
        self.inner.wait.notify_all();
        Spinner {
            inner: self.inner.clone(),
        }
    }

    /// Print a line that must coexist with the live UI: while the UI is
    /// active it becomes a persistent line above the frame; otherwise it is
    /// printed directly (dry runs, errors, pre-UI phases).
    pub fn println(&self, text: &str) {
        let active = {
            let s = self.inner.state();
            s.ui_started && !s.finished
        };
        if !self.inner.hidden && active {
            let mut s = self.inner.state();
            s.orphans.push((text.to_string(), false));
            self.inner.wait.notify_all();
        } else {
            eprintln!("{text}");
        }
    }

    /// Like `println`, but the line is a *warning* (bold amber in the
    /// live frame; identical plain text when the UI is inactive).
    pub fn warn(&self, text: &str) {
        let active = {
            let s = self.inner.state();
            s.ui_started && !s.finished
        };
        if !self.inner.hidden && active {
            let mut s = self.inner.state();
            s.orphans.push((text.to_string(), true));
            self.inner.wait.notify_all();
        } else {
            eprintln!("{text}");
        }
    }

    /// Finish: stop the ticker, render the final frame on the alternate
    /// screen (persistent lines + the total bar at its final value), then
    /// return to the primary screen so the summary printed afterwards lands
    /// on the main page. Idempotent.
    pub fn finish_all(&self) {
        if self.inner.hidden {
            return;
        }
        if self.inner.state().finished {
            return;
        }
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.wait.notify_all();
        // Reap the ticker *before* the final frame so it cannot race the
        // last write (at most one tick period of wait).
        if let Some(handle) = self.inner.ticker.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = handle.join();
        }
        render_frame(&self.inner, true);
        restore_alt_screen();
    }
}
