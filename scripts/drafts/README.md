# drafts/

Work that is **not** finished, kept deliberately out of `crates/*/tests/` so a red or ignored test never enters the suite.

## mounted_write_chain.rs.draft

The real-machine write chain (`create → write → read back → set_len → delete` on `Z:`), written and run once. **It is red:**

```
cell write chain: The system cannot find the path specified. (os error 3)
```

What is already known, so the next session does not re-derive it:

- the chain fails at the **first** step (creating `written.txt` in the volume root), before any assertion;
- `ERROR_PATH_NOT_FOUND` is what `STATUS_OBJECT_NAME_NOT_FOUND` becomes on the Win32 side, so the refusal comes from one of our
  shells, not from Windows;
- the read chain with the same mount plumbing works (`mounted_read_only` passes), so the table, the registry and the trampolines are
  wired; what differs is the **create path**;
- the first guess — "WinFsp hands `\name`, the backend wants `/name`" — was already implemented (see `to_backend_path`) and did not
  change the error. The next step is a trace of **which** shell refuses: add a one-line trace to `create_shell`/`open_like` failure
  branches (the pattern used for the GSBN finding) and read it, rather than guessing again.

Deliberately not committed as `#[ignore]`: an ignored test looks green in the summary, and this one has never passed.

## Trace run (2026-xx, first session after the draft)

Trace added to `open_like`'s refusal path and to `write_shell`; output:

```
open_like: backend refused path="\\" disposition=Open: InvalidParameter
open_like: backend refused path="\written.txt" disposition=Open: NotFound
```

**Both guesses in the draft were wrong about the cause.** The refusal came from the **fixture**, not from Windows and not from the
shell's path handling: `MockBackend::split` refuses `/` (empty component after the leading slash), so the **root open** failed and
every later step failed behind it. The fixture now answers the root itself (documented as the fixture's limitation, matching the
recorded trait gap: a directory id is not reachable by path).

After that fix the root open succeeds, the create path is reached, and the chain fails **one step later**:

```
write_shell: id=1 at=0 len=0 written=0 before=0
cell write chain: failed to write whole buffer
```

`Write` arrives with `Length == 0` while `std::fs::write` asked for 11 bytes. Either the argument order in the registered
signature is not what the loaded DLL passes (the header says `Buffer, UINT64 Offset, ULONG Length`, but this binary is 2.1 and the
header is a newer mainline build — the same divergence that process 326/393 describes), or Windows really issues a zero-length
write first. **Next step is a probe, not a guess**: register a `Write` trampoline that prints every raw argument once, drive one
`std::fs::write`, and compare with the FSD trace line.

## The log named it (second session)

`LAYER_FS_DEBUG_LOG` was on during a full chain run. The first non-success `>>Op` line is the answer:

```
>>Create [UT----] "\written.txt", FILE_OPEN_IF, CreateOptions=60 ...
<<Create IoStatus=c0000034            <- the earlier bug (Create refused OPEN_IF), now fixed
>>QueryVolumeInformation
<<QueryVolumeInformation IoStatus=c0000023   <- STATUS_BUFFER_TOO_SMALL, unexplained; see below
>>Write ..., Offset=0:0, Length=14
<<Write IoStatus=0                     <- the write itself SUCCEEDS
>>Create [UT----] "\written.txt", FILE_OPEN ...
>>Read ..., Address=..., Offset=0:0, Length=32
<<Read IoStatus=c0000010               <- **Read is not registered at all** (slot 8)
```

So the chain fails at the **read-back** step, because `Read` (slot 8) has never been wired: `read_shell` does not exist yet.
That is the whole remaining gap for the chain — plus one open question:

`QueryVolumeInformation IoStatus=c0000023` (`STATUS_BUFFER_TOO_SMALL`) — our volume shell answers that when the caller's buffer is
smaller than `size_of::<VolumeInfo>()`, but the FSD's query does not print a length, so this needs one probe to see what size it passes.
It is not fatal for the chain (Windows tolerates the retry), but it explains why the volume shows no size.

## Third session: `Read@8` wired, and two new facts

`read_shell` + slot 8 are in. With it, the chain stopped failing at `Read` and **immediately exposed two more things**:

