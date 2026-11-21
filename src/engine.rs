//! The parallel copy engine.
//!
//! * `WorkerPool` — N std threads pulling jobs off a mutex + condvar queue.
//!   All shared state lives behind `Arc<Mutex<..>>` / atomics (see
//!   [`engine_ctx`]); progress handles are cheap clone references into
//!   one thread-safe UI (see `progress`). The crate is
//!   `#![forbid(unsafe_code)]`,
//!   so there are no raw pointers, no unsynchronized accesses, no races.
//! * Jobs are scheduled pybackupsync-style: each destination keeps its own
//!   queue (plan order preserved). Workers scan the destination queues in
//!   selection order (lower priority value first, default 99, then
//!   scheduling order) and take the first destination that has work and a
//!   free lock — so every destination starts at the same time,
//!   higher-priority destinations get first claim on each freed worker,
//!   and no destination starves behind a busy one. Each destination keeps
//!   its own task bar showing its *full* progress (all its files, not just
//!   the current one) while the bar message still names the file currently
//!   being processed.
//! * Only **one job per destination** runs at a time (the queue's
//!   `active` set, keyed by the job's `task_id`): concurrent writes to the same
//!   disk fragment files and stall spinning drives. This is how
//!   `pybackupsync` scheduled tasks — distinct destinations still run
//!   fully in parallel.
//! * Every file copy goes through the OS copy routine in 1 MiB chunks with
//!   live progress, cancellation between chunks and a shared bandwidth
//!   bucket (`--bwlimit` limits the total rate, like rsync).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
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
    /// One queue per destination, in selection order (priority, then
    /// scheduling order). Workers scan this order to pick work.
    queues: Vec<(u32, VecDeque<Job>)>,
    /// Destinations currently executing a job. A destination may only be
    /// picked while it is *not* in this set, so no two jobs ever write
    /// to the same destination at once.
    active: std::collections::HashSet<usize>,
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
                queues: Vec::new(),
                active: std::collections::HashSet::new(),
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

    /// Enqueue one job queue per destination, in selection order
    /// (lower priority value = picked first; within a priority,
    /// scheduling order). Within each queue the plan order is kept.
    pub fn push(&self, groups: Vec<(u32, Vec<Job>)>) {
        {
            let mut st = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            for (priority, jobs) in groups {
                st.queues.push((priority, jobs.into_iter().collect()));
            }
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

/// Pick the next job: scan the destinations in selection order (priority,
/// then scheduling order) and take the first one that has pending work
/// and is not already executing a job. This way every destination
/// starts immediately — a worker never sits behind a busy
/// higher-priority destination — higher-priority destinations get first
/// claim on each freed worker, and one-job-per-destination stays
/// enforced by the `active` set.
fn pick_job(shared: &Shared) -> Option<(Job, usize)> {
    let mut st = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        // First queue (in selection order) with work on a free
        // destination; busy destinations are skipped.
        let idx = st.queues.iter().position(|(_, q)| {
            q.front().is_some_and(|j| !st.active.contains(&j.task_id()))
        });
        if let Some(idx) = idx {
            let job = st.queues[idx].1.pop_front().unwrap();
            let tid = job.task_id();
            st.active.insert(tid);
            return Some((job, tid));
        }
        // Queue drained: only now does shutdown (no more work will come)
        // end this worker. Checking the queue first guarantees a worker
        // that wakes on the shutdown notify still drains pending jobs —
        // join() must never lose queued work.
        if st.shutdown {
            return None;
        }
        // Every destination with work is busy: wait for one to finish.
        st = shared
            .cond
            .wait(st)
            .unwrap_or_else(|p| p.into_inner());
    }
}

/// Free a destination (its job finished) and wake a waiting worker.
fn release_dest(shared: &Shared, tid: usize) {
    let mut st = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
    st.active.remove(&tid);
    drop(st);
    shared.cond.notify_all();
}

fn worker_loop(shared: &Shared, ctx: &Arc<EngineCtx>) {
    loop {
        let Some((job, tid)) = pick_job(shared) else {
            break;
        };
        if cancel::is_cancelled() {
            ctx.record_cancelled();
            release_dest(shared, tid);
            continue;
        }
        // The destination is ours until we release it: execute() opens
        // and closes the file bar, and the previous file's bar is fully
        // closed before the next one opens. Contain a panicking job so
        // the destination is always freed (the pool can't hang) and this
        // worker keeps serving other jobs.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let outcome = execute(&job, ctx);
            ctx.finish_job(outcome);
            release_dest(shared, tid);
        }))
        .is_err();
        if panicked {
            eprintln!("warning: a job panicked; the destination was released");
        }
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
    _task_id: usize,
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
                    ctx.ui.skip_bytes(size);
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
            ctx.ui.skip_bytes(size);
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

    let name = if ctx.show_dst_names {
        // `dest-root/rel` with the run's shared prefix stripped, so
        // concurrent copies to different destinations stay tell-apart
        // even in a narrow name column.
        EngineCtx::dst_display_name(&ctx.labels, dst, rel)
    } else {
        rel.to_string()
    };
    let fbar = ctx.ui.begin_file(&name, size);
    let on_chunk: fsutil::ChunkHook = {
        let fbar = fbar.clone();
        Arc::new(move |done: u64| {
            fbar.report(done);
            true
        })
    };
    // Feeds the bar while the final fsync blocks: honest crawl at flush
    // speed (the watermark is already capped at the file size upstream).
    let on_flush_tick: fsutil::ChunkHook = {
        let fbar = fbar.clone();
        Arc::new(move |w: u64| {
            fbar.report(w);
            true
        })
    };

    let copy_ctx = CopyCtx {
        preserve_perms,
        preserve_time,
        preserve_owner,
        src_meta,
        keep_partial,
        on_chunk,
        on_flush_tick: Some(on_flush_tick),
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
            if done != size {
                // Source size changed mid-copy; the live bar already warns.
                ctx.changed_files.fetch_add(1, Ordering::Relaxed);
            }
            fbar.finish(Some(done));
            JobOutcome::Copied { bytes: done }
        }
        Err(AppError::Cancelled) => {
            fbar.finish(None);
            JobOutcome::Cancelled
        }
        Err(e) => {
            fbar.finish(None);
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: e,
            })
        }
    }
}

