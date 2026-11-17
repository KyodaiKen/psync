//! Source scanning, per-file action decisions (copy / skip / verify /
//! hard-link / backup) and `--delete` planning.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{AppError, Result};
use crate::filter::FilterSet;
use crate::fsutil::safe_join;

/// One source to scan (a trailing-slash source copies its *contents*).
#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub abs: PathBuf,
    pub contents: bool,
}

#[derive(Debug, Clone)]
pub struct DirPlan {
    /// Path relative to the transfer root, no leading/trailing '/'.
    pub rel: String,
    pub dst_abs: PathBuf,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
}

#[derive(Debug, Clone)]
pub enum FileAction {
    /// Plain copy.
    Copy,
    /// Copy only if the SHA-256 checksums actually differ.
    VerifyChecksum,
    /// Back up the stale destination file to `dst + suffix`, then copy.
    Backup,
    /// Hard-link the destination to the identical file in `--link-dest`.
    LinkDest(PathBuf),
    /// Hard-link to another source file with the same inode (`--hard-links`).
    HardLink(String),
    /// Up to date (or intentionally skipped); reason for reports.
    Skip(&'static str),
}

#[derive(Debug, Clone)]
pub struct FilePlan {
    pub rel: String,
    pub src_abs: PathBuf,
    pub dst_abs: PathBuf,
    pub size: u64,
    pub action: FileAction,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone)]
pub struct LinkPlan {
    pub rel: String,
    pub src_abs: PathBuf,
    pub dst_abs: PathBuf,
    pub target: String,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Fifo,
    CharDev(u32, u32),
    BlockDev(u32, u32),
}

