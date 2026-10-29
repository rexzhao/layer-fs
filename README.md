# layerfs

A workspace CAS filesystem for Windows: your files live in a content-addressed store, and a user-mode
file system (WinFsp) serves them at a drive letter or a directory. `commit` publishes what you have,
`restore` puts an older tree back, and `checkout` switches which workspace is being served — all while
programs read and write the volume the ordinary way.

This is the user guide. Architecture and the reasoning behind the design live in `docs/` (`docs/adr`,
`docs/reports`).

---

## What it is (and is not)

- **Is**: a local, single-machine file system whose history is content-addressed; every commit is a tree
  you can go back to, and a workspace is where uncommitted edits sit.
- **Is not**: a network share, a backup tool, or a replacement for `git`. It does not merge; two branches
  diverging is a situation you resolve by hand.
- **Files are read and written through the mount.** The CLI has no file verbs at all, so there is nothing to
  learn beyond the lifecycle commands below.

## Requirements

- **Windows 10/11**, 64-bit.
- **WinFsp** installed (the build checks the DLL version against the ABI layout it was measured on; a
  mismatch is refused instead of misbehaving). Standard user is enough — no administrator rights.
- Rust toolchain to build.

## Build

```powershell
cargo build --release
```

The binary is `target\release\fs-cli.exe`. The examples below call it `layerfs`; rename the file or add
its directory to `PATH` if you prefer that name.

## Quick start

Every line below was run on a real machine before it was written here.

```powershell
layerfs init F:\repo                     # create a store; creates the `main` branch and workspace
layerfs --store F:\repo mount Z: --workspace main
                                         # serve `main` at Z: (Ctrl-C in that window = clean unmount)
dir Z:\                                   # use it like any other drive
echo hello > Z:\hello.txt
type Z:\hello.txt                         # hello

layerfs --store F:\repo branch feat --from main
layerfs --store F:\repo checkout feat     # creates the `feat` workspace (first use; not a switch)
                                         # exit 0, prints `on feat (workspace feat)`

cd Z:\
layerfs --store F:\repo commit -m "first"  # publish what the mount is serving
layerfs status                             # run inside the mount: no --workspace needed
layerfs --store F:\repo checkout feat      # run inside the mount: **switches** the mount to feat
cd %USERPROFILE%                           # leave the volume before asking it to stop
layerfs umount Z:                          # if it reports open handles, see "Stopping a mount"
```

Two things in that sequence are easy to trip over, and both are deliberate:

- **the first `checkout <branch>` creates the workspace** (it does not switch anything); from then on,
  running it from inside a mount switches that mount;
- **a command that needs a workspace and is not inside a mount must be told one**
  (`--workspace <name>`), because there is no ambient "current workspace" any more.

## Mount points: drive letter or directory

| | Drive letter (`Z:`) | Directory (`F:\mnt`) |
|---|---|---|
| Created by | WinFsp picks the volume | WinFsp creates the directory |
| Path discovery inside it | volume label is `layerfs` | detected by the mount point being a volume mount point |
| Notes | the letter must be free | the **parent** must exist, the directory itself must **not** |

Both behave the same once mounted. A directory mount point is visible in the host volume, so "the
directory exists" is *not* a sign the volume is up — `layerfs list` or a successful write is.

## What blocks a `restore` or a `checkout` (a switch)

- **Open file handles block it.** You get exit code 4 and a message that counts **files** separately from
  directories; nothing is changed, and the view is exactly what it was. Close the files and try again.
- **Directory handles do not block it.** A shell whose working directory is inside the mount holds one, so
  counting those would make switching impossible. You get a number and a hint instead:
  `N directory handles (…) now point at the old view; run `cd .` in them to refresh`.
- **The switch takes about 30 ms**, during which the mount point is not usable. The control channel
  (which `status`/`commit`/`restore` use inside a mount) is **serial**: it answers again as soon as the
  switch is done, but not during it. That is the honest answer — the mount really is in transition.
- **Your shell does not break.** `bash`, `cmd` and PowerShell started inside the old volume keep running;
  their next command may see the old view until `cd .` refreshes it.
- **If the new volume cannot come up, the switch rolls back**: exit 1, the old workspace is serving again,
  and what you wrote before the switch is still there.

## Stopping a mount

