# ProcessKit Roadmap

## v0.1.0 (current)
Child-process launch and supervision behind a C ABI: JSON config in / JSON result out, pipe capture with bounded buffers and truncation flags (reader threads, so a full pipe never deadlocks the child), supervisor-enforced timeouts, tree-kill via a Windows Job Object with KILL_ON_JOB_CLOSE (the orphan guarantee), a CommandLineToArgvW-compatible command-line quoter, and elevation via ShellExecuteExW with its OS restrictions enforced at config time (no capture, no tree-kill).

## Next, in order
1. **stdin feeding** (`"stdin": "..."` in the config, written to the child on a writer thread with the same bounded, never-block-the-supervisor discipline as capture): several tools prompt or read patches/config from stdin, and v0.1 gives them EOF and nothing else.
2. **Streaming output callbacks** (an optional C callback invoked per output chunk as it arrives, in addition to the buffered result): progress parsing — yt-dlp's `[download]  42.0%` lines — is the family's most-wanted supervision feature and the reason JobKit's progress engine wants a feed, not a post-mortem.
3. **Process-group kill on Unix via an optional feature** (`process-group`): off Windows, tree-kill is child-only because signaling a process group needs libc, which the zero-dependency default build refuses. An opt-in feature that links libc for `killpg` closes the gap for consumers who accept the dependency knowingly.
4. **Detached start** (`"detached": true`: start a process explicitly outside the job, documented as giving up the orphan guarantee in exchange for a child that survives the parent — the "open the installer and exit" shape, made explicit instead of accidental).

## Explicitly not planned
- A shell: ProcessKit runs programs, not command strings. Callers who want a shell name `cmd` / `sh` as the exe themselves, so quoting and injection stay their visible choice, not a hidden default.
- Priority/affinity/resource limits in the config: the Job Object could carry them, but no app in the family has needed them yet; they arrive with a named consumer, not speculation.
- Async-everything redesign: one blocking handle with poll/wait slices has proven sufficient at this scale; callbacks (item 2) cover the one real push use case.
