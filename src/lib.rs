//! ProcessKit — child-process launch and supervision behind a
//! small C ABI.
//!
//! The problem this solves (WinDevKit PAIN_POINTS #16): every
//! app hand-rolled its own child-process code, and orphaned or
//! hung children were handled ad hoc — a stuck yt-dlp in one
//! app, a forgotten installer child in another. ProcessKit's
//! rules:
//!
//! - **One config in, one result out — JSON both ways.** A run
//!   is fully described by one JSON object (exe, args, cwd,
//!   env, capture, timeout, tree-kill, hidden, elevate) and
//!   fully reported by one JSON result (state, exit code, pid,
//!   elapsed, captured stdout/stderr with truncation flags).
//!   Unknown config keys are errors, not suggestions.
//! - **A child can never outlive its supervisor by accident.**
//!   On Windows the child is assigned to a Job Object with
//!   KILL_ON_JOB_CLOSE, so closing the handle (or the parent
//!   dying) ends the whole tree. Timeouts kill the same way.
//! - **Capture can never deadlock the child.** Reader threads
//!   drain both pipes into bounded buffers from the moment the
//!   child starts; excess output is counted as truncated, never
//!   allowed to fill a pipe and wedge the child mid-run.
//! - **Elevation is honest about its limits.** An elevated
//!   process's pipes and job cannot be controlled by a
//!   non-elevated supervisor — an OS restriction, not a missing
//!   feature — so `elevate` refuses capture and tree-kill
//!   instead of silently dropping them.
//!
//! Config parsing, command-line quoting, capture buffering and
//! the supervision state machine live here and in `json`; OS
//! process control lives in `platform`; the C ABI lives in
//! `ffi`.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/processkit.h).
pub const PK_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;

pub type PkResult<T> = Result<T, (i32, String)>;

/// `pk_wait` wait_ms value meaning "wait indefinitely".
pub const WAIT_INFINITE: u32 = u32::MAX;

/// Default per-stream capture cap: 1 MiB.
pub const DEFAULT_MAX_CAPTURE_BYTES: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// A fully-validated run configuration (see `parse_config`).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub exe: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Extra environment variables, merged over the inherited
    /// environment (an entry here wins over an inherited one
    /// of the same name).
    pub env: Vec<(String, String)>,
    /// Capture stdout/stderr through pipes (default true).
    pub capture: bool,
    /// Per-stream capture cap in bytes (default 1 MiB). Output
    /// beyond the cap is discarded and the result's
    /// `*_truncated` flag is set; the child is never blocked.
    pub max_capture_bytes: usize,
    /// Kill the child after this many milliseconds (default 0
    /// = no timeout).
    pub timeout_ms: u64,
    /// Kill the whole process tree, not just the child
    /// (default true). Job-Object-backed on Windows.
    pub kill_tree: bool,
    /// Do not show a console window for the child (default
    /// true; Windows only — a no-op elsewhere).
    pub hidden: bool,
    /// Launch elevated via the shell "runas" verb (default
    /// false; Windows only). Cannot be combined with capture
    /// or tree-kill — see `parse_config`.
    pub elevate: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            exe: String::new(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            capture: true,
            max_capture_bytes: DEFAULT_MAX_CAPTURE_BYTES,
            timeout_ms: 0,
            kill_tree: true,
            hidden: true,
            elevate: false,
        }
    }
}

const KNOWN_KEYS: &[&str] = &[
    "exe",
    "args",
    "cwd",
    "env",
    "capture",
    "max_capture_bytes",
    "timeout_ms",
    "kill_tree",
    "hidden",
    "elevate",
];

fn invalid(msg: impl Into<String>) -> (i32, String) {
    (ERR_INVALID_ARG, msg.into())
}

