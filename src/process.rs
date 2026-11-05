//! Running native tools safely.
//!
//! Everything WispX delegates (whisper.cpp, curl, clipboard tools, notifiers)
//! goes through [`run`], which guarantees that no misbehaving tool can wedge
//! the daemon: arguments never pass through a shell, output is drained on
//! dedicated threads and bounded, and every call has a deadline and can be
//! cancelled. Tools that fork a background server while still holding our
//! pipes (a classic clipboard-tool hang) cannot block us either.

use crate::sys;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for output after a tool has exited.
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// Enough to explain a failure, without unbounded memory for a chatty tool.
const STDERR_TAIL: usize = 8 * 1024;
const ETXTBSY: i32 = 26;

/// Finds executables on a captured search path.
#[derive(Clone, Debug)]
pub struct Toolbox {
    path: OsString,
}

impl Toolbox {
    pub fn from_env() -> Self {
        Toolbox {
            path: env::var_os("PATH").unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub fn with_path(path: impl Into<OsString>) -> Self {
        Toolbox { path: path.into() }
    }

    /// Resolves a bare name on the search path, or checks an explicit path.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') {
            let path = Path::new(name);
            return is_executable(path).then(|| path.to_path_buf());
        }
        env::split_paths(&self.path)
            // An empty entry would mean "the current directory".
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    }

    pub fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub struct Spec<'a> {
    pub program: &'a Path,
    pub args: &'a [OsString],
    pub stdin: Option<&'a [u8]>,
    pub timeout: Duration,
    pub cancel: Option<&'a AtomicBool>,
    /// When false, stdout and stderr go to /dev/null. Required for tools that
    /// leave a background server running, such as wl-copy.
    pub capture: bool,
    pub stdout_limit: usize,
}

impl<'a> Spec<'a> {
    pub fn new(program: &'a Path, args: &'a [OsString], timeout: Duration) -> Self {
        Spec {
            program,
            args,
            stdin: None,
            timeout,
            cancel: None,
            capture: true,
            stdout_limit: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    /// The last few kilobytes only.
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum RunError {
    Spawn(String, io::Error),
    TimedOut(String, Duration),
    Cancelled,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Spawn(program, error) => write!(f, "cannot run {program}: {error}"),
            RunError::TimedOut(program, limit) => {
                write!(f, "{program} did not finish within {} s", limit.as_secs())
            }
            RunError::Cancelled => write!(f, "cancelled"),
        }
    }
}

pub fn run(spec: Spec<'_>) -> Result<Output, RunError> {
    let name = short_name(spec.program);
    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if spec.capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if spec.capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // Its own group, so a timeout also reaches children of wrapper scripts.
        .process_group(0);

    let mut child = spawn_with_retry(&mut command).map_err(|e| RunError::Spawn(name.clone(), e))?;
    let pid = child.id();

    if let (Some(bytes), Some(mut stdin)) = (spec.stdin, child.stdin.take()) {
        let bytes = bytes.to_vec();
        // Ends with EPIPE at the latest when the child exits or is killed.
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let stdout = child
        .stdout
        .take()
        .map(|pipe| Drain::spawn(pipe, Capture::head(spec.stdout_limit)));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| Drain::spawn(pipe, Capture::tail(STDERR_TAIL)));

    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                kill_and_reap(pid, child);
                return Err(RunError::Spawn(name, error));
            }
        }
        let cancelled = spec.cancel.is_some_and(|c| c.load(Ordering::SeqCst));
        if cancelled || sys::shutdown_requested() {
            kill_and_reap(pid, child);
            return Err(RunError::Cancelled);
        }
        if started.elapsed() >= spec.timeout {
            kill_and_reap(pid, child);
            return Err(RunError::TimedOut(name, spec.timeout));
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    };

    let (stdout, stdout_truncated) = stdout.map_or((Vec::new(), false), Drain::finish);
    let (stderr, _) = stderr.map_or((Vec::new(), false), Drain::finish);
    Ok(Output {
        status,
        stdout,
        stdout_truncated,
        stderr,
    })
}

