# ProcessKit

## TL;DR

One clean C ABI for launching and supervising child processes — the thing every app used to hand-roll, with orphaned and hung children handled ad hoc. Describe a run in one JSON object, get one JSON result: captured stdout/stderr that can never deadlock the child on a full pipe, a timeout that actually kills, tree-kill backed by a Windows Job Object (so a child cannot outlive its supervisor), and elevation rules stated up front instead of silently dropped. Zero dependencies, one small DLL.

```c
char *result;
pk_run("{\"exe\":\"yt-dlp\",\"args\":[\"-f\",\"best\",\"https://example.invalid/v\"],"
       "\"timeout_ms\":30000}",
       &result);
printf("%s\n", result);   // {"state":"exited","exit_code":0,...,"stdout":"..."}
pk_free_string(result);
```

## Quickstart

```c
#include "processkit.h"

/* Synchronous: run to completion. */
char *result = NULL;
if (pk_run("{\"exe\":\"cmd\",\"args\":[\"/C\",\"echo\",\"hello\"]}", &result) == PK_OK) {
    /* {"state":"exited","exit_code":0,"pid":...,"elapsed_ms":...,
        "stdout":"hello\r\n","stderr":"", ...} */
    puts(result);
    pk_free_string(result);
} else {
    fprintf(stderr, "%s\n", pk_last_error());  /* config was invalid */
}

/* Asynchronous: start, poll, wait, close. */
PkProcess *proc = NULL;
if (pk_start("{\"exe\":\"ffmpeg\",\"args\":[\"-i\",\"in.mp4\",\"out.mp3\"],"
             "\"timeout_ms\":600000}", &proc) == PK_OK) {
    char *final = NULL;
    pk_wait(proc, PK_WAIT_INFINITE, &final);   /* full result JSON */
    puts(final);
    pk_free_string(final);
    pk_close(proc);                            /* kills the tree if still running */
}
```

## Why this exists

