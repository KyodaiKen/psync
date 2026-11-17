//! The parallel copy engine.
//!
//! * `WorkerPool` — N std threads pulling jobs off a mutex + condvar queue.
//!   All shared state lives behind `Arc<Mutex<..>>` / atomics (see
//!   [`engine_ctx`]); indicatif bars are cheap clone handles into one
//!   thread-safe `MultiProgress`. The crate is `#![forbid(unsafe_code)]`,
//!   so there are no raw pointers, no unsynchronized accesses, no races.
//! * Jobs are pre-sorted by priority (pybackupsync scheduling: destination
//!   priority, then destination uuid, then stable order), so parallel tasks
//!   interleave nicely across drives. Each destination keeps its own task
//!   bar showing its *full* progress (all its files, not just the current
//!   one) while the bar message still names the file currently being
//!   processed.
//! * Every file copy goes through the OS copy routine in 1 MiB chunks with
//!   live progress, cancellation between chunks and a shared bandwidth
//!   bucket (`--bwlimit` limits the total rate, like rsync).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;

use crate::cancel;
use crate::engine_ctx::{EngineCtx, JobError, JobOutcome};
use crate::error::AppError;
use crate::fsutil::{self, CopyCtx};

/// One unit of work.
pub enum Job {
    /// Copy a file (with live progress).
    File {
        task_id: usize,
        rel: String,
        src: PathBuf,
        dst: PathBuf,
        size: u64,
        keep_partial: bool,
        preserve_perms: bool,
        preserve_time: bool,
        preserve_owner: Option<(u32, u32)>,
        /// Verify checksums first; skip when identical.
        verify_checksum: bool,
        /// Move the existing destination file to `dst + suffix` first.
        backup: bool,
        /// Hard-link instead of copy (`--link-dest` / `--hard-links`).
        link_to: Option<PathBuf>,
        /// Delete the source file after a successful copy
        /// (`--remove-source-files`).
        remove_source: bool,
        /// Stage the copy in DIR first, then move into place (`--temp-dir`).
        temp_dir: Option<PathBuf>,
    },
    /// Create/update a symlink.
    Link {
        task_id: usize,
        rel: String,
        dst: PathBuf,
        target: String,
    },
    /// Create a device node / FIFO (needs root).
    Node {
        task_id: usize,
        rel: String,
        dst: PathBuf,
        kind: crate::plan::NodeKind,
        mode: u32,
    },
}

impl Job {
    pub fn task_id(&self) -> usize {
        match self {
            Job::File { task_id, .. } | Job::Link { task_id, .. } | Job::Node { task_id, .. } => {
                *task_id
            }
        }
    }

    pub fn rel(&self) -> &str {
        match self {
            Job::File { rel, .. } | Job::Link { rel, .. } | Job::Node { rel, .. } => rel,
        }
    }
}

/// Shared queue of pending jobs.
struct QueueState {
    pending: VecDeque<Job>,
    shutdown: bool,
}

struct Shared {
    queue: Mutex<QueueState>,
    cond: Condvar,
}

