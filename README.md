# Terminal Time Machine

**A tiny, safe filesystem time machine for Linux.**

`tt` takes instant, content-addressed snapshots of any directory. Change a file, delete a folder, regret everything: inspect the difference, verify the archive, and restore exactly what existed before.

No daemon. No database. No cloud account. No dependencies beyond Rust's standard library.

```text
$ tt --store /media/backup/tt snapshot ~/Projects/nebula
Created snap-1760112000-123456789-4242  18 directories, 62 files, 4.8 MiB

$ tt --store /media/backup/tt diff snap-old snap-new
~ src/engine.rs
+ docs/benchmark.md
- scripts/deploy.sh
1 added, 1 changed, 1 removed

$ tt --store /media/backup/tt verify
Verified 2 snapshot(s), 124 files, 97 unique objects, 9.3 MiB
```

## Why It Exists

Backups should be boring until the moment they save you. Terminal Time Machine is for developers and Linux users who want a fast checkpoint before a risky refactor, dependency upgrade, cleanup, or script runs wild.

It stores every file by its SHA-256 hash. The second snapshot only writes content that has actually changed, so repeated checkpoints stay small while every snapshot remains independently restorable.

## Features

- Content-addressed storage with SHA-256 deduplication.
- Snapshots regular files, empty directories, permissions, and modification times.
- File-level diffs between any two snapshots.
- Integrity verification for all snapshots or one selected snapshot.
- Restore only to an explicit destination, never over the source directory.
- Refuses existing files by default and refuses to traverse symlinked destination directories.
- Human-readable manifests that preserve non-UTF-8 Linux filenames safely.
- One Rust binary with no third-party crates.

## Install

Requirements: Linux and Rust 1.99 or later.

```bash
git clone <repository-url>
cd terminal-time-machine
cargo install --path .
```

The executable is `tt`. Confirm the installed release with `tt --version`.

## Quick Start

```bash
# Make a checkpoint. The default store is ~/.local/share/terminal-time-machine.
tt snapshot ~/Documents

# Use a separate store for a portable backup disk.
tt --store /media/backup-drive/tt snapshot ~/Documents

# Find the snapshot ID and inspect its contents.
tt list
tt show snap-1760112000-123456789-4242

# Compare two points in time. + added, ~ changed, - removed.
tt diff snap-1760112000-123456789-4242 snap-1760200000-987654321-4242

# Verify every object has the expected size and SHA-256 digest.
tt verify
tt verify snap-1760112000-123456789-4242

# Restore safely into a new directory.
tt restore snap-1760112000-123456789-4242 --to ~/RecoveredDocuments

# Replace existing regular files only when you explicitly mean it.
tt restore snap-1760112000-123456789-4242 --to ~/RecoveredDocuments --force
```

Run `tt --help` for the complete command reference.

## How It Works

The snapshot store has three directories:

```text
terminal-time-machine/
  objects/    SHA-256-addressed file contents, written once
  snapshots/  versioned, human-readable snapshot manifests
  tmp/        short-lived files used for atomic writes
```

Files with the same content share one object, even across different snapshots. Manifests record the original layout, file modes, directory modes, and modification times. Manifests written by version 1 of `tt` remain readable; version 2 adds empty-directory preservation.

By default, the store is `$XDG_DATA_HOME/terminal-time-machine`, falling back to `$HOME/.local/share/terminal-time-machine`. Pass `--store <directory>` before any command to choose another location. Listing or verifying an unused store does not create it; the store is created only when taking the first snapshot.

## Safety Model

- Restore always needs `--to <directory>`; it never modifies the original source automatically.
- Existing destination files stop a restore unless `--force` is given. Even then, only regular files can be replaced.
- Every manifest path is validated. Absolute paths, `..`, and paths through symlinked destination directories are rejected.
- A source directory that contains the snapshot store is rejected to prevent recursive snapshots.
- `verify` checks every referenced object's size and SHA-256 hash before you need a restore.

`tt` intentionally skips symlinks, device files, sockets, FIFOs, ownership, ACLs, extended attributes, and hard-link relationships. It is a safe point-in-time copy tool, not a replacement for an encrypted, off-machine backup strategy.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs these checks on every push and pull request. See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidance and [SECURITY.md](SECURITY.md) for reporting security issues.

## License

[MIT](LICENSE)