Every app in the family had grown its own child-process code, and each handled the failure modes differently — or not at all (WinDevKit pain point #16). ytTrayDL supervises yt-dlp and ffmpeg; InstallScope runs an NSIS installer and a PortableApps launcher; the next app would have written it a fifth time. The bugs are always the same five:

- **The full-pipe deadlock.** A child that fills its stdout pipe blocks mid-write while the parent blocks waiting for it to exit. ProcessKit drains both pipes on reader threads from the moment the child starts; output beyond the capture cap is dropped and flagged, never back-pressured.
- **The orphan.** The parent exits (or crashes) and the child — and its children — keep running. On Windows the child joins a Job Object with `KILL_ON_JOB_CLOSE` *before it is resumed*, so closing the handle, or the parent dying, ends the whole tree at the OS level.
- **The hung child.** Timeouts are enforced by the supervisor itself — on `pk_run`, inside `pk_wait`, and even on `pk_poll`. A timed-out process is killed (tree included) and reported as `"state":"timeout"`, never confused with a natural exit.
- **The quoting bug.** Arguments with spaces and quotes corrupt hand-built command lines. The Windows command line is built with the standard `CommandLineToArgvW`-compatible quoting rules, and the quoter is unit-tested against the nasty vectors (trailing backslashes, embedded quotes, empty args).
- **The elevation surprise.** A non-elevated supervisor cannot capture an elevated process's output or put it in a job — an OS restriction. With ProcessKit, `elevate: true` requires `capture: false` and `kill_tree: false` in the config and fails with `PK_ERR_NOT_SUPPORTED` otherwise, with the reason in the error message. Nothing is silently dropped.

## The config

One JSON object in — for `pk_run` and `pk_start` alike:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `exe` | string | *(required)* | Program to run; a path, or a bare name to find on PATH |
| `args` | string array | `[]` | Arguments, unquoted — ProcessKit quotes them for the platform |
| `cwd` | string | inherit | Working directory for the child |
| `env` | object of strings | none | Added to the child's environment, merged over the inherited one |
| `capture` | bool | `true` | Pipe stdout/stderr and return them in the result |
| `max_capture_bytes` | uint | `1048576` | Per-stream capture cap; excess is dropped, `*_truncated` is set |
| `timeout_ms` | uint | `0` | Kill after this long; `0` means no timeout |
| `kill_tree` | bool | `true` | Kills take the whole process tree, not just the child |
| `hidden` | bool | `true` | No console window (Windows only; a no-op elsewhere) |
| `elevate` | bool | `false` | Run via the shell "runas" verb (Windows only) |

Unknown keys are an error (`PK_ERR_PARSE`) — a typo'd key must never silently change what a run does. Malformed JSON is `PK_ERR_PARSE` too; a well-formed config with a missing/empty `exe` or wrongly-typed values is `PK_ERR_INVALID_ARG`.

## The result

One JSON object out of `pk_run` / `pk_wait`:

```json
{"state":"exited","exit_code":0,"pid":1234,"elapsed_ms":42,
 "stdout":"...","stderr":"...",
 "stdout_truncated":false,"stderr_truncated":false}
```

`state` is `exited`, `timeout`, `killed`, `spawn_failed`, or `running` (a `pk_wait` whose own budget expired — the process was left alone). `exit_code` is `null` unless the process exited on its own. A spawn failure is a *result* for `pk_run` (the call returns `PK_OK`; `pk_last_error()` explains why nothing ran) and an *error* for `pk_start` (there would be nothing to hand back a handle to).

`pk_poll` returns the same shape minus the output fields, and enforces the timeout as a side effect — polling is supervision, not just observation.

## The API in one screen

| Area | Calls |
|---|---|
| Synchronous | `pk_run` (config JSON → result JSON) |
| Asynchronous | `pk_start`, `pk_poll`, `pk_wait` (0 = poll once, `PK_WAIT_INFINITE` = forever), `pk_kill`, `pk_close` |
| Errors | Integer `PK_*` codes + thread-local `pk_last_error()`; string results are caller-owned, freed with `pk_free_string` |

Handles are thread-safe; supervision is serialized inside the library.

## Honest caveats — platform limitations

- **Tree-kill is Windows-complete.** The Job Object kills the real tree. Off Windows there is no zero-dependency way to signal a process group (that needs libc), so kill/timeout/close terminate **the child only** — a grandchild the child spawned can survive, and if it inherited the capture pipes it can hold them open (see the next point). This is stated, not papered over.
- **Capture ends on a grace window, not on EOF alone.** After the child exits, output is drained for up to ~300 ms waiting for pipe EOF. A grandchild still holding a pipe open past that window means later output from it is not captured; the result is never held hostage by it.
- **Captured output is lossy UTF-8.** A tool writing another encoding, or a multibyte character cut at the capture cap, shows U+FFFD replacement characters. Exit codes and states are unaffected.
- **Exit code `-1` off Windows** means the process died by signal without a code (and was not killed through ProcessKit — those report state `killed` with a null code).
- **Elevation is Windows-only** (`runas` via ShellExecuteExW), returns no output (capture is refused, above), and if the user declines the UAC prompt the call fails with `PK_ERR_ACCESS_DENIED`.
- **`hidden` is Windows-only.** There are no console windows to hide elsewhere; the key is accepted and ignored.
- **stdin is closed for the child.** There is no way to feed input in v0.1 (see the roadmap); the child's stdin is the null device / an immediately-closed pipe, so a child that reads stdin gets EOF rather than hanging on your terminal.

## Verification status

35 tests cover: the JSON engine (duplicate keys rejected, surrogate pairs, depth cap), config validation (unknown keys, missing/empty `exe`, wrong types, the elevate combinations), the command-line quoter vectors, the capture buffer (truncation accounting, zero cap), and live supervision on the host — echo capture is byte-exact, non-zero exits pass through, env and cwd reach the child, a 30-second sleeper dies to a 300 ms timeout and reports `timeout`, kill-via-handle reports `killed`, capture truncation flags a noisy child without deadlocking it, a missing exe is `spawn_failed`, and `start`/`poll`/`wait` agree with `pk_run`. The Windows code paths (CreateProcessW, Job Object, ShellExecuteExW) compile and lint clean for `x86_64-pc-windows-msvc` in the local gate; Windows CI runs the full gate plus a real MSVC build and link and the same suite on Windows itself.

Building: `cargo build --release` → `processkit.dll` / `libprocesskit.so`; `cargo test --locked`.

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what's next.