/// A pool of worker threads executing jobs.
pub struct WorkerPool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Spawn `n` workers.
    pub fn spawn(n: usize, ctx: Arc<EngineCtx>) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(QueueState {
                pending: VecDeque::new(),
                shutdown: false,
            }),
            cond: Condvar::new(),
        });
        let mut threads = Vec::with_capacity(n);
        for i in 0..n {
            let shared = shared.clone();
            let ctx = ctx.clone();
            let handle = std::thread::Builder::new()
                .name(format!("psync-worker-{i}"))
                .spawn(move || worker_loop(&shared, &ctx))
                .expect("failed to spawn worker thread");
            threads.push(handle);
        }
        Self { shared, threads }
    }

    /// Enqueue jobs (already sorted by priority).
    pub fn push(&self, jobs: Vec<Job>) {
        {
            let mut st = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            st.pending.extend(jobs);
        }
        self.shared.cond.notify_all();
    }

    /// Signal shutdown and wait for all workers to finish.
    pub fn join(mut self) {
        {
            let mut st = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            st.shutdown = true;
        }
        self.shared.cond.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn worker_loop(shared: &Shared, ctx: &Arc<EngineCtx>) {
    loop {
        let job_opt = {
            let mut st = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if let Some(j) = st.pending.pop_front() {
                    break Some(j);
                }
                if st.shutdown {
                    break None;
                }
                st = shared
                    .cond
                    .wait(st)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };
        let Some(job) = job_opt else {
            break;
        };
        if cancel::is_cancelled() {
            ctx.record_cancelled();
            continue;
        }
        let outcome = execute(&job, ctx);
        ctx.finish_job(outcome);
    }
}

/// Execute one job.
fn execute(job: &Job, ctx: &EngineCtx) -> JobOutcome {
    match job {
        Job::File {
            task_id,
            rel,
            src,
            dst,
            size,
            keep_partial,
            preserve_perms,
            preserve_time,
            preserve_owner,
            verify_checksum,
            backup,
            link_to,
            remove_source,
            temp_dir,
        } => run_file(
            *task_id,
            rel,
            src,
            dst,
            *size,
            *keep_partial,
            *preserve_perms,
            *preserve_time,
            *preserve_owner,
            *verify_checksum,
            *backup,
            link_to.as_deref(),
            *remove_source,
            temp_dir.as_deref(),
            ctx,
        ),
        Job::Link {
            task_id,
            rel,
            dst,
            target,
        } => run_link(*task_id, rel, dst, target, ctx),
        Job::Node {
            task_id,
            rel,
            dst,
            kind,
            mode,
        } => run_node(*task_id, rel, dst, kind, *mode, ctx),
    }
}

fn run_file(
    task_id: usize,
    rel: &str,
    src: &Path,
    dst: &Path,
    size: u64,
    keep_partial: bool,
    preserve_perms: bool,
    preserve_time: bool,
    preserve_owner: Option<(u32, u32)>,
    verify_checksum: bool,
    backup: bool,
    link_to: Option<&Path>,
    remove_source: bool,
    temp_dir: Option<&Path>,
    ctx: &EngineCtx,
) -> JobOutcome {
    let task = &ctx.tasks[task_id];

    if cancel::is_cancelled() {
        return JobOutcome::Cancelled;
    }

    // `--link-dest` / `--hard-links`: hard-link when the source exists.
    if let Some(link_src) = link_to {
        if link_src.exists() {
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::remove_file(dst);
            match fs::hard_link(link_src, dst) {
                Ok(()) => {
                    task.inc_bytes(size);
                    task.finish_file();
                    return JobOutcome::Linked {
                        bytes: size,
                        link_dest: true,
                    };
                }
                Err(e) => {
                    ctx.add_warning(format!(
                        "hard link for {rel}: {e}; falling back to a normal copy"
                    ));
                }
            }
        }
    }

    // Checksum verification (--checksum with equal sizes).
    if verify_checksum {
        let (a, b) = match (fsutil::file_sha256(src), fsutil::file_sha256(dst)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) => {
                return JobOutcome::Error(JobError { rel: rel.to_string(), error: e })
            }
            (_, Err(e)) => {
                return JobOutcome::Error(JobError { rel: rel.to_string(), error: e })
            }
        };
        if a == b {
            task.inc_bytes(size);
            task.finish_file();
            return JobOutcome::Skipped {
                reason: "checksums identical".into(),
            };
        }
    }

    // `--backup`: move the old destination file aside first.
    if backup {
        let backup_path = PathBuf::from(format!("{}{}", dst.display(), ctx.suffix));
        if let Err(e) = fs::rename(dst, &backup_path) {
            return JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("backing up '{}'", dst.display()), e),
            });
        }
        ctx.add_event(format!(
            "backup: {} -> {}",
            dst.display(),
            backup_path.display()
        ));
    }

    // Defensive parent-directory creation (the dir pass does this too, but
    // parallel order must not depend on it).
    if let Some(parent) = dst.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("creating directory '{}'", parent.display()), e),
            });
        }
    }

    // Fresh source metadata (avoids stale TOCTOU data from the scan).
    let src_meta = match fs::metadata(src) {
        Ok(md) => md,
        Err(e) => {
            return JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("stat source '{}'", src.display()), e),
            })
        }
    };

    // `--temp-dir`: stage the copy first, then move it into place.
    let staged: Option<PathBuf> = match temp_dir {
        Some(td) => {
            let t = match fsutil::safe_join(td, rel) {
                Ok(t) => t,
                Err(e) => {
                    return JobOutcome::Error(JobError { rel: rel.to_string(), error: e })
                }
            };
            if let Some(parent) = t.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return JobOutcome::Error(JobError {
                        rel: rel.to_string(),
                        error: AppError::io(format!("creating '{}'", parent.display()), e),
                    });
                }
            }
            Some(t)
        }
        None => None,
    };
    let copy_target = staged.as_deref().unwrap_or(dst);

    task.set_current_file(rel);

    let last_done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hook_task = task.clone();
    let on_chunk: fsutil::ChunkHook = Box::new(move |done: u64| {
        let prev = last_done.swap(done, std::sync::atomic::Ordering::Relaxed);
        let delta = done.wrapping_sub(prev);
        hook_task.inc_bytes(delta);
        true
    });

    let copy_ctx = CopyCtx {
        preserve_perms,
        preserve_time,
        preserve_owner,
        src_meta,
        keep_partial,
        on_chunk,
    };

    match fsutil::copy_file(src, copy_target, &copy_ctx, &ctx.pacer) {
        Ok(done) => {
            if let Some(t) = &staged {
                // Move into place (rename is atomic on one filesystem).
                let _ = fs::remove_file(dst);
                if let Err(e) = fs::rename(t, dst) {
                    if e.kind() == std::io::ErrorKind::CrossesDevices {
                        // Rare: temp dir on another filesystem — plain copy.
                        if let Err(e2) = std::fs::copy(t, dst) {
                            let _ = fs::remove_file(t);
                            return JobOutcome::Error(JobError {
                                rel: rel.to_string(),
                                error: AppError::io(format!("moving '{}' to '{}'", t.display(), dst.display()), e2),
                            });
                        }
                        let _ = fs::remove_file(t);
                    } else {
                        return JobOutcome::Error(JobError {
                            rel: rel.to_string(),
                            error: AppError::io(format!("moving '{}' to '{}'", t.display(), dst.display()), e),
                        });
                    }
                }
            }
            if remove_source {
                match fs::remove_file(src) {
                    Ok(()) => {
                        ctx.add_event(format!("removed source: {}", src.display()))
                    }
                    Err(e) => {
                        ctx.add_warning(format!("could not remove source '{}': {e}", src.display()))
                    }
                }
            }
            ctx.record_transferred(dst.to_path_buf());
            task.finish_file();
            JobOutcome::Copied { bytes: done }
        }
        Err(AppError::Cancelled) => JobOutcome::Cancelled,
        Err(e) => {
            task.count_error();
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: e,
            })
        }
    }
}

