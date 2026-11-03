# Performance baseline (first, this machine)

Two arms on **the same physical disk**: arm A is the layerfs mount, arm B is the host directory holding the store. The clock covers **the workload only** —
`init`, mount and teardown are outside it (see "Timing boundary" below).

No thresholds: these numbers are a report. A scenario that *fails* aborts the run; a number that is merely large is data.

## Environment

- harness: `cargo run --release --example perfbench -- --a <mount> --b <host dir> --runs N [--scale F] [--scenarios …]`
- runs per scenario: 1 (a warm-up run per scenario is done and not reported)
- scale 0.02 and 0.1 (fractions of the card's counts; shapes unchanged)
- machine: the development host (SATA SSD, F: volume); Windows 11, WinFsp 2.1.25156

## Timing boundary (structural, not measured)

The harness process starts **after** the volume is already serving, and it never mounts or unmounts anything. Every number below is therefore a workload number
by construction: there is no code path in which mount time could be included.

The mount duration itself is **not reported**: every probe available to the harness is a filesystem operation, and a filesystem probe issued while the letter
exists but the volume is not serving yet **blocks** (measured three ways: `ls` polling 56 s, `timeout 1 cmd /c dir` 61.6 s, a bounded loop that never returned).
A probe that hangs measures the harness, not the mount. The fix is a non-blocking signal over the control pipe (a `serving` flag), which is a protocol change and
belongs to a later card if anyone needs the number.

## Numbers — scale 0.02 (2% of the card's counts)

| scenario | load | A | B | A/B |
|---|---|---|---|---|
| S1 write 64 MiB (zeroes) | 64 MiB | 72.4 MiB/s | 1177.4 MiB/s | 16× |
| S1 read 64 MiB (zeroes) | 64 MiB | 448.3 MiB/s | 2487.9 MiB/s | 5.5× |
| S2 write 64 MiB (random) | 64 MiB | 89.5 MiB/s | 1682.0 MiB/s | 19× |
| S2 read 64 MiB (random) | 64 MiB | 522.2 MiB/s | 2924.4 MiB/s | 5.6× |
| S3 random 4 KiB read | 64 MiB file | 2651.9 op/s (0.377 ms) | 38266.5 op/s (0.026 ms) | 14× |
| S4 random 4 KiB write | 64 MiB file | 561.9 op/s (1.78 ms) | 10818.4 op/s (0.09 ms) | 19× |
| S5 create | 1000 files, 3 deep | 72.4 op/s (13.5 ms) | 1973.0 op/s (0.45 ms) | 27× |
| S5 stat | 1000 files | 830.1 op/s (0.274 ms) | 36812.1 op/s (0.023 ms) | 44× |
| S5 list | 1000 files | 243.7 op/s (4.1 ms) | 10030.1 op/s (0.1 ms) | 41× |
| S5 delete | 1000 files | 371.7 op/s (2.6 ms) | 5849.2 op/s (0.18 ms) | 16× |
| S6 build | 100 dirs × 100 files | 122.2 op/s (8.7 ms) | 1765.9 op/s (0.56 ms) | 14× |
| S6 walk | 100 dirs × 100 files | 80.2 op/s (12.5 ms) | 1721.2 op/s (0.58 ms) | 21× |
| S7 create | 5000 × 1 KiB | 90.4 op/s (10.8 ms) | 1971.3 op/s (0.44 ms) | 22× |
| S7 read | 5000 × 1 KiB | 1794.8 op/s (0.35 ms) | 29213.3 op/s (0.03 ms) | 16× |
| S7 delete | 5000 × 1 KiB | 290.1 op/s (2.95 ms) | 4497.0 op/s (0.19 ms) | 16× |
| S8 copy 64 MiB | 64 MiB | 24.5 MiB/s | 570.4 MiB/s | 23× |
| S9 overwrite 10% of 64 MiB | 6.4 MiB in place | 62.8 MiB/s | 985.4 MiB/s | 16× |

## Numbers — scale 0.1 (trend check)

| scenario | A | B | A/B |
|---|---|---|---|
| S1 write 64 MiB (zeroes) | 7.4 op/s (135 ms) | 354.4 op/s (2.8 ms) | 48× |
| S5 create | 81.1 op/s (11.4 ms) | 1550.5 op/s (0.57 ms) | 19× |

The two scales agree in order of magnitude, so the cost is **not** an artefact of the small counts: it scales with the work.

## Attribution appendix — where the 13.5 ms of a create goes

Every line below is a measurement on this machine, taken by narrowing the suspect one layer at a time. Instruments were `LAYERFS_DEBUG_SEG` (env-gated) and
throwaway variants that were reverted before anything was committed.

| step | question | reading | conclusion |
|---|---|---|---|
| 1 | is SQLite's per-transaction cost the answer? | one commit per row **0.009 ms** (NORMAL), 1.27 ms (FULL), measured with the product's own pragmas | no — three orders of magnitude too small |
| 2 | is `synchronous = FULL` the answer? | switching the main DB to NORMAL: create **13.5 → 9.77 ms** | it is 3.7 ms of it, not all |
| 3 | is the content path (hash + store write) the answer? | 0-byte create **7.54 ms** vs 1 KiB create **9.38 ms** | content is ~1.8 ms; ~80% is per-operation machinery |
| 4 | is the open path (resolve, context, handle) the answer? | open an existing file **0.185 ms** | no — 46× cheaper than a create |
| 5 | is the overlay transaction machinery the answer? | `guard=0 ns lock=200 ns begin=1.1 µs commit=130 µs`, **body 7.47 ms** | no — the cost is inside the body |
| 6 | where inside the body? | `lookup 3.07 ms`, `inode 0`, `create_inode 78 µs`, **`bind 4.63 ms`** | both ends of the body are **parent-directory entry** work |

**Total:** `13.5 ms ≈ 3.7 (FULL sync) + 3.1 (lookup) + 4.6 (bind) + 1.8 (content) + 0.13 (transaction)`.

The shape of that result matters more than the numbers: the cost is not hashing, not transactions and not FFI — it is **reading and writing the parent
directory's entry block on every operation**, with the parent living in the base snapshot. A create is not "one slow thing"; it is a handful of fixed steps
whose fixed steps are individually cheap and whose parent-directory steps are not.

An intermediate reading that pointed the other way is recorded here because it was wrong and instructive: an early hypothesis was "one shell redirect is amplified
into a dozen transactions, ~0.7 ms each". The transaction measurement (step 5) refuted it.

## Gaps (honest list)

1. **Scale.** 0.02 and 0.1 only; the card's full counts (1.0) were not run — S5/S6/S7 through the mount are slow enough that a full run did not finish within
   20 minutes on the first attempt. The two scales that were run agree, so the trend is known even though the absolute full-scale numbers are not.
2. **Mount time.** Not reported, with the structural reason given above.
3. **S10 (`git init` + 100 files + commit) is absent**: `git init` fails on a layerfs mount —
   `error: could not write config file Y:/gw/.git/config: No such file or directory` (exit 128). That is a **compatibility finding**, not a harness problem,
   and it is tracked as its own card.

## What the harness cannot tell you

- It cannot verify that the two arms share a physical disk; it prints both paths and says so.
- It does not measure cache-cold behaviour: every run is warm by the time it is measured (each scenario writes its own data first).
- It reports one run in the tables above; `--runs N` repeats the whole set and reports the last, and a re-run more than 10× away means the instrument is broken
  rather than the product being slow.
