//! Platform layer: the only place OS process APIs are
//! touched.
//!
//! The contract `lib` supervises against:
//! - `spawn` starts the child and (when capture is on) starts
//!   reader threads draining its pipes immediately — a child
//!   must never block on a full pipe while we wait.
//! - `poll_exit` / `wait_exit` report an exit code at most
//!   once the process is really finished and reaped.
//! - `kill` terminates the child; on Windows with a job it
//!   terminates the whole tree.
//! - Dropping a `Proc` releases everything. On Windows the
//!   job handle closes with it, and KILL_ON_JOB_CLOSE ends
//!   any tree still running — the orphan guarantee. Off
//!   Windows, a dropped `Proc` kills a still-running child
//!   only when tree-kill was configured (`close` semantics in
//!   `lib`); tree-kill off Windows is child-only — there is
//!   no zero-dependency way to signal a process group without
//!   libc, and pretending otherwise would be worse.
//!
//! Every Windows extern block carries #[link(name = "...")]
//! naming its import library. Without it the build fails at
//! LINK time with LNK2019 — check and clippy cannot catch
//! it; CI's real build can.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use crate::ERR_NOT_SUPPORTED;
use crate::{CaptureBuffer, Config};

pub type PResult<T> = Result<T, (i32, String)>;

/// How long `finish_output` waits for pipe EOF after the
/// child has exited. Output is normally complete the moment
/// the child dies; the grace window exists only because a
/// grandchild can inherit the pipes and hold them open past
/// the child's death. Joining reader threads unboundedly
/// would hang the supervisor on precisely the orphaned
/// processes this library exists to control — so after the
/// grace window the readers are detached (they end at EOF on
/// their own; the buffers are shared Arcs) and the result
/// carries what was captured.
pub(crate) const DRAIN_GRACE_MS: u64 = 300;

