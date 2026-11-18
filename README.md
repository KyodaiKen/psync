# psync

`rsync`, but **local-only** — with **parallel transfer**, **live progress
bars**, and **YAML config files** — written in Rust, with
`#![forbid(unsafe_code)]`.

> **Local-only by design:** psync syncs between paths on this machine
> (disks, local or NAS mounts). It does not speak the rsync remote/daemon
> protocol and rejects `user@host::` specs. Need a remote hop? Mount it
> (NFS/SMB/SSHFS) and sync locally, or use plain rsync for that jump —
> see the compatibility notes below.

```
$ psync -av --stats -j 4 src/ dst/
Starting Copy. Total data: 195.4 KB

s photos/a.jpg   [████████████████████████████████████████] 97.7 KB/97.7 KB (100%) 1.6 MB/s ETA 0s
s videos/b.mov   [███████████████████████▊                  ] 48.8 KB/97.7 KB (50%) 1.5 MB/s ETA 0s
. total          [███████████████████████████████▋          ] 146.5 KB/195.4 KB (75%) 3.1 MB/s ETA 0s

done: the file bars clear; the final total line stays.
Number of files transferred: 10
Total transferred file size: 195.4 KB
```

Bars use your glyph style: `progress_chars("█▉▊▋▌▍▎▏ ")` with
`tick_chars(".sSs.")` for spinners (see `progress.rs`).

- **Cadence:** redraws and the spinner animate at a steady 250 ms —
  smooth, low-CPU, no erratic spinner. Rendering is a small single-threaded
  custom engine (not `indicatif`): the frame is cleared and rewritten as
  one unit, so bars never leave stale lines behind.
- **Owns the screen while it runs:** the UI works on the terminal's
  *alternate screen* — a dedicated page for the live frame. On finish,
  error, Ctrl-C or SIGTERM (and even on a panic) psync restores the
  primary screen, so finished bars never linger in your scrollback; the
  final report and error messages print on your normal terminal.
- **Modern colors:** 256-color styling — file bars fill with a blue→green
  ramp, the grand total bar uses a brighter cyan ramp, names and messages
  are dimmed for focus. Color turns off automatically when `NO_COLOR` is
  set or `TERM=dumb`.
- **Fits your terminal:** file names are center-ellipsized (`mid…dle`)
  so every line stays strictly inside the terminal width; on narrow
  terminals the speed drops out of the line. `--width COLS` overrides the
  detected width (for testing).
- **Perfectly aligned columns:** the name and message column widths are
  computed once from the *whole* planned file set, so every job's line has
  identical column widths and the ellipsis fits exactly.
- **Config mode names the destination:** with several destinations,
  each progress line is shown as `dest-root/relative/path` (the prefix
  shared by all destinations of the run is stripped), so you always see
  which copy is which — e.g. `dest-a/data/archive.tar.gz` vs
  `dest-b/mirror/data/archive.tar.gz`. rsync mode keeps the classic
  source-relative names.
- **Units:** sizes default to the classic KB = 1024 B convention;
  `--si-units` switches to strict SI (kB = 1000 B) everywhere.
- **One bar per active file** (at most `-j` at a time) plus one grand
  total bar drawn at the *bottom* of the frame; skipped/hard-linked files
  count into the total instantly, so an up-to-date run shows no bars at
  all.

## Features

- **rsync-compatible CLI** (`SRC... DST`, `-a`, `-c`, `-n`, `-v`, `--delete`,
  `--exclude`, `--checksum`, `--bwlimit`, filters, sizes, depth, backup,
  link-dest, …) — see `psync --help` for the full surface.
- **Parallel copies**: `-j/--jobs N` worker threads share one job queue; a
  single token-bucket pacer enforces the `--bwlimit` *total* rate, like rsync.
- **Live progress** (custom single-threaded renderer): one bar per active
  file (up to `-j` at a time) + a grand total bar at the bottom of the
  frame, 256-color styling (auto-off with `NO_COLOR`), perfectly aligned
  name/message columns, human-readable sizes/speeds, 250 ms smooth
  cadence, `--si-units`, `--stats` summary, `--itemize-changes`
  (`*aiuc`-style lines), `--log-file`. The UI works on the alternate
  screen and hands the terminal back on every exit path. Warnings and
  phase messages coexist with the live frame as persistent lines above it.