/// Retries the spurious "text file busy" another thread's concurrent fork can
/// cause for a script that was only just written.
pub fn spawn_with_retry(command: &mut Command) -> io::Result<Child> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(ETXTBSY) && attempts < 10 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

fn kill_and_reap(pid: u32, mut child: Child) {
    let _ = sys::signal_group(pid, sys::SIGKILL);
    let _ = child.kill();
    // After SIGKILL this only blocks for uninterruptible I/O, so do not make
    // the caller wait for that.
    thread::spawn(move || {
        let _ = child.wait();
    });
}

pub fn short_name(program: &Path) -> String {
    program
        .file_name()
        .unwrap_or_else(|| OsStr::new("program"))
        .to_string_lossy()
        .into_owned()
}

/// The last meaningful line of a tool's stderr, safe to show in a notification.
pub fn last_error_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    let clean: String = line.chars().filter(|c| !c.is_control()).take(200).collect();
    Some(clean)
}

/// Bounded collection of a pipe's bytes.
pub struct Capture {
    data: Vec<u8>,
    limit: usize,
    keep_tail: bool,
    truncated: bool,
}

impl Capture {
    pub fn head(limit: usize) -> Self {
        Capture {
            data: Vec::new(),
            limit,
            keep_tail: false,
            truncated: false,
        }
    }

