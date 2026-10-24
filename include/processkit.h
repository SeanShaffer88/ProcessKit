/* ProcessKit.dll — child-process launch and supervision.
 *
 * One clean C ABI for the thing every app in the family used
 * to hand-roll, with orphaned and hung children handled ad
 * hoc (WinDevKit PAIN_POINTS #16): spawn a child, capture its
 * output without ever letting a full pipe deadlock it, enforce
 * a timeout, kill the whole tree, and get one JSON result.
 *
 * A run is configured by ONE JSON object:
 *
 *   {"exe": "yt-dlp", "args": ["--version"], "cwd": "...",
 *    "env": {"K": "V"}, "capture": true,
 *    "max_capture_bytes": 1048576, "timeout_ms": 0,
 *    "kill_tree": true, "hidden": true, "elevate": false}
 *
 *   exe               (string, required, non-empty)
 *   args              (array of strings, default [])
 *   cwd               (string, optional; default: inherit)
 *   env               (object of strings, default none; merged
 *                      OVER the inherited environment)
 *   capture           (bool, default true) pipe stdout/stderr
 *   max_capture_bytes (uint, default 1048576) PER-STREAM cap;
 *                      output beyond it is dropped and the
 *                      result's *_truncated flag is set — the
 *                      child is never blocked by a full pipe
 *   timeout_ms        (uint, default 0 = no timeout)
 *   kill_tree         (bool, default true) kills take the whole
 *                      process tree (Job Object on Windows;
 *                      child-only off Windows — see below)
 *   hidden            (bool, default true) no console window
 *                      (Windows only; a no-op elsewhere)
 *   elevate           (bool, default false) launch via the
 *                      shell "runas" verb (Windows only)
 *
 * Unknown keys are an error (PK_ERR_PARSE), never ignored.
 *
 * ELEVATION RULES (OS restrictions, stated up front rather
 * than silently dropped): an elevated process's pipes and job
 * cannot be controlled by a non-elevated supervisor, so
 * "elevate": true REQUIRES "capture": false and
 * "kill_tree": false and fails with PK_ERR_NOT_SUPPORTED
 * otherwise.
 *
 * Results are ONE JSON object:
 *
 *   {"state": "exited", "exit_code": 0, "pid": 1234,
 *    "elapsed_ms": 42, "stdout": "...", "stderr": "...",
 *    "stdout_truncated": false, "stderr_truncated": false}
 *
 * state is "exited" | "timeout" | "killed" | "spawn_failed"
 * (or "running" for a pk_wait that timed out — the process is
 * left alone in that case). exit_code is null for timeout,
 * killed, and spawn_failed. Captured output is decoded as
 * LOSSY UTF-8: a tool that writes another encoding (or a
 * multibyte sequence cut at the capture cap) shows U+FFFD
 * replacement characters rather than failing the run.
 *
 * ORPHAN GUARANTEE: on Windows the child is assigned to a Job
 * Object with KILL_ON_JOB_CLOSE before it runs, so closing
 * the handle ends the whole tree — and so does the parent
 * process dying. Off Windows, closing with kill_tree set
 * kills the child (only the child: there is no std-only way
 * to kill a process group, and this header will not pretend
 * otherwise).
 *
 * All strings are UTF-8, NUL-terminated. char* results are
 * caller-owned: free with pk_free_string(). Errors set a
 * thread-local pk_last_error() (message + code).
 */
#ifndef PROCESSKIT_H
#define PROCESSKIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(PK_BUILD)
#define PK_API __declspec(dllexport)
#elif defined(_WIN32)
#define PK_API __declspec(dllimport)
#else
#define PK_API
#endif

enum {
    PK_OK = 0,
    PK_ERR_INVALID_ARG = 1,  /* missing/empty exe, wrong value types, null pointers */
    PK_ERR_NOT_SUPPORTED = 2,/* elevate combinations; elevate off Windows */
    PK_ERR_NOT_FOUND = 3,    /* exe (or cwd) does not exist */
    PK_ERR_ACCESS_DENIED = 4,/* permission denied; elevation declined */
    PK_ERR_PLATFORM = 5,     /* an OS call failed; see pk_last_error() */
    PK_ERR_INTERNAL = 6,
    PK_ERR_PARSE = 7         /* config is not valid JSON, or has an unknown key */
};

typedef struct PkProcess PkProcess;

PK_API const char *pk_version(void);
PK_API const char *pk_last_error(void);
PK_API int pk_last_error_code(void);
PK_API void pk_free_string(char *p);

/* Run a config to completion and return the result JSON
 * (caller-owned; free with pk_free_string()).
 *
 * Returns PK_OK when a result was produced — INCLUDING a
 * spawn failure, which is a result (state "spawn_failed"),
 * not a call failure; pk_last_error() then says why nothing
 * ran. Returns a PK_ERR_* code (and *out_result_json is
 * NULL) when the config itself is invalid. */
PK_API int pk_run(const char *config_json, char **out_result_json);

/* Start a process and return immediately with a handle.
 * Unlike pk_run, a spawn failure here IS a call failure:
 * a PK_ERR_* code and *out_handle is NULL. The handle is
 * thread-safe; supervision is serialized inside. */
PK_API int pk_start(const char *config_json, PkProcess **out_handle);

/* Non-blocking status as JSON:
 *   {"state": "running"|"exited"|"timeout"|"killed",
 *    "exit_code": <int|null>, "pid": <int>, "elapsed_ms": <int>}
 * Polling ENFORCES the configured timeout: polling a process
 * whose deadline has passed kills it and reports "timeout". */
PK_API int pk_poll(PkProcess *handle, char **out_json);

/* Block up to wait_ms for the process to finish and return
 * the full result JSON (the pk_run shape). wait_ms of 0
 * polls once; PK_WAIT_INFINITE (0xFFFFFFFF) waits forever.
 * If the wait itself expires, the result's state is
 * "running" (output captured so far included) and the
 * process is left running — nothing is killed. */
#define PK_WAIT_INFINITE 0xFFFFFFFFu
PK_API int pk_wait(PkProcess *handle, uint32_t wait_ms,
                   char **out_result_json);

/* Kill the process now (its whole tree when kill_tree is
 * non-zero and the platform supports it). The final state
 * is reported by the next pk_poll / pk_wait as "killed"
 * with a null exit code. Killing a finished process is
 * PK_OK, not an error. */
PK_API int pk_kill(PkProcess *handle, int kill_tree);

/* Close and free the handle. If the process is still
 * running and tree-kill was configured, it is killed first
 * (see ORPHAN GUARANTEE above); with tree-kill off, a
 * still-running process is detached. NULL is a no-op.
 * Anything through the handle after this is use-after-close. */
PK_API void pk_close(PkProcess *handle);

#ifdef __cplusplus
}
#endif
#endif /* PROCESSKIT_H */
