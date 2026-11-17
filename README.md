# psync

`rsync`, but with **parallel transfer**, **live progress bars**, and
**pybackupsync-compatible YAML config files** — written in Rust, with
`#![forbid(unsafe_code)]`.

```
$ psync -av --stats -j 4 src/ dst/
Starting Copy. Total data: 195.4 KB

dst/ 195.4KB [███████████████████████████▏] 195.4KB/195.4KB (100%) 3.2MiB/s
Copy operation finished after 0.3s.
Number of files transferred: 10
Total transferred file size: 195.4 KB
```

Bars use your glyph style: `progress_chars("█▉▊▋▌▍▎▏ ")` with
`tick_chars(".sSs.")` for spinners (see `progress.rs`).

## Features

- **rsync-compatible CLI** (`SRC... DST`, `-a`, `-c`, `-n`, `-v`, `--delete`,
  `--exclude`, `--checksum`, `--bwlimit`, filters, sizes, depth, backup,
  link-dest, …) — see `psync --help` for the full surface.
- **Parallel copies**: `-j/--jobs N` worker threads share one job queue; a
  single token-bucket pacer enforces the `--bwlimit` *total* rate, like rsync.
- **Live progress** via `indicatif`: one bar per destination, file/byte
  counts, human-readable stats (`-h`), `--stats` summary, `--itemize-changes`
  (`*aiuc`-style lines), `--log-file`.
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
- Devices/FIFOs need root; otherwise they are skipped with a warning.

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
  progress.rs    indicatif bars/spinners
  report.rs      itemize lines + stats summary
  cancel.rs      SIGINT/SIGTERM flag (signal-hook, async-signal safe)
  space.rs       free-space vs. needed-space logic
  error.rs       thiserror error chain + exit-code mapping
```

## License

MIT