fn bool_field(v: &json::Value, key: &str, default: bool) -> PkResult<bool> {
    match v.get(key) {
        None => Ok(default),
        Some(json::Value::Bool(b)) => Ok(*b),
        Some(other) => Err(invalid(format!(
            "\"{key}\" must be a boolean, got {}",
            other.type_name()
        ))),
    }
}

fn uint_field(v: &json::Value, key: &str, default: u64) -> PkResult<u64> {
    match v.get(key) {
        None => Ok(default),
        Some(val) => match val.as_i64() {
            Some(n) if n >= 0 => Ok(n as u64),
            _ => Err(invalid(format!(
                "\"{key}\" must be a non-negative integer, got {}",
                val.type_name()
            ))),
        },
    }
}

/// Parse and validate a config JSON object.
///
/// Error discipline: malformed JSON and unknown keys are
/// `ERR_PARSE`; a well-formed document whose values are
/// missing, empty, or of the wrong type is `ERR_INVALID_ARG`;
/// combinations the OS cannot honour (anything with
/// `elevate`) are `ERR_NOT_SUPPORTED`.
pub fn parse_config(text: &str) -> PkResult<Config> {
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("config is not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or((ERR_PARSE, "config must be a JSON object".to_string()))?;
    for (key, _) in obj {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err((ERR_PARSE, format!("unknown config key \"{key}\"")));
        }
    }
    let exe = match value.get("exe") {
        None => {
            return Err(invalid(
                "config is missing required key \"exe\"".to_string(),
            ))
        }
        Some(json::Value::Str(s)) if !s.is_empty() => s.clone(),
        Some(json::Value::Str(_)) => return Err(invalid("\"exe\" must not be empty")),
        Some(other) => {
            return Err(invalid(format!(
                "\"exe\" must be a string, got {}",
                other.type_name()
            )))
        }
    };
    let args = match value.get("args") {
        None => Vec::new(),
        Some(args_v) => {
            let arr = args_v.as_array().ok_or_else(|| {
                invalid(format!(
                    "\"args\" must be an array of strings, got {}",
                    args_v.type_name()
                ))
            })?;
            let mut args = Vec::with_capacity(arr.len());
            for (i, a) in arr.iter().enumerate() {
                match a.as_str() {
                    Some(s) => args.push(s.to_string()),
                    None => {
                        return Err(invalid(format!(
                            "\"args[{i}]\" must be a string, got {}",
                            a.type_name()
                        )))
                    }
                }
            }
            args
        }
    };
    let cwd = match value.get("cwd") {
        None => None,
        Some(cwd_v) => match cwd_v.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                return Err(invalid(format!(
                    "\"cwd\" must be a string, got {}",
                    cwd_v.type_name()
                )))
            }
        },
    };
    let env = match value.get("env") {
        None => Vec::new(),
        Some(env_v) => {
            let pairs = env_v.as_object().ok_or_else(|| {
                invalid(format!(
                    "\"env\" must be an object of string values, got {}",
                    env_v.type_name()
                ))
            })?;
            let mut env = Vec::with_capacity(pairs.len());
            for (k, val) in pairs {
                match val.as_str() {
                    Some(s) => env.push((k.clone(), s.to_string())),
                    None => {
                        return Err(invalid(format!(
                            "\"env.{k}\" must be a string, got {}",
                            val.type_name()
                        )))
                    }
                }
            }
            env
        }
    };
    let cfg = Config {
        exe,
        args,
        cwd,
        env,
        capture: bool_field(&value, "capture", true)?,
        kill_tree: bool_field(&value, "kill_tree", true)?,
        hidden: bool_field(&value, "hidden", true)?,
        elevate: bool_field(&value, "elevate", false)?,
        timeout_ms: uint_field(&value, "timeout_ms", 0)?,
        max_capture_bytes: uint_field(
            &value,
            "max_capture_bytes",
            DEFAULT_MAX_CAPTURE_BYTES as u64,
        )? as usize,
    };

    if cfg.elevate && cfg.capture {
        return Err((
            ERR_NOT_SUPPORTED,
            "elevate with capture is not supported: an elevated process's output \
             cannot be captured through pipes owned by a non-elevated process \
             (OS restriction). Set \"capture\": false to run elevated"
                .to_string(),
        ));
    }
    if cfg.elevate && cfg.kill_tree {
        return Err((
            ERR_NOT_SUPPORTED,
            "elevate with kill_tree is not supported: an elevated process cannot \
             be assigned to a job object owned by a non-elevated process \
             (OS restriction). Set \"kill_tree\": false to run elevated"
                .to_string(),
        ));
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Windows command-line quoting
// ---------------------------------------------------------------------------

/// Quote one argument for a Windows command line, following the
/// standard MSVCRT / `CommandLineToArgvW` rules: an argument
/// needs quotes when it is empty or contains a space, tab, or
/// double quote; inside quotes, backslashes are literal except
/// before a quote (where they double, plus one escape).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            c => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Build a full Windows command line from exe + args.
pub fn build_command_line(exe: &str, args: &[String]) -> String {
    let mut out = quote_arg(exe);
    for arg in args {
        out.push(' ');
        out.push_str(&quote_arg(arg));
    }
    out
}

// ---------------------------------------------------------------------------
// Capture buffer
// ---------------------------------------------------------------------------

use std::io::Read;
use std::sync::{Arc, Mutex};

/// A bounded byte buffer shared with a pipe reader thread.
/// Bytes beyond the cap are dropped and the truncated flag is
/// set — the reader keeps draining regardless, so a chatty
/// child can never fill its pipe and deadlock.
pub struct CaptureBuffer {
    cap: usize,
    inner: Mutex<CaptureInner>,
    eof: std::sync::atomic::AtomicBool,
}

struct CaptureInner {
    data: Vec<u8>,
    truncated: bool,
}

impl CaptureBuffer {
    pub fn new(cap: usize) -> CaptureBuffer {
        CaptureBuffer {
            cap,
            inner: Mutex::new(CaptureInner {
                data: Vec::new(),
                truncated: false,
            }),
            eof: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// True once the reader thread has seen EOF on the pipe.
    /// `finish_output` waits on this — briefly, not forever:
    /// a grandchild that inherited the pipe can hold it open
    /// long after the child died, and joining the reader
    /// thread unboundedly would hang the supervisor on exactly
    /// the orphaned processes this library exists to control.
    pub fn is_eof(&self) -> bool {
        self.eof.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_eof(&self) {
        self.eof.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn push(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let room = self.cap.saturating_sub(inner.data.len());
        let take = room.min(chunk.len());
        inner.data.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            inner.truncated = true;
        }
    }

    /// Lossy UTF-8 snapshot plus the truncation flag. Output is
    /// decoded lossily by design (documented in the header): a
    /// truncated multi-byte sequence at the cap renders as
    /// U+FFFD rather than failing the whole run.
    pub fn snapshot(&self) -> (String, bool) {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (
            String::from_utf8_lossy(&inner.data).into_owned(),
            inner.truncated,
        )
    }
}

/// Spawn a thread that drains `src` into `buf` until EOF,
/// marking the buffer's EOF flag on the way out.
pub fn pump<R: Read + Send + 'static>(
    mut src: R,
    buf: Arc<CaptureBuffer>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match src.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.push(&chunk[..n]),
            }
        }
        buf.mark_eof();
    })
}

