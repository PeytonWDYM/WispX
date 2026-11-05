//! `wispx` with no command: opens the app and gets out of the way.
//!
//! The app runs in a session of its own, so closing the terminal does not reach
//! it. What it prints goes to a private log, shown here if it fails to start.

use super::instance;
use crate::ipc::ClientError;
use crate::paths;
use crate::process::Toolbox;
use crate::sys;
use std::env;
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const START_LIMIT: Duration = Duration::from_secs(20);
const LOG_TAIL: usize = 12;
const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_NO_APP: i32 = 2;

fn has_display() -> bool {
    cfg!(windows)
        || ["WAYLAND_DISPLAY", "DISPLAY"]
            .iter()
            .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn app_program() -> Option<PathBuf> {
    let sibling = env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(format!("wispx-gui{}", env::consts::EXE_SUFFIX)))
        .filter(|path| path.is_file());
    sibling.or_else(|| Toolbox::from_env().find("wispx-gui"))
}

pub fn app_available() -> bool {
    app_program().is_some()
}

/// A later launch brings the settings window up. The token the desktop gave this
/// process goes along, since it is what lets that window take focus on Wayland.
fn handover(background: bool) -> String {
    if background {
        return "ping".to_string();
    }
    match env::var("XDG_ACTIVATION_TOKEN") {
        Ok(token)
            if !token.is_empty() && token.len() < 128 && !token.contains(char::is_whitespace) =>
        {
            format!("show {token}")
        }
        _ => "show".to_string(),
    }
}

pub fn open(config: Option<&Path>, background: bool) -> i32 {
    if !has_display() {
        eprintln!(
            "wispx: no graphical session here, so the app cannot open. `wispx daemon` runs \
             without a window, and `wispx help` lists every command"
        );
        return EXIT_FAILED;
    }
    match instance::send(&handover(background)) {
        Ok(_) => return EXIT_OK,
        Err(ClientError::NotRunning) => {}
        Err(ClientError::Failed(error)) => {
            eprintln!("wispx: {error}");
            return EXIT_FAILED;
        }
    }
    let Some(program) = app_program() else {
        eprintln!(
            "wispx: this build has no desktop app (wispx-gui was not found). `wispx daemon` runs \
             without a window, and `wispx help` lists every command"
        );
        return EXIT_NO_APP;
    };
    match start(&program, config, background) {
        Ok(()) => EXIT_OK,
        Err(message) => {
            eprintln!("wispx: {message}");
            EXIT_FAILED
        }
    }
}

fn start(program: &Path, config: Option<&Path>, background: bool) -> Result<(), String> {
    let runtime = paths::runtime_dir()?;
    let log_path = runtime.join("app.log");
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let log = options
        .open(&log_path)
        .map_err(|error| format!("cannot open {}: {error}", log_path.display()))?;
    let mut command = Command::new(program);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    if background {
        command.arg("--background");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    sys::detach(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", program.display()))?;

    let deadline = Instant::now() + START_LIMIT;
    loop {
        if instance::send("ping").is_ok() {
            return Ok(());
        }
        match child.try_wait() {
            // Another launch got there first and has been told to come forward.
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "the app stopped straight away ({status}).{}",
                    log_tail(&log_path)
                ));
            }
            Ok(None) | Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the app did not open within {} s.{}",
                START_LIMIT.as_secs(),
                log_tail(&log_path)
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n");
    if tail.is_empty() {
        String::new()
    } else {
        format!("\n{tail}")
    }
}