#[derive(Debug, Clone)]
pub struct NodePlan {
    pub rel: String,
    pub dst_abs: PathBuf,
    pub kind: NodeKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// A fully planned transfer (before execution).
#[derive(Debug, Default, Clone)]
pub struct Plan {
    pub dirs: Vec<DirPlan>,
    pub files: Vec<FilePlan>,
    pub links: Vec<LinkPlan>,
    pub nodes: Vec<NodePlan>,
    /// Every source path (files, dirs, links) relative to the transfer root.
    /// Used by `--delete` to know what must survive.
    pub src_rels: HashSet<String>,
    /// Bytes that will actually be copied.
    pub total_copy_bytes: u64,
    /// Files decided to skip.
    pub skipped: u64,
    /// Files whose destination is a directory (fatal per-file conflict).
    pub conflicts: u64,
}

/// Options governing the scan.
pub struct ScanOpts {
    pub recursive: bool,
    /// `-l`: symlinks are copied as symlinks.
    pub links: bool,
    /// `-L`: follow symlinks and copy their contents.
    pub copy_links: bool,
    /// `-D`: transfer devices/FIFOs (needs root to create).
    pub devices: bool,
    pub filters: FilterSet,
    pub max_depth: Option<u32>,
    pub max_file_bytes: Option<u64>,
    pub min_file_bytes: Option<u64>,
    /// `-x`: stay on one filesystem.
    pub one_fs: bool,
    /// `--hard-links`: deduplicate same-inode files.
    pub hard_links: bool,
}

impl Default for ScanOpts {
    fn default() -> Self {
        Self {
            recursive: true,
            links: false,
            copy_links: false,
            devices: false,
            filters: FilterSet::new(),
            max_depth: None,
            max_file_bytes: None,
            min_file_bytes: None,
            one_fs: false,
            hard_links: false,
        }
    }
}

/// Options governing per-file copy/skip decisions.
pub struct DecideOpts {
    pub checksum: bool,
    pub size_only: bool,
    pub modify_window: i64,
    pub update: bool,
    pub ignore_times: bool,
    pub backup: bool,
    pub suffix: String,
    pub link_dest: Option<PathBuf>,
    /// pybackupsync-compatible config-mode comparison.
    pub compare: CompareDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareDecision {
    /// rsync semantics (size then mtime within window) — default.
    Rsync,
    /// Config mode: size only (pybackupsync parity).
    SizeOnly,
    /// Config mode: mtime then size.
    TimeSize,
    /// Config mode / --checksum: verify with SHA-256.
    Checksum,
}

impl Default for DecideOpts {
    fn default() -> Self {
        Self {
            checksum: false,
            size_only: false,
            modify_window: 0,
            update: false,
            ignore_times: false,
            backup: false,
            suffix: "~".into(),
            link_dest: None,
            compare: CompareDecision::Rsync,
        }
    }
}

/// Scan all sources into a single `Plan` (before destination comparison).
pub fn scan_sources(specs: &[SourceSpec], opts: &ScanOpts) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut inode_map: HashMap<(u64, u64), String> = HashMap::new();
    for spec in specs {
        let md = fs::symlink_metadata(&spec.abs).map_err(|e| {
            AppError::io(
                format!("source '{}' does not exist or is not readable", spec.abs.display()),
                e,
            )
        })?;
        if md.is_file() {
            // Single-file source.
            let name = spec
                .abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            let rel = name.clone();
            if opts.filters.is_excluded(&rel, false) {
                continue;
            }
            if let Some(max) = opts.max_file_bytes {
                if md.len() > max {
                    // rsync counts size-filtered files as skipped.
                    plan.skipped += 1;
                    continue;
                }
            }
            if let Some(min) = opts.min_file_bytes {
                if md.len() < min {
                    plan.skipped += 1;
                    continue;
                }
            }
            plan.src_rels.insert(rel.clone());
            plan.files.push(FilePlan {
                rel,
                src_abs: spec.abs.clone(),
                dst_abs: PathBuf::new(), // filled in by bind_destination
                size: md.len(),
                action: FileAction::Copy,
                mtime: md.modified().ok(),
                mode: md.mode(),
                uid: md.uid(),
                gid: md.gid(),
            });
            continue;
        }
        if !md.is_dir() {
            // Symlink source: copy as link or as its file.
            if !opts.links && !opts.copy_links {
                continue;
            }
            let name = spec
                .abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "link".into());
            let target = fs::read_link(&spec.abs)
                .map_err(|e| AppError::io(format!("reading symlink {}", spec.abs.display()), e))?;
            plan.src_rels.insert(name.clone());
            plan.links.push(LinkPlan {
                rel: name.clone(),
                src_abs: spec.abs.clone(),
                dst_abs: PathBuf::new(),
                target: target.to_string_lossy().into_owned(),
            });
            continue;
        }
        let root_dev = md.dev();
        let root = if spec.contents { String::new() } else { root_name(&spec.abs) };
        walk_dir(
            &spec.abs,
            root.as_str(),
            0,
            root_dev,
            opts,
            &mut plan,
            &mut inode_map,
        )?;
    }
    // Bind destination paths (dst_abs is set later via bind_destination).
    Ok(plan)
}

/// `file_name` of a path, without extension-less weirdness.
fn root_name(p: &Path) -> String {
    p.file_name()
        .map(|n| format!("{}/", n.to_string_lossy()))
        .unwrap_or_else(|| "/".into())
}

