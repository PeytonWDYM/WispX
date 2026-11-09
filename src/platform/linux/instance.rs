//! One WispX per user session: a lock file elects the owner, and a private socket
//! lets later launches wake its Options window or ask it to quit.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceEvent {
    Wake,
    Quit,
}
impl InstanceEvent {
    fn word(self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Quit => "quit",
        }
    }
    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "wake" => Some(Self::Wake),
            "quit" => Some(Self::Quit),
            _ => None,
        }
    }
}
pub struct Instance {
    _lock: File,
    socket: PathBuf,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// `$XDG_RUNTIME_DIR/WispX`, private to this user; falls back to the cache directory.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No runtime directory"))?;
    let dir = base.join("WispX");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

impl Instance {
    /// Returns `None` after handing `request` to the instance that already runs.
    pub fn acquire(
        dir: &Path,
        request: InstanceEvent,
        sink: Arc<dyn Fn(InstanceEvent) + Send + Sync>,
    ) -> io::Result<Option<Self>> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join("instance.lock"))?;
        let socket = dir.join("instance.sock");
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                signal(&socket, request)?;
                return Ok(None);
            }
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
        // The lock proves any existing socket file belongs to a process that has exited.
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = thread::Builder::new()
            .name("instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut line = String::new();
                    if BufReader::new(stream).read_line(&mut line).is_ok()
                        && let Some(event) = InstanceEvent::parse(&line)
                    {
                        sink(event);
                    }
                }
            })?;
        Ok(Some(Self {
            _lock: lock,
            socket,
            stopping,
            thread: Some(thread),
        }))
    }
}

/// The owner binds its socket just after taking the lock, so allow it a moment.
fn signal(socket: &Path, event: InstanceEvent) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match UnixStream::connect(socket) {
            Ok(mut stream) => return writeln!(stream, "{}", event.word()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Asks a running instance to quit; `Ok(false)` when none is running.
pub fn quit_running(dir: &Path) -> io::Result<bool> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("instance.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => {
            signal(&dir.join("instance.sock"), InstanceEvent::Quit)?;
            Ok(true)
        }
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // Wake the blocking accept so the thread observes the flag.
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: a second launch runs a second app; a stale socket from a
    // crashed owner blocks the next start; wake/quit requests are lost or
    // confused; the listener thread outlives its owner.
    use super::*;
    use std::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wispx-instance-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn second_launch_wakes_or_quits_the_owner() {
        let dir = scratch("wake");
        // A stale socket file must not prevent the owner from binding.
        fs::write(dir.join("instance.sock"), b"").expect("stale socket");
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let owner = Instance::acquire(
            &dir,
            InstanceEvent::Wake,
            Arc::new(move |event| {
                let _ = tx.lock().expect("sender").send(event);
            }),
        )
        .expect("acquire")
        .expect("first launch owns the instance");
        let second =
            Instance::acquire(&dir, InstanceEvent::Wake, Arc::new(|_| {})).expect("second acquire");
        assert!(second.is_none());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(InstanceEvent::Wake)
        );
        assert!(quit_running(&dir).expect("quit"));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(InstanceEvent::Quit)
        );
        drop(owner);
        assert!(!quit_running(&dir).expect("no owner"));
        assert!(!dir.join("instance.sock").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_parse_strictly() {
        assert_eq!(InstanceEvent::parse("wake\n"), Some(InstanceEvent::Wake));
        assert_eq!(InstanceEvent::parse(" quit "), Some(InstanceEvent::Quit));
        assert_eq!(InstanceEvent::parse("open"), None);
    }
}
