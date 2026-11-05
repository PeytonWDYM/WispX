//! The only `unsafe` in WispX: two libc calls that std does not expose.
//!
//! std can deliver neither SIGTERM (only SIGKILL through `Child::kill`) nor
//! install a signal handler. We need both so recorders can flush their last
//! buffer and so a terminated daemon removes its socket and private audio.
//! std already links libc, so declaring these adds no dependency.

#![allow(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_shutdown_signal(_signum: i32) {
    // An atomic store is async-signal-safe; everything else happens on the
    // threads that poll `shutdown_requested`.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Routes SIGINT, SIGTERM and SIGHUP to the flag read by `shutdown_requested`.
pub fn install_shutdown_handlers() {
    for signum in [SIGHUP, SIGINT, SIGTERM] {
        // SAFETY: the handler only performs an atomic store, and `signal` has
        // no other preconditions for these catchable signals.
        unsafe {
            signal(signum, on_shutdown_signal);
        }
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Sends `sig` to the process group led by `pid`, reaching wrapper scripts'
/// children as well as the wrapper itself. Only call this before waiting for
/// `pid`: after that its number may belong to someone else.
pub fn signal_group(pid: u32, sig: i32) -> io::Result<()> {
    let group = i32::try_from(pid).map_err(|_| invalid_pid())?;
    // Never let a zero or a stray -1 become "every process we may signal".
    if group <= 1 {
        return Err(invalid_pid());
    }
    // SAFETY: `kill` takes plain integers and reports failure through errno.
    let result = unsafe { kill(-group, sig) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn invalid_pid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid process id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_pids_that_would_broadcast() {
        assert!(signal_group(1, SIGTERM).is_err());
        assert!(signal_group(0, SIGTERM).is_err());
    }
}
