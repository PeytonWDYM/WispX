//! Keeps the desktop app to one running copy, and lets later launches reach it.
//!
//! As with the daemon, a lock decides who runs, so a crash can't leave anything
//! stale. The socket next to it answers `ping`, `show`, `inspect` and `quit`.

use crate::ipc::{self, ClientError};
use crate::paths;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

const MAX_LINE: u64 = 256;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Long enough for the app to stop everything it started.
const REPLY_WAIT: Duration = Duration::from_secs(15);

fn lock_path(runtime: &Path) -> PathBuf {
    runtime.join("app.lock")
}

fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join("app.sock")
}

/// `Ok(None)` means the app is already running.
pub fn acquire(runtime: &Path) -> Result<Option<File>, String> {
    crate::daemon::acquire_lock(&lock_path(runtime))
}

/// Answers commands on background threads. Call only while holding the lock.
pub fn serve(
    runtime: &Path,
    handler: impl Fn(String, Sender<Value>) + Send + Sync + 'static,
) -> Result<(), String> {
    let path = socket_path(runtime);
    let listener =
        ipc::bind(&path).map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    let handler = Arc::new(handler);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let handler = Arc::clone(&handler);
            thread::spawn(move || answer(&stream, handler.as_ref()));
        }
    });
    Ok(())
}

fn answer(stream: &ipc::Stream, handler: &(impl Fn(String, Sender<Value>) + ?Sized)) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    if BufReader::new(stream.take(MAX_LINE))
        .read_line(&mut line)
        .is_err()
    {
        return;
    }
    let (reply, answered) = mpsc::channel();
    handler(line.trim().to_string(), reply);
    let value = answered
        .recv_timeout(REPLY_WAIT)
        .unwrap_or_else(|_| json!({ "ok": false, "message": "WispX did not answer in time" }));
    let mut writer = stream;
    let _ = writer.write_all(format!("{value}\n").as_bytes());
}

/// Sends a command to the running app, if there is one.
pub fn send(command: &str) -> Result<Value, ClientError> {
    // Windows only lets the running copy come forward if the one just started says so.
    #[cfg(windows)]
    if command.starts_with("show") {
        crate::sys::allow_foreground();
    }
    let socket = paths::client_socket_path().with_file_name("app.sock");
    let stream = ipc::Stream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::NotRunning,
        _ => ClientError::Failed(format!("cannot reach the WispX app: {error}")),
    })?;
    let failed = |error: io::Error| ClientError::Failed(format!("WispX app connection: {error}"));
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(failed)?;
    stream
        .set_read_timeout(Some(REPLY_WAIT + IO_TIMEOUT))
        .map_err(failed)?;
    let mut writer = &stream;
    writer
        .write_all(format!("{command}\n").as_bytes())
        .map_err(failed)?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(failed)?;
    serde_json::from_str(&line)
        .map_err(|_| ClientError::Failed("the WispX app sent an invalid reply".to_string()))
}