/// Recursive directory walk. `rel_prefix` ends with '/' or is "".
fn walk_dir(
    dir_abs: &Path,
    rel_prefix: &str,
    depth: u32,
    root_dev: u64,
    opts: &ScanOpts,
    plan: &mut Plan,
    inode_map: &mut HashMap<(u64, u64), String>,
) -> Result<()> {
    if let Some(n) = opts.max_depth {
        if depth > n {
            return Ok(());
        }
    }
    let mut entries: Vec<fs::DirEntry> = match fs::read_dir(dir_abs) {
        Ok(it) => it.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            return Err(AppError::io(
                format!("reading directory '{}'", dir_abs.display()),
                e,
            ))
        }
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue; // vanished or unreadable — skip (rsync prints an error; we count it as a scan skip)
        };
        let ft = md.file_type();
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}{name}")
        };
        if opts.filters.is_excluded(&rel, ft.is_dir()) {
            continue;
        }

        if ft.is_dir() {
            if opts.one_fs && md.dev() != root_dev {
                continue; // -x: do not cross filesystem boundaries
            }
            plan.src_rels.insert(rel.clone());
            plan.dirs.push(DirPlan {
                rel: rel.clone(),
                dst_abs: PathBuf::new(),
                mtime: md.modified().ok(),
                mode: md.mode(),
            });
            if opts.recursive {
                let sub = if rel.is_empty() {
                    String::new()
                } else {
                    format!("{rel}/")
                };
                walk_dir(&path, &sub, depth + 1, root_dev, opts, plan, inode_map)?;
            }
            continue;
        }

        if ft.is_symlink() {
            if opts.links {
                let target = fs::read_link(&path)
                    .map_err(|e| AppError::io(format!("reading symlink '{rel}'"), e))?;
                plan.src_rels.insert(rel.clone());
                plan.links.push(LinkPlan {
                    rel: rel.clone(),
                    src_abs: path.clone(),
                    dst_abs: PathBuf::new(),
                    target: target.to_string_lossy().into_owned(),
                });
            } else if opts.copy_links {
                let Ok(tmd) = fs::metadata(&path) else {
                    continue; // broken link
                };
                if tmd.is_file() {
                    push_file(&path, &rel, &tmd, opts, plan, inode_map);
                } else if tmd.is_dir() {
                    if opts.recursive {
                        let sub = format!("{rel}/");
                        walk_dir(&path, &sub, depth + 1, root_dev, opts, plan, inode_map)?;
                    }
                }
            } else if opts.recursive {
                // -r without -l: follow symlinks to files, not to dirs.
                let Ok(tmd) = fs::metadata(&path) else {
                    continue;
                };
                if tmd.is_file() {
                    push_file(&path, &rel, &tmd, opts, plan, inode_map);
                }
            }
            // else: non-recursive, no -l → skipped (rsync behaviour)
            continue;
        }

        if ft.is_file() {
            push_file(&path, &rel, &md, opts, plan, inode_map);
            continue;
        }

        if ft.is_fifo() {
            if opts.devices {
                plan.src_rels.insert(rel.clone());
                plan.nodes.push(NodePlan {
                    rel,
                    dst_abs: PathBuf::new(),
                    kind: NodeKind::Fifo,
                    mode: md.mode(),
                    uid: md.uid(),
                    gid: md.gid(),
                });
            }
            continue;
        }

        if opts.devices {
            let class = md.mode() & 0o170000;
            let kind = match class {
                0o60000 => NodeKind::BlockDev(
                    libc::major(md.rdev()) as u32,
                    libc::minor(md.rdev()) as u32,
                ),
                0o20000 => NodeKind::CharDev(
                    libc::major(md.rdev()) as u32,
                    libc::minor(md.rdev()) as u32,
                ),
                _ => continue, // sockets and friends are never transferred
            };
            plan.src_rels.insert(rel.clone());
            plan.nodes.push(NodePlan {
                rel,
                dst_abs: PathBuf::new(),
                kind,
                mode: md.mode(),
                uid: md.uid(),
                gid: md.gid(),
            });
        }
        // sockets: never transferred (rsync behaviour)
    }
    Ok(())
}

fn push_file(
    path: &Path,
    rel: &str,
    md: &fs::Metadata,
    opts: &ScanOpts,
    plan: &mut Plan,
    inode_map: &mut HashMap<(u64, u64), String>,
) {
    let size = md.len();
    if let Some(max) = opts.max_file_bytes {
        if size > max {
            return;
        }
    }
    if let Some(min) = opts.min_file_bytes {
        if size < min {
            return;
        }
    }
    plan.src_rels.insert(rel.to_string());
    let action = if opts.hard_links {
        let key = (md.dev(), md.ino());
        match inode_map.get(&key) {
            Some(primary) => FileAction::HardLink(primary.clone()),
            None => {
                inode_map.insert(key, rel.to_string());
                FileAction::Copy
            }
        }
    } else {
        FileAction::Copy
    };
    plan.files.push(FilePlan {
        rel: rel.to_string(),
        src_abs: path.to_path_buf(),
        dst_abs: PathBuf::new(),
        size,
        action,
        mtime: md.modified().ok(),
        mode: md.mode(),
        uid: md.uid(),
        gid: md.gid(),
    });
}

