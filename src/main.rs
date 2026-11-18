//! psync — rsync-compatible local synchronizer with parallel transfer and
//! live progress bars.
//!
//! Two modes:
//! * **rsync mode** — `psync [OPTIONS] SRC... DST` with the full rsync
//!   option surface (plus `--jobs`, `--bwlimit`).
//! * **backup config mode** — `psync -C config.yaml` (pybackupsync
//!   compatible: UUID drives, priorities, deletion policies, fstrim,
//!   unmount prompts).
//!
//! Thread-safety: `#![forbid(unsafe_code)]` — every shared state lives
//! behind `Arc<Mutex<..>>` / atomics (see `engine_ctx`), progress bars are
//! cheap handles into one thread-safe `MultiProgress`, workers communicate
//! only through a mutex + condvar queue. No shell is ever spawned for user
//! data paths, and `fsutil::safe_join` rejects path traversal.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
// The internal API surface (report/stats helpers, job-outcome details,
// config-mode option parsers) is intentionally wider than today's call
// sites; reserved for the upcoming stats/reporting work.
#![allow(dead_code)]

mod cancel;
mod cli;
mod config;
mod engine;
mod engine_ctx;
mod error;
mod filter;
mod fsutil;
mod mount;
mod plan;
mod progress;
mod report;
mod space;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use clap::Parser;

use crate::cli::Cli;
use crate::engine_ctx::EngineCtx;
use crate::error::{AppError, Result};
use crate::filter::{FilterSet, RuleAction};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    cancel::install_handlers();
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // Help / version are not errors: print them the way clap
            // intends (stdout, fully formatted) and exit 0.
            use clap::error::ErrorKind;
            if e.kind() == ErrorKind::DisplayHelp || e.kind() == ErrorKind::DisplayVersion {
                let _ = e.print();
                return 0;
            }
            eprintln!("psync: {e}");
            return 24;
        }
    };
    if cli.help {
        use clap::CommandFactory;
        let _ = Cli::command().print_long_help();
        return 0;
    }
    if cli.version {
        println!("psync {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    if let Err(e) = cli.validate() {
        eprintln!("psync: {e}");
        return 24;
    }
    match run_inner(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("psync: error: {}", error::format_chain(&e));
            match e {
                AppError::Usage(_)
                | AppError::Config(_)
                | AppError::Unsupported(_)
                | AppError::NotImplemented(_) => 24,
                _ => 23,
            }
        }
    }
}

fn run_inner(cli: &Cli) -> Result<i32> {
    if cli.config.is_some() {
        run_config_mode(cli)
    } else {
        run_rsync_mode(cli)
    }
}

// ---------------------------------------------------------------------------
// rsync mode
// ---------------------------------------------------------------------------

