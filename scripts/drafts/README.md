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
