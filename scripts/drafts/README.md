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

## Real backend + SlashBackend trace: the conversion is uniform, the child arrives as `Open`

```
SLASH create_or_open "\"    -> "/"   Open
SLASH mkdir           "\d"  -> "/d"          ← the directory is created
SLASH create_or_open "\d"   -> "/d"   Open
SLASH create_or_open "\d\a" -> "/d/a" Open    ← the child, asked for as **Open**
SLASH create_or_open "\d\a" -> "/d/a" Open
SLASH create_or_open "\d"   -> "/d"   Open
```

Two facts:

1. **The translation is uniform**: `\d` → `/d`, `\d\a` → `/d/a`. So the nested-name hypothesis is dead — it is not the fixture translating
   differently for nested names;
2. The child creation reaches us as **`Disposition::Open`**, not `OpenIf`. `Open` on a missing leaf is - correctly - "not found", so nothing is
   created and the whole downstream (`05`, `07`, `09`, `10`) fails.

So the next question is narrow and answerable with one print: **what create options does Windows use for `write("d/a")` inside a directory it just
created** — and why is the disposition `Open` there while the same operation in the root arrives as `OpenIf` (`raw=0x03000060`)?

## Three-way print: the child `Create` **succeeds** (status 0) and the OS still says "path not found"

```
FFI create raw=0x02200021 disposition=Create  options=0x200021  → status=0   (mkdir "d")
STEP mkdir: Ok(())
STEP write: Err(3 NotFound)
FFI create raw=0x03000060 disposition=OpenIf  options=0x60      → status=0   (create "d\a")
```

So per the dispatcher's own decomposition (`fsop.c` `FileOpenIf`: GSBN → maybe `Open` → `Create`, upgrading only on exactly `0xC0000034`), the
create of the child was **asked for and answered successfully**. Nothing failed at the create. The "path not found" comes from something *after*
it — a callback that is not `Create`, and that we are not printing.