fn run_rsync_mode(cli: &Cli) -> Result<i32> {
    let dry = cli.dry_run;
    if cli.paths.len() < 2 {
        return Err(AppError::Usage(
            "need at least one source and one destination (try --help)".into(),
        ));
    }
    let all = cli.paths.clone();
    let dst_raw = all[all.len() - 1].clone();
    let src_raw = &all[..all.len() - 1];

    let rules = build_rules(cli)?;

    // Resolve sources (rsync trailing-slash semantics).
    let mut specs: Vec<plan::SourceSpec> = Vec::new();
    for sp in src_raw {
        let s = sp.to_string_lossy().into_owned();
        let contents = s.ends_with('/');
        let cleaned = s.trim_end_matches('/').to_string();
        let p = Path::new(&cleaned);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| AppError::io("getting current directory", e))?
                .join(p)
        };
        if contents && abs.is_file() {
            return Err(AppError::Usage(format!(
                "'{}' is a file; the trailing '/' is only valid for directories",
                abs.display()
            )));
        }
        let canonical = fs::canonicalize(&abs)
            .map_err(|e| AppError::io(format!("resolving source '{}'", abs.display()), e))?;
        specs.push(plan::SourceSpec {
            abs: canonical,
            contents,
        });
    }

    let dst = resolve_destination(&dst_raw, dry)?;

    // Never let the destination sit inside a source (self-sync).
    for s in &specs {
        if dst == s.abs || dst.starts_with(&s.abs) {
            return Err(AppError::Usage(format!(
                "destination '{}' is inside source '{}'; refusing to sync a tree into itself",
                dst.display(),
                s.abs.display()
            )));
        }
    }

    let units = if cli.si_units {
        fsutil::Units::Si
    } else {
        fsutil::Units::Jedec
    };
    let ui = progress::ProgressUi::new(units, cli.width);
    let spin = ui.spinner("scan", "scanning source…");

    let scan_opts = plan::ScanOpts {
        recursive: cli.recursive(),
        links: cli.links(),
        copy_links: cli.copy_links,
        devices: cli.devices(),
        filters: rules.clone(),
        max_depth: cli.max_depth,
        max_file_bytes: cli.max_file_bytes(),
        min_file_bytes: cli.min_filesize,
        one_fs: cli.one_fs,
        hard_links: cli.hard_links,
    };

    let mut plan = plan::scan_sources(&specs, &scan_opts)?;
    spin.finish();

    let decide = plan::DecideOpts {
        checksum: cli.checksum,
        size_only: cli.size_only,
        modify_window: cli.modify_window,
        update: cli.update,
        ignore_times: cli.ignore_times,
        backup: cli.backup,
        suffix: cli.suffix.clone().unwrap_or_else(|| "~".into()),
        link_dest: cli.link_dest.clone(),
        compare: plan::CompareDecision::Rsync,
    };
    plan::bind_destination(&mut plan, &dst, &decide)?;

    // Resolve `VerifyChecksum` entries now (rsync `-c` parity): the checksum,
    // not the size/time heuristic, is the authority. Identical files become
    // Skipped, differing ones plain copies — so dry-run and itemize report
    // the real outcome.
    resolve_checksums(&mut plan);

    // `--delete` planning (protected files survive; deepest dirs first).
    let delete_wanted =
        cli.delete || cli.delete_before || cli.delete_after || cli.delete_during;
    let delete_plan = if delete_wanted {
        plan::compute_deletes(
            &dst,
            &plan.src_rels,
            &rules,
            cli.delete || cli.delete_before || cli.delete_during,
            cli.prune_empty,
        )?
    } else {
        plan::DeletePlan {
            files: Vec::new(),
            dirs: Vec::new(),
        }
    };

    // Itemized lines (shared by dry-run listing and --itemize-changes).
    let flags = itemize_flags(cli);
    let mut itemize: Vec<report::ItemizeLine> = Vec::new();
    for f in &plan.files {
        let (kind, action, bytes) = match &f.action {
            plan::FileAction::Skip(_) => ('f', '=', 0u64),
            plan::FileAction::Copy => ('f', 'c', f.size),
            plan::FileAction::VerifyChecksum => ('f', 'u', f.size),
            plan::FileAction::Backup => ('f', 'c', f.size),
            plan::FileAction::LinkDest(_) | plan::FileAction::HardLink(_) => {
                ('f', 'l', f.size)
            }
        };
        itemize.push(report::ItemizeLine {
            kind,
            action,
            flags: flags.clone(),
            rel: f.rel.clone(),
            bytes,
        });
    }
    for l in &plan.links {
        itemize.push(report::ItemizeLine {
            kind: 'l',
            action: 'c',
            flags: flags.clone(),
            rel: l.rel.clone(),
            bytes: 0,
        });
    }
    for n in &plan.nodes {
        itemize.push(report::ItemizeLine {
            kind: '-',
            action: 'c',
            flags: flags.clone(),
            rel: n.rel.clone(),
            bytes: 0,
        });
    }
    if cli.verbose > 1 {
        for d in &plan.dirs {
            itemize.push(report::ItemizeLine {
                kind: 'd',
                action: 'c',
                flags: flags.clone(),
                rel: d.rel.clone(),
                bytes: 0,
            });
        }
    }
    if delete_wanted {
        for p in &delete_plan.files {
            itemize.push(report::ItemizeLine {
                kind: 'f',
                action: 'd',
                flags: String::new(),
                rel: p.to_string_lossy().into_owned(),
                bytes: 0,
            });
        }
        for p in &delete_plan.dirs {
            itemize.push(report::ItemizeLine {
                kind: 'd',
                action: 'd',
                flags: String::new(),
                rel: p.to_string_lossy().into_owned(),
                bytes: 0,
            });
        }
    }

    // Dry-run / list-only: report and stop.
    if dry || cli.list_only {
        let fmt = cli.out_format.as_deref();
        for l in &itemize {
            println!("{}", l.render(fmt, cli.human, units));
        }
        let to_copy = plan
            .files
            .iter()
            .filter(|f| !matches!(f.action, plan::FileAction::Skip(_)))
            .count();
        println!("Number of files to be transferred: {to_copy}");
        println!(
            "Total size to be transferred: {}",
            fsutil::format_bytes(plan.total_copy_bytes, units)
        );
        println!("Number of files skipped: {}", plan.skipped);
        if cli.stats {
            print!("{}", dry_summary(&plan, cli.human, units).render());
        }
        if let Some(logf) = &cli.log_file {
            if let Err(e) = report::write_log_file(logf, &itemize) {
                eprintln!("warning: writing log file: {e}");
            }
        }
        return Ok(0);
    }

    // Directories first (depth order), with modes.
    {
        let mut dirs = plan.dirs.clone();
        dirs.sort_by(|a, b| {
            a.rel.rmatches('/').count().cmp(&b.rel.rmatches('/').count())
        });
        for d in &dirs {
            if let Err(e) = fs::create_dir_all(&d.dst_abs) {
                return Err(AppError::io(
                    format!("creating directory '{}'", d.dst_abs.display()),
                    e,
                ));
            }
            if cli.perms() {
                if let Err(e) = fsutil::set_mode(&d.dst_abs, d.mode) {
                    eprintln!(
                        "warning: setting mode on '{}': {e}",
                        d.dst_abs.display()
                    );
                }
            }
        }
    }

    // Grand total bar (one bar per active file is opened by the workers
    // as they go). Created even when nothing will be copied, so a no-op
    // run still shows the total bar — instantly at 100% — instead of a
    // bare frame.
    {
        let hints: Vec<(String, u64)> = plan
            .files
            .iter()
            .filter(|f| !matches!(f.action, plan::FileAction::Skip(_)))
            .map(|f| (f.rel.clone(), f.size))
            .collect();
        ui.set_file_hints(&hints);
        ui.begin_total("total", plan.total_copy_bytes);
    }

    let pacer = cli.bwlimit_kib().map(|k| Mutex::new(fsutil::Pacer::new(k)));
    let ctx = Arc::new(EngineCtx::new(
        ui.clone(),
        vec![dst.to_string_lossy().into_owned()],
        pacer,
        false,
        cli.suffix.clone().unwrap_or_else(|| "~".into()),
        false,
    ));

    // Deletion phase: before (also --delete-during, rsync-like).
    if cli.delete || cli.delete_before || cli.delete_during {
        let spin = ui.spinner("del", "deleting extraneous files…");
        execute_deletes(&delete_plan, &ctx);
        spin.finish();
    }

    // Build the job list.
    let mut jobs: Vec<engine::Job> = Vec::new();
    for f in &plan.files {
        let (verify, backup, link_to) = match &f.action {
            plan::FileAction::Skip(_) => continue,
            plan::FileAction::Copy => (false, false, None),
            plan::FileAction::VerifyChecksum => (true, false, None),
            plan::FileAction::Backup => (false, true, None),
            plan::FileAction::LinkDest(p) => (false, false, Some(p.clone())),
            plan::FileAction::HardLink(rel) => (false, false, Some(dst.join(rel))),
        };
        jobs.push(engine::Job::File {
            task_id: 0,
            rel: f.rel.clone(),
            src: f.src_abs.clone(),
            dst: f.dst_abs.clone(),
            size: f.size,
            keep_partial: cli.partial,
            preserve_perms: cli.perms(),
            preserve_time: cli.times(),
            preserve_owner: if cli.owner() {
                Some((f.uid, f.gid))
            } else {
                None
            },
            verify_checksum: verify,
            backup,
            link_to,
            remove_source: cli.remove_source,
            temp_dir: cli.temp_dir.clone(),
        });
    }
    for l in &plan.links {
        jobs.push(engine::Job::Link {
            task_id: 0,
            rel: l.rel.clone(),
            dst: l.dst_abs.clone(),
            target: l.target.clone(),
        });
    }
    for n in &plan.nodes {
        jobs.push(engine::Job::Node {
            task_id: 0,
            rel: n.rel.clone(),
            dst: n.dst_abs.clone(),
            kind: n.kind.clone(),
            mode: n.mode,
        });
    }

    // Run the worker pool.
    let n_workers = cli.jobs.unwrap_or(2);
    let pool = engine::WorkerPool::spawn(n_workers, ctx.clone());
    pool.push(jobs);
    pool.join();

    // Post-processing (dir mtimes, --chmod).
    post_process(cli, &ctx, &plan);

    // Deletion phase: after.
    if cli.delete_after {
        let spin = ui.spinner("del", "deleting extraneous files…");
        execute_deletes(&delete_plan, &ctx);
        spin.finish();
    }

    Ok(final_report(cli, &ctx, &plan, &itemize, &ui))
}