/// Fill in destination paths and decide copy/skip per file.
pub fn bind_destination(plan: &mut Plan, dst_root: &Path, opts: &DecideOpts) -> Result<()> {
    for d in &mut plan.dirs {
        d.dst_abs = safe_join(dst_root, &d.rel)?;
    }
    for l in &mut plan.links {
        l.dst_abs = safe_join(dst_root, &l.rel)?;
    }
    for n in &mut plan.nodes {
        n.dst_abs = safe_join(dst_root, &n.rel)?;
    }
    for f in &mut plan.files {
        f.dst_abs = safe_join(dst_root, &f.rel)?;
        decide_file(f, opts);
        match &f.action {
            FileAction::Copy | FileAction::VerifyChecksum | FileAction::Backup => {
                plan.total_copy_bytes += f.size;
            }
            FileAction::LinkDest(_) | FileAction::HardLink(_) => {}
            FileAction::Skip(reason) => {
                plan.skipped += 1;
                if *reason == "destination is not a regular file" {
                    plan.conflicts += 1;
                }
            }
        }
    }
    Ok(())
}

/// Decide the action for one file (mutates `f.action`).
pub fn decide_file(f: &mut FilePlan, opts: &DecideOpts) {
    // Start from the scan-time action (Copy / HardLink).
    let base_hard_link = match &f.action {
        FileAction::HardLink(rel) => Some(rel.clone()),
        _ => None,
    };

    let dst_md = match fs::symlink_metadata(&f.dst_abs) {
        Ok(md) => Some(md),
        Err(_) => None,
    };

    match dst_md {
        None => {
            // Destination missing: link-dest opportunity first.
            if let Some(link_dir) = &opts.link_dest {
                let cand = link_dir.join(&f.rel);
                if let Some(action) = link_dest_action(f, &cand, opts) {
                    f.action = action;
                    return;
                }
            }
            f.action = FileAction::Copy;
        }
        Some(md) => {
            if !md.is_file() {
                f.action = FileAction::Skip("destination is not a regular file");
                return;
            }
            if opts.ignore_times {
                f.action = FileAction::Copy;
                return;
            }
            if opts.checksum || opts.compare == CompareDecision::Checksum {
                // `-c`: the checksum is the authority — the size/time
                // heuristic below is bypassed entirely.
                f.action = if md.len() == f.size {
                    FileAction::VerifyChecksum
                } else {
                    FileAction::Copy
                };
                return;
            }
            if !needs_copy(f, &md, opts) {
                f.action = FileAction::Skip("up to date");
                return;
            }
            // `--update`: never overwrite a destination that is newer.
            if opts.update {
                if let (Some(src_t), Some(dst_t)) = (f.mtime, md.modified().ok()) {
                    if dst_t > src_t {
                        f.action = FileAction::Skip("newer at destination");
                        return;
                    }
                }
            }
            if opts.backup {
                f.action = FileAction::Backup;
            } else {
                f.action = FileAction::Copy;
            }
        }
    }

    // Restore hard-link intent (the destination may already exist).
    if let Some(rel) = base_hard_link {
        if matches!(f.action, FileAction::Copy) {
            f.action = FileAction::HardLink(rel);
        }
    }
}

/// Decide whether the file content differs from the destination, according
/// to the configured comparison mode.
fn needs_copy(f: &FilePlan, dst_md: &fs::Metadata, opts: &DecideOpts) -> bool {
    let size_diff = dst_md.len() != f.size;
    let window = if opts.modify_window < 0 { 0 } else { opts.modify_window as u64 };
    let time_diff = match (f.mtime, dst_md.modified().ok()) {
        (Some(s), Some(d)) => s
            .duration_since(d)
            .or_else(|_| d.duration_since(s))
            .map(|diff| diff.as_secs() > window)
            .unwrap_or(true),
        _ => true,
    };
    if opts.size_only || opts.compare == CompareDecision::SizeOnly {
        return size_diff;
    }
    if opts.compare == CompareDecision::TimeSize {
        return time_diff || size_diff;
    }
    // rsync default: size first, then mtime within the modify window.
    if size_diff {
        return true;
    }
    time_diff
}

