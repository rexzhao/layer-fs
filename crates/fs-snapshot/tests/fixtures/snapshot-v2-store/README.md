# Frozen v2 store fixture (P3-0c)

**Do not regenerate this fixture.** It exists because profile v3 will refuse v2 roots: once the write path
only emits v3, a *real* v2 store can no longer be produced, and P3-2's "refuse an old root" criterion would
degrade into "synthesise a v2 root by editing a header" — which tests self-identification, not refusal.

- profile: v2 (`Profile::V2`, the id recorded inside the root is
  `530c5650571a1e25367fe03eedec148132ce0339e200627e0c53519f42fbb339`) — read with `open_any`, never with a
  hard-coded profile, because the root states its own
- root: eed636c6225f5a685457f8eec2d9a642d5d60fbd9f8bde867902c7d69890192f
- commit: cf35ea5a456cdbf36d9c0f591f993ae623b92ffe1fc9ce373890f00450c2b219
- snapshot root: eed636c6225f5a685457f8eec2d9a642d5d60fbd9f8bde867902c7d69890192f
- content length: 32768
- content blake3: b88eeed285256715c3a0ae2c46d69407628efbab876974c9728c8a404c5bbe33
- objects: **7 index rows** in 2 packs, of which **5 are reachable** from the recorded root below. The other two
  are the genesis commit's own tree (`0c660a66…c9a427`, role `FilesystemRoot`) and its inode table — they belong to
  the repository, not to this snapshot, which is why a walk from `root` finds five.
- total fixture bytes: 189106

## How it was generated

At the commit that introduced this directory, with the v2 writer (no product code was changed for the fixture):

```text
V2_FIXTURE_OUT=crates/fs-snapshot/tests/fixtures/snapshot-v2-store \
  cargo test -p fs-workspace --test v2_fixture_generator -- --ignored --nocapture
```

The generator is kept (ignored) for provenance only. Running it to "fix" a failing manifest check destroys the
property this fixture exists for.

### What re-running the generator does and does not reproduce

**The root id is not reproducible, and this fixture does not claim it is.** `InodeRecord.created_ns` and
`modified_ns` are wall-clock values and are part of the encoded tree, so two runs at the same commit produce
different (equally valid) roots. Measured, running the generator a second time at the commit above:

| property | this fixture | second run | reproducible? |
| --- | --- | --- | --- |
| root id | `eed636c6…90192f` | `95b2b7e0…e356f3` | **no** (timestamps are in the tree) |
| commit id | `cf35ea5a…c2b219` | `b83dd2d3…d9b8cbf3` | no (derived from the root) |
| content bytes (blake3) | `b88eeed2…5bbe33` | `b88eeed2…5bbe33` | **yes** |
| content length | 32768 | 32768 | yes |
| objects / packs / commits / snapshots | 7 / 2 / 2 / 1 | 7 / 2 / 2 / 1 | **yes** |

So the claim this fixture makes is: **it was written by the v2 writer, its shape is reproducible, and its bytes are
pinned by the manifest below** — not "the same root id comes out again". A determinism harness (injected clock)
does not exist in this build, and inventing one for a fixture would be a product change the card forbids.

## What it contains

One workspace `w1` on `main`: a directory `/docs` with one file `/docs/readme.txt` of 32768 bytes (a repeating
16 KiB pattern, so the file spans more than one chunk), one commit, and one snapshot of the same tree.

## There is no v1 fixture, and why

This build **cannot** produce a real v1 store: `Profile::CURRENT` is `V2`, and the write path always uses it —
v1 exists only as a *read* profile for history. Synthesising a v1 store by editing a v2 root's format byte is
exactly what the card forbids ("blank means go back; do not fake it"), because it would prove nothing about a
store a v1 writer ever produced. If a genuine v1 store is ever needed, it has to come from an old build, and
this README should say which one.

## Manifest

Every file below `store/` with its exact length and BLAKE3. `tests/v2_fixture.rs` recomputes this list and fails
if anything differs — including an extra file.

```manifest
store/packs/2735a2bbb75c7f0fb948551ef69f750b.pack 33237 9eb8a938416934bc03fd46bad810c5f382ccdf296399c56f3a4f90c591738082
store/packs/85808e23bbfd3dbff02a87dd14c93ed1.pack 221 37b250b9d600ec25b597b76c6f62869fc66cba7ebc84ff70750db36718e44bcb
store/store.sqlite 90112 653b070ac9c53560bf996ffc8e31573d975cf25bbda349883979d820e88fd5f7
store/workspaces/b7c7bd8830c2c987e0be1d3b606f40da/overlay.sqlite 65536 0de54b4781a89d9b27193f8a836528098791d982e840d6448420948d67fbbeb6
```
