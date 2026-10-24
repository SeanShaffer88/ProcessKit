# ProcessKit — agent notes

- The elevate constraints are load-bearing, not incidental: `elevate` + capture and `elevate` + kill_tree MUST stay refused with PK_ERR_NOT_SUPPORTED (an elevated process's pipes/job cannot be controlled by a non-elevated supervisor — OS restriction). Do not "fix" a refusal report by silently dropping capture or the job; the config error is the feature.
- Job assignment order on Windows is load-bearing: CreateProcess with CREATE_SUSPENDED → SetInformationJobObject (KILL_ON_JOB_CLOSE) → AssignProcessToJobObject → ResumeThread. Moving the resume earlier reopens the escape race the suspension exists to close.
- finish_output must never join reader threads unboundedly: a grandchild holding an inherited pipe open hangs the supervisor on exactly the orphans this DLL exists to control (this bit during the v0.1 build — a timeout test took the child's full 30 s). The bounded EOF grace (platform::DRAIN_GRACE_MS) + detach is the design; keep it.
- Capture buffers are capped and the truncated flag is the contract: excess output is dropped and flagged, never back-pressured into the child's pipe.
- Zero dependencies; Win32 bindings are hand-declared in src/platform.rs and every extern block carries #[link]. The JSON engine (src/json.rs) is the family design — duplicate keys in one object are a PARSE ERROR by design; do not "fix" that into last-wins.
- Tree-kill honesty: Job-Object-backed (real tree) on Windows, child-only off Windows. Docs must keep saying so; do not let platform parity be implied where it doesn't exist.
- Gate: WinDevKit tools/gate.sh (fmt + clippy both targets + full tests). Check/clippy do not link — CI's real Windows build is the link gate.