// ---------------------------------------------------------------------------
// backup config mode (pybackupsync compatible)
// ---------------------------------------------------------------------------

/// Resolve pending `VerifyChecksum` decisions by actually hashing both sides
/// (rsync `-c` parity). Identical files become Skipped; differing files plain
/// Copies. Plan aggregates are adjusted to match.
fn resolve_checksums(plan: &mut plan::Plan) {
    for f in &mut plan.files {
        if !matches!(f.action, plan::FileAction::VerifyChecksum) {
            continue;
        }
        match (fsutil::file_sha256(&f.src_abs), fsutil::file_sha256(&f.dst_abs)) {
            (Ok(a), Ok(b)) if a == b => {
                f.action = plan::FileAction::Skip("checksums identical".into());
                plan.total_copy_bytes = plan.total_copy_bytes.saturating_sub(f.size);
                plan.skipped += 1;
            }
            (Ok(_), Ok(_)) => {
                f.action = plan::FileAction::Copy;
            }
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("warning: cannot checksum '{}': {e}", f.rel);
                f.action = plan::FileAction::Copy;
            }
        }
    }
}

fn run_config_mode(cli: &Cli) -> Result<i32> {
    let cfg_path = cli.config.clone().expect("config mode checked in run_inner");
    let cfg = config::load(&cfg_path)?;
    if cfg.destinations.is_empty() {
        return Err(AppError::Config("no destinations configured".into()));
    }
    let dry = cli.dry_run;
    let registry = mount::MountRegistry::new();

    // ---- source ----
    let src_base =
        registry.resolve(cfg.source.uuid.as_deref(), &cfg.source.mount_point, false)?;
    let src_path = cfg.source.path.trim();
    let src_root = if src_path.is_empty() {
        PathBuf::from(src_base.clone())
    } else {
        fsutil::safe_join(Path::new(&src_base), src_path)?
    };
    if !src_root.is_dir() {
        return Err(AppError::Config(format!(
            "source path is not a directory: {}",
            src_root.display()
        )));
    }
    eprintln!(
        "Source: {}{}",
        src_base,
        if src_path.is_empty() {
            String::new()
        } else {
            format!("/{src_path}")
        }
    );

    let mut rules = FilterSet::new();
    for p in &cfg.source.exclude_patterns {
        rules.add(RuleAction::Exclude, p)?;
    }

    let scan_opts = plan::ScanOpts {
        recursive: true,
        links: true,
        copy_links: false,
        devices: false,
        filters: rules,
        max_depth: None,
        max_file_bytes: None,
        min_file_bytes: None,
        one_fs: false,
        hard_links: false,
    };

    let units = if cli.si_units {
        fsutil::Units::Si
    } else {
        fsutil::Units::Jedec
    };
    let ui = progress::ProgressUi::new(units, cli.width);
    let spin = ui.spinner("scan", &format!("scanning {}", src_root.display()));
    let base_plan = plan::scan_sources(
        &[plan::SourceSpec {
            abs: src_root.clone(),
            contents: true,
        }],
        &scan_opts,
    )?;
    spin.finish();

    if base_plan.files.is_empty() && base_plan.links.is_empty() && base_plan.nodes.is_empty()
    {
        ui.finish_all();
        eprintln!("No files found in source; nothing to do.");
        return Ok(0);
    }

    struct DestPlan {
        label: String,
        dst_root: PathBuf,
        priority: u32,
        sort_key: String,
        plan: plan::Plan,
    }

    let mut dests: Vec<DestPlan> = Vec::new();
    for (di, d) in cfg.destinations.iter().enumerate() {
        let base = registry.resolve(d.uuid.as_deref(), &d.mount_point, true)?;
        let dpath = d.path.trim();
        let dst_root = if dpath.is_empty() {
            PathBuf::from(base.clone())
        } else {
            fsutil::safe_join(Path::new(&base), dpath)?
        };
        if !dry {
            fs::create_dir_all(&dst_root).map_err(|e| {
                AppError::io(format!("creating destination {}", dst_root.display()), e)
            })?;
        }
        ui.println(&format!(
            "Destination {}: {}{} (priority {}, compare: {})",
            di + 1,
            base,
            if dpath.is_empty() {
                String::new()
            } else {
                format!("/{dpath}")
            },
            d.priority,
            d.compare
        ));

        let decide = plan::DecideOpts {
            checksum: false,
            size_only: false,
            modify_window: 0,
            update: false,
            ignore_times: false,
            backup: false,
            suffix: "~".into(),
            link_dest: None,
            compare: match d.compare {
                config::CompareMode::Size => plan::CompareDecision::SizeOnly,
                config::CompareMode::SizeTime => plan::CompareDecision::Rsync,
                config::CompareMode::TimeSize => plan::CompareDecision::TimeSize,
                config::CompareMode::Checksum => plan::CompareDecision::Checksum,
            },
        };

        let mut p = base_plan.clone();
        plan::bind_destination(&mut p, &dst_root, &decide)?;
        resolve_checksums(&mut p);

        let to_copy: Vec<&plan::FilePlan> = p
            .files
            .iter()
            .filter(|f| !matches!(f.action, plan::FileAction::Skip(_)))
            .collect();

        if to_copy.is_empty() {
            ui.println("  up to date.");
        } else {
            let needed: i64 = to_copy
                .iter()
                .map(|f| {
                    let existing = fs::symlink_metadata(&f.dst_abs)
                        .map(|m| m.len() as i64)
                        .unwrap_or(0);
                    f.size as i64 - existing
                })
                .sum();
            ui.println(&format!(
                "  would copy {} files ({} new data)",
                to_copy.len(),
                fsutil::format_bytes(p.total_copy_bytes, units)
            ));
            if needed > 0 && !dry {
                if d.deletion_exclude_patterns.iter().any(|x| x.trim() == "*") {
                    ui.println("  (deletion disabled by deletion_exclude_patterns)");
                } else {
                    let mut dirset = std::collections::BTreeSet::new();
                    for f in &to_copy {
                        if let Some(parent) = f.dst_abs.parent() {
                            dirset.insert(parent.to_string_lossy().into_owned());
                        }
                    }
                    let target_dirs: Vec<String> = dirset.into_iter().collect();
                    space::ensure_space(
                        &base,
                        &target_dirs,
                        needed,
                        d.deletion_policy,
                        &d.deletion_exclude_patterns,
                        dry,
                        units,
                    )?;
                }
            }
        }

        dests.push(DestPlan {
            label: format!(
                "{}{}",
                base,
                if dpath.is_empty() {
                    String::new()
                } else {
                    format!("/{dpath}")
                }
            ),
            dst_root,
            priority: d.priority,
            sort_key: d.uuid.clone().unwrap_or_else(|| d.mount_point.clone()),
            plan: p,
        });
    }

    // Create all destination directories first (depth order) — including
    // empty ones, like rsync does.
    {
        let mut all_dirs: Vec<&plan::DirPlan> = Vec::new();
        for dp in &dests {
            all_dirs.extend(dp.plan.dirs.iter());
        }
        all_dirs.sort_by(|a, b| {
            a.rel.rmatches('/').count().cmp(&b.rel.rmatches('/').count())
        });
        if !dry {
            for d in &all_dirs {
                if let Err(e) = fs::create_dir_all(&d.dst_abs) {
                    return Err(AppError::io(
                        format!("creating directory '{}'", d.dst_abs.display()),
                        e,
                    ));
                }
                if let Err(e) = fsutil::set_mode(&d.dst_abs, d.mode) {
                    eprintln!("warning: setting mode on '{}': {e}", d.dst_abs.display());
                }
            }
        }
    }

    // ---- jobs: pybackupsync-style scheduling.
    // A flat (priority, destination, rel) sort would queue *all* of one
    // destination's files before the next destination's, so with the
    // per-destination lock the second destination could only start once
    // the first destination's first file finished. Instead each
    // destination keeps its own queue (plan order preserved) and
    // destinations interleave round-robin within a priority tier
    // (lower value = runs first, default 99): one job per destination
    // per pass, in scheduling order. Every destination starts at the
    // same time; one-job-per-destination stays enforced by the engine's
    // per-destination locks.
    let mut keyed: Vec<(u32, usize, String, engine::Job)> = Vec::new();
    for (di, dp) in dests.iter().enumerate() {
        for f in &dp.plan.files {
            let (verify, backup, link_to) = match &f.action {
                plan::FileAction::Skip(_) => continue,
                plan::FileAction::Copy => (false, false, None),
                plan::FileAction::VerifyChecksum => (true, false, None),
                plan::FileAction::Backup => (false, true, None),
                plan::FileAction::LinkDest(p) => (false, false, Some(p.clone())),
                plan::FileAction::HardLink(rel) => (false, false, Some(dp.dst_root.join(rel))),
            };
            keyed.push((
                dp.priority,
                di,
                f.rel.clone(),
                engine::Job::File {
                    task_id: di,
                    rel: f.rel.clone(),
                    src: f.src_abs.clone(),
                    dst: f.dst_abs.clone(),
                    size: f.size,
                    keep_partial: cli.partial,
                    preserve_perms: true,
                    preserve_time: true,
                    preserve_owner: None,
                    verify_checksum: verify,
                    backup,
                    link_to,
                    remove_source: cli.remove_source,
                    temp_dir: cli.temp_dir.clone(),
                },
            ));
        }
        for l in &dp.plan.links {
            keyed.push((
                dp.priority,
                di,
                l.rel.clone(),
                engine::Job::Link {
                    task_id: di,
                    rel: l.rel.clone(),
                    dst: l.dst_abs.clone(),
                    target: l.target.clone(),
                },
            ));
        }
        for n in &dp.plan.nodes {
            keyed.push((
                dp.priority,
                di,
                n.rel.clone(),
                engine::Job::Node {
                    task_id: di,
                    rel: n.rel.clone(),
                    dst: n.dst_abs.clone(),
                    kind: n.kind.clone(),
                    mode: n.mode,
                },
            ));
        }
    }
    let mut jobs: Vec<engine::Job> = Vec::new();
    {
        use std::collections::{BTreeMap, VecDeque};
        let mut tier: BTreeMap<u32, Vec<(usize, VecDeque<engine::Job>)>> = BTreeMap::new();
        for (pri, di, _rel, job) in keyed {
            let cols = tier.entry(pri).or_default();
            match cols.iter_mut().find(|(d, _)| *d == di) {
                Some((_, q)) => q.push_back(job),
                None => {
                    let mut q = VecDeque::new();
                    q.push_back(job);
                    cols.push((di, q));
                }
            }
        }
        for (_, mut cols) in tier {
            let mut any = true;
            while any {
                any = false;
                for (_, q) in cols.iter_mut() {
                    if let Some(j) = q.pop_front() {
                        jobs.push(j);
                        any = true;
                    }
                }
            }
        }
    }

    let total_new_bytes: u64 = dests.iter().map(|d| d.plan.total_copy_bytes).sum();
    let nothing = jobs.is_empty();

    if dry {
        // The scan spinner may have used the alternate screen; back to the
        // main page before the dry-run report prints.
        ui.finish_all();
        for dp in &dests {
            eprintln!(
                "  [dry-run] would sync {} -> {}",
                src_root.display(),
                dp.dst_root.display()
            );
        }
        eprintln!("Total data to copy: {}", fsutil::format_bytes(total_new_bytes, units));
        return Ok(0);
    }

    // ---- progress: one bar per active file (workers open them as they
    // go) + one grand total bar. The file set is known now, so the name
    // and message columns are fixed for the whole run.
    let pacer = cli.bwlimit_kib().map(|k| Mutex::new(fsutil::Pacer::new(k)));
    let labels: Vec<String> = dests.iter().map(|d| d.label.clone()).collect();
    if total_new_bytes > 0 {
        let hints: Vec<(String, u64)> = jobs
            .iter()
            .filter_map(|j| match j {
                // Config mode names the *destination* file (shared prefix
                // stripped), so each line shows which copy it is.
                engine::Job::File { dst, rel, size, .. } => {
                    Some((EngineCtx::dst_display_name(&labels, dst, rel), *size))
                }
                _ => None,
            })
            .collect();
        ui.set_file_hints(&hints);
    }
    // The total bar always exists — even a no-op run shows it at 100%
    // ("0 B/0 B") so the frame never degenerates to bare text lines.
    ui.begin_total("total", total_new_bytes);

    // ---- engine
    let ctx = Arc::new(EngineCtx::new(ui.clone(), labels, pacer, false, "~".into(), true));

    ui.println(&format!(
        "Starting Copy. Total data: {}",
        fsutil::format_bytes(total_new_bytes, units)
    ));

    let n_workers = cli.jobs.unwrap_or(cfg.max_parallel_processes);
    let pool = engine::WorkerPool::spawn(n_workers, ctx.clone());
    pool.push(jobs);
    pool.join();

    ui.finish_all();

    // The UI owns the alternate screen until here; from this point on the
    // terminal is back on the main page, so these land in the normal
    // scrollback.
    if nothing {
        eprintln!("All destinations are up to date; nothing to copy.");
    }

    // ---- unmount auto-mounted drives (prompt unless -U); the UI is
    // finished first so the prompt never collides with the live frame.
    let mut auto = registry.take_auto_mounted();
    if !auto.is_empty() {
        mount::unmount_all(&mut auto, !cli.umount);
    }
    let errors: Vec<_> = {
        let g = ctx.errors.lock().unwrap_or_else(|p| p.into_inner());
        g.clone()
    };
    if !errors.is_empty() {
        report::print_errors(&errors);
    }
    if cli.stats || !errors.is_empty() {
        let s = report::Summary {
            files_scanned: base_plan.files.len() as u64,
            dirs_scanned: base_plan.dirs.len() as u64,
            links_scanned: base_plan.links.len() as u64,
            copied_files: ctx.copied_files.load(Ordering::Relaxed),
            copied_bytes: ctx.copied_bytes.load(Ordering::Relaxed),
            changed_files: ctx.changed_files.load(Ordering::Relaxed),
            linked_files: ctx.linked_files.load(Ordering::Relaxed),
            skipped_files: dests
                .iter()
                .map(|d| d.plan.skipped)
                .sum::<u64>()
                + ctx.skipped_files.load(Ordering::Relaxed),
            deleted_files: ctx.deleted_files.load(Ordering::Relaxed),
            deleted_bytes: ctx.deleted_bytes.load(Ordering::Relaxed),
            error_files: ctx.error_count(),
            cancelled_jobs: ctx.cancelled_jobs.load(Ordering::Relaxed),
            elapsed_secs: ctx.started.elapsed().as_secs_f64(),
            human: cli.human,
            units,
        };
        print!("{}", s.render());
    }
    let cancelled = ctx.cancelled_jobs.load(Ordering::Relaxed);
    if cancelled > 0 {
        eprintln!("psync: cancelled by user ({} job(s) did not run)", cancelled);
    }
    if !errors.is_empty() || cancelled > 0 {
        return Ok(23);
    }
    eprintln!(
        "Copy operation finished after {:.1}s.",
        ctx.started.elapsed().as_secs_f64()
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Build the filter set from CLI rules.
///
/// Rule order (documented compromise, since clap splits the option lists):
/// `--filter` specs, then `--include`, then `--exclude`, then
/// `--filter-from`/`--include-from`/`--exclude-from` files. First match
/// still wins within that order — the common
/// `--include='*.pdf' --exclude='*'` pattern works as expected.
fn build_rules(cli: &Cli) -> Result<FilterSet> {
    let mut fs = FilterSet::new();
    for spec in &cli.filter {
        fs.add_spec(spec)?;
    }
    for p in &cli.include {
        fs.add(RuleAction::Include, p)?;
    }
    for p in &cli.exclude {
        fs.add(RuleAction::Exclude, p)?;
    }
    for f in &cli.filter_from {
        fs.load_file(f, RuleAction::Exclude)?;
    }
    for f in &cli.include_from {
        fs.load_file(f, RuleAction::Include)?;
    }
    for f in &cli.exclude_from {
        fs.load_file(f, RuleAction::Exclude)?;
    }
    Ok(fs)
}

/// Resolve the destination path, creating it when it does not exist.
fn resolve_destination(raw: &Path, dry: bool) -> Result<PathBuf> {
    let s = raw.to_string_lossy().into_owned();
    let cleaned = s.trim_end_matches('/').to_string();
    if cleaned.is_empty() {
        return Err(AppError::Usage("empty destination path".into()));
    }
    let p = Path::new(&cleaned);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| AppError::io("getting current directory", e))?
            .join(p)
    };
    if abs.exists() {
        let c = fs::canonicalize(&abs)
            .map_err(|e| AppError::io(format!("resolving destination '{}'", abs.display()), e))?;
        if !c.is_dir() {
            return Err(AppError::Usage(format!(
                "destination '{}' exists but is not a directory",
                abs.display()
            )));
        }
        Ok(c)
    } else if dry {
        Ok(abs)
    } else {
        fs::create_dir_all(&abs)
            .map_err(|e| AppError::io(format!("creating destination '{}'", abs.display()), e))?;
        let c = fs::canonicalize(&abs)
            .map_err(|e| AppError::io(format!("resolving destination '{}'", abs.display()), e))?;
        Ok(c)
    }
}

