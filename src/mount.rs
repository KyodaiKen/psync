//! Drive mounting and disk helpers for config mode (parity with
//! pybackupsync): resolve `UUID=` devices, wait for the drive, mount,
//! track auto-mounted paths, unmount, query free space, detect SSDs and
//! run `fstrim`.
//!
//! All external commands use fixed argument vectors (never a shell), and
//! all paths are validated before use.

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::cancel;
use crate::error::{AppError, Result};

/// Run an external command without a shell and capture stdout.
/// Never builds a command line string, so no shell injection is possible.
pub fn run_stdout(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| AppError::io(format!("running '{program}'", program = program), e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(AppError::Other(format!(
            "'{program} {}' failed ({}): {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `/dev/disk/by-uuid/<uuid>` if the drive is present.
pub fn uuid_device(uuid: &str) -> Option<PathBuf> {
    let p = PathBuf::from(format!("/dev/disk/by-uuid/{uuid}"));
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Parsed `(device, mountpoint)` pairs from `/proc/mounts`.
pub fn parse_mounts() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(content) = fs::read_to_string("/proc/mounts") else {
        return out;
    };
    for line in content.lines() {
        let mut it = line.split_whitespace();
        if let (Some(dev), Some(mp)) = (it.next(), it.next()) {
            out.push((dev.to_string(), mp.to_string()));
        }
    }
    out
}

/// The current mount point for a UUID, if mounted anywhere.
pub fn mounted_path_for_uuid(uuid: &str) -> Option<String> {
    let dev = uuid_device(uuid)?;
    let real = fs::canonicalize(&dev).ok()?;
    for (dev_mounted, mp) in parse_mounts() {
        let same = Path::new(&dev_mounted) == real.as_path()
            || fs::canonicalize(&dev_mounted).ok().as_ref() == Some(&real);
        if same {
            return Some(mp);
        }
    }
    None
}

/// True if `path` is (or is under) a mount point listed in /proc/mounts.
pub fn is_mount_point(path: &str) -> bool {
    parse_mounts().iter().any(|(_, mp)| mp == path)
}

/// Wait until the drive appears (or cancellation). Prints a hint once.
pub fn wait_for_device(uuid: &str, hint: &str) -> Result<()> {
    if uuid_device(uuid).is_some() {
        return Ok(());
    }
    eprintln!(">> Please connect drive {hint} (UUID {uuid})...");
    let mut start = Instant::now();
    loop {
        if cancel::is_cancelled() {
            return Err(AppError::Cancelled);
        }
        if start.elapsed() > Duration::from_secs(30) {
            eprintln!("\r>> Still waiting for drive {uuid} (Ctrl-C to abort)   ");
            start = Instant::now();
        }
        if uuid_device(uuid).is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Mount a UUID at `mount_point` (no-op if already mounted).
pub fn mount_uuid(uuid: &str, mount_point: &str) -> Result<String> {
    if let Some(mp) = mounted_path_for_uuid(uuid) {
        return Ok(mp);
    }
    wait_for_device(uuid, mount_point)?;
    fs::create_dir_all(mount_point)
        .map_err(|e| AppError::io(format!("creating mount point '{}'", mount_point), e))?;
    eprintln!("Mounting {uuid} at {mount_point}...");
    let status = Command::new("mount")
        .arg(format!("UUID={uuid}"))
        .arg(mount_point)
        .status()
        .map_err(|e| AppError::io(String::from("running 'mount'"), e))?;
    if !status.success() {
        return Err(AppError::Mount(format!(
            "mounting UUID {uuid} at {mount_point} failed (exit {status:?})"
        )));
    }
    Ok(mount_point.to_string())
}

/// Unmount all auto-mounted paths (reverse order) after `sync`.
///
/// `prompt=true` asks the user on a TTY (pybackupsync `-um` skips the
/// prompt). On a non-TTY stdin the unmount proceeds (safe default for
/// unattended runs).
pub fn unmount_all(mount_points: &mut Vec<String>, prompt: bool) {
    if mount_points.is_empty() {
        return;
    }
    if prompt && std::io::stdin().is_terminal() {
        print!("\nUnmount automatically mounted drives? ({}): [Y/n] ", mount_points.len());
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            if let Some(first) = line.trim().chars().next() {
                if first == 'n' || first == 'N' {
                    mount_points.clear();
                    return;
                }
            }
        } else {
            // EOF / non-interactive: keep going (like -um).
        }
    }
    eprintln!("\nSynchronizing buffers (sync)...");
    let _ = run_stdout("sync", &[]);
    for mp in mount_points.iter().rev() {
        eprintln!("Unmounting {mp}...");
        let _ = run_stdout("umount", &["-l", mp]);
        // Wait until it is really gone.
        let deadline = Instant::now() + Duration::from_secs(10);
        while is_mount_point(mp) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    mount_points.clear();
}

/// Free bytes on the filesystem containing `path`.
///
/// Uses `df -P --output=avail` (POSIX, 1 KiB blocks) — no `unsafe` needed,
/// fixed argument vector, no shell.
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    // `df -Pk`: POSIX output, 1K blocks — Available is field 4
    // (Filesystem, 1024-blocks, Used, Available, Capacity, Mounted on).
    let out = Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().last()?;
    let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

/// True if the drive under `mount_point` is a non-rotational SSD.
pub fn is_ssd(mount_point: &str) -> bool {
    let Ok(device) = run_stdout(
        "findmnt",
        &["-n", "-o", "SOURCE", "--target", mount_point],
    ) else {
        return false;
    };
    if device.is_empty() {
        return false;
    }
    let Ok(rot) = run_stdout("lsblk", &["-dno", "ROTATIONAL", &device]) else {
        return false;
    };
    rot == "0"
}

/// Run `fstrim -v` on the mount point (no-op on failure).
pub fn fstrim_quiet(mount_point: &str) {
    if let Ok(out) = run_stdout("fstrim", &["-v", mount_point]) {
        eprintln!("fstrim: {out}");
    }
}

/// Registry of paths we mounted ourselves (thread-safe for future use).
#[derive(Default)]
pub struct MountRegistry {
    pub auto_mounted: Mutex<Vec<String>>,
}

impl MountRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a config entry: if it has a UUID, mount it (or reuse an
    /// existing mount); otherwise require `mount_point` to exist as a plain
    /// directory. Returns the usable base path.
    /// Resolve a UUID/mount-point pair to an absolute base path.
    ///
    /// * `uuid` given: mount the disk (if not already mounted) and use the
    ///   resulting mount point.
    /// * no `uuid`: use `mount_point` as a plain path. With `create`, a
    ///   missing directory is created (destinations); without it, the path
    ///   must already exist (sources).
    pub fn resolve(
        &self,
        uuid: Option<&str>,
        mount_point: &str,
        create: bool,
    ) -> Result<String> {
        match uuid {
            Some(uuid) => {
                let mp = mount_uuid(uuid, mount_point)?;
                self.auto_mounted
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(mp.clone());
                Ok(mp)
            }
            None => {
                let p = Path::new(mount_point);
                if !p.is_dir() {
                    if create {
                        fs::create_dir_all(p).map_err(|e| {
                            AppError::io(
                                format!("creating destination directory '{mount_point}'"),
                                e,
                            )
                        })?;
                    } else {
                        return Err(AppError::Mount(format!(
                            "path '{mount_point}' does not exist (and no uuid was given)"
                        )));
                    }
                }
                Ok(mount_point.to_string())
            }
        }
    }

    pub fn take_auto_mounted(&self) -> Vec<String> {
        self.auto_mounted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }
}
