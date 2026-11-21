//! Filesystem primitives: safe path joining, size handling, token-bucket
//! bandwidth pacing, checksums and the OS-level file copy routine.
//!
//! The copy routine performs the exact same work as the operating system's
//! own copy path (`std::fs::copy` internally: open source, open destination,
//! chunked `read()` + `write()` syscalls), but in 1 MiB chunks so that
//! progress can be reported live, the transfer can be cancelled between
//! chunks, a bandwidth limit can be enforced, and permissions/timestamps can
//! be preserved afterwards.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

/// Copy chunk size (1 MiB): large enough to saturate NVMe/HDD bandwidth,
/// small enough for smooth progress indication.
pub const COPY_BUFFER: usize = 1024 * 1024;

/// Join a relative sub-path onto a base path, refusing to escape the base.
///
/// This is the defence-in-depth guard for every path that originates from
/// user data (config files, filter rules, `--link-dest`, ...): absolute
/// sub-paths and `..` components are rejected instead of silently escaping
/// the destination root.
pub fn safe_join(base: &Path, sub: &str) -> Result<PathBuf> {
    let sub_path = Path::new(sub);
    if sub_path.is_absolute() {
        return Err(AppError::Other(format!(
            "refusing to use absolute sub-path '{}' inside '{}'",
            sub_path.display(),
            base.display()
        )));
    }
    for comp in sub_path.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return Err(AppError::Other(format!(
                "refusing to use '..' in sub-path '{}' inside '{}'",
                sub_path.display(),
                base.display()
            )));
        }
    }
    Ok(base.join(sub_path))
}

/// Parse a human readable size: plain bytes (`123`), or a number with a
/// binary unit suffix (`512K`, `10M`, `1.5G`, `2T`). Also accepts the
/// `KiB`/`MiB`/... spellings.
pub fn parse_size(input: &str) -> Result<u64> {
    let s = input.trim();
    if s.is_empty() {
        return Err(AppError::Usage("empty size argument".into()));
    }
    let bytes_per_kib = 1024u64;
    let (num_part, suffix) = if let Some(pos) = s.find(|c: char| c.is_ascii_alphabetic()) {
        (&s[..pos], &s[pos..])
    } else {
        (s, "")
    };
    let multiplier: u64 = match suffix {
        "" | "B" | "b" => 1,
        "K" | "KB" | "Kib" | "KiB" | "k" | "kb" => bytes_per_kib,
        "M" | "MB" | "Mib" | "m" | "mb" => bytes_per_kib.pow(2),
        "G" | "GB" | "Gib" | "g" | "gb" => bytes_per_kib.pow(3),
        "T" | "TB" | "Tib" | "t" | "tb" => bytes_per_kib.pow(4),
        other => {
            return Err(AppError::Usage(format!(
                "cannot parse size '{input}' (unknown unit '{other}')"
            )))
        }
    };
    let num: f64 = num_part
        .parse()
        .map_err(|_| AppError::Usage(format!("cannot parse size '{input}'")))?;
    if !(0.0..=1e15).contains(&num) || !num.is_finite() {
        return Err(AppError::Usage(format!("cannot parse size '{input}'")));
    }
    Ok((num as u128 * multiplier as u128) as u64)
}

/// Format a byte count like `12.3 GB` (JEDEC units, like the Python original).
/// Byte-unit convention for human-readable output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Units {
    /// Default: the old "KB" = 1024 B (JEDEC-style labels).
    Jedec,
    /// Strict SI: 1 KB = 1000 B (the masochists' choice, `--si-units`).
    Si,
}

impl Units {
    pub fn base(self) -> f64 {
        match self {
            Units::Jedec => 1024.0,
            Units::Si => 1000.0,
        }
    }
}

/// Human-readable byte count with the chosen convention:
/// `B`, `KB`, `MB`, `GB`, `TB` over a 1024- (default) or 1000-base.
pub fn format_bytes(n: u64, units: Units) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let base = units.base();
    let mut v = n as f64;
    let mut i = 0;
    while v >= base && i < UNITS.len() - 1 {
        v /= base;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        // Three decimals for every unit from KB up (as requested): the
        // byte column stays whole ("5 B").
        format!("{v:.3} {}", UNITS[i])
    }
}