fn run_link(_task_id: usize, rel: &str, dst: &Path, target: &str, ctx: &EngineCtx) -> JobOutcome {
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
        Ok(()) => JobOutcome::Linked {
            bytes: 0,
            link_dest: false,
        },
        Err(e) => {
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("creating symlink '{}'", dst.display()), e),
            })
        }
    }
}

fn run_node(
    _task_id: usize,
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
            JobOutcome::Linked { bytes: 0, link_dest: false }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::Other(format!(
                    "creating node '{}' ({})", rel, stderr.trim()
                )),
            })
        }
        Err(e) => {
            JobOutcome::Error(JobError {
                rel: rel.to_string(),
                error: AppError::io(format!("running node-creation tool for '{}'", dst.display()), e),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_ctx::EngineCtx;
    use crate::progress::ProgressUi;

    /// Regression test for the shutdown race: main() pushes all jobs and
    /// calls join() immediately, so the worker threads may still be
    /// starting up when `shutdown` is set. Workers must drain every queued
    /// job — join() must never lose work that was already enqueued. (The
    /// old pick_job checked `shutdown` before the queue and bailed,
    /// dropping the whole queue.)
    #[test]
    fn join_drains_jobs_pushed_before_it() {
        let base = std::env::temp_dir().join(format!(
            "psync-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = base.join("src");
        let dst_a = base.join("dstA");
        let dst_b = base.join("dstB");
        for d in [&src_dir, &dst_a, &dst_b] {
            fs::create_dir_all(d).unwrap();
        }
        let payload: Vec<u8> = (0..256u16).map(|i| (i % 251) as u8).collect();
        let mut groups: Vec<(u32, Vec<Job>)> = Vec::new();
        for (di, pri, dst) in [(0usize, 1u32, &dst_a), (1usize, 2u32, &dst_b)] {
            let mut jobs = Vec::new();
            for i in 0..4 {
                let name = format!("f{i}.bin");
                let src = src_dir.join(&name);
                fs::write(&src, &payload).unwrap();
                jobs.push(Job::File {
                    task_id: di,
                    rel: name.clone(),
                    src,
                    dst: dst.join(&name),
                    size: payload.len() as u64,
                    keep_partial: false,
                    preserve_perms: true,
                    preserve_time: true,
                    preserve_owner: None,
                    verify_checksum: false,
                    backup: false,
                    link_to: None,
                    remove_source: false,
                    temp_dir: None,
                });
            }
            groups.push((pri, jobs));
        }
        let ui = ProgressUi::new(fsutil::Units::Si, None);
        let ctx = Arc::new(EngineCtx::new(
            ui,
            vec![String::from("A"), String::from("B")],
            None,
            false,
            String::new(),
            true,
        ));
        let pool = WorkerPool::spawn(2, ctx.clone());
        pool.push(groups);
        // This is the exact main() sequence: join right after push.
        pool.join();
        for dst in [&dst_a, &dst_b] {
            for i in 0..4 {
                let p = dst.join(format!("f{i}.bin"));
                assert!(p.is_file(), "missing {}", p.display());
                assert_eq!(fs::read(&p).unwrap(), payload, "bad content in {}", p.display());
            }
        }
        assert_eq!(
            ctx.copied_files.load(std::sync::atomic::Ordering::Relaxed),
            8
        );
        fs::remove_dir_all(&base).ok();
    }
}
