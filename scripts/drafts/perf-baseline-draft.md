# Performance baseline (this machine)

Two arms, **the same physical disk**: arm A is the layerfs mount, arm B is the host directory the store lives in.
The clock covers the workload only — `init`, mount and teardown are outside it.

## Environment

- date: 1793693719 s since the epoch (UTC seconds)
- runs per scenario: 1 (one warm-up, not reported)
- scenarios: S1, S2, S3, S4, S5, S6, S7, S8, S9, S10
- **mount time (outside the clock): 0.000 s**

## Numbers

| scenario | load | arm A throughput | arm B throughput | A p50 | A p95 | A p99 | B p50 | B p95 | B p99 |
|---|---|---|---|---|---|---|---|---|---|
| S1 write 64MiB (zeroes) | 64 MiB, zeroes | 56.5 op/s | 919.9 op/s | 17.688 ms | 17.688 ms | 17.688 ms | 1.087 ms | 1.087 ms | 1.087 ms |
| ↳ MiB/s | 64 MiB, zeroes | 72.4 | 1177.4 | | | | | | |
| S1 read 64MiB (zeroes) | 64 MiB, zeroes | 350.3 op/s | 1943.6 op/s | 2.855 ms | 2.855 ms | 2.855 ms | 0.514 ms | 0.514 ms | 0.514 ms |
| ↳ MiB/s | 64 MiB, zeroes | 448.3 | 2487.9 | | | | | | |
| S2 write 64MiB (random) | 64 MiB, pseudo-random | 69.9 op/s | 1314.1 op/s | 14.299 ms | 14.299 ms | 14.299 ms | 0.761 ms | 0.761 ms | 0.761 ms |
| ↳ MiB/s | 64 MiB, pseudo-random | 89.5 | 1682.0 | | | | | | |
| S2 read 64MiB (random) | 64 MiB, pseudo-random | 407.9 op/s | 2284.7 op/s | 2.451 ms | 2.451 ms | 2.451 ms | 0.438 ms | 0.438 ms | 0.438 ms |
| ↳ MiB/s | 64 MiB, pseudo-random | 522.2 | 2924.4 | | | | | | |
| S3 random 4KiB read | 2000 ops, 4 KiB, 64 MiB file | 2651.9 op/s | 38266.5 op/s | 0.348 ms | 0.657 ms | 0.880 ms | 0.023 ms | 0.028 ms | 0.099 ms |
| ↳ MiB/s | 2000 ops, 4 KiB, 64 MiB file | 10.4 | 149.5 | | | | | | |
| S4 random 4KiB write | 2000 ops, 4 KiB, 64 MiB file | 561.9 op/s | 10818.4 op/s | 1.727 ms | 2.485 ms | 3.235 ms | 0.085 ms | 0.153 ms | 0.175 ms |
| ↳ MiB/s | 2000 ops, 4 KiB, 64 MiB file | 2.2 | 42.3 | | | | | | |
| S5 create 1000 | 1000 files, 3 levels deep | 72.4 op/s | 1973.0 op/s | 13.533 ms | 19.690 ms | 20.178 ms | 0.450 ms | 0.656 ms | 1.212 ms |
| S5 stat 1000 | 1000 files, 3 levels deep | 830.1 op/s | 36812.1 op/s | 0.274 ms | 0.421 ms | 18.659 ms | 0.023 ms | 0.053 ms | 0.060 ms |
| S5 list 1000 | 1000 files, 3 levels deep | 243.7 op/s | 10030.1 op/s | 4.103 ms | 4.103 ms | 4.103 ms | 0.100 ms | 0.100 ms | 0.100 ms |
| S5 delete 1000 | 1000 files, 3 levels deep | 371.7 op/s | 5849.2 op/s | 2.600 ms | 3.354 ms | 3.639 ms | 0.184 ms | 0.211 ms | 0.216 ms |
| S6 build 100x100 | 100 dirs × 100 files | 122.2 op/s | 1765.9 op/s | 8.730 ms | 9.131 ms | 9.131 ms | 0.563 ms | 0.747 ms | 0.747 ms |
| S6 walk 100x100 | 100 dirs × 100 files | 80.2 op/s | 1721.2 op/s | 12.469 ms | 12.469 ms | 12.469 ms | 0.581 ms | 0.581 ms | 0.581 ms |
| S7 create 5000x1KiB | 5000 files × 1 KiB | 90.4 op/s | 1971.3 op/s | 10.782 ms | 13.529 ms | 17.298 ms | 0.441 ms | 0.861 ms | 1.045 ms |
| ↳ MiB/s | 5000 files × 1 KiB | 0.1 | 1.9 | | | | | | |
| S7 read 5000x1KiB | 5000 files × 1 KiB | 1794.8 op/s | 29213.3 op/s | 0.350 ms | 0.648 ms | 0.763 ms | 0.030 ms | 0.072 ms | 0.090 ms |
| ↳ MiB/s | 5000 files × 1 KiB | 1.8 | 28.5 | | | | | | |
| S7 delete 5000x1KiB | 5000 files × 1 KiB | 290.1 op/s | 4497.0 op/s | 2.950 ms | 6.304 ms | 7.285 ms | 0.190 ms | 0.370 ms | 0.784 ms |
| S8 copy 64MiB | 64 MiB | 19.1 op/s | 445.6 op/s | 52.342 ms | 52.342 ms | 52.342 ms | 2.244 ms | 2.244 ms | 2.244 ms |
| ↳ MiB/s | 64 MiB | 24.5 | 570.4 | | | | | | |
| S9 overwrite 10% of 64MiB | 64 MiB | 490.4 op/s | 7698.2 op/s | 2.039 ms | 2.039 ms | 2.039 ms | 0.130 ms | 0.130 ms | 0.130 ms |
| ↳ MiB/s | 64 MiB | 62.8 | 985.4 | | | | | | |

Every operation above reported success; a failure would have aborted the run.

**No thresholds.** These numbers are a report, not a gate: they describe this machine on this day.