/// Human-readable transfer speed (bytes per second).
pub fn format_speed(bytes_per_sec: f64, units: Units) -> String {
    format!("{}/s", format_bytes(bytes_per_sec as u64, units))
}

/// Compact duration like `0d00h00m05s` (pybackupsync `format_time`).
pub fn format_eta(total_secs: u64) -> String {
    let d = total_secs / 86400;
    let h = (total_secs % 86400) / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if d > 0 {
        format!("{d}d{h:02}h{m:02}m")
    } else {
        format!("{h:02}h{m:02}m{s:02}s")
    }
}

/// Detailed duration like `00d 00h 00m 05s 123ms` (pybackupsync
/// `format_time_detailed`).
pub fn format_duration_detailed(total_secs: f64) -> String {
    let total_ms = (total_secs * 1000.0).round() as i64;
    if total_ms < 0 {
        return "N/A".into();
    }
    let ms = (total_ms % 1000) as u64;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let total_h = total_m / 60;
    let h = total_h % 24;
    let d = total_h / 24;
    format!("{d:02}d {h:02}h {m:02}m {s:02}s {ms:03}ms")
}

/// Token-bucket bandwidth pacer (bytes per second).
///
/// One pacer is shared by all workers, so `--bwlimit` limits the *total*
/// transfer rate just like rsync.
pub struct Pacer {
    rate: f64,
    tokens: f64,
    cap: f64,
    last: Instant,
}

impl Pacer {
    /// Create a pacer for `kib_per_sec` KiB/s.
    pub fn new(kib_per_sec: u64) -> Self {
        let rate = (kib_per_sec.max(1) as f64) * 1024.0;
        Self {
            rate,
            tokens: rate,
            cap: rate,
            last: Instant::now(),
        }
    }

    /// Block until `bytes` bytes are allowed through the bucket.
    pub fn wait_for(&mut self, bytes: u64) {
        loop {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last).as_secs_f64();
            self.last = now;
            self.tokens = (self.tokens + elapsed * self.rate).min(self.cap);
            if self.tokens >= bytes as f64 {
                self.tokens -= bytes as f64;
                return;
            }
            // Keep the credit we already have — only the missing part needs
            // to accrue while we sleep.
            let deficit = bytes as f64 - self.tokens;
            std::thread::sleep(Duration::from_secs_f64(deficit / self.rate));
        }
    }
}

/// Callback invoked by [`copy_file`] after each chunk is written.
/// Receives the cumulative honest progress for the file (see the progress
/// semantics on [`copy_file`]); return `false` to abort the transfer (the
/// partial destination file is removed unless [`CopyCtx::keep_partial`] is
/// set). `Arc` so the same callback can be shared with the flush-tick
/// thread.
pub type ChunkHook = Arc<dyn Fn(u64) -> bool + Send + Sync>;

/// Context for a single file copy.
pub struct CopyCtx {
    /// Preserve the source file's permission bits on the destination.
    pub preserve_perms: bool,
    /// Preserve the source file's modification time on the destination.
    pub preserve_time: bool,
    /// Preserve ownership (no-op when not running as root).
    pub preserve_owner: Option<(u32, u32)>,
    /// Source metadata (len, modified, uid/gid, mode).
    pub src_meta: fs::Metadata,
    /// Keep a partial destination file on failure/cancel (rsync `-P`).
    pub keep_partial: bool,
    /// Progress callback.
    pub on_chunk: ChunkHook,
    /// Optional callback ticked while the final `fsync` is in flight, with
    /// the device-side watermark (bytes actually on the device so far,
    /// capped at the file size): keeps the progress bar crawling during the
    /// flush instead of freezing. Return `false` to stop ticking.
    pub on_flush_tick: Option<ChunkHook>,
}

impl CopyCtx {
    /// Clean up the destination file if the transfer fails and `-P` is off.
    pub fn cleanup(&self, dst: &Path) {
        if !self.keep_partial {
            let _ = fs::remove_file(dst);
        }
    }
}

/// How far into the page cache a file may be before a frozen
/// `/proc/self/io` counter is declared broken and progress reporting falls
/// back to page-cache progress (256 MiB).
const DEVIO_FALLBACK_AFTER: u64 = 256 * 1024 * 1024;