// ---------------------------------------------------------------------------
// Results and supervision
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    Running,
    Exited,
    Timeout,
    Killed,
    SpawnFailed,
}

impl ProcState {
    pub fn name(&self) -> &'static str {
        match self {
            ProcState::Running => "running",
            ProcState::Exited => "exited",
            ProcState::Timeout => "timeout",
            ProcState::Killed => "killed",
            ProcState::SpawnFailed => "spawn_failed",
        }
    }
}

/// The full report of one run (the `pk_run` / `pk_wait` shape).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcResult {
    pub state: ProcState,
    pub exit_code: Option<i32>,
    pub pid: u32,
    pub elapsed_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl ProcResult {
    pub fn to_json(&self) -> String {
        let exit = match self.exit_code {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"state\":\"{}\",\"exit_code\":{},\"pid\":{},\"elapsed_ms\":{},\
             \"stdout\":\"{}\",\"stderr\":\"{}\",\
             \"stdout_truncated\":{},\"stderr_truncated\":{}}}",
            self.state.name(),
            exit,
            self.pid,
            self.elapsed_ms,
            json_escape(&self.stdout),
            json_escape(&self.stderr),
            self.stdout_truncated,
            self.stderr_truncated,
        )
    }
}

/// The lighter `pk_poll` shape: no captured output.
#[derive(Clone, Debug, PartialEq)]
pub struct PollInfo {
    pub state: ProcState,
    pub exit_code: Option<i32>,
    pub pid: u32,
    pub elapsed_ms: u64,
}