/// Execute a delete plan (files then deepest-first dirs).
fn execute_deletes(dp: &plan::DeletePlan, ctx: &EngineCtx) {
    for p in &dp.files {
        let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(p) {
            Ok(()) => {
                ctx.deleted_files.fetch_add(1, Ordering::Relaxed);
                ctx.deleted_bytes.fetch_add(size, Ordering::Relaxed);
            }
            Err(e) => ctx.add_warning(format!("delete '{}': {e}", p.display())),
        }
    }
    for d in &dp.dirs {
        if let Err(e) = fs::remove_dir(d) {
            ctx.add_warning(format!("delete dir '{}': {e}", d.display()));
        }
    }
}

/// Post-copy bookkeeping: directory mtimes and `--chmod`.
fn post_process(cli: &Cli, ctx: &EngineCtx, plan: &plan::Plan) {
    // Directory modification times (deepest first), like rsync.
    if cli.times() && !cli.omit_dir_times {
        let mut dirs = plan.dirs.clone();
        dirs.sort_by(|a, b| {
            b.rel.rmatches('/').count().cmp(&a.rel.rmatches('/').count())
        });
        for d in &dirs {
            if let Some(mtime) = d.mtime {
                // `std::fs::set_times` updates mtime (atime left unchanged).
                let t = fs::FileTimes::new().set_modified(mtime);
                if let Err(e) = fs::set_times(&d.dst_abs, t) {
                    ctx.add_warning(format!(
                        "setting dir mtime '{}': {e}",
                        d.dst_abs.display()
                    ));
                }
            }
        }
    }
    // `--chmod`: apply only to files we actually transferred.
    if let Some(mode_s) = &cli.chmod {
        match parse_mode(mode_s) {
            Ok(mode) => {
                let list = ctx
                    .transferred
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                for p in &list {
                    if let Err(e) = fsutil::set_mode(p, mode) {
                        ctx.add_warning(format!("chmod {}: {e}", p.display()));
                    }
                }
            }
            Err(e) => {
                eprintln!("psync: {e}");
            }
        }
    }
}