1. **`MockBackend::read` violates the trait contract.** The trait says "a short read at EOF is zero-padded by the caller's contract,
   not here", but the mock pads to `len`. With `Read` wired that turns into: Windows asks 32 bytes, gets 32, asks at offset 32, gets 32
   again… until the process runs **out of memory**. Fixing the mock (short read) makes the three existing expectations that encode the
   padded behaviour fail:
   `contract::the_mock_satisfies_the_trait_contract`, `criteria_1_2::a_supersede_is_visible_through_every_handle`,
   `mock_identity::supersede_truncates_in_place_and_keeps_the_id` — all of them assert **eight zero bytes** from an empty file, i.e. the
   padding. So the fix needs those expectations updated in the same commit, justified by the trait doc (not by "the test failed").
   The mock change was therefore reverted for now; `read_shell` stays.

2. **`SetInformation [EndOfFile]` now answers `c000000d`** (`STATUS_INVALID_PARAMETER`) — that is the next blocker:

```
>>SetInformation [EndOfFile] 00000293F8019A00, FileSize = 0:5
<<SetInformation IoStatus=c000000d
```

   `set_file_size_shell` returns that code when the `FileContext` is null, so the first question is whether WinFsp passes a null context
   for `SetInformation` (it does for some operations) or whether the slot's argument order is off. Same trace-first rule as before.

## Resolved (fourth session)

The write chain **passes on the real machine**, and the draft is gone: the test now lives at
`crates/fs-winfsp/tests/mounted_write_chain.rs` and runs on every full run (skipping itself only when WinFsp is absent).

Readings from the passing run:

```
write chain counters: create 0 -> 1, write 0 -> 1, set_size 0 -> 1, cleanup 0 -> 7
```

plus the assertions in the test: bytes read back **equal to what was written**, `set_len(5)` leaves the first five bytes,
`remove_file` leaves nothing, `contexts` balanced `1 -> 1`, `registry misses = 0`, `released_after_delete = true`.

Three fixes got there, each found by a reading rather than by reasoning:

1. `Create` must accept **all six dispositions** — `std::fs::write` sends `FILE_OPEN_IF` (raw `0x03000060`), which the
   "no approximation" whitelist refused, so no file was ever created (`a5f1d80`).
2. `SetFileSize` has **five** parameters — `(fs, FileContext, NewSize, BOOLEAN SetAllocationSize, FileInfo*)`; the missing
   `BOOLEAN` put the output pointer one slot early (`c000000d`).
3. `Read` was never registered at all (slot 8), and once it was, the count had to be the **bytes actually read** — reporting the
   padded length made Windows ask again at the next offset until the process ran out of memory.

The last fix is done **without** changing either backend: the shell asks the size first and clamps the request to what remains
(`want = min(length, size - offset)`), so EOF is a genuine zero-byte read. Both backends zero-pad their answers, which is fine for
them — the padding contract stays where the trait put it, and the adapter makes EOF visible.

## L2 mounted side: a crash, and the interpreter trap

Running the L2 script **against a live mount** (through a temporary harness that mounts `MockBackend` and shells out) found two things:

1. **`bash` is not `bash` here.** `std::process::Command::new("bash")` resolves to `C:\Windows\System32\bash.exe` (WSL), which cannot
   see `F:` at all — the script reports "No such file or directory" for a file that exists. The Git Bash
   (`C:/Program Files/Git/bin/bash.exe`) is the one the script was written for; it must be named explicitly.
2. With the Git Bash, the mounted run **aborted the test process**: `0xc0000374` (`STATUS_HEAP_CORRUPTION`). That is the first crash the
   mounted side has produced, and it is a **memory-corruption** class finding, not a behaviour difference — it needs the same
   trace-first treatment (which callback with which buffer) before anything else is claimed about L2 results.

Neither the harness nor the crash diagnosis is committed yet: the harness was temporary, and a crash report is worth nothing until the
failing callback is named.

### Logged re-run of the L2 mounted side

With `LAYER_FS_DEBUG_LOG` on, the run still aborts (`0xc0000374`) and the log's tail is:

```
>>Create [UT----] "\", FILE_OPEN, CreateOptions=200000, ...
>>QueryInformation 000001FC272B68A0
>>QueryInformation 000001FC272B68A0
>>Cleanup 000001FC272B68A0
>>Close   000001FC272B68A0
```

