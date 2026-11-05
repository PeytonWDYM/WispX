//! What `src/sys.rs` does on Linux. Windows has no signals: a child is either stopped
//! outright or not at all, and everything that matters to a clean stop reaches the
//! daemon by closing its stdin or asking it over the pipe.

use super::ffi::*;
use std::io;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

/// How long a console being closed, or the session ending, waits for the threads
/// that notice `shutdown_requested`. Windows ends the process when the handler returns.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_console_event(event: u32) -> i32 {
    SHUTDOWN.store(true, Ordering::SeqCst);
    if matches!(
        event,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    ) {
        thread::sleep(CLOSE_GRACE);
    }
    1
}

/// Routes Ctrl+C, closing the console and the session ending to the flag read by
/// `shutdown_requested`.
pub fn install_shutdown_handlers() {
    // SAFETY: the handler is a function that lives as long as the program.
    unsafe {
        SetConsoleCtrlHandler(Some(on_console_event), 1);
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Asks the threads that poll `shutdown_requested` to wind down.
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Starts `command` with no tie to this console, so closing it does not reach it.
pub fn detach(command: &mut Command) {
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Stops a process that is still ours. A polite request has no equivalent here, so
/// `SIGTERM` does nothing and the caller's grace period simply runs out. Only call
/// this while holding the child, whose open handle keeps `pid` from being reused.
pub fn signal_group(pid: u32, sig: i32) -> io::Result<()> {
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process id",
        ));
    }
    if sig != SIGKILL {
        return Ok(());
    }
    // SAFETY: plain values go in, and the handle is checked and closed here.
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = Owned(process);
        if TerminateProcess(process.0, 1) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Puts this process in a job that takes its children with it however it ends, since
/// nothing on Windows does that by itself. For the daemon alone: in the desktop app
/// this would take down programs it opens, like the editor showing the config file.
pub fn end_children_with_this_process() {
    let limits = JobExtendedLimits {
        basic: JobBasicLimits {
            limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..JobBasicLimits::default()
        },
        ..JobExtendedLimits::default()
    };
    // SAFETY: `limits` outlives the call and its size is passed along. The job handle
    // is left open on purpose: closing it is what the system does as this process ends.
    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return;
        }
        let set = SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            (&raw const limits).cast(),
            size_of::<JobExtendedLimits>() as u32,
        );
        if set != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

/// Lets the running copy of the app bring its window forward, which Windows only
/// allows when the program the user just started says so.
pub fn allow_foreground() {
    // SAFETY: takes a plain value.
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
    }
}
