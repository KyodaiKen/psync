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
/// Receives the cumulative number of bytes written; return `false` to abort
/// the transfer (the partial destination file is removed unless
/// [`CopyCtx::keep_partial`] is set).
pub type ChunkHook = Box<dyn Fn(u64) -> bool>;

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
}

impl CopyCtx {
    /// Clean up the destination file if the transfer fails and `-P` is off.
    pub fn cleanup(&self, dst: &Path) {
        if !self.keep_partial {
            let _ = fs::remove_file(dst);
        }
    }
}

/// Copy `src` to `dst` with progress, cancellation and bandwidth limiting.
///
/// Returns the number of bytes written.
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
            if !(ctx.on_chunk)(done) {
                ctx.cleanup(dst);
                return Err(AppError::Cancelled);
            }
        }
    }

    // --- preserve metadata -------------------------------------------------
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
    let _ = out_file.sync_all();
    Ok(done)
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