- **Config mode**: `psync -C config.yaml` — drop-in replacement for
  **pybackupsync**: UUID-based drive mounting, per-destination `path`,
  `priority`, `compare` (size/sizeTime/timeSize/checksum), deletion policies
  with exclude patterns, free-space checks before copying, fstrim hooks and
  auto-unmount prompt.
- **Correctness**: full metadata preservation (perms, mtime, owner when
  root, symlinks as symlinks, devices/FIFOs with root), empty directory
  recreation, `--delete` that honors exclude protection, `--update`,
  `--backup`/`--suffix`, `--link-dest` hard-linking, per-file SHA-256
  verification under `--checksum`.
- **Safety**: zero `unsafe` code, no shell is ever spawned for user data
  paths (`mkfifo`/`mknod` aside, which need root), `safe_join` rejects path
  traversal, `Ctrl-C` (SIGINT/SIGTERM) aborts cleanly and leaves no partial
  files unless `-P`.

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build --release
# binary: target/release/psync
```

## Usage

### rsync mode

```sh
psync [OPTIONS] SRC... DST
```

Examples:

```sh
# Archive-mode sync with parallelism, dry run first:
psync -avn --stats src/ dst/

# Real run: 4 workers, 10 MiB/s total cap, stats at the end:
psync -av -j 4 --bwlimit 10240 src/ dst/

# Mirror the source (delete extras), protect docs/:
psync -av --delete --exclude='docs/' src/ dst/

# Detect silent bit-rot: compare by SHA-256:
psync -acn src/ dst/

# Skip files the destination considers newer:
psync -a -u src/ dst/
```

### config mode (pybackupsync compatible)

```sh
psync -C /path/to/backup.yaml
```

Example config (same shape as pybackupsync):

```yaml
source:
  uuid: FED7-AF89          # optional: mount by UUID; if absent,
  mount_point: /mnt/tmp    # the path is used directly and created if missing
  exclude_patterns:
    - "*.log"

destinations:
  - path: backups
    uuid: E6FF-034F
    mount_point: /mnt/hdda
    priority: 1
    compare: size           # size | sizeTime | timeSize | checksum
    deletion_policy: none   # none | delete
    deletion_exclude_patterns:
      - "*.log"

max_parallel_processes: 4
fstrim_after: true
unmount_prompt: true
```

Behavior notes:

- Destinations are synced in **priority order** (1 first), like pybackupsync.
- Before each destination the **free space** is checked against the data to
  copy; with a deletion policy the reclaimable space is taken into account.
- Drives auto-mounted by UUID are unmounted again at the end (prompt on TTY).
- `compare: checksum` runs the same SHA-256 verification as `-c`.

## Exit codes

| code | meaning                                   |
|------|-------------------------------------------|
| 0    | success (including "nothing to do")       |
| 23   | partial transfer / per-file errors, or `Ctrl-C` |
| 24   | usage, filter or config error             |

## Compatibility notes (vs. rsync)

- `-h` is **human-readable sizes** (rsync parity). `--help` is `--help`,
  `-V/--version` the version.
- Local-only: `-z`, `--timeout`, `--numeric-ids`, `-s` are accepted and
  ignored; remote `user@host::` specs are rejected.
- `--delete-during` is mapped to `--delete-before`.
- Device nodes (char/block) need root (`mknod`); without it they are
  reported as per-file errors, not silently skipped. FIFOs only need write
  access to the destination directory; restoring ownership to another user
  always needs root.

## Project layout

```
src/
  main.rs        orchestration: both modes, reporting, exit codes
  cli.rs         clap 4 CLI (rsync flag surface + --jobs/--bwlimit/-C)
  config.rs      pybackupsync YAML schema (serde_yaml)
  plan.rs        scan → decision (size/time/checksum/update/backup) → job plan
  engine.rs      worker pool, job execution, outcomes
  engine_ctx.rs  shared thread-safe state (tasks, pacer, events)
  fsutil.rs      copy engine, SHA-256, token-bucket Pacer, formatting
  filter.rs      rsync-style include/exclude rules + delete protection
  mount.rs       UUID mounting, space checks, auto-unmount
  progress.rs    custom 250 ms renderer: aligned bars + phase spinners
  report.rs      itemize lines + stats summary
  cancel.rs      SIGINT/SIGTERM flag (signal-hook, async-signal safe)
  space.rs       free-space vs. needed-space logic
  error.rs       thiserror error chain + exit-code mapping
```

## License

MIT
