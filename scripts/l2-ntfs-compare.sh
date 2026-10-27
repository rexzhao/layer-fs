#!/usr/bin/env bash
# L2 NTFS comparison probe — one script, two roots.
#
# The card's L2 acceptance compares the mounted volume against a real NTFS directory, class by class. The script takes a
# **root prefix** and runs the same twelve checks against it, so the two sides differ only in `$1`. That is what stops a
# "difference" from being a difference between two scripts.
#
# Usage:
#   scripts/l2-ntfs-compare.sh "F:/work/ntfs-ref"      # NTFS side
#   scripts/l2-ntfs-compare.sh "Z:/"                   # mounted volume (drive letter)
#   scripts/l2-ntfs-compare.sh "F:/work/mnt-dir-1/"    # mounted volume (directory)
#
# Output is one line per class: `NN name: <observation>`. Every line prints the **observation**, not a pass/fail: the
# comparison is done by diffing the two runs, and a script that decided pass/fail itself would also hide a difference in
# wording.
set -u

ROOT="${1:-}"
if [ -z "$ROOT" ]; then
  echo "usage: $0 <root>" >&2
  exit 2
fi
ROOT="${ROOT%/}"

# `[ -d ]` on a **bare drive letter** is not the same question as `[ -d ]` on that drive's root: bash (Git Bash) answers `d=1` for `Z:` and `d=0` for
# `Z:/`. The trailing separator is stripped above so paths can be built without doubling it, so the class that asks "is the root a directory" has to
# put one back — otherwise the class is red for a reason that has nothing to do with the filesystem under test (measured: `Z:` -> 1, `Z:/` -> 0).
ROOT_AS_DIR="$ROOT/"

note() { printf '%s\n' "$*"; }

# 01 — does the root exist and is it a directory (the very first thing every caller asks).
if [ -d "$ROOT_AS_DIR" ]; then note "01 root: directory"; else note "01 root: not a directory or missing"; fi

# 02 — creating a file, then reading it back: content round trip.
printf 'alpha' >"$ROOT/l2-file" 2>/dev/null
note "02 create+read: $(cat "$ROOT/l2-file" 2>/dev/null || echo '<unreadable>')"

# 03 — creating a file that already exists (NTFS: truncates with O_TRUNC, otherwise EEXIST).
: >"$ROOT/l2-file" 2>/dev/null && note "03 truncate existing: ok" || note "03 truncate existing: refused"

# 04 — opening a missing file (must be "not found", never "created").
if [ -r "$ROOT/l2-missing" ]; then note "04 missing file: readable (wrong)"; else note "04 missing file: not found"; fi

# 05 — directory creation, then listing it (ordering is the part both sides must agree on).
mkdir "$ROOT/l2-dir" 2>/dev/null
: >"$ROOT/l2-dir/b"; : >"$ROOT/l2-dir/a"
note "05a dir listing (sorted): $(ls -1 "$ROOT/l2-dir" 2>/dev/null | sort | tr '
' ',')"
note "05b dir listing (raw order): $(ls -1 "$ROOT/l2-dir" 2>/dev/null | tr '
' ',')"

# 06 — removing a non-empty directory (NTFS: refused).
rmdir "$ROOT/l2-dir" 2>/dev/null && note "06 rmdir non-empty: removed (wrong)" || note "06 rmdir non-empty: refused"

# 07 — renaming: the old name disappears, the new one answers.
mv "$ROOT/l2-file" "$ROOT/l2-renamed" 2>/dev/null
note "07 rename: old=$([ -e "$ROOT/l2-file" ] && echo present || echo gone) new=$([ -e "$ROOT/l2-renamed" ] && echo present || echo gone)"

# 08 — case-only rename on a case-preserving volume: the bytes change, the entry does not.
mv "$ROOT/l2-renamed" "$ROOT/L2-RENAMED" 2>/dev/null
note "08 case-only rename: $(ls -1 "$ROOT" 2>/dev/null | grep -i '^l2-renamed$' || echo '<missing>')"

# 09 — delete an open file (NTFS: allowed if the handle allows delete sharing; the name goes away at close).
exec 9>"$ROOT/l2-delete"
rm -f "$ROOT/l2-delete" 2>/dev/null
note "09 delete while open: name=$([ -e "$ROOT/l2-delete" ] && echo present || echo gone)"
exec 9>&-

# 10 — write while another handle reads: the reader must see the new bytes (no per-handle snapshot).
printf 'beta' >"$ROOT/L2-RENAMED" 2>/dev/null
note "10 concurrent read after write: $(cat "$ROOT/L2-RENAMED" 2>/dev/null || echo '<unreadable>')"

# 11 — a name that does not fit the component limit (NTFS: refused with a name error).
long_name="$(printf 'x%.0s' $(seq 1 300))"
: >"$ROOT/$long_name" 2>/dev/null && note "11 long name: created (check component limit)" || note "11 long name: refused"

# 12 — attributes: setting the read-only bit and reading it back.
#
# `attrib`'s output format belongs to cmd, not to the volume: the comparable part is the **semantics** (is the read-only bit
# set, is it a directory), and the raw line is recorded for the report.
attrib +R "$ROOT/L2-RENAMED" >/dev/null 2>&1
raw_attrs="$(attrib "$ROOT/L2-RENAMED" 2>/dev/null | tr -s ' ')"
readonly=no
case "$raw_attrs" in *\ R\ *|*R*) readonly=yes;; esac
directory=no
[ -d "$ROOT/L2-RENAMED" ] && directory=yes
note "12a attributes (semantic): readonly=$readonly directory=$directory"
note "12b attributes (raw): ${raw_attrs:-<unavailable>}"
attrib -R "$ROOT/L2-RENAMED" >/dev/null 2>&1

# Cleanup is deliberately *not* attempted: on the mounted side a failing cleanup would itself be an observation, and the
# NTFS side runs in a throwaway directory.