/// Parse a chmod-style mode: octal (`644`, `0755`) or symbolic
/// (`a+rX`, `ug+rw`, `u-s` …). Pure parsing — nothing is executed.
fn parse_mode(input: &str) -> Result<u32> {
    let s = input.trim();
    if s.is_empty() {
        return Err(AppError::Usage("empty --chmod mode".into()));
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return u32::from_str_radix(s, 8).map_err(|_| {
            AppError::Usage(format!(
                "invalid octal chmod mode '{s}' (expected e.g. 644 or 0755)"
            ))
        });
    }
    let class_mask = |who: u32| {
        let mut m = 0u32;
        if who & 4 != 0 {
            m |= 0o7777;
        }
        if who & 2 != 0 {
            m |= 0o77;
        }
        if who & 1 != 0 {
            m |= 0o7;
        }
        m
    };
    let mut mode: Option<u32> = None;
    for spec in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0usize;
        let mut who: u32 = 0;
        while i < chars.len() {
            match chars[i] {
                'u' => who |= 4,
                'g' => who |= 2,
                'o' => who |= 1,
                'a' => who = 7,
                _ => break,
            }
            i += 1;
        }
        if who == 0 || i >= chars.len() {
            return Err(AppError::Usage(format!(
                "invalid chmod spec '{spec}' (expected e.g. a+rX, ug+rw, u-s)"
            )));
        }
        let op = chars[i];
        i += 1;
        if !matches!(op, '+' | '-' | '=') {
            return Err(AppError::Usage(format!(
                "invalid chmod operator '{op}' in '{spec}'"
            )));
        }
        let mut masked = 0u32;
        let mut any = false;
        while i < chars.len() {
            any = true;
            match chars[i] {
                'r' => {
                    if who & 4 != 0 {
                        masked |= 0o400;
                    }
                    if who & 2 != 0 {
                        masked |= 0o40;
                    }
                    if who & 1 != 0 {
                        masked |= 0o4;
                    }
                }
                'w' => {
                    if who & 4 != 0 {
                        masked |= 0o200;
                    }
                    if who & 2 != 0 {
                        masked |= 0o20;
                    }
                    if who & 1 != 0 {
                        masked |= 0o2;
                    }
                }
                'x' => {
                    if who & 4 != 0 {
                        masked |= 0o100;
                    }
                    if who & 2 != 0 {
                        masked |= 0o10;
                    }
                    if who & 1 != 0 {
                        masked |= 0o1;
                    }
                }
                's' => {
                    if who & 4 != 0 {
                        masked |= 0o4000;
                    }
                    if who & 2 != 0 {
                        masked |= 0o2000;
                    }
                }
                't' => {
                    if who & 1 != 0 {
                        masked |= 0o1000;
                    }
                }
                c => {
                    return Err(AppError::Usage(format!(
                        "invalid chmod letter '{c}' in '{spec}'"
                    )))
                }
            }
            i += 1;
        }
        if !any {
            return Err(AppError::Usage(format!(
                "missing permission letters in chmod spec '{spec}'"
            )));
        }
        let base = mode.unwrap_or(0o777);
        let next = match op {
            '=' => (base & !class_mask(who)) | masked,
            '-' => base & !masked,
            _ => base | masked,
        };
        mode = Some(next & 0o7777);
    }
    mode.ok_or_else(|| AppError::Usage("empty --chmod mode".into()))
}