/// Device-side progress tracking via the `write_bytes` counter of Linux's
/// `/proc/self/io`: the bytes the kernel has actually written to the
/// storage device on behalf of this process.
///
/// Progress is clamped to this watermark, so a bar reflects data that has
/// *landed* rather than data merely sitting in the page cache: during the
/// final `fsync` the bar keeps crawling at the real flush speed and only
/// reaches 100% once the device has really accepted the data.
///
/// The counter is process-wide, so parallel copies can push it ahead; the
/// clamp to a file's own written bytes keeps every file's bar honest (it
/// can never overtake its own progress).
///
/// If the counter never advances (kernel without io accounting, or a
/// device that never accepted a byte), reporting falls back to page-cache
/// progress after a grace period so a bar never freezes at 0%.
#[derive(Debug)]
struct DevioTracker {
    /// `write_bytes` when the copy started (the counter is process-wide, so
    /// only deltas make sense).
    base: u64,
    /// Highest delta observed so far (monotonic).
    watermark: u64,
    /// Set once the counter is declared broken (frozen at 0 past the grace
    /// period): report page-cache progress from then on.
    disabled: bool,
}

impl DevioTracker {
    fn new() -> Option<Self> {
        // Escape hatch / A/B switch for the old (page-cache) behavior.
        if std::env::var_os("PSYNC_PROGRESS_NO_DEVIO").map(|v| !v.is_empty()).unwrap_or(false) {
            return None;
        }
        Some(Self {
            base: device_write_bytes()?,
            watermark: 0,
            disabled: false,
        })
    }

    /// Fold in a fresh `write_bytes` reading (the watermark is monotonic).
    fn observe(&mut self, write_bytes: Option<u64>) {
        if let Some(cur) = write_bytes {
            let delta = cur.saturating_sub(self.base);
            if delta > self.watermark {
                self.watermark = delta;
            }
        }
    }

    /// Honest progress for `written` bytes accepted into the page cache.
    fn honest(&mut self, written: u64) -> u64 {
        if self.disabled {
            return written;
        }
        if self.watermark == 0 && written >= DEVIO_FALLBACK_AFTER {
            // Frozen at 0 despite a lot of writes: the counter is not
            // accounting (or the device never accepted a byte) — report
            // what we know, the page-cache side.
            self.disabled = true;
            return written;
        }
        written.min(self.watermark)
    }
}

