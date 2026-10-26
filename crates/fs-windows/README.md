# `fs-windows` — the driver-independent Windows model (P3-1, L1)

No FFI, no WinFsp, no `cfg(windows)`-only dependency: the whole crate is a **model** of the Windows semantics the
P3 frontend must reproduce, so `cargo test -p fs-windows` runs on any platform and in CI without an SDK.

It **does** depend on `fs-namespace`, for one thing only: the disposition table and the failure vocabulary it
produces are shared with the core (P3-2 §12). The core is what performs the action, the frontend is what classifies
the request, and both must use the same eighteen cells — so the table lives there and is re-exported here
(`fs-windows::disposition`), not copied.

Every module's knowledge is a **table with a missing-cell-is-red mechanism**, and each table's cardinality is
asserted against a **literal** (not against another enum's length, which would move with it):

| module | table | cardinality assertion |
| --- | --- | --- |
| `path` | one row per rule, accept + reject case | `tests/path.rs`: `Rule::ALL.len() == 10`, `TABLE.len() == 10`, `MAX_COMPONENT_UTF16 == 255` |
| `disposition` | 6 dispositions × 3 existence states | `tests/disposition.rs`: `ALL.len() == 6`, `ALL.len() == 3`, rows 6, columns 3, `TABLE.len() == 18` |
| `share` | 8⁴ mask combinations | `tests/share.rs`: `cells == 8*8*8*8` |
| `delete` | 6 states × 5 events | `tests/delete.rs`: `STATES.len() == 6`, `Event::ALL.len() == 5`, `cells == 30` |
| `error` | `Failure` → `NtStatus`, exhaustive `match` | `tests/error.rs`: `ALL_FAILURES.len() == 16`, codes 17, names 17 |
| `attrs` | 32 masks × 2 kinds | `tests/attrs.rs`: `STORABLE.len() == 4` |
| `time` | anchored instants, not just round trips | `tests/time.rs`: 1970 → `116444736000000000`, `i64::MAX` → `208678456368547758` |
| `length` | ranges and truncate plans | `tests/length.rs`: overflow, EOF, zero-fill range |
| `dirent` | 1000 entries × 8 page sizes | `tests/dirent.rs`: concatenation equals the listing |
| `volume` | one source for the constants | `tests/volume.rs`: report is stable and does not follow the host disk |

No fallback arms: `tests/no_wildcards.rs` scans the crate's own sources for `_ =>` and fails on a hit (including in
comments, so `grep` stays trustworthy). The scanner is assembled at runtime and has a **positive control** that runs
the same rule over a planted text.

## Measured mutations (what "red" actually looks like)

These were run by hand, one at a time, with the file restored afterwards:

| mutation | result |
| --- | --- |
| delete one `TABLE` row in `disposition` | **compile error**: `expected array with a size of 18, found one with a size of 17` — the typed array length catches it before any test runs |
| invert one term of the share rule (`!existing_share.has` → `existing_share.has`) | **2 tests fail**, message carries the coordinates: `cell new_desired=1 new_share=0 existing_desired=0 existing_share=0` |
| drop the trailing-dot rule in `path` | **1 test fails**: `TrailingDotOrSpace reject case "\a.\b" must be refused` |
| plant `_ =>` in `volume.rs` | **`no_wildcards` fails** and prints `volume.rs:33: match value { 0 => 0, _ => 1 }` |
| make `CleanupDelete` delete without a mark (during development) | caught by the state-machine walk, which is why the falsy branch exists |

The first row is worth keeping in mind when reading the card's "deleting a row must go red": for a typed array the
red is a **compile error**, not a test failure — stronger, but a different signal.
