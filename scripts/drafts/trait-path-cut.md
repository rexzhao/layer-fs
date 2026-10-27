# Next cut: `Path { key, display }` + the shape sweep (473)

**This is a plan, not a work log.** The cut is one of those that needs a whole fresh budget (445): trait shape changes are mechanical but touch every
call site, and doing them in two sittings produces two half-shapes. Nothing here has been started.

## Why these four items are one cut

```
(trait) Path { key, display }      the matching key and the bytes to hand back, as one value
(②) ApiBackend folding            falls out of `Path` — the adapter produces `key`, the store keeps `display`
(③) two-backend agreement test    only meaningful once ② exists
(shape) DirOutcome { id, binding }  the other parked trait shape change; same layer, same sweep
```

`②` cannot be done alone: folding a path at the backend entrance also folds the bytes that get **stored**, which loses the display spelling. That is the
same mistake the mock made once and still documents at `mock.rs:104` ("folding inside `split` alone would have made the display name folded too —
measured: an entry created as `A` listed as `a`"). The correct shape needs both values across the boundary, i.e. this cut.

## Order inside the cut (each step compiles before the next)

1. **Introduce `Path`** with an `From<&[u8]>` that folds (`fs_semantics::name::fold_bytes`) and keeps the raw bytes. No call site changes yet.
2. **Change `WorkspaceOps`'s path parameters** to `&Path` in one commit — the compiler lists every call site, which is exactly why this is one step and
   not twelve.
3. **`ApiBackend`**: pass `key` to `rules` lookups, `display` to `bind`/`create`. This is ②.
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
branch p3-4/mount @ a0b6f10 ；175/175 (fs-winfsp+fs-api) ；652/652 (workspace, lock) ；clippy 0 ；worktree clean
```