/// Bytes the kernel has written to the device on behalf of this process
/// (Linux `/proc/self/io`), or `None` if the counter is unavailable.
fn device_write_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/io").ok()?;
    s.lines()
        .find_map(|line| line.strip_prefix("write_bytes:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Copy `src` to `dst` with progress, cancellation and bandwidth limiting.
///
/// Returns the number of bytes written.
///
/// Progress semantics: the per-chunk callback reports *honest* progress —
/// the page-cache writes clamped to how much the kernel has actually landed
/// on the device (`/proc/self/io`, see [`DevioTracker`]), and the flush-
/// tick callback keeps reporting that watermark while the final `fsync`
/// blocks. The caller's final report is what closes a bar at 100%, which
/// happens only after `fsync` succeeded. Without the counter (non-Linux,
/// or `PSYNC_PROGRESS_NO_DEVIO=1`) the callback sees the page-cache
/// progress, as before.
pub fn copy_file(src: &Path, dst: &Path, ctx: &CopyCtx, pacer: &Option<std::sync::Mutex<Pacer>>) -> Result<u64> {
    let mut in_file = File::open(src)
        .map_err(|e| AppError::io(format!("opening source '{src}'", src = src.display()), e))?;
    let mut out_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)
        .map_err(|e| AppError::io(format!("creating destination '{dst}'", dst = dst.display()), e))?;

    let total = ctx.src_meta.len();
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut done: u64 = 0;
    let mut devio = DevioTracker::new();

    loop {
        if crate::cancel::is_cancelled() {
            ctx.cleanup(dst);
            return Err(AppError::Cancelled);
        }
        let n = in_file
            .read(&mut buf)
            .map_err(|e| AppError::io(format!("reading '{src}'", src = src.display()), e))?;
        if n == 0 {
            break;
        }
        if let Some(mutex) = pacer {
            let mut guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
            guard.wait_for(n as u64);
        }
        let wr = out_file.write_all(&buf[..n]);
        if wr.is_err() {
            let e = wr.unwrap_err();
            ctx.cleanup(dst);
            return Err(AppError::io(format!("writing '{dst}'", dst = dst.display()), e));
        }
        done += n as u64;
        if done < total {
            let reported = if let Some(d) = devio.as_mut() {
                d.observe(device_write_bytes());
                d.honest(done)
            } else {
                done
            };
            if !(ctx.on_chunk)(reported) {
                ctx.cleanup(dst);
                return Err(AppError::Cancelled);
            }
        }
    }

    // --- preserve metadata + flush to the device ---------------------------
    finalize_copy(dst, &|| out_file.sync_all(), ctx, devio.as_ref(), total)?;
    Ok(done)
}

/// How often the flush-tick thread polls the device-side watermark.
const FLUSH_TICK_INTERVAL: Duration = Duration::from_millis(150);

/// Finalize a finished copy: preserve metadata, then flush the data to
/// stable storage.
///
/// A failed flush is a **hard failure**: until `fsync` succeeds the data
/// is only as durable as the page cache, so reporting success would be a
/// lie (a failed backup is worse than a known-bad one). The partial
/// destination is cleaned up exactly like any other mid-copy failure —
/// kept only under `keep_partial` (`-P`), same as a failed write.
///
/// While the (blocking) flush runs, a helper thread polls the device-side
/// watermark (`devio`) and feeds it through `ctx.on_flush_tick`, so the
/// progress bar crawls to 100% at the real flush speed instead of freezing
/// — and the bar only lands at 100% once `fsync` itself has succeeded
/// (the caller's final report).
fn finalize_copy(
    dst: &Path,
    sync: &dyn Fn() -> std::io::Result<()>,
    ctx: &CopyCtx,
    devio: Option<&DevioTracker>,
    size: u64,
) -> Result<()> {
    if ctx.preserve_perms {
        let _ = fs::set_permissions(dst, ctx.src_meta.permissions());
    }
    if let Some((uid, gid)) = ctx.preserve_owner {
        // Best effort: chown only works as root (or with CAP_CHOWN).
        let _ = unsafe_chown(dst, uid, gid);
    }
    if ctx.preserve_time {
        if let Ok(modified) = ctx.src_meta.modified() {
            // `std::fs::set_times` updates mtime (atime left unchanged).
            let t = std::fs::FileTimes::new().set_modified(modified);
            let _ = fs::set_times(dst, t);
        }
    }

    // Tick the bar while the flush blocks: the watermark only advances as
    // the kernel lands data on the device, so this is an honest crawl.
    let mut tick_stop: Option<Arc<AtomicBool>> = None;
    let mut tick_thread: Option<thread::JoinHandle<()>> = None;
    if let (Some(d), Some(hook)) = (devio, ctx.on_flush_tick.as_ref()) {
        let hook = Arc::clone(hook);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let base = d.base;
        let handle = thread::spawn(move || loop {
            if stop_flag.load(Ordering::Relaxed) || crate::cancel::is_cancelled() {
                break;
            }
            thread::sleep(FLUSH_TICK_INTERVAL);
            if stop_flag.load(Ordering::Relaxed) || crate::cancel::is_cancelled() {
                break;
            }
            if let Some(cur) = device_write_bytes() {
                if !hook(cur.saturating_sub(base).min(size)) {
                    break;
                }
            }
        });
        tick_stop = Some(stop);
        tick_thread = Some(handle);
    }

    let res = sync();

    if let Some(stop) = &tick_stop {
        stop.store(true, Ordering::Relaxed);
    }
    if let Some(handle) = tick_thread {
        let _ = handle.join();
    }

    res.map_err(|e| {
        ctx.cleanup(dst);
        AppError::io(
            format!("fsync of '{}' (flushing data to the device)", dst.display()),
            e,
        )
    })
}

/// Best-effort `chown` of a path (only takes effect as root / with
/// CAP_CHOWN, like rsync without `--numeric-ids` as a regular user).
/// Pure safe Rust — the syscall goes through `std::os::unix::fs::chown`.
fn unsafe_chown(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
}

/// Set the permission bits of a path (chmod via `std::fs::set_permissions`).
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Stream SHA-256 of a file (rsync `--checksum` comparison).
pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .map_err(|e| AppError::io(format!("opening for checksum '{path}'", path = path.display()), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUFFER];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| AppError::io(format!("reading for checksum '{path}'", path = path.display()), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Shorten a path for narrow progress lines: `a/very/long/path.jpg` becomes
/// `a/very/…h.jpg` (pybackupsync `shorten_path`).
pub fn shorten_path(path: &str, max_len: usize) -> String {
    if path.chars().count() <= max_len || max_len < 7 {
        return path.to_string();
    }
    let half = (max_len - 1) / 2;
    let chars: Vec<char> = path.chars().collect();
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "psync-fsutil-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Real src/dst pair plus a `CopyCtx` with the requested `keep_partial`
    /// and an in-flight (partial) destination file already present.
    fn setup(tag: &str, keep_partial: bool) -> (PathBuf, PathBuf, CopyCtx) {
        let dir = scratch(tag);
        let src = dir.join("in.bin");
        let dst = dir.join("out.bin");
        let payload: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &payload).unwrap();
        fs::write(&dst, b"partial-so-far").unwrap();
        let ctx = CopyCtx {
            preserve_perms: true,
            preserve_time: true,
            preserve_owner: None,
            src_meta: fs::metadata(&src).unwrap(),
            keep_partial,
            on_chunk: Arc::new(|_| true),
            on_flush_tick: None,
        };
        (src, dst, ctx)
    }

    fn simulated_disk_failure() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "simulated device failure",
        ))
    }

    /// A failed fsync must surface as a copy error — reporting success here
    /// would mean "data may not be on the device" — and the partial
    /// destination must be removed just like after a failed write.
    #[test]
    fn fsync_failure_is_reported_and_partial_removed() {
        let (_src, dst, ctx) = setup("fsync-fail", false);
        let err = finalize_copy(&dst, &simulated_disk_failure, &ctx, None, 0).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("fsync"), "error must name the failed flush: {msg}");
        assert!(!dst.exists(), "partial destination must be removed after failed fsync");
    }

    /// With `-P` the partial destination survives a failed flush, so the
    /// user can inspect it.
    #[test]
    fn fsync_failure_with_keep_partial_keeps_the_file() {
        let (_src, dst, ctx) = setup("fsync-fail-p", true);
        let err = finalize_copy(&dst, &simulated_disk_failure, &ctx, None, 0).unwrap_err();
        assert!(err.to_string().contains("fsync"));
        assert!(dst.exists(), "keep_partial must preserve the file");
    }

    /// A successful flush leaves the destination in place.
    #[test]
    fn fsync_success_finalizes_cleanly() {
        let (_src, dst, ctx) = setup("fsync-ok", false);
        finalize_copy(&dst, &|| Ok(()), &ctx, None, 0).unwrap();
        assert!(dst.exists());
    }

    /// The cleanup contract every failure path (write error, cancel, failed
    /// flush) relies on.
    #[test]
    fn cleanup_removes_partial_unless_keep_partial() {
        let (_src, dst_rm, ctx_rm) = setup("cleanup-rm", false);
        ctx_rm.cleanup(&dst_rm);
        assert!(!dst_rm.exists());

        let (_src, dst_keep, ctx_keep) = setup("cleanup-keep", true);
        ctx_keep.cleanup(&dst_keep);
        assert!(dst_keep.exists());
    }

    /// End to end: a successful copy stays byte-identical and keeps the
    /// source's mtime + permissions (pins the refactored finalize tail).
    #[test]
    fn copy_file_success_is_byte_identical_with_metadata() {
        let dir = scratch("e2e");
        let src = dir.join("in.bin");
        let dst = dir.join("out.bin");
        let payload: Vec<u8> = (0..1024u32).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(&src, &payload).unwrap();
        let t = std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_767_261_600),
        );
        fs::set_times(&src, t).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();

        let ctx = CopyCtx {
            preserve_perms: true,
            preserve_time: true,
            preserve_owner: None,
            src_meta: fs::metadata(&src).unwrap(),
            keep_partial: false,
            on_chunk: Arc::new(|_| true),
            on_flush_tick: None,
        };
        let pacer: Option<std::sync::Mutex<Pacer>> = None;
        let done = copy_file(&src, &dst, &ctx, &pacer).unwrap();
        assert_eq!(done, payload.len() as u64);
        assert_eq!(fs::read(&dst).unwrap(), payload);
        assert_eq!(
            fs::metadata(&dst).unwrap().modified().unwrap(),
            fs::metadata(&src).unwrap().modified().unwrap(),
            "mtime must be preserved"
        );
        assert_eq!(
            fs::metadata(&dst).unwrap().permissions(),
            fs::metadata(&src).unwrap().permissions(),
            "permissions must be preserved"
        );
    }

    /// The watermark clamps progress to what is actually on the device,
    /// never regresses, and passes through once caught up (or when the
    /// reported progress is below the watermark).
    #[test]
    fn devio_watermark_clamps_and_is_monotonic() {
        let mut t = DevioTracker { base: 1000, watermark: 0, disabled: false };
        t.observe(Some(1060)); // 60 bytes landed on the device
        assert_eq!(t.honest(100), 60);
        t.observe(Some(1040)); // counter regression: watermark keeps its max
        assert_eq!(t.honest(100), 60);
        t.observe(None); // unreadable counter: unchanged
        assert_eq!(t.honest(100), 60);
        t.observe(Some(1100)); // device caught up
        assert_eq!(t.honest(100), 100);
        assert_eq!(t.honest(80), 80); // below the watermark: raw value
    }

    /// A counter frozen at 0 (no io accounting / device never accepted a
    /// byte) must not freeze the bar at 0%: after the grace period, report
    /// page-cache progress.
    #[test]
    fn devio_frozen_counter_falls_back_after_grace() {
        let mut t = DevioTracker { base: 0, watermark: 0, disabled: false };
        t.observe(Some(0));
        assert_eq!(t.honest(DEVIO_FALLBACK_AFTER - 1), 0);
        assert_eq!(t.honest(DEVIO_FALLBACK_AFTER), DEVIO_FALLBACK_AFTER);
        assert!(t.disabled);
        assert_eq!(t.honest(DEVIO_FALLBACK_AFTER + 5), DEVIO_FALLBACK_AFTER + 5);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn device_write_bytes_counter_readable() {
        assert!(device_write_bytes().is_some());
    }

    /// While the (slow) flush blocks, the flush-tick hook must still be
    /// fed — this is what keeps the bar crawling toward 100% at real flush
    /// speed — and it must stop once the flush returns.
    #[test]
    fn flush_tick_fires_during_slow_sync() {
        let dir = scratch("flush-tick");
        let src = dir.join("in.bin");
        let dst = dir.join("out.bin");
        fs::write(&src, [0u8; 4096]).unwrap();
        fs::write(&dst, [0u8; 4096]).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let last_w = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let (c2, l2) = (calls.clone(), last_w.clone());
        let hook: ChunkHook = Arc::new(move |w: u64| {
            c2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            l2.store(w, std::sync::atomic::Ordering::Relaxed);
            true
        });
        let ctx = CopyCtx {
            preserve_perms: false,
            preserve_time: false,
            preserve_owner: None,
            src_meta: fs::metadata(&src).unwrap(),
            keep_partial: false,
            on_chunk: Arc::new(|_| true),
            on_flush_tick: Some(hook),
        };
        // Longer than one tick interval, so at least one tick is guaranteed
        // while the flush is in flight.
        let slow = || {
            thread::sleep(FLUSH_TICK_INTERVAL * 3);
            Ok(())
        };
        let devio = DevioTracker { base: 0, watermark: 100, disabled: false };
        finalize_copy(&dst, &slow, &ctx, Some(&devio), 4096).unwrap();
        assert!(
            calls.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the flush-tick hook must fire while the flush blocks"
        );
        assert!(
            last_w.load(std::sync::atomic::Ordering::Relaxed) <= 4096,
            "ticked values must stay within the file size"
        );
        assert!(dst.exists());
    }
}