impl PollInfo {
    pub fn to_json(&self) -> String {
        let exit = match self.exit_code {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"state\":\"{}\",\"exit_code\":{},\"pid\":{},\"elapsed_ms\":{}}}",
            self.state.name(),
            exit,
            self.pid,
            self.elapsed_ms,
        )
    }
}

use std::time::{Duration, Instant};

/// A supervised child process. All supervision policy —
/// timeout enforcement, kill bookkeeping, result shaping —
/// lives here so it is identical on every platform; the
/// platform layer only knows how to spawn, probe, wait on,
/// and kill one process.
pub struct Running {
    cfg: Config,
    proc: Option<platform::Proc>,
    pid: u32,
    started: Instant,
    kill_requested: bool,
    final_result: Option<ProcResult>,
}

impl Running {
    pub fn start(cfg: Config) -> PkResult<Running> {
        let proc = platform::spawn(&cfg)?;
        let pid = proc.pid();
        Ok(Running {
            cfg,
            proc: Some(proc),
            pid,
            started: Instant::now(),
            kill_requested: false,
            final_result: None,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn timeout_deadline_passed(&self) -> bool {
        self.cfg.timeout_ms > 0
            && self.started.elapsed() >= Duration::from_millis(self.cfg.timeout_ms)
    }

    /// One supervision step. Returns the final result when the
    /// process has finished (naturally, by kill, or by timeout
    /// enforcement) and None while it is still running.
    fn check(&mut self) -> PkResult<Option<ProcResult>> {
        if let Some(r) = &self.final_result {
            return Ok(Some(r.clone()));
        }
        let proc = self
            .proc
            .as_mut()
            .ok_or((ERR_INTERNAL, "process already closed".to_string()))?;
        if let Some(code) = proc.poll_exit()? {
            let state = if self.kill_requested {
                ProcState::Killed
            } else {
                ProcState::Exited
            };
            let exit = if self.kill_requested {
                None
            } else {
                Some(code)
            };
            return Ok(Some(self.finalize(state, exit)));
        }
        if self.timeout_deadline_passed() {
            let tree = self.cfg.kill_tree;
            if let Some(proc) = self.proc.as_mut() {
                let _ = proc.kill(tree);
                let _ = proc.wait_exit(Some(5000));
            }
            return Ok(Some(self.finalize(ProcState::Timeout, None)));
        }
        Ok(None)
    }

    fn finalize(&mut self, state: ProcState, exit_code: Option<i32>) -> ProcResult {
        let (stdout, stdout_truncated, stderr, stderr_truncated) = match self.proc.as_mut() {
            Some(proc) => proc.finish_output(),
            None => (String::new(), false, String::new(), false),
        };
        let result = ProcResult {
            state,
            exit_code,
            pid: self.pid,
            elapsed_ms: self.elapsed_ms(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        };
        self.final_result = Some(result.clone());
        result
    }

    /// Non-blocking status. Enforces the configured timeout:
    /// polling a process whose deadline has passed kills it
    /// and reports `timeout`.
    pub fn poll(&mut self) -> PkResult<PollInfo> {
        if let Some(r) = self.check()? {
            return Ok(PollInfo {
                state: r.state,
                exit_code: r.exit_code,
                pid: r.pid,
                elapsed_ms: r.elapsed_ms,
            });
        }
        Ok(PollInfo {
            state: ProcState::Running,
            exit_code: None,
            pid: self.pid,
            elapsed_ms: self.elapsed_ms(),
        })
    }

    /// Block up to `wait_ms` for the process to finish
    /// (`WAIT_INFINITE` waits forever). Returns the full
    /// result; if the wait itself expires first, the result's
    /// state is `running` with the output captured so far and
    /// the process is left alone.
    pub fn wait(&mut self, wait_ms: u32) -> PkResult<ProcResult> {
        if let Some(r) = self.check()? {
            return Ok(r);
        }
        let wait_deadline = if wait_ms == WAIT_INFINITE {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(wait_ms as u64))
        };
        loop {
            // Block in short slices so the supervision step
            // (exit probe + timeout deadline) stays in charge.
            let mut slice_ms: u64 = 20;
            if let Some(d) = wait_deadline {
                let remain = d.saturating_duration_since(Instant::now()).as_millis() as u64;
                slice_ms = slice_ms.min(remain.max(1));
            }
            if self.cfg.timeout_ms > 0 {
                let remain_timeout = (self.cfg.timeout_ms as u128)
                    .saturating_sub(self.started.elapsed().as_millis())
                    as u64;
                slice_ms = slice_ms.min(remain_timeout.max(1));
            }
            let exited = match self.proc.as_mut() {
                Some(proc) => proc.wait_exit(Some(slice_ms as u32))?,
                None => None,
            };
            match exited {
                Some(code) => {
                    let state = if self.kill_requested {
                        ProcState::Killed
                    } else {
                        ProcState::Exited
                    };
                    let exit = if self.kill_requested {
                        None
                    } else {
                        Some(code)
                    };
                    return Ok(self.finalize(state, exit));
                }
                None => {
                    if let Some(r) = self.check()? {
                        return Ok(r);
                    }
                    if let Some(d) = wait_deadline {
                        if Instant::now() >= d {
                            return Ok(self.running_result());
                        }
                    }
                }
            }
        }
    }

    fn running_result(&mut self) -> ProcResult {
        let (stdout, stdout_truncated, stderr, stderr_truncated) = match self.proc.as_ref() {
            Some(proc) => proc.snapshot_output(),
            None => (String::new(), false, String::new(), false),
        };
        ProcResult {
            state: ProcState::Running,
            exit_code: None,
            pid: self.pid,
            elapsed_ms: self.elapsed_ms(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        }
    }

    /// Kill the child (its whole tree when `tree` is set and
    /// the platform supports it). The final state is reported
    /// by the next poll/wait as `killed`.
    pub fn kill(&mut self, tree: bool) -> PkResult<()> {
        if self.final_result.is_some() {
            return Ok(());
        }
        self.kill_requested = true;
        match self.proc.as_mut() {
            Some(proc) => proc.kill(tree),
            None => Ok(()),
        }
    }

    /// Release the process. If it is still running and
    /// tree-kill was configured, it is killed first — on
    /// Windows, closing the Job Object does this by itself
    /// (KILL_ON_JOB_CLOSE); off Windows the child is killed
    /// directly. With tree-kill off, a still-running child is
    /// detached (left running).
    pub fn close(mut self) {
        if self.final_result.is_none() && self.cfg.kill_tree {
            if let Some(proc) = self.proc.as_mut() {
                let _ = proc.kill(true);
                let _ = proc.wait_exit(Some(2000));
            }
        }
        // Dropping the platform Proc closes its handles; on
        // Windows the job handle close is the second, OS-level
        // guarantee that the tree cannot outlive us.
        self.proc.take();
    }
}

/// Run a config to completion (the `pk_run` semantics).
/// Spawn failure is a *result*, not an error: the returned
/// result has state `spawn_failed`, and the spawn error is
/// returned alongside it so the FFI layer can set the
/// thread-local last-error while still handing back JSON.
pub fn run_config(cfg: Config) -> (ProcResult, Option<(i32, String)>) {
    let started = Instant::now();
    match Running::start(cfg) {
        Ok(mut running) => {
            let result = match running.wait(WAIT_INFINITE) {
                Ok(r) => r,
                Err((code, msg)) => {
                    let r = ProcResult {
                        state: ProcState::SpawnFailed,
                        exit_code: None,
                        pid: running.pid(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        stdout: String::new(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                    };
                    return (r, Some((code, msg)));
                }
            };
            (result, None)
        }
        Err((code, msg)) => {
            let result = ProcResult {
                state: ProcState::SpawnFailed,
                exit_code: None,
                pid: 0,
                elapsed_ms: started.elapsed().as_millis() as u64,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            };
            (result, Some((code, msg)))
        }
    }
}

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_json() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn config_defaults() {
        let cfg = parse_config(r#"{"exe":"thing"}"#).unwrap();
        assert_eq!(cfg.exe, "thing");
        assert!(cfg.args.is_empty());
        assert_eq!(cfg.cwd, None);
        assert!(cfg.capture);
        assert_eq!(cfg.max_capture_bytes, DEFAULT_MAX_CAPTURE_BYTES);
        assert_eq!(cfg.timeout_ms, 0);
        assert!(cfg.kill_tree);
        assert!(cfg.hidden);
        assert!(!cfg.elevate);
    }

    #[test]
    fn config_full() {
        let cfg = parse_config(
            r#"{"exe":"x","args":["a","b c"],"cwd":"/tmp","env":{"K":"V"},
               "capture":false,"max_capture_bytes":42,"timeout_ms":7,
               "kill_tree":false,"hidden":false}"#,
        )
        .unwrap();
        assert_eq!(cfg.args, vec!["a".to_string(), "b c".to_string()]);
        assert_eq!(cfg.cwd.as_deref(), Some("/tmp"));
        assert_eq!(cfg.env, vec![("K".to_string(), "V".to_string())]);
        assert!(!cfg.capture);
        assert_eq!(cfg.max_capture_bytes, 42);
        assert_eq!(cfg.timeout_ms, 7);
        assert!(!cfg.kill_tree);
        assert!(!cfg.hidden);
    }

    #[test]
    fn config_bad_json_is_parse_error() {
        assert_eq!(parse_config("{nope").unwrap_err().0, ERR_PARSE);
        assert_eq!(parse_config("[1,2]").unwrap_err().0, ERR_PARSE);
        assert_eq!(parse_config("").unwrap_err().0, ERR_PARSE);
    }

    #[test]
    fn config_unknown_key_is_parse_error() {
        let err = parse_config(r#"{"exe":"x","bogus":1}"#).unwrap_err();
        assert_eq!(err.0, ERR_PARSE);
        assert!(err.1.contains("bogus"), "{}", err.1);
    }

    #[test]
    fn config_duplicate_key_is_parse_error() {
        assert_eq!(
            parse_config(r#"{"exe":"x","exe":"y"}"#).unwrap_err().0,
            ERR_PARSE
        );
    }

    #[test]
    fn config_missing_or_empty_exe_is_invalid_arg() {
        assert_eq!(parse_config(r#"{}"#).unwrap_err().0, ERR_INVALID_ARG);
        assert_eq!(
            parse_config(r#"{"exe":""}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_config(r#"{"exe":42}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
    }

    #[test]
    fn config_wrong_types_are_invalid_arg() {
        for text in [
            r#"{"exe":"x","args":"oops"}"#,
            r#"{"exe":"x","args":["ok",3]}"#,
            r#"{"exe":"x","env":["K"]}"#,
            r#"{"exe":"x","env":{"K":1}}"#,
            r#"{"exe":"x","capture":"yes"}"#,
            r#"{"exe":"x","timeout_ms":-5}"#,
            r#"{"exe":"x","timeout_ms":1.5}"#,
            r#"{"exe":"x","cwd":9}"#,
        ] {
            assert_eq!(parse_config(text).unwrap_err().0, ERR_INVALID_ARG, "{text}");
        }
    }

    #[test]
    fn config_elevate_combinations() {
        // capture defaults to true, kill_tree defaults to
        // true: a bare elevate config must be refused on both.
        let err = parse_config(r#"{"exe":"x","elevate":true,"capture":false}"#).unwrap_err();
        assert_eq!(err.0, ERR_NOT_SUPPORTED);
        assert!(err.1.contains("kill_tree"), "{}", err.1);
        let err = parse_config(r#"{"exe":"x","elevate":true,"kill_tree":false}"#).unwrap_err();
        assert_eq!(err.0, ERR_NOT_SUPPORTED);
        assert!(err.1.contains("capture"), "{}", err.1);
        // Fully opted out: accepted.
        let cfg = parse_config(r#"{"exe":"x","elevate":true,"capture":false,"kill_tree":false}"#)
            .unwrap();
        assert!(cfg.elevate && !cfg.capture && !cfg.kill_tree);
    }

    #[test]
    fn quoter_vectors() {
        assert_eq!(quote_arg("abc"), "abc");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a\\"), "a\\");
        assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_arg("C:\\plain\\path"), "C:\\plain\\path");
        assert_eq!(
            quote_arg("C:\\Program Files\\app.exe"),
            "\"C:\\Program Files\\app.exe\""
        );
        assert_eq!(quote_arg("tab\there"), "\"tab\there\"");
        assert_eq!(
            build_command_line("my tool.exe", &["a b".into(), "x".into()]),
            "\"my tool.exe\" \"a b\" x"
        );
    }

    #[test]
    fn capture_buffer_truncates_and_keeps_draining() {
        let buf = CaptureBuffer::new(8);
        buf.push(b"abcd");
        assert_eq!(buf.snapshot(), ("abcd".to_string(), false));
        buf.push(b"efghij"); // 4 fit, 2 dropped
        assert_eq!(buf.snapshot(), ("abcdefgh".to_string(), true));
        buf.push(b"more"); // nothing fits; still counted
        assert_eq!(buf.snapshot(), ("abcdefgh".to_string(), true));
    }

    #[test]
    fn capture_buffer_zero_cap() {
        let buf = CaptureBuffer::new(0);
        buf.push(b"x");
        assert_eq!(buf.snapshot(), (String::new(), true));
        let buf = CaptureBuffer::new(0);
        assert_eq!(buf.snapshot(), (String::new(), false));
    }

    #[test]
    fn result_json_shape() {
        let r = ProcResult {
            state: ProcState::Exited,
            exit_code: Some(3),
            pid: 42,
            elapsed_ms: 12,
            stdout: "hi \"there\"\n".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: true,
        };
        assert_eq!(
            r.to_json(),
            "{\"state\":\"exited\",\"exit_code\":3,\"pid\":42,\"elapsed_ms\":12,\
             \"stdout\":\"hi \\\"there\\\"\\n\",\"stderr\":\"\",\
             \"stdout_truncated\":false,\"stderr_truncated\":true}"
        );
        let p = PollInfo {
            state: ProcState::Running,
            exit_code: None,
            pid: 42,
            elapsed_ms: 5,
        };
        assert_eq!(
            p.to_json(),
            "{\"state\":\"running\",\"exit_code\":null,\"pid\":42,\"elapsed_ms\":5}"
        );
    }
}