- `layerfs umount Z:` — asks the mount to unmount. It **refuses (exit 4, with a count) while any handle is
  open, including a directory handle a shell holds because its working directory is inside the mount**.
  Close those and ask again.
- **A small count can remain right after reading or writing through the mount** (measured on this build:
  `still has 1 open handle(s)`, even with no shell inside the volume). The way out is the mount's own
  console: **Ctrl-C in the window running `mount`** stops it without waiting for handles, because the user
  in front of it asked for that.
- `layerfs umount Z: --force` — clears a **stale record** (a mount whose process died). It says so plainly:
  the record is gone, the volume may still be attached.
- **Ctrl-C** in the mount's console is an orderly stop: flush, unmount, remove the record.

## Troubleshooting

| What you see | What it means | Way out |
|---|---|---|
| `0xC0000035` from `mount` | the drive letter is taken, often by a dead mount's leftover | `layerfs umount <point> --force`, or pick another letter |
| `0xC0000022` from `mount` | the slot is a real volume (a partition or an optical drive) | use a free letter, or a directory mount point |
| `database is locked` (exit 4) | another process is using that workspace | close it, or `checkout` a different branch |
| `workspace X is mounted at Y (pid N)` (exit 4) | the workspace is being served right now | use that mount, or unmount it first |
| `no workspace specified` | the command is outside a mount and has no `--workspace` | add `--workspace <name>` or `cd` into a mount |
| `mount needs a store: add --store <dir>` (exit 2) | `mount` was given no repository (it is not implied by the current directory) | add `--store`, or run it from inside a mount |
| `refusing to mount: winfsp … was not the layout …` | the installed WinFsp differs from the measured ABI | install the version named in the message, or `--no-version-gate` if you know what you are doing |

Mount records live in `%LOCALAPPDATA%\layerfs\mounts\` (override with `LAYERFS_MOUNTS_DIR`). A record is
held open by its mount and removed when the process dies, so a leftover record means a power cut —
`umount --force` is the way out.

## Commands at a glance

| Command | What it does |
|---|---|
| `init <path>` | create a store, a scope and the `main` branch |
| `branch [<name>] [--from <src>]` | list branches, or create one |
| `branch delete <name>` | delete a branch (its history stays a GC root) |
| `checkout <branch>` | create or resume the workspace on a branch; inside a mount, **switch** it |
| `workspace discard <name>` | drop a workspace's overlay and row |
| `commit -m <message> [--force]` | publish the workspace as a commit |
| `status` | workspace, branch, base commit, view root, dirty state |
| `log [branch] [limit]` | commit history |
| `reset <commit> [--expect <c>]` | move the branch head |
| `restore <commit> [--discard]` | replace the view with a commit's tree |
| `revert` | undo the head with a new commit |
| `verify [--deep]` | check objects and references |
| `stats` | object and repository counts |
| `gc [--dry-run]` | collect unreachable objects (offline; refuses while a writer holds the store) |
| `snapshot [list \| create \| drop <root>]` | keep a tree alive without moving a branch |
| `mount <point> [--workspace <n>] [--no-version-gate]` | serve a workspace over WinFsp |
| `umount <point> [--force]` | ask a mount to stop (or clear a stale record) |
| `list` | the recorded mounts |
| `version` | the WinFsp DLL version versus the measured layout |

`layerfs --help` is the single source of truth for this table. `layerfs help <command>` describes one
command's arguments and the notes that matter for it (what blocks a `restore`, what a switch does to open
handles, and so on).

## Exit codes

```
0  success
2  usage       (the command line itself is wrong)
3  not found   (store / workspace / branch / commit)
4  busy        (in use, another process holds it, handles still open)
5  conflict    (branch moved, nothing to commit, already exists)
1  anything else
```

## Known limitations

- **Several mounts in one session can go half-dead** (a known family, tracked in
  `docs/reports/known-issues.md`); a fresh `layerfs` invocation is the workaround.
- **No merge**: diverging branches are resolved by hand.
- **No change notification**: another process's write can take until you re-open to become visible in
  some tools.
- **One mount per process**: the CLI serves a single mount and exits when it stops.

## Where to read more

- `docs/adr/ADR-010-cli-without-current-workspace.md` — why the CLI looks the way it does.
- `docs/adr/ADR-008-pack-gc.md` — the store's pack format and garbage collection.
- `docs/reports/` — what was measured, and what is known to be broken.
