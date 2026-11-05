//! Named pipes dressed as Unix sockets, so `ipc` and `app::instance` read the same on
//! both systems. A socket's path is only used to pick the pipe's name, since what
//! keeps others out here is the pipe's own access list and, for clients, checking
//! who answered: a pipe's name lives in a namespace anyone on the machine can create in.
//!
//! All I/O is overlapped, which is what lets a blocked read have a timeout and be
//! woken by `shutdown` from another thread, as a socket's can.

use super::ffi::*;
use super::security::{self, Descriptor};
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BUFFER: u32 = 64 * 1024;
/// How long to wait for a free instance of a pipe that exists.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// A process that just lost the lock may hold the pipe a moment longer.
const BIND_RETRY: Duration = Duration::from_secs(1);

/// The pipe for this socket path, in this session.
fn pipe_name(path: &Path) -> Vec<u16> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let text = path.to_string_lossy().to_lowercase();
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut session = 0;
    // SAFETY: takes plain values and writes to a local.
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session);
    }
    wide(&format!(r"\\.\pipe\wispx-s{session}-{hash:016x}"))
}

#[derive(Debug)]
struct Inner {
    pipe: Owned,
    closed: Owned,
    read_closed: AtomicBool,
    write_closed: AtomicBool,
    read_timeout: AtomicU32,
    write_timeout: AtomicU32,
    nonblocking: AtomicBool,
}

/// One end of a connection. Clones share it, as with a socket's duplicated descriptor.
#[derive(Debug)]
pub struct PipeStream {
    inner: Arc<Inner>,
}

fn millis(timeout: Option<Duration>) -> u32 {
    match timeout {
        None => INFINITE,
        Some(limit) => u32::try_from(limit.as_millis())
            .unwrap_or(INFINITE - 1)
            .clamp(1, INFINITE - 1),
    }
}