Next instrument: log **every** callback that answers non-zero (a one-line hook in the shells' single exit path), not just `Create`. The failure is now
known to be downstream of a successful create, which is a much smaller place to look than "the directory is broken".

## Methodology hazard found: two mounts in one test process

The step test above was appended to a copy of `mounted_write_chain.rs`, so **two tests mounted `Z:` in the same process**. The FSD log for that run
contains only the *other* test's calls (`\written.txt`) — the directory operations are absent, which means they did not go through the mount whose
trace I was reading.

Consequence: **every reading produced by a copy-paste test that also mounts must be re-taken with exactly one mounting test per process** (or with a
distinct mount point). The earlier "child `Create` returns 0 while the OS says path not found" reading may be sound, but it was taken under this
hazard and should be re-confirmed the same way before anything is built on it.

## Key-shape hypothesis: dead. The child create still arrives as `Open`

Clean instrument (one mount, keys printed):

```
KEY create_or_open raw="\d"   key="/d"   resolved=Some(1) -> Err(NotADirectory)
KEY open_dir       raw="\d"   key="/d"   resolved=Some(1) -> Ok(1)      ← the new fallback works
KEY read_dir(1) -> 0
KEY create_or_open raw="\D\a" key="/D/a" resolved=None    -> Err(NotFound)
KEY create_or_open raw="\d\a" key="/d/a" resolved=None    -> Err(NotFound)
```

- **the two routes use the same key** (`/d`) and the same resolved id (1) — so the "different key shapes" suspicion (426, third appearance) is **dead**;
- the child never appears with a *creating* disposition: both attempts are `Open`, which correctly cannot create, so `/d/a` is never made.

So the open question is exactly one: **why does the dispatcher not upgrade `Open` to `Create`** here, when its own source says it upgrades on
`STATUS_OBJECT_NAME_NOT_FOUND` — which is the code we return. The instrument above did not print the disposition; the next one must, together with the
exact status returned, for every call.

## L2 after the GSBN fix: `05` flipped green, `06` became a real difference

Mounted side (one mount, script driven through Git Bash):

```
01 root: not a directory or missing
02 create+read: alpha                     ← matches NTFS
03 truncate existing: ok                  ← matches
04 missing file: not found                ← matches
05a dir listing (sorted): a,b,            ← **matches NTFS now** (was empty before the GSBN fix)
05b dir listing (raw order): a,b,         ← matches
06 rmdir non-empty: removed (wrong)       ← NTFS refuses; **we delete a non-empty directory**
07 rename: old=present new=gone
08 case-only rename: <missing>
09 delete while open: name=present
10 concurrent read after write: <unreadable>
11 long name: refused                     ← matches
12a attributes (semantic): readonly=yes directory=no   ← matches
```

So the mapping prediction held for `05` (child creation was the single root cause there), and `06` is a **newly visible real defect**: removing a
non-empty directory is refused by NTFS and allowed by us. `07`/`08`/`09`/`10` remain, and `01` is the separate root-metadata question.

Next: `06` first (shortest, a real semantic rule: a directory with children cannot be removed), then `07` (rename) which likely also explains `10`.

## 07 cracked: Windows asks to rename the **upcased** source name

```
WITNESS before rename: [("/L2-FILE", None), ("/l2-renamed", None)]
CALL rename "/L2-FILE" -> "/l2-renamed" replace=true -> Err(NotFound)
```

The file was created as `/l2-file`. The rename request carries **`/L2-FILE`** — the FSD upcased the name (a normalisation it performs for
case-insensitive volumes). Our storage is byte-keyed and therefore **case-sensitive**, so the source does not resolve, the rename is refused, and
`08`/`09`/`10` fail downstream of it.

So `07`, `08`, `09` and `10` are **one defect**: no case-insensitive matching anywhere below the frontend. That is the same prerequisite `08` was
known to have (manager 436: `\WRITTEN.TXT`, `\D\a`), now measured on the rename path as well.

Where it belongs: at **resolution**, once — not in each shell. Both backends must fold the same way (the shared contract can assert it), and the
frontend must not grow a second, different folding.

## The fold cannot live in `split()` — it loses the display bytes

Attempted step 1 (fold the **matching key** while keeping the **display bytes**), and hit the structural reason it is not a one-line change:

`split()` is the single entry point every path goes through, so folding there is the cheapest place — **but `split()` returns the components, and
folding them means the raw bytes are gone**. `mkdir`/`create_or_open` then store `Binding { id, display: name }` with `name` already folded, so the
directory lists `a` where the caller wrote `A`. Measured: `raw_names` answered `[[97]]` for an entry created as `A`.

So `split()` must return **`(folded, raw)`** per component, and only the last one is needed for display. That is the shape of the next attempt; the
work was reverted rather than left half-applied, because a fold that silently rewrites names is exactly the "looks right, means something else"
state this project keeps paying for.

## Fold step 1 is **two coupled changes**, not one — measured attempt, reverted

`split() -> Vec<Component { folded, raw }>` was implemented and it compiles in isolation, but it is inseparable from the *other* half:
`children` must become `BTreeMap<(parent, folded), Binding { id, display }>` **in the same commit**, because every read of `children` now needs the
binding's `id` while every write needs the component's `raw`. Doing one without the other leaves the tree uncompilable in between.

So the cut is:

1. `Component { folded, raw }` + `split` returning pairs;
2. `Binding { id, display }` as the `children` value;
3. the ~15 sites that read `children` switch from `*child` to `child.id`, and the ~4 write sites from `id` to `Binding { id, display: raw }`;
4. the two expectation flips (inverted `mock_identity` case; synthetic entries in `criterion_6_dirent`);
5. then the core side, then L2.

Measured on the attempt: it is a **single-commit** change of about 20 sites; splitting it produces a red tree, which is worse than not starting.

## L2 after the fold (folded key + display bytes): `07` flipped, `02` regressed

```
01 root: not a directory or missing
02 create+read: <unreadable>              ← was "alpha" before the fold
03 truncate existing: ok
04 missing file: not found
05a dir listing (sorted): a,b,            ← ok
06 rmdir non-empty: removed (wrong)
07 rename: old=gone new=present           ← **flipped green** (matches NTFS)
08 case-only rename: <missing>
09 delete while open: name=present
10 concurrent read after write: <unreadable>   ← downstream of 02
11 long name: refused
12a attributes (semantic): readonly=no
12b attributes (raw):  Z:\l2-renamed
```

`07` behaves like NTFS now — the fold was its root cause, as predicted. But `02` (write then read back) stopped working, and `10` fails with
it, which points at the **path the fold now returns**: `raw_names`/`read_dir` answer `display`, while `create_or_open` stores `display: raw`; if any
consumer still compares against the folded key, the round trip breaks. That is the next thing to look at — a regression introduced by this cut, not a
pre-existing difference.

## Four faces in one instrument: two are fine in isolation, one is a real defect

```
STEP 02 write: Ok(())     STEP 02 read: Ok("alpha")      ← the write/read chain works on a fresh mount
STEP 06 rmdir: Ok(())     STEP 06 still there: false      ← **non-empty directory removed** (the rule did not fire)
STEP 01 is_dir: true                                     ← the root answers a stat
STEP 12 readonly: Ok(false)
```

Three observations:

1. **`02` works in isolation** — so the L2 script's `<unreadable>` is not the backend: it is the *state the script left behind* (it runs `01`'s failing
   `stat` and `03`'s truncation before it). Worth re-reading the script's ordering rather than the storage;
2. **`06` is a real defect again**: removing a non-empty directory **succeeded**. The rule exists in `MockBackend::remove_at`, so the removal must be
   reaching the store by a path that does not go through it — the deletion is performed from the adapter's handle close, and for a **directory**
   `open_like` passes `binding: None`, so `Adapter::close` cannot find a name and swallows the failure. Something else then removed it.
3. `01` now answers — the root fix holds.

The next question is therefore narrow: **which path removes a directory when `remove_at` is never called with its name?**

## L2 scoreboard after the directory-binding + CanDelete fix: **7 of 12**

```
01 root: series               ours: not a directory          ✗
02 create+read: alpha         ours: <unreadable>             ✗
03 truncate: ok               ✓
04 missing: not found         ✓
05a/05b listing: a,b,         ✓
06 rmdir non-empty: refused   ✓   ← flipped by `9d5f56d`
07 rename: old=gone new=present ✓ ← flipped by the fold
08 case-only rename           ours: <missing>                ✗
09 delete while open          ours: present                  ✗
10 concurrent read: beta      ours: <unreadable>             ✗ (downstream of 02)
11 long name: refused         ✓
12a readonly=yes              ours: readonly=no              ✗
```

Remaining: `01` (root stat — the instrument says `is_dir` works in isolation, so the script's first call is what differs), `02` (a plain
write-then-`cat` that works in the instrument but not inside the script), `08` (case-only rename), `09` (delete while open), `12a` (attributes).

`02` is the most suspicious one *because* it works standalone: the difference is what `01` leaves behind when its `stat` fails. That suggests
`01` should be fixed first — it may be the upstream cause of `02` and `10`.

## bash probes: `[ -d ]` works here; `cat` fails with **"Invalid request code"**, before and after it

```
BASH printf alpha > 'Z:/f1' && cat 'Z:/f1'   -> exit=1  stderr: cat: 'Z:/f1': Invalid request code
BASH [ -d 'Z:/' ]                            -> d=0            ← the root **is** a directory here
BASH printf alpha > 'Z:/f2' && cat 'Z:/f2'   -> exit=1  stderr: cat: 'Z:/f2': Invalid request code
```

Two conclusions, and they kill both hypotheses as stated:

1. **`01` is not a backend failure**: `[ -d 'Z:/' ]` answers `d=0` in this instrument. Whatever the L2 script hits is specific to its own earlier state,
   not to `stat`;
2. **`02`/`10` are not "downstream of 01"** (assumption A): `cat` fails **before** any `-d` ran, so the shared cause is the *opening form* bash uses
   (assumption B). The message is `Invalid request code` — `STATUS_INVALID_DEVICE_REQUEST` — i.e. **a callback we do not implement is being asked**,
   and the `>>Create` trace shows the file was never even created (`create_or_open "/f1" Open -> NotFound` over and over).

So the next question is a *slot* question, not a resolution question: **which operation does MSYS's read/write path request that `std::fs` does not** —
a candidate list from the unimplemented members (`GetEa`/`SetEa`, `Control`, `SetSecurity`, `GetStreamInfo`).

## The refused operation is named: a `Read` that arrives **after** `Close`

From the FSD log of one `printf alpha > f1 && cat f1` under bash:

```
>>Read 0000…0008, Offset=0:0, Length=3        <<Read IoStatus=0[3]
>>Cleanup 0000…0008                          <<Cleanup IoStatus=0
>>Close   0000…0008                          <<Close IoStatus=0
>>Read 0000…0008, Offset=0:0, Length=131072  <<Read IoStatus=c0000010   ← refused
```

So the read that fails is a **second, large read of the same FileContext after that context was closed** — our `close_shell`
released the id from the table (correctly: `Close` means the context is gone), and the later `Read` finds no id and refuses with
`STATUS_INVALID_DEVICE_REQUEST`, which MSYS reports as "Invalid request code".

The question is therefore not "which slot is missing" (324/455's plan is answered: it *is* `Read`) but **why Windows reads after closing the
handle** — a lifecycle question, and the same family as the double-close finding: our `Close` may be freeing the context while the FSD still holds
the file object. The next instrument should print the `FileContext` id at `open`/`read`/`cleanup`/`close` for one bash `cat`, so the pairing is
visible on our side too.

## `cat` reads once, we answer 3 bytes — and nothing reaches stdout

```
BACKEND read(id=1, offset=0, len=3) -> Ok(3)
BASH exit 0  out "exit=0"  err ""
```

The clamp is not the suspect any more: `offset=0`, `len=3`, the backend answers 3 bytes, no EOF, no second read. Yet `cat` prints nothing while
exiting 0.

So the defect is in **how the answered bytes reach the caller's buffer**, not in the offset arithmetic. The next probe writes the first bytes of
`Buffer` *before* and *after* our copy (and prints `*PBytesTransferred`), which separates "we copied the wrong bytes" from "we copied to the wrong
place". (`wc -c` read 5 bytes correctly through the same shell, so the buffer plumbing works on that path — the difference is the length-3 read.)

## The last centimetre: two reads, two buffers, and our EOF answer swallows the live one

Working buffer printed at the trampoline, one `printf > f1; cat f1` under bash:

```
TRAMP read buffer=0x2c273f50000 offset=0 length=3        -> BACKEND read(len=3) -> "alp"
TRAMP read buffer=0x2c273f60000 offset=0 length=131072   -> (no backend call)
```

Two facts:

1. the buffers **differ**, so these are two different requests (a small read and the cached read-ahead);
2. the second one **never reaches the backend** — the shell takes the `is_closed` branch and answers EOF. So a read that belongs to a *live* caller
   is being answered as if the handle were closed: the graveyard test fires for a context that is still in use, which means the id in that request
   was already released by an earlier `Close` of the *same* id.

That points at the lifecycle again, from a new side: **the FSD is still issuing reads for a handle whose context we released**, and my EOF shortcut hides
it (it made `cat` fail quietly instead of loudly). The next instrument must print the *context id* per read on both sides — the buffer addresses already
show the pairing is not 1:1.

## id pairing: `CLOSE 8 … READ 8 … CLOSE 8` — the FSD closes, reads, and closes again

Ids printed on our side during one `printf > f1; cat f1` (EOF fallback off):

```
OPEN 5 file_id=1
CLOSE 5
OPEN 6 file_id=1        OPEN 7 file_id=1
CLOSE 6                 CLOSE 6          ← the same id closed twice
OPEN 8 file_id=1        OPEN 9 file_id=1
CLOSE 8
READ STALE id=8         ← a read on a context we have released
CLOSE 8                 ← and the same id closed a third time
```

Three facts, and they choose between the three candidate mechanisms:

1. the id **really was released** before the read (`CLOSE 8` precedes `READ STALE id=8`) — so **(c) is excluded**, this is not our bookkeeping marking
   a live context as closed;
2. **`CLOSE` arrives more than once for the same id** (6 twice, 8 twice) — the FSD closes the same file object repeatedly, which is the
   double-close family we first met in the heap-corruption hunt;
3. the read sits **between** two closes of that id, so the object is still alive from the FSD's point of view while we consider it gone.

So the mechanism is (a)/(b): **the FSD keeps using a file object after (and across) our `Close`**. The honest next step is not another fallback but deciding
*what a `Close` means for us*: today it frees the context immediately, and everything arriving afterwards is refused — including operations that belong to a
still-live file object. Keeping the context alive until the object is really gone (a graveyard with an explicit "never reused" id) would make the later
operations answerable instead of refused.

## L2 after the reference-counted contexts: **10 of 12**

```
01 root: not a directory       ✗   NTFS: directory
02 create+read: alpha          ✓   ← the whole "Invalid request code" chain, closed
03 truncate: ok                ✓
04 missing: not found          ✓
05a/05b listing: a,b,          ✓
06 rmdir non-empty: refused    ✓
07 rename: old=gone new=present ✓
08 case-only rename: <missing> ✗
09 delete while open: name=gone ✓  ← flipped with the lifecycle fix
10 concurrent read: beta       ✓  ← flipped with the lifecycle fix
11 long name: refused          ✓
12a readonly=yes               ✗   ours: readonly=no
```

`09` and `10` flipped with the reference count, as predicted — they were the same lifecycle defect seen from two sides. What is left is
`01` (the script's own first `stat`), `08` (case-only rename) and `12a` (attributes), and each has a distinct question already written down.

## L2 after `SetBasicInfo`: **11 of 12**

```
01 root: not a directory        ✗  (the script's own first stat)
02 … 07, 09, 10, 11             ✓
08 case-only rename: <missing>  ✗
12a attributes: readonly=yes    ✓   ← flipped by `SetBasicInfo@12` (raw shows `R`)
```

Only two classes left, and both have a written next step: `01` is to be moved to the end of the script (cheapest discriminator between
"script order" and "backend"), and `08` is the case-only rename path.

## `01` moved to the end: still red — so it is not script ordering

With the `01` class executed **last** (after every other class has run), it still answers `not a directory or missing`. And in the isolated
instrument (where `[ -d 'Z:/' ]` runs first) the same expression answers `d=0`.

So the root's `stat` works when it is the *first* thing that happens and fails once other classes have run. That is **state dependent on our side**, and
the bisect is cheap: run the script with an early `exit` after each class and note the first one after which `01` starts failing.

`08` (case-only rename) is unchanged by this — it is the remaining functional gap, and it has its own criterion to run (manager 442: fold matching,
case-only is not a collision, identity preserved).

## `01` cracked — and it was the **script**, not the backend

The bisect (ask `[ -d ]` after every class) answered `yes` at every point, so no class leaves state that breaks the root stat. The difference was
in the question itself:

```
[ -d 'Z:' ]   -> d=1     ← the script's `ROOT="${ROOT%/}"` leaves a bare drive letter
[ -d 'Z:/' ]  -> d=0     ← the same root, with its separator
```

Git Bash does not accept a bare drive letter as a directory. On NTFS the root is a plain path, so the class was never asked this question there —
which is exactly why `01` looked like "our side is broken" while every instrument that used `Z:/` passed. Fix: the script keeps `ROOT_AS_DIR="$ROOT/"`
for the class that asks "is the root a directory", with the measurement written next to it.

## L2 now: **11 of 12**, only `08` red

```
01 directory ✓   02 alpha ✓   03 ok ✓   04 not found ✓   05a/05b a,b, ✓   06 refused ✓
07 old=gone new=present ✓   09 name=gone ✓   10 beta ✓   11 refused ✓   12a readonly=yes ✓ (raw `R`)   12b ✓
08 case-only rename: <missing> ✗
```

## `08`: the `Rename` never reaches the backend at all

One mount, `printf x > Z:/l2-renamed`, then `mv Z:/l2-renamed Z:/L2-RENAMED`:

```
PRE  ls -1 Z:/          -> l2-renamed
MV   mv … ; echo rc=$?  -> rc=0
POST ls -1a Z:/         -> . .. l2-renamed        ← still the old spelling
STAT Z:/l2-renamed      -> ino 8948360027621016923
CALL …                  -> no `Rename` in the backend call log
```

So the FSD (or the client above it) decides the request is a **no-op** and answers success: our side is never asked, the display spelling stays, and
the class is red. That is a different question from "our rename is wrong" — the rename path was never exercised here.

Prime suspect, and the reason NTFS behaves differently: we answer the *destination* spelling (`L2-RENAMED`) as an existing name (the fold), and a
case-insensitive client that compares "same file → nothing to do" therefore returns early. The next experiment is to ask whether the request reaches
`SetInformation`/`Rename` when the FSD's own volume parameters are compared against `memfs` (`CasePreservedNames` **and** `CaseSensitiveSearch`),
which is the one place memfs declares both.

## `08` boundary run: neither route reaches us, and the flag is not the condition

```
mv  Z:/l2-renamed Z:/L2-RENAMED   -> rc=0    POST: l2-renamed      (no Rename call)
ren Z:\l2-second  L2-SECOND       -> rc=0    POST: l2-second       (no Rename call)
```

So it is not an MSYS shape (both routes use the same Win32 rename entry point) — but it is also **not** the volume-parameter guess: setting bit 0
(`CaseSensitiveSearch`) and re-running the same two commands changed **nothing**. The short-circuit is therefore inside the FSD (or above it), and
the next step is to read its source for the case-only branch rather than to keep flipping flags on our side.

Our own `rename_shell` was checked and does **not** short-circuit: it converts both names and calls the backend. The absence of a `Rename` in the
call log is genuine.

## `08` disposition (U16): recorded as a platform difference, and the script now says so

`fsop.c` dispatches `FileRenameInformation` straight to `Interface->Rename`, and flipping `CaseSensitiveSearch` changes no reading, so the
short-circuit is above user mode. The class therefore has three possible answers and the script now names all three:

```
08 case-only rename: renamed                              (NTFS-like behaviour — not what a mounted volume does)
08 case-only rename: request-not-delivered (…unchanged)   (the true reading here)
08 case-only rename: <missing>
```

`request-not-delivered` is not a pass — it is the measurement. Whether case-only rename is a promised feature belongs to spec/ADR-009; if it is, it
becomes P3-4b work through a mechanism that does not depend on this request reaching us.

**Mounted-side scoreboard: 11 of 11 reachable classes green** (`08` carries a documented platform difference).

## Bulk text replacement: the rule, written down (419 / 486)

Four accidents came from the same root — a replacement tool that does not respect boundaries:

```
419  perl -0pi with \n patterns silently did nothing on a CRLF tree (three times, and the edit "succeeded")
486  a \' -> ' replacement edited three unrelated files (caught by the compiler, not by the tool)
```

So, from now on:

1. **`.gitattributes` decides line endings**, not any developer's `core.autocrlf`. The index is LF; a Windows checkout is CRLF by design.
2. **A bulk replacement names its directory set first** and is never run over `crates/` as a whole.
3. **After any bulk replacement, `git status --porcelain` is read as a list of files** and compared against what was intended — an unexpected path is a
   bug in the replacement, not a bonus fix.
4. **The pre-commit hook prints every staged path**, so that list is visible before the commit exists, and refuses a CRLF blob in the index.

Enable the hook once per clone:

```sh
git config core.hooksPath .githooks
```

## QA L2 blockers, first probe: even the **root** is unusable with the real backend (491)

`scripts/drafts/subdir-probe.rs.txt` mounts the **real** backend (`ApiBackend` over `fs-workspace`) and asks bash the three questions:

```
MOUNT result = Ok(true)                       ← the mount call reports success
BACKEND read_dir(root) = Ok([])               ← the workspace root lists, empty (correct: nothing created yet)
MKDIR   mkdir Z:/l2-dir   -> No such file or directory   rc=1
LSROOT  ls Z:/            -> No such file or directory
WRITE   Z:/l2-dir/a       -> No such file or directory
LSDIR   ls Z:/l2-dir      -> No such file or directory
```

No `create:`/`open:` line from the shells appeared at all, i.e. **the FSD never reached our callbacks**. That is a *stronger* failure than
QA's ①/② — they had the root listing working, so their mount was live and the defect is on the subdirectory path. Here the drive is not usable at
all, which points at the mount handshake (volume info / root open) as the first suspect, not at subdirectory resolution.

**Difference to nail down with QA before diagnosing further:** with `MockBackend` the same shape works (`mounted_read_only`, `mounted_write_chain`),
so the variable is the real backend, not the mount code path — but the first reader to check is the handshake sequence, printed from inside the
callbacks, because "the mount returned Ok" and "the drive answers" are two different facts.

## The mount reports success and is still not there (`MountReport`, real backend)

```
MOUNT create=0x00000000 mount=0x00000000 start=0x00000000 unmount=0xC000000D dispatcher=0x00000000
RUST read_dir("Z:")    -> NotFound (os error 3)
RUST read_dir("Z:/")   -> NotFound        ← the 317 trap is not the explanation
bash ls 'Z:/'          -> No such file or directory
BACKEND read_dir(root) -> Ok([])          ← the workspace itself is fine
```

`create`, `mount` and `start` all returned 0 and the dispatcher result is 0 (alive), yet **neither Rust nor bash can see the volume**, and no
callback line appears. So "the mount call succeeded" and "the volume is reachable" are two different facts, and the first does not imply the second —
exactly the shape the `MountReport` doc block predicts for a dead dispatcher, except the dispatcher says it is alive.

Next readings needed, in this order:
1. **log every callback entry** (`Create`/`GetVolumeInfo`/`GetSecurityByName`/`ReadDirectory`) — "no output" is currently used as evidence that the FSD
   never called us, but nothing is logged on the success path, so that inference is unsupported (the eprintln calls only fire on error branches);
2. **run the same mount code with `MockBackend` from this same test file** — QA's working mount uses a different backend *and* a different scaffold, so
   "backend" and "scaffold" are two variables and only one has been varied so far.

## The real backend answers about the root correctly — so the wiring is not the break

```
create_or_open("/", Open)     = Err(NotADirectory)        ← the **file** door refusing a directory, which GSBN falls back from
open_dir("/")                 = Ok(0)
dir_binding("/")              = Some(DirOutcome { id: 0, binding: (0, []) })
get_info(0)                   = Ok((Directory, 0))
read_dir(0)                   = Ok(0 entries)
create_or_open("/x", Create)  = Ok(1)
read_dir(0) after create      = Ok(["x"])
```

So the mock's root special case is not needed: the real backend resolves `/`, opens it as a directory, binds it and lists it. The earlier hypothesis
("ApiBackend has no answer for `/`, so the handshake dies there") is therefore **wrong**, and the remaining difference between QA's working mount and
mine is the **scaffold**, not the backend.

Next reading, still unvaried: run the *same* mount code from the *same* test file with `MockBackend`. Two variables have been changed at once so far
(backend **and** scaffold), and only by holding one still does the third answer mean anything.

## The probe is the thing that is broken — narrowing so far (494)

```
Mock + 1 MiB volume + 1 GiB volume + Z: / X: / V: / W: / U: / P: + fs-api crate + fs-winfsp crate
   -> every combination: RUST write = os error 3, read_dir = os error 3, bash ls = No such file
MOUNT create=0 mount=0 start=0 dispatcher=0    (the call reports success every time)
```

And yet, **in the same test binary**, `mounted_write_chain` mounts on `X:` and writes, reads, truncates and deletes successfully. So the mount machinery,
the volume geometry, the backend and the drive letter are **all excluded** — what is left is *how the probe uses `mount_read_only`*.

The one structural difference visible in the code: `mounted_write_chain` **returns `Err` from its probe when anything fails**, so a mount that does not
work is reported as "the mount did not work" (and its assertion fails loudly). My probe prints the failure and **returns `Ok(())`**, which makes
`looked_like_a_mount` trivially true — the mount is then torn down and the prints are the only trace. That is exactly the "instrument that cannot fail"
shape (434): a probe whose success condition is `Ok(())` measures nothing.

Next step: rewrite the probe so that **every failure comes back as `Err`** and let the existing mount machinery fail the test. Until that is done, no
reading from this probe means anything about the file system.

## Ready-timing probe: the variable is **the test file**, not the operation (503)

Known-good and known-bad, same crate in some cases, same operations, letters free (`Get-PSDrive` lists only C,D,E,F,S,Y):

```
fs-winfsp::mounted_write_chain        (Mock, X:)   write/read/set_len/remove  -> PASS, 0.04 s
fs-api::mounted_real_backend          (Api, V:)    write/read/list/create/list -> PASS, 2.1 s (incl. the reverted wait)
fs-api::mounted_mock_same_shape       (Mock, U:)   same as above              -> PASS, 0.05 s
fs-winfsp::instrument_ready_timing    (Mock, V: X:) write at t=4 ms -> os error 3 ; read_dir polled 200 ms -> never succeeds
```

So: same backend, same letter, same operation, same crate — and one shape serves while the other never does. The wait experiments are reverted; nothing in `src/` was changed by this round.

Next experiment is a bisect, not a hypothesis: copy `mounted_write_chain.rs` and change **one** thing per run (probe body → my body; assertions → removed; letter → V:), until the copy stops working. The first single change that flips PASS→FAIL is the variable.

## Bisect (504): steps 4 and 2 both excluded — the difference is inside the probe body

```
step 4  the never-working body appended to the *working* file (same binary, same crate, same call)
        -> BISECT4 write = os error 3      ⇒ the compile unit is NOT the variable
step 2  add the "is the letter free?" lookup *before* mounting (a failed lookup on a not-yet-existing drive)
        -> BISECT2 pre_mount_exists=false ; write still os error 3   ⇒ the pre-mount probe is NOT the variable
```

So the variable is inside the probe **body** itself, and only two textual differences remain between the working body and mine:

1. `root_of(path).join("written.txt")` versus `format!("{root}probe.txt")` — same string on Windows, so this is the least likely;
2. **what the body does before writing**: the working body writes `hello layer-fs` (13 bytes) where mine writes `x` (1 byte), and mine has an extra
   `trim_end_matches` on the path text.

The next run changes exactly one of those two, in the working file, and reads the same two lines.

## Bisect (506): the run configuration is not the variable either

Same file, same crate, same filter style, three repeats each:

```
full file (2 tests, nextest runs them in parallel)   -> 2 passed, 2/2 runs
my new test, filtered, alone                          -> write = os error 3, 3/3 runs
the *original* test, filtered, alone                  -> PASS, 0.05 s
```

So it is not "two mounts warm something": the original test passes **alone**, mine fails **alone**, in the same file. The variable is inside the body after
all — and both path constructions (`Path::join` and `format!`) fail in my body, so the difference is not how the path string is built.

What is left between the two bodies:

1. the original **checks the letter is free by a call I did not make** in that exact form (bisect step 2 used `Path::exists`, the original uses its own
   helper) — the *kind* of pre-mount call may matter, not just "a call happened";
2. my body has a `println!` of both `PathBuf`s **before** the write, and it writes twice (once per construction) where the original writes once;
3. my body returns the write's `Result` while the original returns an outcome computed after four operations.

The decisive next run is a copy of the original body **verbatim** under a new test name: if that passes alone, the delta is one of (2)/(3) and can be
bisected line by line; if it fails, the name/filter is somehow involved and the whole comparison needs re-examining.

## Bisect (507): the body is fine — what my probe *omitted* is not

```
verbatim copy of the working body, new name, run alone   -> PASS  (X: and V:)
same copy + a print of both PathBufs *before* the write  -> PASS
same copy renamed to the name that used to fail          -> PASS
```

So the failing shape is not "a print before the write", not the test name, not the letter, not the path construction. What the failing body did **not** do is the
rest of the working body: **read the bytes back, `set_len`, remove the file**. And it did one thing the working body does not: **two writes to the same
path in a row**.

The next single change is therefore: take the verbatim copy and delete its `read`/`set_len`/`remove`, leaving only the write. If that fails, "a mount whose
first and only operation is a create+write" is the shape, and the defect is in how the very first operation after mount is served — which is exactly the
half-alive mount QA saw, with the extra detail that the *content* of the first operation decides it.

## Bisect (508): replay is deterministic — and the delta is not what it looks like

```
bisect6_replay (the body that failed before, re-created verbatim)   -> BOTH writes os error 3, 2/2 runs
write_only_first_operation (same file, same mount call, one write)  -> PASS, 2/2 runs
write_only + an unused second PathBuf built with format!            -> PASS
write_only + a second write to the same path                        -> PASS
```

So the same file, same mount call, same letter, same backend serves one body and not the other, deterministically. The remaining differences of the
failing body are: it **returns the write's `Result`** (instead of `Ok(())`), and it holds **two `PathBuf`s alive across the write**. Everything else has
been changed one at a time and kept passing.

Next single change: make `write_only_first_operation` return the write's `Result` unchanged — the last structural difference.

Note for the record: four bisect steps have now landed on "it still fails / it still passes" without moving, which is the shape of a variable that is not in
the code being edited. The one thing all failing shapes had in common so far is that they were created **after** the working ones in the same file and
run **filtered** — a property that has to be tested deliberately (not assumed away) before more body-level bisecting.

## Experiment A (509): position in the file is not the variable either

The failing body placed **first** in the file (before the working test's definition) still answers `os error 3` on its first write, 2/2 runs. So the
test-harness ordering / link order is excluded.

What is left, and it is now the honest shortlist:

1. **session / machine state** — something left behind by earlier mounts on this machine (the QA blockers happened on this same box in the same period).
   Test: a clean session (new shell, ideally a reboot or another machine), then run the failing body alone.
2. **codegen-sensitive difference** — the failing body holds two `PathBuf`s across the write and returns `Err`; adding either alone did not flip it.
   If a codegen difference really changes filesystem behaviour, something is unspecified (UB) somewhere, and the cheap probe is to run the same test
   under `cargo test` instead of `cargo nextest` (different harness, same code).

Until one of those is read, the mounting defect stays **unexplained**, and it is recorded as such rather than as a product defect.

## Harness contrast (510): `cargo test` and `cargo nextest` fail **identically**

```
cargo test     : via_join = os error 3 ; via_format = os error 3
cargo nextest  : via_join = os error 3 ; via_format = os error 3
```

So process isolation / harness / link order are all excluded (A and this), and the delta really is inside the body. Two bodies that differ only in
*constructing one extra `PathBuf`* produce different filesystem results — that is not a property a filesystem can have, and it moves the leading
suspicion to **undefined behaviour in our FFI layer** (an argument read from the wrong slot, an uninitialised value, a lifetime that the compiler is free
to reshape). The heap-corruption saga had exactly this flavour.

Decisive cheap probe, using instrumentation we already have: share the `MockBackend` through an `Arc`, run both bodies, and compare `mock.calls()`.

- failing body: if `CreateOrOpen` **never appears** in the call log, the request dies above our callbacks (FSD/mount), so the body's shape changed how the
  OS routed it — that would be strange but it is a reading;
- failing body: if `CreateOrOpen` **appears and returns Ok** while the caller gets `os error 3`, the *answer* is corrupted on the way back, which points at
  the out-parameter/signature layer (where two wrong signatures have already been found by measurement).

## The failing body's request **does** reach us — and it is never a `Create` (511)

Shared mock, the failing body, then the call log:

```
CALLS via_join = os error 3 ; via_format = os error 3
CALL CreateOrOpen([92], Open)                                "\\"            (root)
CALL CreateOrOpen([92, w r i t t e n . t x t], Open)          "\written.txt"
CALL CreateOrOpen([92, w r i t t e n . t x t], Open)          again
CALL CreateOrOpen([92], Open) ...                            (repeat)
```

So the FSD asks us to **open** a file that does not exist, again and again, and never follows up with a `Create`. The OS's `os error 3` is
`ERROR_PATH_NOT_FOUND`; the FSD's own create-upgrade (`fsop.c`: `FileOpenIf` = GSBN → Open → **Create, only on exactly `0xC0000034`**) never fires, which
means the answer we gave is not `STATUS_OBJECT_NAME_NOT_FOUND`.

Our handler for a missing file in `\written.txt` is the file door's `create_or_open(..., Open)`, whose `NotFound` maps to… that mapping is the next thing
to read, and the criterion is precise: **the status returned for a missing name must be `0xC0000034`**, or Windows will not upgrade an `Open` into a
`Create` and no file can be created through the mount in this shape.

## **Both** gates answer `0xC0000034` — and Windows still will not create (512)

```
door-status create/open("\\", Open)            -> 0xC0000034
door-status create/open("\written.txt", Open) -> 0xC0000034
gsbn-status("\written.txt")                   -> 0xC0000034
gsbn-status("\\")                              -> 0xC0000034
STATUS via_join = os error 3 ; via_format = os error 3
```

So the status we return is the one the upgrade requires, **twice over**, and no `Create` is ever sent. That kills the "wrong status" explanation and moves the
question one step: what does Windows actually ask for? `std::fs::write` opens with `CREATE_ALWAYS`, i.e. `FILE_OVERWRITE_IF`, and WinFsp routes that
disposition to **`Overwrite` (slot 5)**, not to `Create`. Our call log shows no `Create`, no `Overwrite` line either (the mock only logs `WorkspaceOps`
calls, and `Overwrite` reaches us as `create_shell` with disposition `Supersede`… or not at all).

Next probe, one line: log the **slot** that arrived (`Create` vs `Overwrite`) with its `CreateOptions` and the status we answer. If `Overwrite` is being
called for a file that does not exist, the answer must still let the create happen.

## Slot probe (513): `Overwrite@5` is **never** called — only `Create@0` with disposition `Open`, four times

```
SLOT Create@0 arrived, options -> Open      (x4)
STATUS via_join = os error 3 ; via_format = os error 3
```

So the assumption that `std::fs::write` (CREATE_ALWAYS → `FILE_OVERWRITE_IF`) reaches `Overwrite` is **wrong here**: the only slot that arrives is `Create`,
carrying disposition `Open`, and we answer `0xC0000034` every time and never get a follow-up.

That contradicts the documented `FileOpenIf` / `FileOverwriteIf` sequences, which means the request is being handled **before** those paths, or the sequence
differs for a mounted volume in this state. The instrument this file has not used yet is our own **debug log**, which records the calls WinFsp actually makes
and their statuses — that is the next reading, and it must come before another guess about which door is wrong.