pub(crate) fn finish_drain(
    out_buf: &Arc<CaptureBuffer>,
    err_buf: &Arc<CaptureBuffer>,
    out_thread: &mut Option<JoinHandle<()>>,
    err_thread: &mut Option<JoinHandle<()>>,
) -> (String, bool, String, bool) {
    let deadline = Instant::now() + Duration::from_millis(DRAIN_GRACE_MS);
    loop {
        let out_done = out_thread.is_none() || out_buf.is_eof();
        let err_done = err_thread.is_none() || err_buf.is_eof();
        if (out_done && err_done) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Join readers that finished; detach any still blocked
    // on a pipe a grandchild is holding open.
    if out_buf.is_eof() {
        if let Some(t) = out_thread.take() {
            let _ = t.join();
        }
    } else {
        out_thread.take();
    }
    if err_buf.is_eof() {
        if let Some(t) = err_thread.take() {
            let _ = t.join();
        }
    } else {
        err_thread.take();
    }
    let (so, st) = out_buf.snapshot();
    let (se, et) = err_buf.snapshot();
    (so, st, se, et)
}

// ---------------------------------------------------------------------------
// Non-Windows: std::process
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::process::{Child, Command, Stdio};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{ERR_ACCESS_DENIED, ERR_NOT_FOUND, ERR_PLATFORM};

    pub struct Proc {
        child: Child,
        pid: u32,
        out_buf: Arc<CaptureBuffer>,
        err_buf: Arc<CaptureBuffer>,
        out_thread: Option<JoinHandle<()>>,
        err_thread: Option<JoinHandle<()>>,
        kill_on_close: bool,
    }

    fn map_spawn_err(exe: &str, e: &io::Error) -> (i32, String) {
        let code = match e.kind() {
            io::ErrorKind::NotFound => ERR_NOT_FOUND,
            io::ErrorKind::PermissionDenied => ERR_ACCESS_DENIED,
            _ => ERR_PLATFORM,
        };
        (code, format!("spawning \"{exe}\": {e}"))
    }

    pub fn spawn(cfg: &Config) -> PResult<Proc> {
        if cfg.elevate {
            return Err((
                ERR_NOT_SUPPORTED,
                "elevate (the shell \"runas\" verb) is only available on Windows".to_string(),
            ));
        }
        let mut cmd = Command::new(&cfg.exe);
        cmd.args(&cfg.args).stdin(Stdio::null());
        if let Some(cwd) = &cfg.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        if cfg.capture {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // `hidden` is a no-op here: there are no console
        // windows to hide off Windows.
        let mut child = cmd.spawn().map_err(|e| map_spawn_err(&cfg.exe, &e))?;
        let pid = child.id();
        let out_buf = Arc::new(CaptureBuffer::new(cfg.max_capture_bytes));
        let err_buf = Arc::new(CaptureBuffer::new(cfg.max_capture_bytes));
        let out_thread = child.stdout.take().map(|s| crate::pump(s, out_buf.clone()));
        let err_thread = child.stderr.take().map(|s| crate::pump(s, err_buf.clone()));
        Ok(Proc {
            child,
            pid,
            out_buf,
            err_buf,
            out_thread,
            err_thread,
            kill_on_close: cfg.kill_tree,
        })
    }

    impl Proc {
        pub fn pid(&self) -> u32 {
            self.pid
        }

        pub fn poll_exit(&mut self) -> PResult<Option<i32>> {
            match self.child.try_wait() {
                Ok(Some(status)) => Ok(Some(status.code().unwrap_or(-1))),
                Ok(None) => Ok(None),
                Err(e) => Err((ERR_PLATFORM, format!("waiting on child: {e}"))),
            }
        }

        pub fn wait_exit(&mut self, timeout_ms: Option<u32>) -> PResult<Option<i32>> {
            let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms as u64));
            loop {
                if let Some(code) = self.poll_exit()? {
                    return Ok(Some(code));
                }
                match deadline {
                    Some(d) => {
                        if Instant::now() >= d {
                            return Ok(None);
                        }
                        std::thread::sleep(
                            Duration::from_millis(2)
                                .min(d.saturating_duration_since(Instant::now())),
                        );
                    }
                    None => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }

        pub fn kill(&mut self, _tree: bool) -> PResult<()> {
            // Off Windows, kill is child-only (see the module
            // docs); `_tree` is accepted for signature parity.
            if let Some(_code) = self.poll_exit()? {
                return Ok(()); // already finished
            }
            self.child
                .kill()
                .map_err(|e| (ERR_PLATFORM, format!("killing child: {e}")))
        }

        pub fn snapshot_output(&self) -> (String, bool, String, bool) {
            let (so, st) = self.out_buf.snapshot();
            let (se, et) = self.err_buf.snapshot();
            (so, st, se, et)
        }

        pub fn finish_output(&mut self) -> (String, bool, String, bool) {
            finish_drain(
                &self.out_buf,
                &self.err_buf,
                &mut self.out_thread,
                &mut self.err_thread,
            )
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            if self.kill_on_close && matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: direct Win32
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::thread::JoinHandle;

    use super::*;
    use crate::{build_command_line, quote_arg, ERR_ACCESS_DENIED, ERR_NOT_FOUND, ERR_PLATFORM};

    type Handle = *mut c_void;
    type Bool = i32;

    const NULL_HANDLE: Handle = std::ptr::null_mut();
    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
    const SW_HIDE: i32 = 0;
    const SW_SHOWNORMAL: i32 = 1;
    const ERROR_CANCELLED: u32 = 1223;

    #[repr(C)]
    struct SecurityAttributes {
        n_length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: Bool,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_count: u16,
        reserved2: *mut u8,
        std_input: Handle,
        std_output: Handle,
        std_error: Handle,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        flags: u32,
        hwnd: Handle,
        verb: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
        inst_app: Handle,
        id_list: *mut c_void,
        cls: *const u16,
        key_class: Handle,
        hot_key: u32,
        icon_or_monitor: Handle,
        process: Handle,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateProcessW(
            application_name: *const u16,
            command_line: *mut u16,
            process_attributes: *mut SecurityAttributes,
            thread_attributes: *mut SecurityAttributes,
            inherit_handles: Bool,
            creation_flags: u32,
            environment: *const u16,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> Bool;
        fn CreatePipe(
            read_handle: *mut Handle,
            write_handle: *mut Handle,
            attributes: *mut SecurityAttributes,
            size: u32,
        ) -> Bool;
        fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> Bool;
        fn TerminateProcess(process: Handle, exit_code: u32) -> Bool;
        fn CreateJobObjectW(attributes: *mut SecurityAttributes, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            info_class: i32,
            info: *const c_void,
            info_len: u32,
        ) -> Bool;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> Bool;
        fn ResumeThread(thread: Handle) -> u32;
        fn GetProcessId(process: Handle) -> u32;
        fn GetLastError() -> u32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> Bool;
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last_error(context: &str) -> (i32, String) {
        let code = unsafe { GetLastError() };
        let mapped = match code {
            2 | 3 | 267 => ERR_NOT_FOUND,
            5 => ERR_ACCESS_DENIED,
            ERROR_CANCELLED => ERR_ACCESS_DENIED,
            _ => ERR_PLATFORM,
        };
        (mapped, format!("{context}: Win32 error {code}"))
    }

    unsafe fn close(handle: Handle) {
        if !handle.is_null() {
            CloseHandle(handle);
        }
    }

    /// Merged environment block for CreateProcessW: the
    /// inherited environment with config entries overriding
    /// same-named (case-insensitively) inherited ones, sorted
    /// by key as CreateProcess expects, double-NUL terminated.
    fn build_env_block(extra: &[(String, String)]) -> Vec<u16> {
        let mut entries: Vec<(String, String, String)> = Vec::new();
        for (k, v) in std::env::vars() {
            entries.push((k.to_uppercase(), k, v));
        }
        for (k, v) in extra {
            let upper = k.to_uppercase();
            if let Some(slot) = entries.iter_mut().find(|e| e.0 == upper) {
                slot.1 = k.clone();
                slot.2 = v.clone();
            } else {
                entries.push((upper, k.clone(), v.clone()));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut block: Vec<u16> = Vec::new();
        for (_, k, v) in &entries {
            let line = format!("{k}={v}");
            block.extend(OsStr::new(&line).encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    pub struct Proc {
        process: Handle,
        job: Handle,
        pid: u32,
        out_buf: Arc<CaptureBuffer>,
        err_buf: Arc<CaptureBuffer>,
        out_thread: Option<JoinHandle<()>>,
        err_thread: Option<JoinHandle<()>>,
    }

    pub fn spawn(cfg: &Config) -> PResult<Proc> {
        if cfg.elevate {
            return spawn_elevated(cfg);
        }
        unsafe {
            let mut sa = SecurityAttributes {
                n_length: std::mem::size_of::<SecurityAttributes>() as u32,
                security_descriptor: std::ptr::null_mut(),
                inherit_handle: 1,
            };
            // Pipes. Read ends are made non-inheritable so the
            // child doesn't keep them open; write ends travel
            // in the startup info.
            let (mut out_read, mut out_write) = (NULL_HANDLE, NULL_HANDLE);
            let (mut err_read, mut err_write) = (NULL_HANDLE, NULL_HANDLE);
            let (mut in_read, mut in_write) = (NULL_HANDLE, NULL_HANDLE);
            if cfg.capture {
                if CreatePipe(&mut out_read, &mut out_write, &mut sa, 0) == 0 {
                    return Err(last_error("CreatePipe (stdout)"));
                }
                if CreatePipe(&mut err_read, &mut err_write, &mut sa, 0) == 0 {
                    close(out_read);
                    close(out_write);
                    return Err(last_error("CreatePipe (stderr)"));
                }
                if CreatePipe(&mut in_read, &mut in_write, &mut sa, 0) == 0 {
                    close(out_read);
                    close(out_write);
                    close(err_read);
                    close(err_write);
                    return Err(last_error("CreatePipe (stdin)"));
                }
                SetHandleInformation(out_read, HANDLE_FLAG_INHERIT, 0);
                SetHandleInformation(err_read, HANDLE_FLAG_INHERIT, 0);
                SetHandleInformation(in_write, HANDLE_FLAG_INHERIT, 0);
            }

            let mut startup: StartupInfoW = std::mem::zeroed();
            startup.cb = std::mem::size_of::<StartupInfoW>() as u32;
            if cfg.capture {
                startup.flags |= STARTF_USESTDHANDLES;
                startup.std_input = in_read;
                startup.std_output = out_write;
                startup.std_error = err_write;
            }

            let mut flags: u32 = 0;
            if cfg.hidden {
                flags |= CREATE_NO_WINDOW;
            }
            if cfg.kill_tree {
                // Suspended so the job assignment below
                // happens before the child can spawn anything.
                flags |= CREATE_SUSPENDED;
            }
            let env_block;
            let env_ptr: *const u16 = if cfg.env.is_empty() {
                std::ptr::null()
            } else {
                flags |= CREATE_UNICODE_ENVIRONMENT;
                env_block = build_env_block(&cfg.env);
                env_block.as_ptr()
            };

            let mut cmdline = wide(&build_command_line(&cfg.exe, &cfg.args));
            // Passing the application name pins the exact
            // binary when the exe names a path; a bare name is
            // left to the command-line PATH search instead
            // (CreateProcessW does not search PATH for an
            // explicit application name).
            let app_wide;
            let app_ptr: *const u16 = if cfg.exe.contains('\\') || cfg.exe.contains('/') {
                app_wide = wide(&cfg.exe);
                app_wide.as_ptr()
            } else {
                std::ptr::null()
            };
            let cwd_wide;
            let cwd_ptr: *const u16 = match &cfg.cwd {
                Some(cwd) => {
                    cwd_wide = wide(cwd);
                    cwd_wide.as_ptr()
                }
                None => std::ptr::null(),
            };

            let mut info: ProcessInformation = std::mem::zeroed();
            let ok = CreateProcessW(
                app_ptr,
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                if cfg.capture { 1 } else { 0 },
                flags,
                env_ptr,
                cwd_ptr,
                &mut startup,
                &mut info,
            );
            if ok == 0 {
                let err = last_error(&format!("CreateProcessW \"{}\"", cfg.exe));
                close(out_read);
                close(out_write);
                close(err_read);
                close(err_write);
                close(in_read);
                close(in_write);
                return Err(err);
            }
            // The child's pipe ends are its now; ours (write
            // ends, stdin read end) close so EOF propagates.
            close(out_write);
            close(err_write);
            close(in_read);
            close(in_write);

            // Job assignment — before ResumeThread, so nothing
            // the child spawns can escape the tree.
            let mut job = NULL_HANDLE;
            if cfg.kill_tree {
                job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                let mut assigned = false;
                if !job.is_null() {
                    let mut limits: JobObjectExtendedLimitInformation = Default::default();
                    limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    if SetInformationJobObject(
                        job,
                        JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                        &limits as *const _ as *const c_void,
                        std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                    ) != 0
                        && AssignProcessToJobObject(job, info.process) != 0
                    {
                        assigned = true;
                    }
                }
                if !assigned {
                    let err = last_error("assigning the child to a job object");
                    TerminateProcess(info.process, 1);
                    WaitForSingleObject(info.process, 5000);
                    close(info.thread);
                    close(info.process);
                    close(job);
                    close(out_read);
                    close(err_read);
                    return Err(err);
                }
                ResumeThread(info.thread);
            }
            close(info.thread);

            let out_buf = Arc::new(CaptureBuffer::new(cfg.max_capture_bytes));
            let err_buf = Arc::new(CaptureBuffer::new(cfg.max_capture_bytes));
            let out_thread = if cfg.capture {
                let file = std::fs::File::from_raw_handle(out_read);
                Some(crate::pump(file, out_buf.clone()))
            } else {
                None
            };
            let err_thread = if cfg.capture {
                let file = std::fs::File::from_raw_handle(err_read);
                Some(crate::pump(file, err_buf.clone()))
            } else {
                None
            };
            Ok(Proc {
                process: info.process,
                job,
                pid: info.process_id,
                out_buf,
                err_buf,
                out_thread,
                err_thread,
            })
        }
    }

    fn spawn_elevated(cfg: &Config) -> PResult<Proc> {
        // Config validation has already refused capture and
        // kill_tree here: ShellExecuteExW "runas" gives back a
        // process handle, but the elevated process cannot be
        // piped or jobbed by us — that is the OS restriction
        // the config rules exist to state up front.
        unsafe {
            let verb = wide("runas");
            let file = wide(&cfg.exe);
            let params_joined = cfg
                .args
                .iter()
                .map(|a| quote_arg(a))
                .collect::<Vec<_>>()
                .join(" ");
            let params = wide(&params_joined);
            let dir_wide;
            let mut info: ShellExecuteInfoW = std::mem::zeroed();
            info.cb_size = std::mem::size_of::<ShellExecuteInfoW>() as u32;
            info.flags = SEE_MASK_NOCLOSEPROCESS;
            info.verb = verb.as_ptr();
            info.file = file.as_ptr();
            info.parameters = if cfg.args.is_empty() {
                std::ptr::null()
            } else {
                params.as_ptr()
            };
            info.directory = match &cfg.cwd {
                Some(cwd) => {
                    dir_wide = wide(cwd);
                    dir_wide.as_ptr()
                }
                None => std::ptr::null(),
            };
            info.show = if cfg.hidden { SW_HIDE } else { SW_SHOWNORMAL };
            if ShellExecuteExW(&mut info) == 0 {
                let err = last_error("ShellExecuteExW (runas)");
                if err.0 == ERR_ACCESS_DENIED {
                    return Err((
                        ERR_ACCESS_DENIED,
                        "elevation was declined or is not available (ShellExecuteExW runas)"
                            .to_string(),
                    ));
                }
                return Err(err);
            }
            if info.process.is_null() {
                return Err((
                    ERR_PLATFORM,
                    "ShellExecuteExW returned no process handle".to_string(),
                ));
            }
            let pid = GetProcessId(info.process);
            Ok(Proc {
                process: info.process,
                job: NULL_HANDLE,
                pid,
                out_buf: Arc::new(CaptureBuffer::new(0)),
                err_buf: Arc::new(CaptureBuffer::new(0)),
                out_thread: None,
                err_thread: None,
            })
        }
    }

    impl Proc {
        pub fn pid(&self) -> u32 {
            self.pid
        }

        pub fn poll_exit(&mut self) -> PResult<Option<i32>> {
            unsafe {
                match WaitForSingleObject(self.process, 0) {
                    WAIT_OBJECT_0 => {
                        let mut code: u32 = 0;
                        if GetExitCodeProcess(self.process, &mut code) == 0 {
                            return Err(last_error("GetExitCodeProcess"));
                        }
                        Ok(Some(code as i32))
                    }
                    WAIT_TIMEOUT => Ok(None),
                    other => Err((
                        ERR_PLATFORM,
                        format!("WaitForSingleObject returned {other}"),
                    )),
                }
            }
        }

        pub fn wait_exit(&mut self, timeout_ms: Option<u32>) -> PResult<Option<i32>> {
            unsafe {
                match WaitForSingleObject(self.process, timeout_ms.unwrap_or(INFINITE)) {
                    WAIT_OBJECT_0 => {
                        let mut code: u32 = 0;
                        if GetExitCodeProcess(self.process, &mut code) == 0 {
                            return Err(last_error("GetExitCodeProcess"));
                        }
                        Ok(Some(code as i32))
                    }
                    WAIT_TIMEOUT => Ok(None),
                    other => Err((
                        ERR_PLATFORM,
                        format!("WaitForSingleObject returned {other}"),
                    )),
                }
            }
        }

        pub fn kill(&mut self, tree: bool) -> PResult<()> {
            unsafe {
                if let Some(_code) = self.poll_exit()? {
                    return Ok(()); // already finished
                }
                let ok = if tree && !self.job.is_null() {
                    TerminateJobObject(self.job, 1)
                } else {
                    TerminateProcess(self.process, 1)
                };
                if ok == 0 {
                    // A race with natural exit is fine; a live
                    // process we cannot kill is not.
                    if let Some(_code) = self.poll_exit()? {
                        return Ok(());
                    }
                    return Err(last_error("terminating the process"));
                }
                Ok(())
            }
        }

        pub fn snapshot_output(&self) -> (String, bool, String, bool) {
            let (so, st) = self.out_buf.snapshot();
            let (se, et) = self.err_buf.snapshot();
            (so, st, se, et)
        }

        pub fn finish_output(&mut self) -> (String, bool, String, bool) {
            finish_drain(
                &self.out_buf,
                &self.err_buf,
                &mut self.out_thread,
                &mut self.err_thread,
            )
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            unsafe {
                // Closing the job handle is the orphan
                // guarantee: KILL_ON_JOB_CLOSE terminates
                // every process still in the job.
                close(self.job);
                close(self.process);
            }
        }
    }
}

pub use imp::{spawn, Proc};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_config, ProcState, Running, WAIT_INFINITE};

    fn config_for(exe: &str, args: &[&str]) -> Config {
        Config {
            exe: exe.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Config::default()
        }
    }

    fn echo_config() -> Config {
        if cfg!(windows) {
            config_for("cmd", &["/C", "echo", "hello"])
        } else {
            config_for("/bin/echo", &["hello"])
        }
    }

    fn sleeper_config(seconds: u32) -> Config {
        if cfg!(windows) {
            let line = format!("ping -n {} 127.0.0.1 > NUL", seconds + 1);
            let mut cfg = config_for("cmd", &[]);
            cfg.args = vec!["/C".to_string(), line];
            cfg
        } else {
            let line = format!("sleep {seconds}");
            let mut cfg = config_for("/bin/sh", &[]);
            cfg.args = vec!["-c".to_string(), line];
            cfg
        }
    }

    #[test]
    fn echo_capture_exact() {
        let (result, err) = crate::run_config(echo_config());
        assert!(err.is_none(), "{err:?}");
        assert_eq!(result.state, ProcState::Exited);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim_end(), "hello");
        assert!(result.stderr.is_empty(), "stderr: {:?}", result.stderr);
        assert!(!result.stdout_truncated);
        assert!(result.pid > 0);
    }

    #[test]
    fn nonzero_exit_passthrough() {
        let cfg = if cfg!(windows) {
            config_for("cmd", &["/C", "exit 3"])
        } else {
            config_for("/bin/sh", &["-c", "exit 3"])
        };
        let (result, err) = crate::run_config(cfg);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(result.state, ProcState::Exited);
        assert_eq!(result.exit_code, Some(3));
    }

    #[test]
    fn env_var_visible_to_child() {
        let mut cfg = if cfg!(windows) {
            config_for("cmd", &["/C", "echo %PK_TEST_VALUE%"])
        } else {
            config_for("/bin/sh", &["-c", "echo $PK_TEST_VALUE"])
        };
        cfg.env = vec![("PK_TEST_VALUE".to_string(), "hello42".to_string())];
        let (result, err) = crate::run_config(cfg);
        assert!(err.is_none(), "{err:?}");
        assert!(
            result.stdout.contains("hello42"),
            "stdout: {:?}",
            result.stdout
        );
    }

    #[test]
    fn cwd_is_honored() {
        let dir = std::env::temp_dir().join(format!("processkit-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = if cfg!(windows) {
            config_for("cmd", &["/C", "cd"])
        } else {
            config_for("/bin/sh", &["-c", "pwd"])
        };
        cfg.cwd = Some(dir.to_string_lossy().into_owned());
        let (result, err) = crate::run_config(cfg);
        assert!(err.is_none(), "{err:?}");
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            result.stdout.contains(&leaf),
            "stdout {:?} should contain {leaf}",
            result.stdout
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timeout_kills_sleeper() {
        let mut cfg = sleeper_config(30);
        cfg.timeout_ms = 300;
        let (result, err) = crate::run_config(cfg);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(result.state, ProcState::Timeout);
        assert_eq!(result.exit_code, None);
        assert!(
            result.elapsed_ms >= 250 && result.elapsed_ms < 10_000,
            "elapsed {}",
            result.elapsed_ms
        );
    }

    #[test]
    fn truncation_flags_and_no_deadlock() {
        let long = "a".repeat(500);
        let mut cfg = if cfg!(windows) {
            config_for("cmd", &["/C", "echo", &long])
        } else {
            config_for("/bin/echo", &[&long])
        };
        cfg.max_capture_bytes = 16;
        let (result, err) = crate::run_config(cfg);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(result.state, ProcState::Exited);
        assert!(result.stdout_truncated);
        assert_eq!(result.stdout.len(), 16);
    }

    #[test]
    fn spawn_failed_for_missing_exe() {
        let cfg = config_for("/nonexistent/processkit-nope-12345", &[]);
        let (result, err) = crate::run_config(cfg);
        assert_eq!(result.state, ProcState::SpawnFailed);
        assert_eq!(result.exit_code, None);
        assert!(err.is_some(), "spawn error should be reported");
        assert!(Running::start(config_for("/nonexistent/processkit-nope-12345", &[])).is_err());
    }

    #[test]
    fn start_poll_wait_matches_run() {
        let mut running = Running::start(echo_config()).unwrap();
        let pid = running.pid();
        assert!(pid > 0);
        let waited = running.wait(WAIT_INFINITE).unwrap();
        assert_eq!(waited.state, ProcState::Exited);
        assert_eq!(waited.exit_code, Some(0));
        assert_eq!(waited.stdout.trim_end(), "hello");
        let polled = running.poll().unwrap();
        assert_eq!(polled.state, ProcState::Exited);
        assert_eq!(polled.exit_code, Some(0));
        assert_eq!(polled.pid, pid);
        running.close();
    }

    #[test]
    fn kill_via_handle_reports_killed() {
        let mut running = Running::start(sleeper_config(30)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        running.kill(true).unwrap();
        let waited = running.wait(WAIT_INFINITE).unwrap();
        assert_eq!(waited.state, ProcState::Killed);
        assert_eq!(waited.exit_code, None);
        running.close();
    }

    #[test]
    fn wait_with_short_budget_returns_running() {
        let mut running = Running::start(sleeper_config(30)).unwrap();
        let early = running.wait(100).unwrap();
        assert_eq!(early.state, ProcState::Running);
        assert_eq!(early.exit_code, None);
        running.kill(true).unwrap();
        let waited = running.wait(WAIT_INFINITE).unwrap();
        assert_eq!(waited.state, ProcState::Killed);
        running.close();
    }

    #[test]
    fn poll_enforces_timeout() {
        let mut cfg = sleeper_config(30);
        cfg.timeout_ms = 300;
        let mut running = Running::start(cfg).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(700));
        let info = running.poll().unwrap();
        assert_eq!(info.state, ProcState::Timeout);
        assert_eq!(info.exit_code, None);
        running.close();
    }

    #[test]
    fn parse_then_run_round_trip() {
        // The full JSON path: parse a config and run it.
        let text = if cfg!(windows) {
            r#"{"exe":"cmd","args":["/C","echo","hello"]}"#
        } else {
            r#"{"exe":"/bin/echo","args":["hello"]}"#
        };
        let cfg = parse_config(text).unwrap();
        let (result, err) = crate::run_config(cfg);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(result.stdout.trim_end(), "hello");
    }
}
