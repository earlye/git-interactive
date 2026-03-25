# Agent notes for `git-interactive`

## Context Management:

Keep responses terse; If the user calls your attention to a change in a plan, just say "got it" without re-explaining the change, unless you need more clarity.

## Rust: subprocess I/O and TUIs

When a crate uses `std::process::Command` while drawing a TUI (ratatui, alternate screen, etc.):

- **`Command::output()`** captures **stdout and stderr** into memory. They are **not** written to the terminal by default. Prefer this when you need to read output.
- **`Command::status()`** (and **`spawn()`** without redirecting stdio) uses the **default** stdio configuration: **stdin, stdout, and stderr are inherited** from the parent process. Any child (e.g. `git`, `gpg`) can **print directly into the TUI** and corrupt the screen or scrollback.

**Convention for this repo:** If you only need the exit code from a subprocess, either:

1. Use **`output()`** and ignore `stdout`/`stderr`, or  
2. Call **`stdin(Stdio::null())`**, **`stdout(Stdio::null())`**, and **`stderr(Stdio::null())`** (from `std::process::Stdio`) before **`.status()`**.

Apply the same care to **any** helper that wraps `Command` (e.g. a `git_c`-style function).

This was the root cause of “garbage” / duplicated text appearing below or inside the UI in `git-interactive-repos` when probing many repos: **`git`** was writing to **inherited stderr** during **`status()`** calls.

## Related

- `src/git-interactive-repos/src/main.rs` — `git_c()` uses `output()` with explicit pipes; `git_cmd_status()` nulls stdio before `status()`.
- `src/git-interactive-signing-key/src/main.rs` — `set_signing_key()` uses null stdio before `status()`; `get_current_signing_key()` and `get_gpg_keys()` use `output()` (captured).