So the corruption happens around a **root open + two `QueryInformation` (GetFileInfo) calls**, with no `>>Read`/`>>QueryDirectory`
afterwards — i.e. the crash is near `GetFileInfo`/`GetSecurity` on a freshly created directory handle, not in the listing or the
read path. Candidate order (unchanged, but now narrower): an out-pointer write in `get_file_info_shell`/`get_security*`, not
`DirBuffer` arithmetic.

The log gives the neighbourhood, not the first fault: the next step is **PageHeap** (`gflags /p /enable <exe> /full`), which breaks at the
offending write instead of at the abort.

### Guard in place, crash still there — so it is not a `FileInfo` out-pointer

With `write_is_safe` (VirtualQuery) guarding every `FileInfo` write, the L2 mounted run still aborts and the guard **never fires**:
`bad_output_pointer_count()` stays 0, and no "refusing to write ... to 0x..." line is printed. So the corrupting write is somewhere else.

Remaining candidates, in the order the log's neighbourhood suggests:

1. `read_directory`'s `DirBuffer` (records + alignment) — the one callback that writes a **variable-length** payload;
2. `get_security` / `get_security_by_name` (`*size` and an attribute word);
3. `get_volume_info`'s prefix write for classes other than `FileFsSizeInformation` (QA cleared only that class: the query that passes a
   filesystem-name string has a different length and shape).

The same VirtualQuery guard can be applied to each of those in turn — one guard, one run, until one of them refuses.

## Double-Close: the three numbers (L2 mounted run, release witness on)

```
our counters:  open_like entries 122, create_shell 4, contexts allocated 115, freed 97, double releases 18
FSD log:       >>Create 244, >>Close 117, >>Open 0
```

Reading them together:

- **115 = 97 + 18**: every context is freed once successfully, and 18 *extra* `Close` calls arrive for addresses that are already out of
  the live set. The bookkeeping adds up exactly, so this is not a leak and not a lost free — it is **more closes than contexts**;
- `>>Create` (244) versus `create_shell` (4) is a second anomaly worth the same treatment: the FSD logs far more creates than reach our
  callback, which either means most are answered by the FSD itself (a class gate?) or that our `Create` is not the one being called for
  most of them. **Not explained yet**;
- `>>Open 0` with `open_like` entered 122 times says every open goes through **`Create`** in this FSD (`Open` is not used when `Create`
  is registered) — that is consistent with the header's note that `Create` supersedes `Open`.

So the next experiment is not "find the double close" but **"who calls Close twice"**: attach the FSD log and pair each `>>Close` line with the
`FileContext` value printed by our side. A pairing (same pointer, two `>>Close` lines, one `>>Create`) settles it in one run.

## Close pairing: the pointer repeats because the allocator reuses it

Tracing every `close_shell` (`CLOSE-TRACE #n ctx=0x…`) shows the **same address** arriving many times:

```
#00001 ctx=0x…ebfb0     #00002 ctx=0x…ebfb0     #00006 ctx=0x…ebfb0     #00008 ctx=0x…ebfb0
```

Two readings are consistent with that, and the trace alone cannot separate them:

1. **address reuse** — the context is freed, the allocator hands the same address to the next `Box::new`, and a *late* close of the old
   handle lands on the new context (which would be worse than a double free: it frees the wrong object);
2. **genuine repeated close** — WinFsp really calls `Close` more than once per context.

The experiment that separates them is to **stop reuse**: in trace mode, keep every freed context in a `Vec` (leak them on purpose) so an
address can never come back. Then a repeated pointer in `CLOSE-TRACE` is proof of (2), and its absence proves (1).

## Pinned-address experiment: the ambiguity resolves against "reuse"

With `PIN_ADDRESSES = 1` (released contexts are deliberately leaked, so an address can never come back) and `CLOSE_TRACE = 1`:

```
CLOSE-TRACE #00000 ctx=0x…298c0     #00001 ctx=0x…294e0     #00002 ctx=0x…29580
CLOSE-TRACE #00005 ctx=0x…f9520  ← refused: not a live context
CLOSE-TRACE #00008 ctx=0x…f98a0  ← refused: not a live context
```

Two facts, and they change the question:

1. **No address repeats** once reuse is impossible — so the earlier "same pointer many times" was mostly **allocator reuse**, not repeated closes of one
   context;
2. The refusals are for addresses that **never appear as opened** in this trace — i.e. `close_shell` is being handed contexts that `open_like`
   never produced. That is a different defect from a double close: something returns success **without** handing out a context, and the OS closes
   whatever was in that slot.