impl PipeStream {
    fn wrap(pipe: Handle) -> io::Result<PipeStream> {
        let pipe = Owned(pipe);
        Ok(PipeStream {
            inner: Arc::new(Inner {
                pipe,
                closed: Owned::event()?,
                read_closed: AtomicBool::new(false),
                write_closed: AtomicBool::new(false),
                read_timeout: AtomicU32::new(INFINITE),
                write_timeout: AtomicU32::new(INFINITE),
                nonblocking: AtomicBool::new(false),
            }),
        })
    }

    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<PipeStream> {
        let name = pipe_name(path.as_ref());
        let deadline = Instant::now() + BUSY_WAIT;
        loop {
            // SAFETY: the name is NUL-terminated, the rest are plain values, and asking
            // for identification only means a server can't act as this user.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            if handle != invalid_handle() {
                let stream = PipeStream::wrap(handle)?;
                check_server(stream.inner.pipe.0)?;
                return Ok(stream);
            }
            if last_error() != ERROR_PIPE_BUSY {
                return Err(io::Error::last_os_error());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::from_raw_os_error(ERROR_PIPE_BUSY as i32));
            }
            // SAFETY: the name is NUL-terminated. It failing just means trying again.
            unsafe {
                WaitNamedPipeW(name.as_ptr(), millis(Some(left)));
            }
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner
            .read_timeout
            .store(millis(timeout), Ordering::SeqCst);
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner
            .write_timeout
            .store(millis(timeout), Ordering::SeqCst);
        Ok(())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.nonblocking.store(nonblocking, Ordering::SeqCst);
        Ok(())
    }

    pub fn try_clone(&self) -> io::Result<PipeStream> {
        Ok(PipeStream {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Wakes anything blocked on this connection and refuses more I/O. The other end
    /// sees it close when the last clone here is gone.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if matches!(how, Shutdown::Write | Shutdown::Both) {
            self.inner.write_closed.store(true, Ordering::SeqCst);
        }
        if matches!(how, Shutdown::Read | Shutdown::Both) {
            self.inner.read_closed.store(true, Ordering::SeqCst);
            // SAFETY: the event is valid for as long as `inner` is.
            if unsafe { SetEvent(self.inner.closed.0) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn read_bytes(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let inner = &self.inner;
        if buffer.is_empty() || inner.read_closed.load(Ordering::SeqCst) {
            return Ok(0);
        }
        if inner.nonblocking.load(Ordering::SeqCst) {
            let mut available = 0u32;
            // SAFETY: with no buffer this only reports, writing to the local.
            let ok = unsafe {
                PeekNamedPipe(
                    inner.pipe.0,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &raw mut available,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return match last_error() {
                    code if closed_error(code) => Ok(0),
                    code => Err(io::Error::from_raw_os_error(code as i32)),
                };
            }
            if available == 0 {
                return Err(ErrorKind::WouldBlock.into());
            }
        }
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let pointer = buffer.as_mut_ptr();
        let timeout = inner.read_timeout.load(Ordering::SeqCst);
        match inner.transfer(timeout, |overlapped| {
            // SAFETY: `transfer` does not return until this operation has finished or
            // been cancelled, so `buffer` stays valid throughout.
            unsafe { ReadFile(inner.pipe.0, pointer, length, ptr::null_mut(), overlapped) }
        }) {
            Ok(count) => Ok(count),
            Err(Stop::Closed) => Ok(0),
            Err(Stop::Timeout) => Err(ErrorKind::TimedOut.into()),
            Err(Stop::Failed(code)) if closed_error(code) => Ok(0),
            Err(Stop::Failed(code)) => Err(io::Error::from_raw_os_error(code as i32)),
        }
    }

    fn write_bytes(&self, buffer: &[u8]) -> io::Result<usize> {
        let inner = &self.inner;
        if inner.write_closed.load(Ordering::SeqCst) {
            return Err(ErrorKind::BrokenPipe.into());
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let pointer = buffer.as_ptr();
        let timeout = inner.write_timeout.load(Ordering::SeqCst);
        match inner.transfer(timeout, |overlapped| {
            // SAFETY: as in `read_bytes`.
            unsafe { WriteFile(inner.pipe.0, pointer, length, ptr::null_mut(), overlapped) }
        }) {
            Ok(count) => Ok(count),
            Err(Stop::Closed) => Err(ErrorKind::BrokenPipe.into()),
            Err(Stop::Timeout) => Err(ErrorKind::TimedOut.into()),
            Err(Stop::Failed(code)) if closed_error(code) => Err(ErrorKind::BrokenPipe.into()),
            Err(Stop::Failed(code)) => Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
}

enum Stop {
    Closed,
    Timeout,
    Failed(u32),
}

fn closed_error(code: u32) -> bool {
    matches!(
        code,
        ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA | ERROR_HANDLE_EOF
    )
}

impl Inner {
    /// Runs one operation to its end, however that comes about.
    fn transfer(
        &self,
        timeout: u32,
        start: impl FnOnce(*mut Overlapped) -> i32,
    ) -> Result<usize, Stop> {
        let event = Owned::event().map_err(|_| Stop::Failed(last_error()))?;
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: event.0,
        };
        let mut woken = WAIT_OBJECT_0;
        if start(&raw mut overlapped) == 0 {
            let code = last_error();
            if code != ERROR_IO_PENDING {
                return Err(Stop::Failed(code));
            }
            let handles = [event.0, self.closed.0];
            // SAFETY: both handles live as long as this call.
            woken = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, timeout) };
            if woken != WAIT_OBJECT_0 {
                // SAFETY: names an operation that is still outstanding, if it is.
                unsafe {
                    CancelIoEx(self.pipe.0, &raw mut overlapped);
                }
            }
        }
        let mut count = 0u32;
        // SAFETY: waiting here means the system has let go of `overlapped` and of the
        // caller's buffer before either can go away.
        let finished =
            unsafe { GetOverlappedResult(self.pipe.0, &raw mut overlapped, &raw mut count, 1) };
        if finished != 0 {
            return Ok(count as usize);
        }
        let code = last_error();
        if code == ERROR_OPERATION_ABORTED {
            // What got through before the cancel still counts, or a write would repeat it.
            if count > 0 {
                return Ok(count as usize);
            }
            return Err(if woken == WAIT_TIMEOUT {
                Stop::Timeout
            } else {
                Stop::Closed
            });
        }
        Err(Stop::Failed(code))
    }
}

impl Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_bytes(buffer)
    }
}

impl Read for &PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_bytes(buffer)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_bytes(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_bytes(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Anyone can make a pipe with any name, so make sure this one is this user's before
/// saying anything to it: requests here can carry settings, and replies a transcript.
fn check_server(pipe: Handle) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: the pipe is open, and the process handle is checked and owned from here.
    let server = unsafe {
        if GetNamedPipeServerProcessId(pipe, &raw mut pid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        Owned(process)
    };
    let ours = security::current_user()?;
    // SAFETY: the handle was just opened for this kind of query.
    let theirs = unsafe { security::user_of(server.0) }?;
    if !ours.same_as(&theirs) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "another user's program is holding the WispX pipe",
        ));
    }
    Ok(())
}

/// Listens on a pipe. There is always an instance waiting, so a client arriving
/// between two connections finds the pipe busy, and not missing.
pub struct PipeListener {
    name: Vec<u16>,
    descriptor: Descriptor,
    waiting: Mutex<Option<Owned>>,
}

// SAFETY: the descriptor is only read, and the system copies it on each use.
unsafe impl Send for PipeListener {}
// SAFETY: as above, and `waiting` is behind a lock.
unsafe impl Sync for PipeListener {}

impl PipeListener {
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<PipeListener> {
        let name = pipe_name(path.as_ref());
        let descriptor = Descriptor::only_this_user("GA", false)?;
        let deadline = Instant::now() + BIND_RETRY;
        let first = loop {
            match new_instance(&name, &descriptor, true) {
                Ok(instance) => break instance,
                Err(error)
                    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(50));
                }
                // Refused for good means someone else holds this name, which the
                // first-instance flag is there to catch.
                Err(error) => return Err(error),
            }
        };
        Ok(PipeListener {
            name,
            descriptor,
            waiting: Mutex::new(Some(first)),
        })
    }

    fn accept(&self) -> io::Result<PipeStream> {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let instance = match waiting.take() {
            Some(instance) => instance,
            None => new_instance(&self.name, &self.descriptor, false)?,
        };
        wait_for_client(&instance)?;
        // Before handing this one over, so the pipe never looks missing.
        *waiting = new_instance(&self.name, &self.descriptor, false).ok();
        let handle = instance.0;
        std::mem::forget(instance);
        PipeStream::wrap(handle)
    }

    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

pub struct Incoming<'a> {
    listener: &'a PipeListener,
}

impl Iterator for Incoming<'_> {
    type Item = io::Result<PipeStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept())
    }
}

fn new_instance(name: &[u16], descriptor: &Descriptor, first: bool) -> io::Result<Owned> {
    let attributes = descriptor.attributes();
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: the name is NUL-terminated and the attributes outlive the call. The pipe
    // mode of 0 means bytes, read as bytes, with blocking left to overlapped I/O.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &raw const attributes,
        )
    };
    if handle == invalid_handle() {
        return Err(io::Error::last_os_error());
    }
    Ok(Owned(handle))
}

fn wait_for_client(instance: &Owned) -> io::Result<()> {
    let event = Owned::event()?;
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: event.0,
    };
    // SAFETY: `overlapped` stays valid since this waits for the operation to end.
    unsafe {
        if ConnectNamedPipe(instance.0, &raw mut overlapped) != 0 {
            return Ok(());
        }
        match last_error() {
            ERROR_PIPE_CONNECTED => Ok(()),
            ERROR_IO_PENDING => {
                let mut count = 0;
                if GetOverlappedResult(instance.0, &raw mut overlapped, &raw mut count, 1) != 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
            code => Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
}
