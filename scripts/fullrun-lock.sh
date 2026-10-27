#!/bin/sh
# Serialise full test runs on this machine (P3-4 / B-17).
#
# **Why a lock instead of attribution**: the 1546 family (`SQLITE_IOERR_TRUNCATE`) only appears when two full runs overlap, so
# "was QA replaying while I ran?" turned every red into a timing question nobody could answer afterwards. Mutual exclusion
# removes the question — a run either owns the machine or waits, and the file records who held it.
#
# Usage:
#   scripts/fullrun-lock.sh run -- cargo nextest run --release --workspace --no-fail-fast
#   scripts/fullrun-lock.sh status
#
# Rules, as code:
# - **fresh** lock (< 2 h) ⇒ wait, never break it in;
# - **missing** lock ⇒ take it and run;
# - **stale** lock (> 2 h) ⇒ record the takeover and proceed — a crashed run must not block the machine forever, and the takeover
#   is printed so "somebody broke in" is never silent;
# - the lock is removed on exit **and** on interrupt, so a Ctrl-C does not leave the fleet waiting.
set -eu

LOCK="${LAYERFS_LOCK:-F:/work/.layerfs-fullrun.lock}"
STALE_SECS=7200

say() { printf '%s\n' "$*" >&2; }

owner() {
    [ -f "$LOCK" ] && head -n 3 "$LOCK" 2>/dev/null | tr '\n' ' ' || true
}

age_secs() {
    # Age in seconds from the lock's own timestamp line (portable: the content is written by this script).
    [ -f "$LOCK" ] || { echo 0; return; }
    stamp=$(sed -n 's/^time=//p' "$LOCK" 2>/dev/null | head -n 1)
    [ -n "${stamp:-}" ] || { echo 0; return; }
    now=$(date +%s)
    echo $((now - stamp))
}

take() {
    {
        echo "owner=${LAYERFS_OWNER:-$(whoami 2>/dev/null || echo unknown)}"
        echo "command=$*"
        echo "time=$(date +%s)"
        echo "started=$(date 2>/dev/null || echo unknown)"
    } >"$LOCK"
}

case "${1:-}" in
status)
    if [ -f "$LOCK" ]; then
        say "locked: $(owner) age=$(age_secs)s"
    else
        say "free"
    fi
    exit 0
    ;;
run)
    shift
    [ "${1:-}" = "--" ] && shift
    ;;
*)
    say "usage: $0 run -- <command...> | $0 status"
    exit 2
    ;;
esac

# Wait for a fresh holder; take over only a stale one.
waited=0
while [ -f "$LOCK" ]; do
    age=$(age_secs)
    if [ "$age" -gt "$STALE_SECS" ]; then
        say "lock stale (${age}s > ${STALE_SECS}s): taking over from [$(owner)] — recorded, not silent"
        break
    fi
    if [ "$waited" -ge 1800 ]; then
        say "waited 30 min for [$(owner)]; giving up rather than running concurrently (that is the whole point of the lock)"
        exit 3
    fi
    [ "$waited" -eq 0 ] && say "waiting for [$(owner)]"
    sleep 10
    waited=$((waited + 10))
done

take "$@"
# Remove on ANY exit path, including a failed or interrupted run.
trap 'rm -f "$LOCK"' EXIT INT TERM HUP
say "lock acquired: $(owner)"

"$@"
