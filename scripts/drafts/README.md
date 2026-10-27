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
