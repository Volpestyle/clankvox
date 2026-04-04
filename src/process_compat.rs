//! Cross-platform process management helpers.
//!
//! Abstracts Unix-specific APIs (`killpg`, `process_group`, `sh -c`) behind a
//! platform-neutral interface so that `music.rs` and `stream_publish.rs` compile
//! on both Unix and Windows without `#[cfg]` scattered through business logic.

use std::io;
use std::process::Command;

use tracing::warn;

// ── Shell quoting ──────────────────────────────────────────────────────────

/// Quote a string for embedding in a shell pipeline command.
///
/// - Unix: wraps in single quotes with escaped interior single-quotes.
/// - Windows: wraps in double quotes with escaped interior double-quotes.
#[cfg(unix)]
pub(crate) fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(windows)]
pub(crate) fn shell_quote(s: &str) -> String {
    // cmd.exe recognises double-quote delimiters; interior double quotes are
    // escaped with a backslash for the child process.
    let escaped = s.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Quote a string intended as a single-quoted literal argument embedded in a
/// larger shell pipeline string (e.g. `--extractor-args 'youtube:...'`).
///
/// On Unix this produces `'value'`; on Windows it produces `"value"`.
#[cfg(unix)]
pub(crate) fn shell_quote_arg(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(windows)]
pub(crate) fn shell_quote_arg(s: &str) -> String {
    let escaped = s.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

// ── Shell spawning ─────────────────────────────────────────────────────────

/// Create a `Command` that runs `pipeline` through the platform shell.
///
/// - Unix: `sh -c "pipeline"`
/// - Windows: `cmd /c "pipeline"`
///
/// The returned `Command` already has a new process group configured so that
/// the entire child tree can be signalled together.
pub(crate) fn shell_command(pipeline: &str) -> Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = Command::new("sh");
        cmd.process_group(0);
        cmd.args(["-c", pipeline]);
        cmd
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NEW_PROCESS_GROUP so we can send CTRL_BREAK to the tree.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let mut cmd = Command::new("cmd");
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        cmd.args(["/c", pipeline]);
        cmd
    }
}

// ── Process signal abstraction ─────────────────────────────────────────────

/// Signals that `music.rs` and `stream_publish.rs` need to send.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProcessSignal {
    /// Graceful termination (SIGTERM / TerminateProcess).
    Terminate,
    /// Suspend execution (SIGSTOP / NtSuspendProcess).
    Suspend,
    /// Resume execution (SIGCONT / NtResumeProcess).
    Resume,
}

/// Send a signal to a process group rooted at `pid`.
///
/// On Unix this uses `killpg`. On Windows it uses platform APIs:
/// - `Terminate` → `TerminateProcess` via the kernel32 handle.
/// - `Suspend`/`Resume` → no-op with a warning (not supported on Windows
///   without `NtSuspendProcess` which requires undocumented ntdll APIs).
pub(crate) fn signal_process_group(pid: u32, signal: ProcessSignal) -> io::Result<()> {
    if pid == 0 {
        return Ok(());
    }

    #[cfg(unix)]
    {
        let sig = match signal {
            ProcessSignal::Terminate => libc::SIGTERM,
            ProcessSignal::Suspend => libc::SIGSTOP,
            ProcessSignal::Resume => libc::SIGCONT,
        };
        // SAFETY: pid originates from Child::id(), the child was spawned with
        // process_group(0), and we guard against pid==0 above.
        #[allow(unsafe_code, clippy::cast_possible_wrap)]
        let rc = unsafe { libc::killpg(pid as libc::pid_t, sig) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(windows)]
    {
        match signal {
            ProcessSignal::Terminate => {
                // Open the process and terminate it. Also send CTRL_BREAK to
                // the process group so piped children (ffmpeg, yt-dlp) exit.
                terminate_process_tree_win32(pid)
            }
            ProcessSignal::Suspend => {
                warn!(pid, "process suspend not implemented on Windows; ignoring");
                Ok(())
            }
            ProcessSignal::Resume => {
                warn!(pid, "process resume not implemented on Windows; ignoring");
                Ok(())
            }
        }
    }
}

/// Terminate a process and its children on Windows.
///
/// Sends CTRL_BREAK to the process group first (graceful), then falls back
/// to `TerminateProcess` on the root pid.
#[cfg(windows)]
fn terminate_process_tree_win32(pid: u32) -> io::Result<()> {
    use std::io::Error;

    // First try CTRL_BREAK_EVENT to the process group (only works if the
    // child was created with CREATE_NEW_PROCESS_GROUP).
    #[allow(unsafe_code)]
    unsafe {
        // GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT=1, pid)
        unsafe extern "system" {
            fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
        }
        let _ = GenerateConsoleCtrlEvent(1, pid);
    }

    // Give the process a short window to exit gracefully, then force-kill.
    std::thread::sleep(std::time::Duration::from_millis(200));

    #[allow(unsafe_code)]
    unsafe {
        unsafe extern "system" {
            fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> *mut std::ffi::c_void;
            fn TerminateProcess(h_process: *mut std::ffi::c_void, u_exit_code: u32) -> i32;
            fn CloseHandle(h_object: *mut std::ffi::c_void) -> i32;
        }
        const PROCESS_TERMINATE: u32 = 0x0001;
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            // Process already exited — not an error.
            return Ok(());
        }
        let rc = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if rc == 0 {
            let err = Error::last_os_error();
            // ERROR_ACCESS_DENIED (5) often means the process already exited.
            if err.raw_os_error() == Some(5) {
                return Ok(());
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Helper: send [`ProcessSignal::Terminate`] to a child process, logging on
/// failure (mirrors the old `terminate_music_child` / `terminate_stream_publish_child`).
pub(crate) fn terminate_child(child: &mut std::process::Child, context: &str) {
    if let Err(error) = signal_process_group(child.id(), ProcessSignal::Terminate) {
        if error.kind() != io::ErrorKind::NotFound {
            warn!(pid = child.id(), error = %error, "{context}: failed to signal process group");
        }
    }
}