/// Check whether `--link-dest` offers an identical file to hard-link.
fn link_dest_action(
    f: &FilePlan,
    candidate: &Path,
    opts: &DecideOpts,
) -> Option<FileAction> {
    let Ok(cand_md) = fs::symlink_metadata(candidate) else {
        return None;
    };
    if !cand_md.is_file() {
        return None;
    }
    if cand_md.len() != f.size {
        return None;
    }
    if opts.checksum || opts.compare == CompareDecision::Checksum {
        let Ok(a) = crate::fsutil::file_sha256(&f.src_abs) else {
            return None;
        };
        let Ok(b) = crate::fsutil::file_sha256(candidate) else {
            return None;
        };
        if a == b {
            return Some(FileAction::LinkDest(candidate.to_path_buf()));
        }
        None
    } else if let (Some(s), Some(d)) = (f.mtime, cand_md.modified().ok()) {
        if s == d {
            return Some(FileAction::LinkDest(candidate.to_path_buf()));
        }
        None
    } else {
        Some(FileAction::LinkDest(candidate.to_path_buf()))
    }
}

/// Plan `--delete`: which destination files and (bottom-up) directories to
/// remove. Destination entries protected by exclude rules are never removed
/// (rsync behaviour).
pub struct DeletePlan {
    pub files: Vec<PathBuf>,
    /// Directories, deepest first.
    pub dirs: Vec<PathBuf>,
}

pub fn compute_deletes(
    dst_root: &Path,
    src_rels: &HashSet<String>,
    filters: &FilterSet,
    delete: bool,
    prune_empty: bool,
) -> Result<DeletePlan> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    walk_dest(dst_root, "", &mut files, &mut dirs)?;

    let mut delete_files = Vec::new();
    for (rel, abs) in &files {
        if src_rels.contains(rel.as_str()) {
            continue;
        }
        if filters.is_protected(rel) {
            continue;
        }
        delete_files.push(abs.clone());
    }

    // Dirs: a directory survives if it is present in the source tree or if
    // at least one file under it survives. Process deepest first.
    let mut dir_rels: Vec<(String, PathBuf)> = dirs
        .into_iter()
        .map(|(r, a)| (r, a))
        .collect();
    dir_rels.sort_by(|a, b| b.0.rmatches('/').count().cmp(&a.0.rmatches('/').count()));

    let deleted_files: HashSet<&Path> = delete_files.iter().map(|p| p.as_path()).collect();
    let mut delete_dirs = Vec::new();
    for (rel, abs) in &dir_rels {
        if src_rels.contains(rel.as_str()) {
            continue;
        }
        if filters.is_protected(rel) {
            continue;
        }
        // Surviving files under this directory?
        let surviving = files.iter().any(|(frel, fabs)| {
            frel.as_str().starts_with(&format!("{rel}/")) && !deleted_files.contains(fabs.as_path())
        });
        if surviving {
            continue;
        }
        // Empty at destination, or only holding files we are deleting.
        if delete || prune_empty {
            delete_dirs.push(abs.clone());
        }
    }

    Ok(DeletePlan {
        files: delete_files,
        dirs: delete_dirs,
    })
}

fn walk_dest(
    dir: &Path,
    rel_prefix: &str,
    files: &mut Vec<(String, PathBuf)>,
    dirs: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let mut entries: Vec<fs::DirEntry> = match fs::read_dir(dir) {
        Ok(it) => it.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(()),
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel = if rel_prefix.is_empty() {
            name
        } else {
            format!("{rel_prefix}{name}")
        };
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if md.file_type().is_dir() {
            dirs.push((rel.clone(), path.clone()));
            walk_dest(&path, &format!("{rel}/"), files, dirs)?;
        } else {
            files.push((rel, path));
        }
    }
    Ok(())
}