Next question is therefore: **which path returns success without setting `PFileContext`?** Candidates to read in order: `create_shell`'s early
returns, `open_like`'s refusal paths (they return non-zero, so their slot stays untouched — the FSD then must not close it), and
`overwrite_shell` (no context parameter at all).

## The stale closes are all **non-null** — candidate 2 confirmed

```
close: 0x18f718ea9c0 is not a live context — refusing to free it (null=false never-handed-out)
stale releases: 0 -> 17
```

Every one of the 17 is a **non-null address we never handed out**. Per the dispatcher's own source (`fsop.c`: the context slots are
zeroed before the call and left alone on failure), a stale `NULL` would mean *we* returned success without writing the slot; a stale
**non-null** value means the value came from somewhere else entirely — the FSD closing a context slot that was never ours.

So the containment stands (`close` refuses), the crash is gone, and the **residual risk** is the one the pinning experiment could not
trigger: an allocator reusing an address so that a stale close lands on a *live* context and frees the wrong object.

### Why an issue number inside the context cannot fix that

Reading a serial out of the pointed-to memory is only safe if the address is ours; under reuse the address *is* ours (the new context), so
the serial read would be the **new** one and the check would pass. WinFsp hands us back only the pointer, so nothing in the pointed-to memory
can distinguish "the context this close belongs to" from "whatever now lives at that address".

### The fix that does not depend on addresses: hand out **indices**, not pointers

`FileContext` = a monotonically increasing id (never reused), kept in a table `id -> Box<EntryContext>`:

- a stale close carries an id that is **not in the table** ⇒ refused, no dereference, no UB;
- a reused address cannot happen because we never interpret the value as an address;
- the table is the single owner, so double-free is impossible by construction.

That is the next cut — it replaces `LIVE_CONTEXTS: BTreeSet<usize>` (address-keyed) with `BTreeMap<u64, Box<EntryContext>>` (id-keyed) and
makes `close_shell` look up by id.

## id-table cut: started, verified compiling, **not landed** (stash: `id-table-wip`)

The design is agreed and the library side works: `FileContext` becomes a **monotonic id** (never reused) kept in
`BTreeMap<u64, Box<EntryContext>>`; `entry_of`/`release_context` never dereference the value, so a stale id is simply absent and a double
release is impossible by construction.

What stopped it: the **direct tests** build an `EntryContext` by hand and pass its address
(`callbacks_direct`, `set_file_size_criterion`, `overwrite_shell_criterion`, `read_directory_core`, `cleanup_shell_criterion`). Under the new
scheme those addresses are not ids, so every one of them must go through `register_context_for_test` — a mechanical but wide edit across five
test files, plus a check that `mounted_write_chain` still passes.

The work-in-progress is in `git stash` under `id-table-wip`; the branch itself is green (`170/170`, clippy 0) at `c50e3c7` + docs.

## `QueryDirectory IoStatus=c0000010` in the L2 run: not the directory open

A minimal sequence (mount → `Create` root → `QueryDirectory` root) **works**:

```
>>Create "\", FILE_OPEN → <<Create IoStatus=0[1] UserContext=0000000000000006
>>QueryDirectory 0000000000000006 → <<QueryDirectory IoStatus=0[128]
```

So the directory open does register a context and the listing answers — the `c0000010` in the L2 script must come from a *different* point of that
script (it creates files, renames, sets attributes, tries long names and a delete-while-open). The trace to read is the one from a full L2 run,
paired with the `>>Create` line that precedes each failing `>>QueryDirectory`.

Note also what the same trace shows: `>>Create "\written.txt", FILE_OPEN` answered `c0000034` (not found) right after a successful create of the
same name — worth pairing as well, since it suggests the second open used a **different name shape** than the first.

## The "second open of the same name" clue was wrong — and the trace says why

```
>>Cleanup [Delete] "\WRITTEN.TXT", 0000000000000007
<<Cleanup IoStatus=0[0]
>>Close 0000000000000007
<<Close IoStatus=0[0]
>>Create [UT----] "\written.txt", FILE_OPEN, CreateOptions=20 …
<<Create IoStatus=c0000034
```

