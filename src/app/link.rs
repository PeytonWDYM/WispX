//! The app's connection to the daemon: finds a running one or starts its own, and
//! streams what it reports. One daemon is ever in charge, so there is no state
//! here to disagree with it.

use crate::ipc::{self, ClientError, Watch};
use crate::paths;
use crate::process::{self, Capture, Drain};
use crate::sys;
use serde_json::Value;
use std::env;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const START_LIMIT: Duration = Duration::from_secs(10);
/// The daemon's own shutdown is bounded and well under these.
const STOP_TERM_AFTER: Duration = Duration::from_secs(6);
const STOP_KILL_AFTER: Duration = Duration::from_secs(8);
const STDERR_TAIL: usize = 4 * 1024;
/// `daemon::EXIT_ALREADY_RUNNING` under a name that says what it means here.
const LOST_RACE: i32 = crate::daemon::EXIT_ALREADY_RUNNING;

/// How a daemon this app started ended.
#[derive(Debug)]
pub struct Exit {
    pub clean: bool,
    pub code: Option<i32>,
    pub message: String,
}

/// A daemon this app started. The thread that holds the process is the only one
/// that signals it, and only before reaping it, so a signal can't hit a stranger.
pub struct OwnedDaemon {
    stop: Arc<AtomicBool>,
    gone: Mutex<Receiver<()>>,
}

impl OwnedDaemon {
    /// Closes its stdin, which is how it knows to stop, and waits for it.
    pub fn stop_and_wait(&self, limit: Duration) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(gone) = self.gone.lock() {
            let _ = gone.recv_timeout(limit);
        }
    }

    pub fn ask_to_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(unix)]
pub fn daemon_program() -> PathBuf {
    env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("wispx"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("wispx"))
}

/// `wispx-gui.exe` can be the daemon too, so a lone copy of it is all anyone needs.
#[cfg(windows)]
pub fn daemon_program() -> PathBuf {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("wispx-gui.exe"));
    match exe.with_file_name("wispx.exe") {
        sibling if sibling.is_file() => sibling,
        _ => exe,
    }
}

fn start_daemon(
    program: &Path,
    config: Option<&Path>,
    on_exit: impl FnOnce(Exit) + Send + 'static,
) -> Result<OwnedDaemon, String> {
    let mut command = Command::new(program);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    command
        .args(["daemon", "--quiet", "--app-child"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    process::own_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", program.display()))?;
    let stderr = child
        .stderr
        .take()
        .map(|pipe| Drain::spawn(pipe, Capture::tail(STDERR_TAIL)));
    let stop = Arc::new(AtomicBool::new(false));
    let (gone_tx, gone) = mpsc::channel();
    {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let exit = supervise(child, &stop, stderr);
            let _ = gone_tx.send(());
            on_exit(exit);
        });
    }
    Ok(OwnedDaemon {
        stop,
        gone: Mutex::new(gone),
    })
}

fn supervise(mut child: Child, stop: &AtomicBool, stderr: Option<Drain>) -> Exit {
    let mut stdin = child.stdin.take();
    let mut stopping_since: Option<Instant> = None;
    let mut signalled_term = false;
    let mut signalled_kill = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(_) => break None,
            Ok(None) => {}
        }
        if stop.load(Ordering::SeqCst) {
            let since = *stopping_since.get_or_insert_with(Instant::now);
            stdin = None;
            let elapsed = since.elapsed();
            if elapsed >= STOP_KILL_AFTER && !signalled_kill {
                let _ = sys::signal_group(child.id(), sys::SIGKILL);
                signalled_kill = true;
            } else if elapsed >= STOP_TERM_AFTER && !signalled_term {
                let _ = sys::signal_group(child.id(), sys::SIGTERM);
                signalled_term = true;
            }
        }
        thread::sleep(Duration::from_millis(30));
    };
    drop(stdin);
    let tail = stderr.map(|drain| drain.finish().0).unwrap_or_default();
    let text = String::from_utf8_lossy(&tail);
    let message = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.strip_prefix("wispx: ").unwrap_or(line).to_string())
        .unwrap_or_default();
    match status {
        Some(status) => Exit {
            clean: status.success(),
            code: status.code(),
            message: if message.is_empty() {
                stopped_by_signal(status)
            } else {
                message
            },
        },
        None => Exit {
            clean: false,
            code: None,
            message,
        },
    }
}

