//! Space management for backup (config) mode — a faithful port of
//! pybackupsync `ensure_space`:
//!
//! * Only the **net** space difference (new size minus existing size) is
//!   required, exactly like the Python original.
//! * Deletion is strictly scoped to the destination directories that are
//!   actually receiving new files — nothing else on the drive is touched.
//! * Policies: `oldest` (FIFO by ctime) and `month_dupes` (keep one file
//!   per YYYY-month — the newest — then thin oldest first).
//! * `deletion_exclude_patterns` protect files; `["*"]` disables deletion
//!   entirely (Python parity).
//! * `fstrim` runs after successful cleanup on SSDs.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cancel;
use crate::config::DeletionPolicy;
use crate::error::Result;
use crate::fsutil::{format_bytes, Units};
use crate::mount;

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    ctime: SystemTime,
    size: u64,
}

fn policy_name(p: DeletionPolicy) -> &'static str {
    match p {
        DeletionPolicy::Oldest => "OLDEST",
        DeletionPolicy::MonthDupes => "MONTH_DUPES",
    }
}

/// Basename match against a shell-style pattern (`*`, `?`).
fn name_matches(name: &str, pattern: &str) -> bool {
    crate::filter::glob_match(pattern, name)
}

/// Collect deletable files from the given (non-recursive) directories.
fn collect_candidates(target_dirs: &[String], exclude_patterns: &[String]) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in target_dirs {
        let dir_path = Path::new(dir);
        if !seen.insert(dir_path.to_path_buf()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(md) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !md.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if exclude_patterns.iter().any(|p| name_matches(&name, p)) {
                continue;
            }
            out.push(Candidate {
                ctime: UNIX_EPOCH + Duration::from_secs(md.ctime().max(0) as u64),
                path,
                size: md.len(),
            });
        }
    }
    out
}

/// Delete one candidate; returns bytes freed.
fn delete_one(
    c: &Candidate,
    label: &str,
    policy: &str,
    dry_run: bool,
) -> Result<u64> {
    if dry_run {
        eprintln!("[dry-run] Would delete ({policy}/{label}): {}", c.path.display());
        return Ok(c.size);
    }
    let size = c.size;
    fs::remove_file(&c.path).map_err(|e| {
        crate::error::AppError::io(
            format!("deleting '{}'", c.path.display()),
            e,
        )
    })?;
    eprintln!("Deleted ({policy}/{label}): {}", c.path.display());
    Ok(size)
}

/// Ensure `needed_bytes` (net) of free space on `mount_point`, deleting
/// from `target_dirs` according to `policy` when necessary.
pub fn ensure_space(
    mount_point: &str,
    target_dirs: &[String],
    needed_bytes: i64,
    policy: DeletionPolicy,
    exclude_patterns: &[String],
    dry_run: bool,
    units: Units,
) -> Result<()> {
    if needed_bytes <= 0 {
        return Ok(());
    }
    let needed = needed_bytes as u64;
    let free = match mount::disk_free_bytes(Path::new(mount_point)) {
        Some(f) => f,
        None => {
            eprintln!(
                "warning: could not determine free space for {mount_point}; assuming enough"
            );
            return Ok(());
        }
    };
    if free >= needed {
        return Ok(());
    }
    let deficit = needed - free;
    eprintln!(
        "Cleaning up (Policy: {}). Need {} more.",
        policy_name(policy),
        format_bytes(deficit, units)
    );

    let candidates = collect_candidates(target_dirs, exclude_patterns);
    if candidates.is_empty() {
        eprintln!(
            "warning: no deletable files found in the target directories; \
             the sync may fail for lack of space"
        );
        return Ok(());
    }

    let mut remaining_free = free;
    let mut deleted_size: u64 = 0;
    let mut stage2_done = false;

    let month_key = |c: &Candidate| -> String {
        let secs = c
            .ctime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let days = secs.div_euclid(86400);
        // Approximate YYYY-MM from unix days (civil-from-days algorithm).
        let (y, m) = civil_from_days(days as i64);
        format!("{y:04}-{m:02}")
    };

    let ordered: Vec<(Candidate, bool)> = match policy {
        DeletionPolicy::Oldest => {
            let mut v = candidates
                .into_iter()
                .map(|c| (c, false))
                .collect::<Vec<_>>();
            v.sort_by_key(|c| c.0.ctime);
            v
        }
        DeletionPolicy::MonthDupes => {
            let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
            for c in candidates {
                groups.entry(month_key(&c)).or_default().push(c);
            }
            let mut month_keys: Vec<String> = groups.keys().cloned().collect();
            month_keys.sort();
            let mut dups: Vec<Candidate> = Vec::new();
            let mut keepers: Vec<Candidate> = Vec::new();
            for (k, mut v) in groups {
                let _ = k;
                v.sort_by_key(|c| c.ctime);
                if v.len() > 1 {
                    dups.extend(v.drain(..v.len() - 1));
                    keepers.push(v.into_iter().next().unwrap());
                } else {
                    keepers.push(v.into_iter().next().unwrap());
                }
            }
            dups.sort_by_key(|c| c.ctime);
            keepers.sort_by_key(|c| c.ctime);
            // Stage 1: all duplicates (oldest first); Stage 2 (only if
            // still short): the monthly archives (oldest first) — same
            // order as pybackupsync.
            dups.into_iter().map(|c| (c, true))
                .chain(keepers.into_iter().map(|c| (c, false)))
                .collect()
        }
    };

    for (c, is_dup) in ordered.iter() {
        if cancel::is_cancelled() {
            return Err(crate::error::AppError::Cancelled);
        }
        if remaining_free >= needed {
            break;
        }
        let label = if policy == DeletionPolicy::MonthDupes && !is_dup {
            if !stage2_done {
                eprintln!(
                    "Monthly duplicates cleaned. Still need {} more. \
                     Starting monthly archive deletion.",
                    format_bytes(needed - remaining_free, units)
                );
                stage2_done = true;
            }
            "ARCHIVE"
        } else {
            "POLICY"
        };
        let freed = delete_one(c, label, policy_name(policy), dry_run)?;
        deleted_size += freed;
        remaining_free = remaining_free.saturating_add(freed);
    }

    if deleted_size > 0 {
        // Optimize SSD after deletion (Python parity).
        if !dry_run && mount::is_ssd(mount_point) {
            eprintln!("SSD detected at {mount_point}. Running fstrim...");
            mount::fstrim_quiet(mount_point);
        }
    }

    if remaining_free >= needed {
        eprintln!("Freed enough space ({}).", format_bytes(deleted_size, units));
    } else {
        eprintln!("Warning: Cleanup finished but might still lack space.");
    }
    Ok(())
}

/// Convert days-since-1970-01-01 to (year, month) — Howard Hinnant's
/// civil_from_days, exact.
fn civil_from_days(z: i64) -> (i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]; 0 = March
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as i64)
}