    pub fn tail(limit: usize) -> Self {
        Capture {
            data: Vec::new(),
            limit,
            keep_tail: true,
            truncated: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.keep_tail {
            self.data.extend_from_slice(bytes);
            if self.data.len() > self.limit {
                let excess = self.data.len() - self.limit;
                self.data.drain(..excess);
                self.truncated = true;
            }
        } else {
            let room = self.limit - self.data.len();
            let taken = room.min(bytes.len());
            self.data.extend_from_slice(&bytes[..taken]);
            self.truncated |= taken < bytes.len();
        }
    }

    pub fn take(&mut self) -> (Vec<u8>, bool) {
        (std::mem::take(&mut self.data), self.truncated)
    }
}

/// Reads a pipe on its own thread. Beyond the capture limit bytes are read and
/// discarded, so a verbose tool can never block on a full pipe.
pub struct Drain {
    capture: Arc<Mutex<Capture>>,
    done: Receiver<()>,
}

impl Drain {
    pub fn spawn(mut pipe: impl Read + Send + 'static, capture: Capture) -> Self {
        let capture = Arc::new(Mutex::new(capture));
        let shared = Arc::clone(&capture);
        let (done_tx, done) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut capture) = shared.lock() {
                            capture.push(&buffer[..n]);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = done_tx.send(());
        });
        Drain { capture, done }
    }

    /// Returns what was captured, without waiting long for a pipe that some
    /// surviving background process still holds open.
    pub fn finish(self) -> (Vec<u8>, bool) {
        let _ = self.done.recv_timeout(DRAIN_GRACE);
        self.capture
            .lock()
            .map(|mut capture| capture.take())
            .unwrap_or_default()
    }

    /// A snapshot of the bytes so far.
    pub fn peek(&self) -> Vec<u8> {
        self.capture
            .lock()
            .map(|capture| capture.data.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::process::ExitStatusExt;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn captures_stdout_stderr_and_stdin() {
        let temp = TempDir::new();
        let script = temp.script("tool", "cat\necho problem >&2\nexit 3");
        let args = os(&[]);
        let mut spec = Spec::new(&script, &args, Duration::from_secs(5));
        spec.stdin = Some(b"from stdin");
        let output = run(spec).unwrap();
        assert_eq!(output.stdout, b"from stdin");
        assert_eq!(last_error_line(&output.stderr).unwrap(), "problem");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn arguments_never_reach_a_shell() {
        let temp = TempDir::new();
        let marker = temp.path().join("pwned");
        let script = temp.script("tool", r#"printf '%s\n' "$@""#);
        let hostile = format!("; touch {} #", marker.display());
        let args = vec![OsString::from(&hostile), OsString::from("$(id)")];
        let output = run(Spec::new(&script, &args, Duration::from_secs(5))).unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text, format!("{hostile}\n$(id)\n"));
        assert!(!marker.exists());
    }

    #[test]
    fn huge_output_on_both_pipes_does_not_deadlock() {
        let temp = TempDir::new();
        let script = temp.script(
            "tool",
            "head -c 5000000 /dev/zero\nhead -c 5000000 /dev/zero >&2\nhead -c 5000000 /dev/zero",
        );
        let args = os(&[]);
        let mut spec = Spec::new(&script, &args, Duration::from_secs(30));
        spec.stdout_limit = 1024;
        let output = run(spec).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1024);
        assert!(output.stdout_truncated);
        assert_eq!(output.stderr.len(), STDERR_TAIL);
    }

    #[test]
    fn timeout_kills_the_tool_and_its_children() {
        let temp = TempDir::new();
        let pid_file = temp.path().join("grandchild");
        let script = temp.script(
            "tool",
            &format!("sleep 60 &\necho $! > {}\nwait", pid_file.display()),
        );
        let args = os(&[]);
        let started = Instant::now();
        let error = run(Spec::new(&script, &args, Duration::from_millis(300))).unwrap_err();
        assert!(matches!(error, RunError::TimedOut(..)), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = || Path::new(&format!("/proc/{}", pid.trim())).exists();
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "grandchild survived the group kill");
    }

    #[test]
    fn cancellation_stops_a_running_tool_promptly() {
        let temp = TempDir::new();
        let script = temp.script("tool", "exec sleep 60");
        let args = os(&[]);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let mut spec = Spec::new(&script, &args, Duration::from_secs(60));
        spec.cancel = Some(&cancel);
        assert!(matches!(run(spec), Err(RunError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_daemonizing_tool_holding_our_pipes_cannot_hang_us() {
        let temp = TempDir::new();
        // Exits at once but leaves a background process owning stdout/stderr.
        let script = temp.script("tool", "sleep 30 &\necho hello");
        let args = os(&[]);
        let started = Instant::now();
        let output = run(Spec::new(&script, &args, Duration::from_secs(10))).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(output.stdout, b"hello\n");
    }

    #[test]
    fn a_signal_death_is_reported_as_failure() {
        let temp = TempDir::new();
        let script = temp.script("tool", "kill -9 $$");
        let args = os(&[]);
        let output = run(Spec::new(&script, &args, Duration::from_secs(5))).unwrap();
        assert!(!output.status.success());
        assert_eq!(output.status.signal(), Some(9));
    }

    #[test]
    fn missing_program_is_a_spawn_error() {
        let args = os(&[]);
        let spec = Spec::new(
            Path::new("/nonexistent/tool"),
            &args,
            Duration::from_secs(1),
        );
        assert!(matches!(run(spec), Err(RunError::Spawn(..))));
    }

    #[test]
    fn toolbox_requires_an_executable_and_ignores_relative_entries() {
        let temp = TempDir::new();
        temp.script("present", "exit 0");
        std::fs::write(temp.path().join("not-executable"), b"x").unwrap();
        let tools = Toolbox::with_path(temp.path());
        assert!(tools.find("present").is_some());
        assert!(tools.find("not-executable").is_none());
        assert!(tools.find("absent").is_none());

        let relative = Toolbox::with_path(":.:relative");
        assert!(relative.find("sh").is_none());
    }

    #[test]
    fn toolbox_checks_explicit_paths_directly() {
        let temp = TempDir::new();
        let script = temp.script("tool", "exit 0");
        let tools = Toolbox::with_path("");
        assert_eq!(tools.find(script.to_str().unwrap()), Some(script));
        assert!(tools.find("/nonexistent/tool").is_none());
    }

    #[test]
    fn tail_capture_keeps_only_the_end() {
        let mut capture = Capture::tail(4);
        capture.push(b"abcdef");
        capture.push(b"gh");
        let (data, truncated) = capture.take();
        assert_eq!(data, b"efgh");
        assert!(truncated);
    }

    #[test]
    fn last_error_line_skips_blanks_and_control_bytes() {
        assert_eq!(
            last_error_line(b"first\n\x1b[31mred\x1b[0m\n  \n").unwrap(),
            "[31mred[0m"
        );
        assert_eq!(last_error_line(b"\n\n"), None);
    }
}
