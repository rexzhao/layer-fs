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