#[cfg(unix)]
fn stopped_by_signal(status: std::process::ExitStatus) -> String {
    status.signal().map_or_else(String::new, |signal| {
        format!("the background service was stopped by signal {signal}")
    })
}

#[cfg(windows)]
fn stopped_by_signal(_status: std::process::ExitStatus) -> String {
    String::new()
}

/// What came of looking for a daemon.
pub enum Connected {
    /// One was already running, so it was left to carry on as it was.
    Attached(Watch),
    /// None was, so this app started it.
    Started(Watch, OwnedDaemon),
    Failed(String),
}

/// Connects to the running daemon, or starts one and waits for it to listen. If
/// one starts at the same moment, that one is used. Blocks, so runs off the UI.
pub fn connect(
    config: Option<&Path>,
    presents_feedback: bool,
    may_start: bool,
    on_exit: impl FnOnce(Exit) + Send + 'static,
) -> Option<Connected> {
    let socket = paths::client_socket_path();
    match Watch::open(&socket, presents_feedback) {
        Ok(watch) => return Some(Connected::Attached(watch)),
        Err(ClientError::NotRunning) => {}
        Err(ClientError::Failed(error)) => return Some(Connected::Failed(error)),
    }
    if !may_start {
        return None;
    }

    let (exit_tx, exit_rx) = mpsc::channel::<Exit>();
    let owned = match start_daemon(&daemon_program(), config, move |exit| {
        let _ = exit_tx.send(exit);
    }) {
        Ok(owned) => owned,
        Err(error) => return Some(Connected::Failed(error)),
    };
    let deadline = Instant::now() + START_LIMIT;
    loop {
        // The socket that matters may only just have been created.
        let socket = paths::client_socket_path();
        if let Ok(watch) = Watch::open(&socket, presents_feedback) {
            // The exit report now belongs to whoever is watching this daemon.
            thread::spawn(move || {
                if let Ok(exit) = exit_rx.recv() {
                    on_exit(exit);
                }
            });
            return Some(Connected::Started(watch, owned));
        }
        match exit_rx.try_recv() {
            Ok(exit) if exit.code == Some(LOST_RACE) => {
                // Some other daemon holds the lock, so it should be listening soon.
                let attached = wait_to_attach(presents_feedback, deadline);
                return Some(attached.map_or_else(
                    || {
                        Connected::Failed(
                            "another WispX daemon is running but is not answering".to_string(),
                        )
                    },
                    Connected::Attached,
                ));
            }
            Ok(exit) => {
                return Some(Connected::Failed(if exit.message.is_empty() {
                    "the background service stopped straight away".to_string()
                } else {
                    exit.message
                }));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Some(Connected::Failed(
                    "the background service stopped straight away".to_string(),
                ));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            owned.ask_to_stop();
            return Some(Connected::Failed(format!(
                "the background service did not start within {} s",
                START_LIMIT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(30));
    }
}

fn wait_to_attach(presents_feedback: bool, deadline: Instant) -> Option<Watch> {
    loop {
        let socket = paths::client_socket_path();
        if let Ok(watch) = Watch::open(&socket, presents_feedback) {
            return Some(watch);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(30));
    }
}

/// Streams events until the daemon goes, handing back how to end it early.
pub fn pump(
    mut watch: Watch,
    mut on_event: impl FnMut(Value) + Send + 'static,
    on_end: impl FnOnce() + Send + 'static,
) -> Option<ipc::Stream> {
    let closer = watch.closer();
    thread::spawn(move || {
        while let Some(event) = watch.next_event() {
            on_event(event);
        }
        on_end();
    });
    closer
}