/// Itemize flags column from the effective flags (`a` when all apply).
fn itemize_flags(cli: &Cli) -> String {
    let bits = [
        (cli.perms(), 'p'),
        (cli.times(), 't'),
        (cli.owner(), 'o'),
        (cli.group(), 'g'),
        (cli.links(), 'L'),
    ];
    if bits.iter().all(|(b, _)| *b) {
        "a".to_string()
    } else {
        bits.iter().filter(|(b, _)| *b).map(|(_, c)| *c).collect()
    }
}

/// `--stats` summary for a finished (or dry) run.
fn dry_summary(plan: &plan::Plan, human: bool, units: fsutil::Units) -> report::Summary {
    let to_copy = plan
        .files
        .iter()
        .filter(|f| !matches!(f.action, plan::FileAction::Skip(_)))
        .count() as u64;
    report::Summary {
        files_scanned: plan.files.len() as u64,
        dirs_scanned: plan.dirs.len() as u64,
        links_scanned: plan.links.len() as u64,
        copied_files: to_copy,
        copied_bytes: plan.total_copy_bytes,
        changed_files: 0,
        linked_files: 0,
        skipped_files: plan.skipped,
        deleted_files: 0,
        deleted_bytes: 0,
        error_files: 0,
        cancelled_jobs: 0,
        elapsed_secs: 0.0,
        human,
        units,
    }
}