The second `Create` answers **not found because the previous step deleted the file**. That is correct behaviour, not a defect — the pairing I proposed
as "the most valuable clue" was a false lead, and the trace settles it in four lines.

So the open lead is back to the one place the L2 script differs from the minimal sequence: some `>>QueryDirectory` answers `c0000010` **in the script's
flow** while the minimal create-then-list run answers `0[128]`. The next run must capture the *first* `c0000010` of a full L2 run together with the
`>>Create` that produced its context.

## Decisive: `mkdir` reports success and the directory is not there

A real-machine run of the L2 script's first steps, one `std::fs` call at a time:

```
STEP write file: Ok(())      ← creating a file in the root works
STEP mkdir:      Ok(())      ← **reports success**
STEP write a:    Err(3, NotFound)   ← inside the new directory: not found
STEP list dir:   Err(3, NotFound)
STEP rename file: Err(2, NotFound)
```

So `Create` for a **directory** answers success while creating nothing usable. That is the "silent success" class in its purest form, and it
explains the L2 classes together: `05` (listing an empty/missing dir), `07` (rename needs the source to be openable), `08`, `09` (delete needs
the entry to resolve).

Root cause is the gap recorded when the make-verb split landed: **there is no way to open an existing directory by path.**
`create_or_open` is the *file* door (it refuses a directory), and the directory branch only runs for `FILE_CREATE`. Windows opens the directory
again with `FILE_OPEN + FILE_DIRECTORY_FILE`, which falls into the file door and is refused — after `mkdir` has already told the caller "ok".

The fix is a trait method — `open_dir(path) -> id` (or a `kind`-aware `create_or_open`) — implemented in both backends. That is a cut of its own,
and it is the one that unblocks the four L2 classes.

## The dispatch never selects `mkdir`/`open_dir`: `FILE_DIRECTORY_FILE` is not arriving

A recording backend (every `WorkspaceOps` call printed with its raw path) run through `create_dir` + a write inside it:

```
CALL create_or_open "\"          Open
CALL create_or_open "\l2-dir"    Open      ← the directory
CALL create_or_open "\l2-dir\a"  Open
CALL create_or_open "\l2-dir\a"  Open
CALL create_or_open "\l2-dir"    Open
```

Three facts:

1. **No `mkdir` and no `open_dir` call at all** — the 2×2 never leaves the `File` arm for these requests;
2. every one of them is `Open`, not `Create`/`OpenIf` — so the creating dispositions never appear either;
3. the paths carry the WinFsp form (`\l2-dir`, no leading slash translation here because this recorder sees the raw bytes).

So the assumption behind the split — "a directory create arrives with `FILE_DIRECTORY_FILE` in the low bits and a creating disposition in the high
byte" — is **not what this DLL sends for these calls**. The next run must print the raw `create_options` word for each of these, to see which of the
two halves is missing (or whether both are, and the FSD is opening first and creating later).

## After the kind fix: `mkdir` works, listing works, **child creation still fails**

```
STEP mkdir:        Ok(())
STEP root list:    Ok(["d"])                 ← the directory exists and is listed
STEP write in d:   Err(3, NotFound)          ← creating a child under it still fails
```

So the "file replaced the directory" accident is gone (the directory survives and lists), and the remaining failure is one level deeper: creating
`\d\a` — the *parent* resolution inside the backend, or the path translation for a nested name.

Next step is the recorder backend again, this time printing every call **with its raw bytes**, for exactly the two operations `mkdir("d")` and
`write("d/a")`, to see whether the nested path arrives as `\d\a` (and whether anything translates the backslashes).

## Recorder vs real backend: two different sequences (so the recorder is the wrong instrument here)

With a **recorder** backend (every call returns an error) the sequence for `create_dir("d")` + `write("d/a")` is:

```
CALL create_or_open "\"     Open
CALL create_or_open "\d"    Open
CALL create_or_open "\d\a"  Open
CALL create_or_open "\d\a"  Open
CALL create_or_open "\d"    Open
```

No `mkdir` at all — but the earlier run **with the real backend** did show the directory create (`raw=0x02200021`, disposition `Create`). So the recorder
changes what the FSD sends (its blanket failures short-circuit the create path), and it cannot be used to study the nested-name question.

The instrument for the remaining question has to be the **real backend plus a trace inside `SlashBackend`** (print the path it receives and the path it
forwards), because that is where a nested name could be translated differently from a single-component one.
