# Next cut: `Path { key, display }` + the shape sweep (473, corrected by 477/478)

**This is a plan, not a work log.** The cut is one of those that needs a whole fresh budget (445): trait shape changes are mechanical but touch every
call site, and doing them in two sittings produces two half-shapes.

**Correction (477):** the first version of this plan left the **store** out of the dependency chain. `read_view::lookup` matches on the **raw bytes that
were stored**, so `Path` alone does not let `ApiBackend` "look up by key" — looking up `README.TXT` cannot find an entry stored as `ReadMe.txt`. The
real chain is: **the layer where lookup happens folds** → the backend can match by key while storing the display → then criterion ③ means something.

**Ruling (478):** `fs-snapshot`'s on-disk page order is **not** changed here (index key + display in separate fields would mean a format version bit and
re-baselining every fixture — that is project **B-18**, not a step in this cut). Fold at the **overlay view**, which is the layer where lookups actually
happen; the snapshot is the data being searched.

**Ruling (478):** the overlay uses a **scan** when the exact lookup misses, not a folded-key **cache**. This is not a performance trade: a scan is slow
**observably**, while a cache that misses an update on any of `bind`/`unbind`/`rename`/`remove_at` is wrong **silently** — which is the exact defect
class this whole campaign has been chasing. A folded-index cache is **B-19**, to be built later *against* the scan as its correctness baseline.

## Why these items are one cut

```
(trait) Path { key, display }        the matching key and the bytes to hand back, as one value
(②) overlay folding (scan)           the layer where lookup happens; the backend then stores the display spelling
(③) two-backend agreement test       only meaningful once ② exists
(shape) DirOutcome { id, binding }   the other parked trait shape change; same layer, same sweep
```

`②` cannot be done alone: folding a path at the backend entrance also folds the bytes that get **stored**, which loses the display spelling. That is the
same mistake the mock made once and still documents at `mock.rs:104` ("folding inside `split` alone would have made the display name folded too —
measured: an entry created as `A` listed as `a`"). The correct shape needs both values across the boundary, i.e. this cut.

## Order inside the cut (each step compiles before the next)

1. **Introduce `Path`** — **done** (`a22c9dd`): `new(raw)` folds with `fs_semantics::name::fold_bytes` and keeps the raw bytes; `display_name()` answers
   `Option` so "this path has no name" cannot become "an empty name in the store". No call site changed.
2. **Change `WorkspaceOps`'s path parameters** to `&Path` in one commit — the compiler lists all **18** implementations (`2` real + `16` test doubles),
   which is exactly why this is one step and not twelve.
3. **Overlay folding, scan version** (478): in `fs-overlay/src/view.rs::lookup_impl`, when the exact lookup misses, scan the base directory and compare
   `fold_bytes`; the overlay's own rows are written with the display spelling, so the same comparison applies to them. **This is where ② actually
   lands** — `ApiBackend` needs no folding of its own once the view matches by key.
   - Criterion: a base entry stored as `ReadMe.txt` is found by looking up `readme.txt`, and the name it reports back is still `ReadMe.txt`.
4. **`DirOutcome { id, binding }`** merge (the other parked shape).
5. **`FileInfoTimeout = 0`**: verify the semantics rather than describing them in a comment (the belief has been unverified since batch (i)).
6. **Criterion ③**: one shared scenario, both backends, same answers — including a case-only rename answered **directly** at the backend (this is also
   the U1 direct test; it bypasses the mount layer, where the FSD short-circuits case-only renames and the request never arrives — U16).

## What must not change while this is done

- `display` bytes are what `read_dir` returns, on every path, including after a rename that only changes case.
- The single fold definition stays `fs_semantics::name::fold_bytes`. No second copy, no local `to_ascii_lowercase`/`to_ascii_uppercase`.
- No criterion is widened, and no test count may drop (450).

## Entry state

```
branch p3-4/mount @ a22c9dd ；656/656 (workspace, lock) ；clippy 0 ；worktree clean
step 1 done ；steps 2–6 not started
```

## Parked as their own projects (477/478)

- **B-18 — snapshot format**: index key and display spelling as separate fields, a format version bit, every fixture re-made and every baseline
  re-timed. Triggered when the fold must be pushed down to disk (or a direct snapshot reader needs the same semantics).
- **B-19 — folded-index cache**: a `(parent, folded) -> display` index in the view layer. Built **against the scan as its baseline**: any divergence is a
  silent wrong lookup, which is why it is not the first version.
