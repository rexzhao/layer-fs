# LayerFS for Windows

A general-purpose **Windows native LayerFS CLI + WinFsp filesystem** built directly on top of the upstream [Ephemeral-AI-Lab/layerfs](https://github.com/Ephemeral-AI-Lab/layerfs) v1.0.0 core engine.

## Architecture

- **Upstream LayerFS Core (`upstream/crates/layerfs-*`)**: Reuses the upstream content-addressed storage (CAS), zstd + prefix-delta pack engine, SQLite persistence (`Durable` WAL/FULL and `Disposable` WAL/OFF), copy-on-write ephemeral overlay, streaming directory importer (`layerfs-project`), incremental commits, 3-way branch merging, and history catalog **without modifying the core storage format** (100% compatible with Linux/macOS LayerFS stores).
- **Win32 Host Shim (`upstream/crates/layerfs-win32-host`)**: Minimal safe Win32 FFI wrapper (NTFS/ReFS detection, file identity, volume free space) so upstream `layerfs-overlay`, `layerfs-store-service`, and `layerfs-project` retain `#![forbid(unsafe_code)]`.
- **Windows Bridge (`crates/fs-layerfs`)**: Bridges upstream `BoundWorkspace`, `layerfs-store-service`, and `layerfs-project` to `fs-winfsp::WorkspaceOps`, handling Windows case-insensitive / case-preserving lookup, `READONLY` ↔ POSIX write-bit mapping, safe read-only exposure of POSIX symlinks, windowed I/O, paginated directory listing, and commit publication gating.
- **WinFsp Native Adapter (`crates/fs-winfsp`, `crates/fs-windows`, `crates/fs-semantics`)**: Native Windows WinFsp filesystem callbacks, NTSTATUS mapping, FILETIME conversion, handle & delete-on-close semantics, kernel share-mode enforcement, and mount admission control.
- **CLI (`crates/fs-cli`, binary `layerfs`)**: General-purpose command-line interface with named-pipe control channel and automatic mount discovery when invoked inside a mounted directory/drive.

## Design Notes (Aligned with Upstream LayerFS)

- **Ephemeral mount overlay**: Matches upstream LayerFS semantics — uncommitted modifications live in the mount's ephemeral overlay until `layerfs commit` persists them into `store.sqlite`. Unmounting discards uncommitted overlay edits.
- **Durability profiles**: Defaults to `--durable` (SQLite WAL + `synchronous=FULL`); pass `--disposable` for upstream's high-throughput WAL + `synchronous=OFF` mode.
- **Windows semantics**: Case-insensitive / case-preserving paths, Windows share-mode exclusion (via WinFsp kernel FSD), `FILE_ATTRIBUTE_READONLY` mapped to POSIX write bits (`0o222`), and imported POSIX symlinks safely exposed as immutable target-text files (no Admin / Developer Mode required).

## Requirements

- Windows 10 / 11 x64
- [WinFsp](https://winfsp.dev/) 2.x installed (`C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll`)
- Rust toolchain (1.85+)

## Build

```powershell
cargo build --release -p fs-cli
# Binary: target\release\layerfs.exe
```

## Quick Start

```powershell
# 1. Create an empty repository, or import an existing host directory via `--from`
layerfs init F:\my-repo --durable --size 128G
layerfs init F:\imported-repo --from F:\existing-project --durable

# 2. Mount the default writable branch `work` to a drive letter or directory mount point
layerfs --store F:\my-repo mount Z: --branch work
# (or mount to a non-existent directory path, e.g. F:\mnt\work)

# 3. Use any Windows app, shell, or Git directly inside Z:\
cd Z:\
git init
echo hello > README.md
git add .
git commit -m "initial"

# 4. Inspect and commit changes inside the mount (auto-discovered from current directory)
layerfs status
layerfs commit -m "first snapshot"

# 5. Fork, branch, diff, 3-way merge, and publish to `main`
layerfs --store F:\my-repo fork feat-a --from work
layerfs --store F:\my-repo fork feat-b --from work
layerfs --store F:\my-repo branch
layerfs --store F:\my-repo merge feat-a feat-b merged -m "merge features"
layerfs --store F:\my-repo diff work merged
layerfs --store F:\my-repo publish merged

# 6. Hot-switch a live mount to another branch (from inside the mount)
layerfs checkout feat-a

# 7. Orderly unmount & optional SQLite space reclamation
cd F:\
layerfs umount Z:
layerfs --store F:\my-repo compact
```

## Commands

| Command | Description |
| --- | --- |
| `layerfs init <path> [--from <dir>] [--durable\|--disposable] [--size <size>]` | Initialize an upstream-compatible LayerFS repository (optionally importing `<dir>`) |
| `layerfs branch [<name>] [--from <src>]` | List branches or fork a new branch |
| `layerfs fork <name> [--from <src>]` | Alias for `branch <name> [--from <src>]` |
| `layerfs checkout <branch>` | Verify a branch, or hot-switch the current mount when run inside one |
| `layerfs commit [-m <msg>] [--force]` | Commit uncommitted ephemeral-overlay changes on the mounted branch |
| `layerfs merge <left> <right> <into> [-m <msg>]` | 3-way merge `<left>` and `<right>` into `<into>` |
| `layerfs publish <branch> [<commit>]` | Publish a branch commit onto `main` as the next immutable Layer |
| `layerfs diff <left> <right>` | Show changed paths between two branches (or `main`) |
| `layerfs log [<branch>] [<limit>]` | List commits on a branch |
| `layerfs status` | Show live mount status (inside mount) or branch status (offline) |
| `layerfs stats` | Repository profile, file size, and logical/canonical/stored byte counts |
| `layerfs compact [--pages <n>]` | Reclaim free SQLite pages in `store.sqlite` (upstream incremental vacuum) |
| `layerfs mount <point> [--branch <name>] [--no-version-gate]` | Mount a branch via WinFsp |
| `layerfs umount <point> [--force]` | Orderly unmount via control pipe |
| `layerfs list` | List recorded mounts and liveness |
| `layerfs version` | Check WinFsp DLL version and ABI compatibility |

## Testing

```powershell
# Core bridge + Windows semantics + WinFsp + CLI (including live WinFsp mount + git + hot-switch + stress tests)
cargo nextest run -p fs-semantics -p fs-windows -p fs-winfsp -p fs-layerfs -p fs-cli --test-threads=1

# Upstream Windows-adapted core crates
cargo nextest run --tests -p layerfs-persistence -p layerfs-store-service -p layerfs-overlay -p layerfs-workspace -p layerfs-project
```

## License

- This repository's Windows adapter and CLI crates are dual-licensed under MIT OR Apache-2.0.
- Adapted upstream LayerFS crates under `upstream/` retain their original MIT license (`upstream/LICENSE`).