/// Final report for a real rsync-mode run; returns the process exit code.
fn final_report(
    cli: &Cli,
    ctx: &EngineCtx,
    plan: &plan::Plan,
    itemize: &[report::ItemizeLine],
    ui: &progress::ProgressUi,
) -> i32 {
    ui.finish_all();

    let errors: Vec<_> = {
        let g = ctx.errors.lock().unwrap_or_else(|p| p.into_inner());
        g.clone()
    };
    if !errors.is_empty() {
        report::print_errors(&errors);
    }

    if cli.itemize || cli.verbose > 0 {
        let fmt = cli.out_format.as_deref();
        for l in itemize {
            println!("{}", l.render(fmt, cli.human, ui.units()));
        }
    }

    if cli.stats || cli.verbose > 0 || !errors.is_empty() {
        let s = report::Summary {
            files_scanned: plan.files.len() as u64,
            dirs_scanned: plan.dirs.len() as u64,
            links_scanned: plan.links.len() as u64,
            copied_files: ctx.copied_files.load(Ordering::Relaxed),
            copied_bytes: ctx.copied_bytes.load(Ordering::Relaxed),
            changed_files: ctx.changed_files.load(Ordering::Relaxed),
            linked_files: ctx.linked_files.load(Ordering::Relaxed),
            skipped_files: plan.skipped + ctx.skipped_files.load(Ordering::Relaxed),
            deleted_files: ctx.deleted_files.load(Ordering::Relaxed),
            deleted_bytes: ctx.deleted_bytes.load(Ordering::Relaxed),
            error_files: ctx.error_count(),
            cancelled_jobs: ctx.cancelled_jobs.load(Ordering::Relaxed),
            elapsed_secs: ctx.started.elapsed().as_secs_f64(),
            human: cli.human,
            units: ui.units(),
        };
        print!("{}", s.render());
    }

    if let Some(logf) = &cli.log_file {
        if let Err(e) = report::write_log_file(logf, itemize) {
            eprintln!("warning: writing log file: {e}");
        }
    }

    let cancelled = ctx.cancelled_jobs.load(Ordering::Relaxed);
    if cancelled > 0 {
        eprintln!("psync: cancelled by user ({} job(s) did not run)", cancelled);
    }
    if !errors.is_empty() || cancelled > 0 {
        23
    } else {
        0
    }
}