fn run_link(task_id: usize, rel: &str, dst: &Path, target: &str, ctx: &EngineCtx) -> JobOutcome {
    if ctx.dry_run {
        return JobOutcome::DryRun { bytes: 0 };
    }
    if let Some(parent) = dst.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("creating directory '{}'", parent.display()), e),
            });
        }
    }
    match fs::read_link(dst) {
        Ok(existing) => {
            if existing.to_string_lossy() == target {
                ctx.tasks[task_id].finish_file();
                return JobOutcome::Skipped {
                    reason: "symlink already correct".into(),
                };
            }
            let _ = fs::remove_file(dst);
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                // Something else occupies the path.
                let _ = fs::remove_file(dst);
            }
        }
    }
    match std::os::unix::fs::symlink(target, dst) {
        Ok(()) => {
            ctx.tasks[task_id].finish_file();
            JobOutcome::Linked {
                bytes: 0,
                link_dest: false,
            }
        }
        Err(e) => {
            ctx.tasks[task_id].count_error();
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("creating symlink '{}'", dst.display()), e),
            })
        }
    }
}

fn run_node(
    task_id: usize,
    rel: &str,
    dst: &Path,
    kind: &crate::plan::NodeKind,
    mode: u32,
    ctx: &EngineCtx,
) -> JobOutcome {
    if ctx.dry_run {
        return JobOutcome::DryRun { bytes: 0 };
    }
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_file(dst);
    let perm = mode & 0o7777;
    // Device nodes and FIFOs are created with coreutils (`mkfifo(1)` /
    // `mknod(1)`): fixed argument vectors, no shell, and no `unsafe` in our
    // crate. FIFOs need only write access to the destination directory;
    // device nodes need root/CAP_MKNOD. A failure is reported per file,
    // exactly like rsync would.
    let out = match kind {
        crate::plan::NodeKind::Fifo => std::process::Command::new("mkfifo").arg(dst).output(),
        crate::plan::NodeKind::CharDev(major, minor) => std::process::Command::new("mknod")
            .arg(dst)
            .arg("c")
            .arg(major.to_string())
            .arg(minor.to_string())
            .output(),
        crate::plan::NodeKind::BlockDev(major, minor) => std::process::Command::new("mknod")
            .arg(dst)
            .arg("b")
            .arg(major.to_string())
            .arg(minor.to_string())
            .output(),
    };
    match out {
        Ok(o) if o.status.success() => {
            let _ = fsutil::set_mode(dst, perm);
            ctx.tasks[task_id].finish_file();
            JobOutcome::Linked { bytes: 0, link_dest: false }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            ctx.tasks[task_id].count_error();
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::Other(format!(
                    "creating node '{}' ({})", rel, stderr.trim()
                )),
            })
        }
        Err(e) => {
            ctx.tasks[task_id].count_error();
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("running node-creation tool for '{}'", dst.display()), e),
            })
        }
    }
}
