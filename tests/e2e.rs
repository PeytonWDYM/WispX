//! End-to-end tests of the real binary, entirely against stand-ins.
//!
//! MOCKED: recorder, whisper-cli, curl and clipboard tools are shell scripts on a
//! private PATH. The portal, KWin and notification server are a mock service on a
//! private `dbus-daemon`, which is real, so messages cross a real bus.
//!
//! Each test clears the environment and gets scratch XDG directories, so nothing
//! reaches the microphone, network, Codex login, clipboard or desktop session.

#[allow(dead_code)]
#[path = "../src/dbus.rs"]
mod dbus;

use dbus::{Connection, Kind, Message, Value};
use serde_json::Value as Json;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TRANSCRIPT: &str = "hello from the fake whisper";
const SECOND: Duration = Duration::from_secs(1);
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";
const REMOTE: &str = "org.freedesktop.portal.RemoteDesktop";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
const CONTROL_L: i32 = 0xffe3;
const SHIFT_L: i32 = 0xffe1;
const KEY_V: i32 = 0x76;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + SECOND * 15;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Coreutils a fake tool may need; they are all it can see.
const UTILITIES: [&str; 6] = ["cat", "cp", "mv", "sleep", "stat", "touch"];

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
}

struct Sandbox {
    root: PathBuf,
    bus_address: Option<String>,
    desktop: &'static str,
    daemon: Option<Child>,
}

impl Sandbox {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/wispx-e2e-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        for dir in ["bin", "home", "run", "config", "data", "codex"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        fs::set_permissions(root.join("run"), Permissions::from_mode(0o700)).unwrap();
        for tool in UTILITIES {
            let real = env::split_paths(&env::var_os("PATH").unwrap())
                .map(|dir| dir.join(tool))
                .find(|path| path.exists())
                .unwrap_or_else(|| panic!("{tool} is needed to run these tests"));
            symlink(real, root.join("bin").join(tool)).unwrap();
        }

        // Half a second of a loud tone, and the same length of silence.
        let loud: Vec<u8> = (0..8000)
            .flat_map(|i| (if i % 2 == 0 { 20_000i16 } else { -20_000 }).to_le_bytes())
            .collect();
        fs::write(root.join("loud.raw"), loud).unwrap();
        fs::write(root.join("silent.raw"), vec![0u8; 16_000]).unwrap();
        fs::write(root.join("model.bin"), b"fake model").unwrap();

        let sandbox = Sandbox {
            root,
            bus_address: None,
            desktop: "",
            daemon: None,
        };
        sandbox.recorder("loud.raw");
        sandbox.whisper_says(TRANSCRIPT);
        // Changes in one step, as the real clipboard does, so that a test reading it
        // while the text is being copied again never finds it half written.
        sandbox.tool(
            "wl-copy",
            &format!(
                r#"[ "$1" = --help ] && {{ echo '--sensitive'; exit 0; }}
printf '%s\n' "$@" > {r}/copy-args
cat > {r}/clipboard.$$
mv {r}/clipboard.$$ {r}/clipboard
echo x >> {r}/copy-count"#,
                r = sandbox.root.display()
            ),
        );
        sandbox.tool(
            "wl-paste",
            &format!(
                "[ -f {r}/clipboard ] || exit 1\ncat {r}/clipboard",
                r = sandbox.root.display()
            ),
        );
        sandbox.tool(
            "curl",
            &format!("touch {}/curl-ran\nexit 1", sandbox.root.display()),
        );
        sandbox.configure(r#""paste":"copy""#);
        sandbox
    }

    fn tool(&self, name: &str, body: &str) {
        write_script(&self.root.join("bin").join(name), body);
    }

    /// A recorder that delivers a fixture and then idles like a live microphone.
    fn recorder(&self, fixture: &str) {
        let r = self.root.display();
        self.tool(
            "pw-record",
            &format!("echo $$ > {r}/recorder.pid\ncat {r}/{fixture}\nexec sleep 60"),
        );
    }

    fn whisper_says(&self, text: &str) {
        let r = self.root.display();
        self.tool(
            "whisper-cli",
            &format!(
                r#"echo $$ > {r}/whisper.pid
printf '%s\n' "$@" > {r}/whisper-args
cp "$4" {r}/saved.wav
stat -c %a "$4" > {r}/saved.mode
echo ' {text}'"#
            ),
        );
    }

    fn whisper_hangs(&self) {
        let r = self.root.display();
        self.tool(
            "whisper-cli",
            &format!(
                r#"cp "$4" {r}/saved.wav
echo $$ > {r}/whisper.pid
exec sleep 60"#
            ),
        );
    }

    /// Writes the config, with `extra` top-level settings. Short holds count, so
    /// tests need not wait around, and sounds are off unless a test wants them.
    fn configure(&self, extra: &str) {
        fs::create_dir_all(self.root.join("config/wispx")).unwrap();
        fs::write(self.config_file(), self.config_text(extra)).unwrap();
    }

    fn config_text(&self, extra: &str) -> String {
        let r = self.root.display();
        format!(
            r#"{{
                "sounds": false,
                "notifications": "none",
                "hotkey": "none",
                "min_hold_ms": 50,
                "audio": {{ "recorder": "pw-record" }},
                "whisper": {{ "binary": "whisper-cli", "model": "{r}/model.bin", "threads": 2 }},
                {extra}
            }}"#
        )
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config/wispx/config.json")
    }

    fn runtime(&self) -> PathBuf {
        self.root.join("run/wispx")
    }

    fn socket(&self) -> PathBuf {
        self.runtime().join("wispx.sock")
    }

    fn token_file(&self) -> PathBuf {
        self.root.join("data/wispx/remote-desktop-token")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wispx"));
        command
            .env_clear()
            .env("PATH", self.root.join("bin"))
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("WAYLAND_DISPLAY", "wispx-e2e-not-a-display")
            .env("XDG_CURRENT_DESKTOP", self.desktop)
            .stdin(Stdio::null());
        if let Some(address) = &self.bus_address {
            command.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        command
    }

    fn wispx(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// Runs a command that should succeed and returns its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.wispx(args);
        assert!(
            output.status.success(),
            "wispx {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn start_daemon(&mut self, args: &[&str]) {
        self.start_daemon_with(&[], args);
    }

    /// `global` goes before the command, where `--config` belongs.
    fn start_daemon_with(&mut self, global: &[&str], args: &[&str]) {
        let log = File::create(self.root.join("daemon.log")).unwrap();
        let child = self
            .command()
            .args(global)
            .arg("daemon")
            .arg("--quiet")
            .args(args)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        wait_until("the daemon to listen", || {
            self.wispx(&["status"]).status.success()
        });
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    fn status(&self) -> Json {
        serde_json::from_str(&self.ok(&["status", "--json"])).unwrap()
    }

    fn state(&self) -> String {
        self.status()["state"].as_str().unwrap().to_string()
    }

    fn wait_for_state(&self, wanted: &str) {
        wait_until(&format!("state {wanted}"), || self.state() == wanted);
    }

    fn pid_of(&self, file: &str) -> u32 {
        let path = self.root.join(file);
        wait_until(file, || {
            fs::read_to_string(&path).is_ok_and(|text| !text.trim().is_empty())
        });
        fs::read_to_string(&path).unwrap().trim().parse().unwrap()
    }

    fn has(&self, file: &str) -> bool {
        self.root.join(file).exists()
    }

    fn clipboard(&self) -> Option<String> {
        fs::read_to_string(self.root.join("clipboard")).ok()
    }

    fn wait_for_clipboard(&self, wanted: &str) {
        wait_until(&format!("clipboard to hold {wanted:?}"), || {
            self.clipboard().as_deref() == Some(wanted)
        });
    }

    /// Lets the recorder deliver its fixture, so a stop has audio to work with.
    fn wait_for_recording(&self) {
        self.pid_of("recorder.pid");
        thread::sleep(Duration::from_millis(150));
    }

    /// Records and stops, with `wispx press` and `wispx release`.
    fn dictate(&self) {
        self.ok(&["press"]);
        self.wait_for_recording();
        self.ok(&["release"]);
    }

    fn saved_backend(&self, path: &Path) -> Json {
        let config: Json = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        config["backend"].clone()
    }

    /// A login the fake curl will accept, with no expiry to check.
    fn sign_in_to_codex(&self) {
        fs::write(
            self.root.join("codex/auth.json"),
            r#"{"tokens":{"access_token":"not-a-real-token"}}"#,
        )
        .unwrap();
    }

    fn job_files(&self) -> Vec<String> {
        fs::read_dir(self.runtime())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("job-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn signal_daemon(&self, signal: &str) {
        let pid = self.daemon.as_ref().unwrap().id();
        let status = Command::new("/bin/sh")
            .args(["-c", &format!("kill -{signal} {pid}")])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn wait_for_exit(&mut self) -> i32 {
        let mut child = self.daemon.take().expect("a daemon was started");
        let deadline = Instant::now() + SECOND * 10;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("the daemon did not exit");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // A tool the daemon started may still be finishing, and can add a file
        // while this runs. Once the directory is gone, it can't come back.
        for _ in 0..50 {
            if fs::remove_dir_all(&self.root).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn process_alive(pid: u32) -> bool {
    // A zombie has exited even though its /proc entry remains.
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit(')')
            .next()
            .is_some_and(|s| !s.trim_start().starts_with('Z'))
    })
}

// Dictation without any desktop integration.

#[test]
fn hold_and_release_copies_the_transcript_and_leaves_nothing_behind() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    assert_eq!(sandbox.state(), "idle");

    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    assert_eq!(sandbox.state(), "recording");
    let level = sandbox.status()["level"].as_f64().unwrap();
    assert!(level > 0.5, "level {level}");

    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    sandbox.wait_for_state("idle");
    assert_eq!(sandbox.ok(&["last"]).trim(), TRANSCRIPT);

    // The backend got a private WAV in the dictation format, named on argv.
    let saved = fs::read(sandbox.root.join("saved.wav")).unwrap();
    assert_eq!(&saved[..4], b"RIFF");
    assert_eq!(u16::from_le_bytes([saved[22], saved[23]]), 1, "mono");
    assert_eq!(
        u32::from_le_bytes([saved[24], saved[25], saved[26], saved[27]]),
        16_000
    );
    assert_eq!(u16::from_le_bytes([saved[34], saved[35]]), 16, "16-bit");
    let mode = fs::read_to_string(sandbox.root.join("saved.mode")).unwrap();
    assert_eq!(mode.trim(), "600");
    let args = fs::read_to_string(sandbox.root.join("whisper-args")).unwrap();
    assert!(args.starts_with(&format!("-m\n{}/model.bin\n-f\n", sandbox.root.display())));

    // Copy mode leaves text for the user, so it may enter clipboard history.
    let copy_args = fs::read_to_string(sandbox.root.join("copy-args")).unwrap();
    assert!(!copy_args.contains("--sensitive"), "{copy_args}");

    // No audio left, the recorder is gone, the local path used no network, and
    // the daemon's log holds no transcript.
    assert!(sandbox.job_files().is_empty());
    let recorder = sandbox.pid_of("recorder.pid");
    wait_until("the recorder to exit", || !process_alive(recorder));
    assert!(!sandbox.has("curl-ran"));
    assert!(!sandbox.daemon_log().contains(TRANSCRIPT));

    sandbox.ok(&["quit"]);
    assert_eq!(sandbox.wait_for_exit(), 0);
}

#[test]
fn copy_last_puts_the_last_transcript_back() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    let error = sandbox.wispx(&["copy-last"]);
    assert_eq!(error.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&error.stderr).contains("Nothing transcribed"));
    assert_eq!(sandbox.wispx(&["last"]).status.code(), Some(1));

    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    fs::remove_file(sandbox.root.join("clipboard")).unwrap();
    sandbox.ok(&["copy-last"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
}

#[test]
fn a_tap_is_discarded_without_transcribing() {
    let mut sandbox = Sandbox::new();
    sandbox.configure(r#""paste":"copy","min_hold_ms":5000"#);
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    let reply = sandbox.ok(&["release"]);
    assert!(reply.contains("Too short"), "{reply}");
    let recorder = sandbox.pid_of("recorder.pid");
    wait_until("the recorder to exit", || !process_alive(recorder));
    assert!(!sandbox.has("whisper-args"));
    assert_eq!(sandbox.clipboard(), None);
    assert_eq!(sandbox.state(), "idle");
}

#[test]
fn silence_is_not_transcribed() {
    let mut sandbox = Sandbox::new();
    sandbox.recorder("silent.raw");
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    let reply = sandbox.ok(&["release"]);
    assert!(reply.contains("Didn't hear anything"), "{reply}");
    assert!(!sandbox.has("whisper-args"));
    assert_eq!(sandbox.clipboard(), None);
}

#[test]
fn an_empty_transcript_changes_nothing() {
    let mut sandbox = Sandbox::new();
    sandbox.whisper_says("[BLANK_AUDIO]");
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    wait_until("whisper to run", || sandbox.has("whisper-args"));
    sandbox.wait_for_state("idle");
    assert_eq!(sandbox.clipboard(), None);
    assert_eq!(sandbox.wispx(&["last"]).status.code(), Some(1));
}

#[test]
fn cancel_discards_the_recording_and_stops_the_recorder() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    let recorder = sandbox.pid_of("recorder.pid");
    assert!(process_alive(recorder));
    sandbox.ok(&["cancel"]);
    wait_until("the recorder to exit", || !process_alive(recorder));
    assert_eq!(sandbox.state(), "idle");
    assert!(!sandbox.has("whisper-args"));
    assert_eq!(sandbox.clipboard(), None);
}

#[test]
fn cancel_while_transcribing_kills_the_backend_and_delivers_nothing() {
    let mut sandbox = Sandbox::new();
    sandbox.whisper_hangs();
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    let whisper = sandbox.pid_of("whisper.pid");
    assert_eq!(sandbox.state(), "transcribing");
    assert_eq!(sandbox.job_files().len(), 1, "audio exists while in use");

    sandbox.ok(&["cancel"]);
    wait_until("the backend to die", || !process_alive(whisper));
    sandbox.wait_for_state("idle");
    assert!(sandbox.job_files().is_empty());
    assert_eq!(sandbox.clipboard(), None);
}

#[test]
fn the_duration_limit_finishes_a_forgotten_recording() {
    let mut sandbox = Sandbox::new();
    sandbox.configure(r#""paste":"copy","max_recording_secs":1"#);
    sandbox.start_daemon(&[]);
    let started = Instant::now();
    sandbox.ok(&["start"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    assert!(started.elapsed() >= SECOND, "stopped early");
    sandbox.wait_for_state("idle");
}

#[test]
fn hands_free_and_locking_a_hold_follow_the_gestures() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);

    // Toggle starts and toggle finishes.
    sandbox.ok(&["toggle"]);
    sandbox.wait_for_recording();
    assert_eq!(sandbox.state(), "recording-hands-free");
    sandbox.ok(&["toggle"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    sandbox.wait_for_state("idle");

    // Toggle while holding keeps recording after the key comes up.
    fs::remove_file(sandbox.root.join("clipboard")).unwrap();
    fs::remove_file(sandbox.root.join("recorder.pid")).unwrap();
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["toggle"]);
    sandbox.ok(&["release"]);
    assert_eq!(sandbox.state(), "recording-hands-free");
    sandbox.ok(&["stop"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
}

#[test]
fn a_recorder_that_fails_is_explained() {
    let mut sandbox = Sandbox::new();
    sandbox.tool("pw-record", "echo 'error: no such target node' >&2\nexit 1");
    sandbox.start_daemon(&[]);
    sandbox.wispx(&["press"]);
    wait_until("the failure to be recorded", || {
        sandbox.status()["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("no such target node"))
    });
    assert_eq!(sandbox.state(), "idle");
    assert!(!sandbox.has("whisper-args"));
}

#[test]
fn missing_tools_and_models_block_recording_with_a_useful_message() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);

    fs::remove_file(sandbox.root.join("model.bin")).unwrap();
    let output = sandbox.wispx(&["press"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("huggingface.co"));
    assert!(!sandbox.has("recorder.pid"), "the mic must stay off");
    assert_eq!(sandbox.state(), "idle");

    fs::write(sandbox.root.join("model.bin"), b"m").unwrap();
    fs::remove_file(sandbox.root.join("bin/pw-record")).unwrap();
    let output = sandbox.wispx(&["press"]);
    assert_eq!(output.status.code(), Some(1));
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("pw-record is not installed"), "{message}");
    assert_eq!(sandbox.state(), "idle");
}

// Backends.

#[test]
fn switching_backends_persists_and_chatgpt_needs_a_login_before_the_mic_opens() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    assert_eq!(sandbox.status()["backend"], "local");

    let reply = sandbox.ok(&["backend", "chatgpt"]);
    assert!(reply.contains("chatgpt.com"), "must warn: {reply}");
    assert_eq!(sandbox.status()["backend"], "chatgpt");
    let config: Json =
        serde_json::from_str(&fs::read_to_string(sandbox.config_file()).unwrap()).unwrap();
    assert_eq!(config["backend"], "chatgpt");
    assert_eq!(config["min_hold_ms"], 50, "other settings survive");

    let output = sandbox.wispx(&["start"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Sign in to Codex"));
    assert!(!sandbox.has("recorder.pid"), "the mic must stay off");
    assert!(!sandbox.has("curl-ran"));

    sandbox.ok(&["backend", "local"]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    assert!(!sandbox.has("curl-ran"));
}

#[test]
fn a_bad_config_reload_keeps_the_working_settings() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    let good = fs::read_to_string(sandbox.config_file()).unwrap();
    fs::write(sandbox.config_file(), r#"{"backend":"nope"}"#).unwrap();
    let output = sandbox.wispx(&["reload"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("backend"));
    assert_eq!(sandbox.status()["backend"], "local");

    fs::write(sandbox.config_file(), good).unwrap();
    sandbox.ok(&["reload"]);
}

#[test]
fn the_backend_command_works_on_the_config_the_daemon_is_using() {
    let mut sandbox = Sandbox::new();
    fs::remove_file(sandbox.config_file()).unwrap();
    let config = sandbox.root.join("daemon.json");
    fs::write(
        &config,
        sandbox.config_text(r#""paste":"copy","backend":"chatgpt""#),
    )
    .unwrap();
    let path = config.to_str().unwrap();
    sandbox.sign_in_to_codex();
    sandbox.start_daemon_with(&["--config", path], &[]);

    assert_eq!(sandbox.ok(&["backend"]).trim(), "chatgpt");
    assert_eq!(sandbox.ok(&["--config", path, "backend"]).trim(), "chatgpt");
    assert_eq!(sandbox.status()["config"], path);
    sandbox.dictate();
    wait_until("the upload", || sandbox.has("curl-ran"));
    sandbox.wait_for_state("idle");
    assert!(!sandbox.has("whisper-args"));
    fs::remove_file(sandbox.root.join("curl-ran")).unwrap();
    fs::remove_file(sandbox.root.join("recorder.pid")).unwrap();

    let reply = sandbox.ok(&["backend", "local"]);
    assert!(reply.starts_with("Backend: local\n"), "{reply}");
    assert!(reply.contains(path), "{reply}");
    assert!(!reply.contains("chatgpt.com"), "{reply}");
    assert_eq!(sandbox.status()["backend"], "local");
    assert_eq!(sandbox.ok(&["backend"]).trim(), "local");
    assert_eq!(sandbox.saved_backend(&config), "local");
    let saved: Json = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["min_hold_ms"], 50, "other settings survive");
    assert!(
        !sandbox.config_file().exists(),
        "the default file is none of this command's business"
    );

    sandbox.dictate();
    sandbox.wait_for_clipboard(TRANSCRIPT);
    assert!(!sandbox.has("curl-ran"), "audio left after the switch");

    sandbox.ok(&["quit"]);
    assert_eq!(sandbox.wait_for_exit(), 0);
    sandbox.start_daemon_with(&["--config", path], &[]);
    assert_eq!(sandbox.status()["backend"], "local");
}

#[test]
fn a_backend_given_at_startup_is_reported_until_it_is_changed() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&["--backend", "chatgpt"]);
    assert_eq!(sandbox.ok(&["backend"]).trim(), "chatgpt");
    assert_eq!(sandbox.saved_backend(&sandbox.config_file()), Json::Null);
    sandbox.ok(&["reload"]);
    assert_eq!(sandbox.ok(&["backend"]).trim(), "chatgpt");

    sandbox.ok(&["backend", "local"]);
    assert_eq!(sandbox.ok(&["backend"]).trim(), "local");
    assert_eq!(sandbox.saved_backend(&sandbox.config_file()), "local");
    sandbox.ok(&["reload"]);
    assert_eq!(sandbox.ok(&["backend"]).trim(), "local");
}

#[test]
fn a_config_file_the_daemon_is_not_using_is_refused_and_left_alone() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    let default_text = fs::read_to_string(sandbox.config_file()).unwrap();
    let default_path = sandbox.config_file();
    let other = sandbox.root.join("other.json");
    fs::write(&other, r#"{"backend":"local"}"#).unwrap();
    let missing = sandbox.root.join("missing.json");

    for (file, args) in [
        (&other, vec!["backend", "chatgpt"]),
        (&other, vec!["backend"]),
        (&missing, vec!["backend", "chatgpt"]),
        (&missing, vec!["backend"]),
    ] {
        let mut words = vec!["--config", file.to_str().unwrap()];
        words.extend(args);
        let output = sandbox.wispx(&words);
        assert_eq!(output.status.code(), Some(1), "{words:?}");
        assert!(output.stdout.is_empty(), "{words:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(default_path.to_str().unwrap()), "{error}");
        assert!(error.contains(file.to_str().unwrap()), "{error}");
        assert!(error.contains("--config"), "{error}");
    }
    assert_eq!(
        fs::read_to_string(&other).unwrap(),
        r#"{"backend":"local"}"#
    );
    assert!(!missing.exists());
    assert_eq!(
        fs::read_to_string(&default_path).unwrap(),
        default_text,
        "nothing changed"
    );
    assert_eq!(sandbox.status()["backend"], "local");

    // Another name for the daemon's own file is that file.
    let link = sandbox.root.join("link.json");
    symlink(&default_path, &link).unwrap();
    let link_text = link.to_str().unwrap();
    sandbox.ok(&["--config", link_text, "backend", "chatgpt"]);
    assert_eq!(sandbox.status()["backend"], "chatgpt");
    assert_eq!(sandbox.saved_backend(&default_path), "chatgpt");
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
}

#[test]
fn a_backend_that_cannot_be_saved_is_not_applied_or_reported_as_changed() {
    let mut sandbox = Sandbox::new();
    sandbox.configure(r#""paste":"copy","backend":"chatgpt""#);
    sandbox.start_daemon(&[]);
    let broken = "{ this is not json";
    fs::write(sandbox.config_file(), broken).unwrap();

    let output = sandbox.wispx(&["backend", "local"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("still chatgpt"), "{error}");
    assert_eq!(sandbox.status()["backend"], "chatgpt");
    assert_eq!(fs::read_to_string(sandbox.config_file()).unwrap(), broken);
    sandbox.ok(&["quit"]);
    assert_eq!(sandbox.wait_for_exit(), 0);

    // No warning about uploads for a switch that did not happen.
    sandbox.configure(r#""paste":"copy""#);
    sandbox.start_daemon(&[]);
    fs::write(sandbox.config_file(), broken).unwrap();
    let output = sandbox.wispx(&["backend", "chatgpt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("still local"), "{error}");
    assert!(!error.contains("chatgpt.com"), "{error}");
    assert_eq!(sandbox.status()["backend"], "local");
}

#[test]
fn without_a_daemon_the_backend_command_edits_the_named_file() {
    let sandbox = Sandbox::new();
    let default_text = fs::read_to_string(sandbox.config_file()).unwrap();
    let other = sandbox.root.join("other.json");
    let path = other.to_str().unwrap();

    let reply = sandbox.ok(&["--config", path, "backend", "chatgpt"]);
    assert!(reply.starts_with("Backend: chatgpt\n"), "{reply}");
    assert!(reply.contains(path), "{reply}");
    assert!(reply.contains("chatgpt.com"), "must warn: {reply}");
    assert_eq!(sandbox.saved_backend(&other), "chatgpt");
    assert_eq!(sandbox.ok(&["--config", path, "backend"]).trim(), "chatgpt");
    assert_eq!(
        fs::read_to_string(sandbox.config_file()).unwrap(),
        default_text
    );

    let reply = sandbox.ok(&["backend", "chatgpt"]);
    assert!(reply.contains("chatgpt.com"), "must warn: {reply}");
    assert_eq!(sandbox.saved_backend(&sandbox.config_file()), "chatgpt");
    assert_eq!(sandbox.ok(&["backend"]).trim(), "chatgpt");
}

#[test]
fn switching_backends_says_what_happens_to_work_under_way() {
    let mut sandbox = Sandbox::new();
    sandbox.whisper_hangs();
    sandbox.start_daemon(&[]);
    sandbox.dictate();
    let whisper = sandbox.pid_of("whisper.pid");

    let reply = sandbox.ok(&["backend", "chatgpt"]);
    assert!(reply.contains("already under way"), "{reply}");
    assert!(reply.contains("wispx cancel"), "{reply}");
    assert_eq!(sandbox.status()["backend"], "chatgpt");
    assert!(process_alive(whisper), "the running job was left to finish");

    sandbox.ok(&["cancel"]);
    wait_until("the backend to die", || !process_alive(whisper));
}

#[test]
fn a_daemon_that_cannot_be_asked_changes_nothing() {
    let sandbox = Sandbox::new();
    if fs::metadata(&sandbox.root).unwrap().uid() == 0 {
        return; // Root connects to anything, and this needs a refusal.
    }
    let text = fs::read_to_string(sandbox.config_file()).unwrap();
    fs::create_dir(sandbox.runtime()).unwrap();
    fs::set_permissions(sandbox.runtime(), Permissions::from_mode(0o700)).unwrap();
    fs::write(sandbox.socket(), b"").unwrap();
    fs::set_permissions(sandbox.socket(), Permissions::from_mode(0o000)).unwrap();

    for args in [&["backend", "chatgpt"][..], &["backend"][..]] {
        let output = sandbox.wispx(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("cannot ask the daemon"), "{error}");
    }
    assert_eq!(fs::read_to_string(sandbox.config_file()).unwrap(), text);
}

// Lifecycle.

#[test]
fn only_one_daemon_runs_and_it_can_be_replaced_after_a_crash() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    let second = sandbox.wispx(&["daemon", "--quiet"]);
    assert_eq!(second.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
    assert_eq!(sandbox.state(), "idle", "the first daemon is unaffected");

    // Hard kill: lock released, but socket and audio from the crash remain.
    let mut child = sandbox.daemon.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(sandbox.socket().exists());
    let leftover = sandbox.runtime().join("job-99.wav");
    fs::write(&leftover, b"private audio").unwrap();
    assert_eq!(sandbox.wispx(&["status"]).status.code(), Some(3));

    sandbox.start_daemon(&[]);
    assert_eq!(sandbox.state(), "idle");
    assert!(!leftover.exists(), "audio from the crash was swept");
}

#[test]
fn quit_and_sigterm_clean_up_even_mid_transcription() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    sandbox.ok(&["quit"]);
    assert_eq!(sandbox.wait_for_exit(), 0);
    assert!(!sandbox.socket().exists());

    sandbox.whisper_hangs();
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    let whisper = sandbox.pid_of("whisper.pid");
    assert_eq!(sandbox.job_files().len(), 1);
    sandbox.signal_daemon("TERM");
    assert_eq!(sandbox.wait_for_exit(), 0);
    assert!(!sandbox.socket().exists());
    assert!(sandbox.job_files().is_empty());
    wait_until("the backend to die", || !process_alive(whisper));
}

#[test]
fn stopping_while_recording_turns_the_microphone_off() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    let recorder = sandbox.pid_of("recorder.pid");
    sandbox.signal_daemon("TERM");
    assert_eq!(sandbox.wait_for_exit(), 0);
    wait_until("the recorder to exit", || !process_alive(recorder));
}

#[test]
fn quitting_waits_for_a_recorder_that_outlived_its_wrapper() {
    let mut sandbox = Sandbox::new();
    let r = sandbox.root.display();
    // Exiting ends the recording, and leaves behind a child ignoring SIGTERM.
    sandbox.tool(
        "pw-record",
        &format!(
            "trap '' TERM\nsleep 60 >/dev/null 2>&1 &\necho $! > {r}/capture.pid\n\
             cat {r}/loud.raw\nsleep 0.2\nexit 0"
        ),
    );
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    let capture = sandbox.pid_of("capture.pid");
    sandbox.wait_for_clipboard(TRANSCRIPT);

    sandbox.ok(&["quit"]);
    assert_eq!(sandbox.wait_for_exit(), 0);
    wait_until("the capture to be killed", || !process_alive(capture));
}

#[test]
fn clients_report_a_missing_daemon_distinctly() {
    let sandbox = Sandbox::new();
    for command in ["press", "status", "cancel", "last", "quit"] {
        let output = sandbox.wispx(&[command]);
        assert_eq!(output.status.code(), Some(3), "{command}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("wispx daemon"));
    }
    assert!(
        !sandbox.runtime().exists(),
        "a client created the directory"
    );
}

#[test]
fn runtime_directory_and_socket_are_private() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon(&[]);
    let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o777;
    assert_eq!(mode(&sandbox.runtime()), 0o700);
    assert_eq!(mode(&sandbox.socket()), 0o600);
}

// One-off commands.

fn wav_with(rate: u32, channels: u16) -> Vec<u8> {
    let data = vec![0u8; 3200];
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

#[test]
fn transcribe_prints_only_the_text_and_rejects_other_formats() {
    let sandbox = Sandbox::new();
    let good = sandbox.root.join("good file.wav");
    fs::write(&good, wav_with(16_000, 1)).unwrap();
    let output = sandbox.wispx(&["transcribe", good.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{TRANSCRIPT}\n")
    );
    assert!(sandbox.has("saved.mode"), "whisper ran");

    let bad = sandbox.root.join("cd.wav");
    fs::write(&bad, wav_with(44_100, 2)).unwrap();
    let output = sandbox.wispx(&["transcribe", bad.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ffmpeg"));

    let output = sandbox.wispx(&["transcribe", "/nonexistent.wav"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(sandbox.wispx(&["transcribe"]).status.code(), Some(2));
    assert!(!sandbox.has("curl-ran"));
}

#[test]
fn transcribe_stops_its_backend_when_interrupted() {
    let sandbox = Sandbox::new();
    sandbox.whisper_hangs();
    let wav = sandbox.root.join("in.wav");
    fs::write(&wav, wav_with(16_000, 1)).unwrap();
    let mut child = sandbox
        .command()
        .args(["transcribe", wav.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let whisper = sandbox.pid_of("whisper.pid");
    Command::new("/bin/sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    let deadline = Instant::now() + SECOND * 10;
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        assert!(Instant::now() < deadline, "transcribe did not stop");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(code, Some(130));
    wait_until("the backend to die", || !process_alive(whisper));
}

#[test]
fn config_commands_use_the_scratch_paths_and_never_overwrite() {
    let sandbox = Sandbox::new();
    assert_eq!(
        sandbox.ok(&["config", "path"]).trim(),
        sandbox.config_file().to_str().unwrap()
    );
    let shown: Json = serde_json::from_str(&sandbox.ok(&["config", "show"])).unwrap();
    assert_eq!(shown["min_hold_ms"], 50);

    assert_eq!(sandbox.wispx(&["config", "init"]).status.code(), Some(1));
    fs::remove_file(sandbox.config_file()).unwrap();
    sandbox.ok(&["config", "init"]);
    let written: Json =
        serde_json::from_str(&fs::read_to_string(sandbox.config_file()).unwrap()).unwrap();
    assert_eq!(written["backend"], "local");
    assert_eq!(written["shortcuts"]["hold"], "CTRL+LOGO+d");
}

#[test]
fn an_alternate_config_file_is_honored() {
    let sandbox = Sandbox::new();
    let other = sandbox.root.join("other.json");
    fs::write(&other, r#"{"backend":"chatgpt"}"#).unwrap();
    let path = other.to_str().unwrap();
    assert_eq!(sandbox.ok(&["--config", path, "backend"]).trim(), "chatgpt");
    assert_eq!(sandbox.ok(&["backend"]).trim(), "local");
}

#[test]
fn setup_writes_only_the_desktop_entries_it_was_asked_for() {
    let sandbox = Sandbox::new();
    let entry = sandbox
        .root
        .join("data/applications/io.github.PeytonWDYM.WispX.desktop");
    let autostart = sandbox
        .root
        .join("config/autostart/io.github.PeytonWDYM.WispX.desktop");

    sandbox.ok(&["setup"]);
    let text = fs::read_to_string(&entry).unwrap();
    assert!(text.contains(&format!(
        "Exec=\"{}\" daemon --quiet",
        env!("CARGO_BIN_EXE_wispx")
    )));
    assert!(!autostart.exists());

    sandbox.ok(&["setup", "--autostart"]);
    assert_eq!(fs::read_to_string(&autostart).unwrap(), text);
}

#[test]
fn doctor_reads_everything_and_touches_nothing() {
    let sandbox = Sandbox::new();
    let output = sandbox.wispx(&["doctor"]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{text}");
    assert!(text.contains("[ok]   recorder"), "{text}");
    assert!(text.contains("[ok]   backend: local whisper.cpp"), "{text}");
    assert!(
        text.contains("[ok]   clipboard: wl-copy/wl-paste"),
        "{text}"
    );
    assert!(text.contains("[warn] daemon: not running"), "{text}");
    assert!(!sandbox.runtime().exists(), "doctor created the directory");
    assert!(!sandbox.has("recorder.pid"), "doctor opened the mic");
    assert!(!sandbox.has("whisper-args"));
    assert!(!sandbox.has("copy-args"));
    assert!(!sandbox.has("curl-ran"));

    fs::remove_file(sandbox.root.join("model.bin")).unwrap();
    let output = sandbox.wispx(&["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("[FAIL] backend"), "{text}");

    // Asking about ChatGPT warns about privacy and looks only at the fixture.
    let output = sandbox.wispx(&["doctor", "--backend", "chatgpt"]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("[warn] privacy"), "{text}");
    assert!(text.contains("[FAIL] backend: Sign in to Codex"), "{text}");
    assert!(!sandbox.has("curl-ran"));
}

// KDE Plasma integration, against a mock desktop on a private real bus.

struct PrivateBus {
    child: Child,
    address: String,
}

impl PrivateBus {
    fn start(root: &Path) -> Option<PrivateBus> {
        let (child, address) = dbus::spawn_test_bus(root)?;
        Some(PrivateBus { child, address })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Default)]
struct DesktopState {
    shortcut_session: Option<String>,
    bound: Vec<(String, String)>,
    remote_sessions: u32,
    allow_remote: bool,
    restore_tokens: Vec<Option<String>>,
    device_types: Vec<u32>,
    remote_session: Option<String>,
    keys: Vec<(i32, u32)>,
    /// What successive focus checks report; the last one repeats.
    focus: Vec<(String, String)>,
    focus_checks: usize,
    /// Leaves focus checks unanswered, the way a busy KWin would.
    hold_focus: bool,
    /// How many held checks may be answered despite `hold_focus`.
    let_through: usize,
    held_focus: usize,
    /// Focus checks that have run to their end.
    finished_focus_checks: usize,
    notifications: Vec<(String, String)>,
}

enum MockCommand {
    Shortcut(&'static str, &'static str),
    CloseShortcutSession,
    CloseRemoteSession,
}

struct MockDesktop {
    _bus: PrivateBus,
    state: Arc<Mutex<DesktopState>>,
    commands: Sender<MockCommand>,
}

impl MockDesktop {
    fn start(sandbox: &mut Sandbox) -> Option<MockDesktop> {
        let bus = PrivateBus::start(&sandbox.root)?;
        sandbox.bus_address = Some(bus.address.clone());
        sandbox.desktop = "KDE";

        let mut connection = Connection::connect(&bus.address).unwrap();
        for name in [PORTAL, "org.kde.KWin", "org.freedesktop.Notifications"] {
            connection.request_name(name, SECOND * 5).unwrap();
        }
        let state = Arc::new(Mutex::new(DesktopState {
            allow_remote: true,
            focus: vec![("{window-a}".to_string(), "org.kde.kate".to_string())],
            ..DesktopState::default()
        }));
        let (commands, inbox) = mpsc::channel();
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            let mut mock = Mock {
                bus: connection,
                state: shared,
                serial: 0,
                scripts: HashMap::new(),
                next_script: 0,
                held: Vec::new(),
            };
            // The bus going away when a test ends is how this stops.
            while mock.step(&inbox).is_ok() {}
        });
        Some(MockDesktop {
            _bus: bus,
            state,
            commands,
        })
    }

    fn focus_on(&self, id: &str, class: &str) {
        self.state().focus = vec![(id.to_string(), class.to_string())];
    }

    /// Dictation is pasted only if focus is already known when it ends. A check
    /// takes a few round trips, so wait for it rather than hope it was quick.
    fn wait_for_focus_check(&self, count: usize) {
        wait_until("the focus check", || {
            self.state().finished_focus_checks >= count
        });
        thread::sleep(Duration::from_millis(150));
    }

    fn wait_for_held_focus_checks(&self, count: usize) {
        wait_until("a focus check to be held", || {
            self.state().held_focus == count
        });
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DesktopState> {
        self.state.lock().unwrap()
    }

    fn press(&self, id: &'static str) {
        self.commands
            .send(MockCommand::Shortcut("Activated", id))
            .unwrap();
    }

    fn release(&self, id: &'static str) {
        self.commands
            .send(MockCommand::Shortcut("Deactivated", id))
            .unwrap();
    }

    fn wait_for_shortcuts(&self, sandbox: &Sandbox) {
        wait_until("shortcuts to bind", || {
            sandbox.status()["hotkey"]
                .as_str()
                .is_some_and(|text| text.contains("Ctrl+Meta+D"))
        });
    }
}

struct Mock {
    bus: Connection,
    state: Arc<Mutex<DesktopState>>,
    serial: u64,
    /// Loaded KWin scripts: their object path, and who to report back to.
    scripts: HashMap<String, String>,
    next_script: i32,
    /// Focus checks waiting to be answered, oldest first.
    held: Vec<Message>,
}

impl Mock {
    fn step(&mut self, inbox: &mpsc::Receiver<MockCommand>) -> Result<(), dbus::Error> {
        while let Ok(command) = inbox.try_recv() {
            match command {
                MockCommand::Shortcut(member, id) => {
                    let session = self.state.lock().unwrap().shortcut_session.clone();
                    if let Some(session) = session {
                        self.bus.emit_signal(
                            PORTAL_PATH,
                            SHORTCUTS,
                            member,
                            vec![
                                Value::path(session),
                                Value::str(id),
                                Value::Uint64(1),
                                Value::vardict(Vec::new()),
                            ],
                        )?;
                    }
                }
                MockCommand::CloseShortcutSession => {
                    let session = self.state.lock().unwrap().shortcut_session.take();
                    if let Some(session) = session {
                        self.bus
                            .emit_signal(&session, SESSION, "Closed", Vec::new())?;
                    }
                }
                MockCommand::CloseRemoteSession => {
                    let session = self.state.lock().unwrap().remote_session.take();
                    if let Some(session) = session {
                        self.bus
                            .emit_signal(&session, SESSION, "Closed", Vec::new())?;
                    }
                }
            }
        }
        for call in self.released_focus_checks() {
            self.answer(&call)?;
        }
        let Some(message) = self.bus.next_message(Duration::from_millis(10))? else {
            return Ok(());
        };
        if message.kind == Kind::MethodCall {
            let mut state = self.state.lock().unwrap();
            if state.hold_focus && message.member.as_deref() == Some("loadScript") {
                state.held_focus += 1;
                drop(state);
                self.held.push(message);
            } else {
                drop(state);
                self.answer(&message)?;
            }
        }
        Ok(())
    }

    fn released_focus_checks(&mut self) -> Vec<Message> {
        let mut state = self.state.lock().unwrap();
        let count = if state.hold_focus {
            state.let_through.min(self.held.len())
        } else {
            self.held.len()
        };
        state.let_through = state.let_through.saturating_sub(count);
        self.held.drain(..count).collect()
    }

    /// Answers a portal request: reply with the handle, then signal the result.
    fn respond(&mut self, call: &Message, code: u32, results: Value) -> Result<(), dbus::Error> {
        let options = call.body.last().unwrap();
        let token = options
            .lookup("handle_token")
            .and_then(Value::as_str)
            .expect("portal requests must carry handle_token")
            .to_string();
        let sender = call.sender.as_deref().unwrap();
        let path = format!(
            "{PORTAL_PATH}/request/{}/{token}",
            sender.trim_start_matches(':').replace('.', "_")
        );
        self.bus.reply(call, vec![Value::path(path.clone())])?;
        self.bus.emit_signal(
            &path,
            REQUEST,
            "Response",
            vec![Value::Uint32(code), results],
        )
    }

    fn session_results(&mut self, call: &Message) -> (String, Value) {
        let token = call.body[0]
            .lookup("session_handle_token")
            .and_then(Value::as_str)
            .expect("CreateSession must carry session_handle_token");
        let sender = call.sender.as_deref().unwrap();
        let handle = format!(
            "{PORTAL_PATH}/session/{}/{token}",
            sender.trim_start_matches(':').replace('.', "_")
        );
        let results = Value::vardict(vec![(
            "session_handle".to_string(),
            Value::str(handle.clone()),
        )]);
        (handle, results)
    }

    fn answer(&mut self, call: &Message) -> Result<(), dbus::Error> {
        let interface = call.interface.as_deref().unwrap_or_default();
        let member = call.member.as_deref().unwrap_or_default();
        match (interface, member) {
            ("org.freedesktop.DBus.Properties", "Get") => {
                let known = [SHORTCUTS, REMOTE].contains(&call.body[0].as_str().unwrap());
                if known {
                    let version = Value::Variant(Box::new(Value::Uint32(2)));
                    self.bus.reply(call, vec![version])
                } else {
                    self.bus
                        .reply_error(call, "org.freedesktop.DBus.Error.InvalidArgs", "no")
                }
            }
            ("org.freedesktop.host.portal.Registry", "Register") => {
                assert_eq!(call.body[0].as_str(), Some("io.github.PeytonWDYM.WispX"));
                self.bus.reply(call, Vec::new())
            }
            (SHORTCUTS, "CreateSession") => {
                let (handle, results) = self.session_results(call);
                self.state.lock().unwrap().shortcut_session = Some(handle);
                self.respond(call, 0, results)
            }
            (SHORTCUTS, "BindShortcuts") => {
                let Value::Array { items, .. } = &call.body[1] else {
                    panic!("BindShortcuts without a list");
                };
                let mut bound = Vec::new();
                let mut described = Vec::new();
                for item in items {
                    let Value::Struct(fields) = item else {
                        panic!("malformed shortcut")
                    };
                    let id = fields[0].as_str().unwrap().to_string();
                    let trigger = fields[1]
                        .lookup("preferred_trigger")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_string();
                    let shown = trigger.replace("CTRL+LOGO+d", "Ctrl+Meta+D");
                    described.push(Value::Struct(vec![
                        Value::str(id.clone()),
                        Value::vardict(vec![(
                            "trigger_description".to_string(),
                            Value::str(shown),
                        )]),
                    ]));
                    bound.push((id, trigger));
                }
                self.state.lock().unwrap().bound = bound;
                let results = Value::vardict(vec![(
                    "shortcuts".to_string(),
                    Value::Array {
                        element: "(sa{sv})".to_string(),
                        items: described,
                    },
                )]);
                self.respond(call, 0, results)
            }
            (REMOTE, "CreateSession") => {
                let (handle, results) = self.session_results(call);
                {
                    let mut state = self.state.lock().unwrap();
                    state.remote_sessions += 1;
                    state.remote_session = Some(handle);
                }
                self.respond(call, 0, results)
            }
            (REMOTE, "SelectDevices") => {
                let options = &call.body[1];
                {
                    let mut state = self.state.lock().unwrap();
                    state
                        .device_types
                        .push(options.lookup("types").and_then(Value::as_u32).unwrap());
                    state.restore_tokens.push(
                        options
                            .lookup("restore_token")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    );
                }
                self.respond(call, 0, Value::vardict(Vec::new()))
            }
            (REMOTE, "Start") => {
                if self.state.lock().unwrap().allow_remote {
                    self.serial += 1;
                    let results = Value::vardict(vec![
                        ("devices".to_string(), Value::Uint32(1)),
                        (
                            "restore_token".to_string(),
                            Value::str(format!("token-{}", self.serial)),
                        ),
                    ]);
                    self.respond(call, 0, results)
                } else {
                    self.respond(call, 1, Value::vardict(Vec::new()))
                }
            }
            (REMOTE, "NotifyKeyboardKeysym") => {
                let keysym = call.body[2].as_i32().unwrap();
                let state = call.body[3].as_u32().unwrap();
                self.state.lock().unwrap().keys.push((keysym, state));
                self.bus.reply(call, Vec::new())
            }
            ("org.kde.kwin.Scripting", "loadScript") => {
                // A held check can outlast its caller, which removes the script.
                let Ok(source) = fs::read_to_string(call.body[0].as_str().unwrap()) else {
                    return self.bus.reply_error(
                        call,
                        "org.freedesktop.DBus.Error.Failed",
                        "no script",
                    );
                };
                let target = source
                    .split("callDBus(\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap()
                    .to_string();
                self.state.lock().unwrap().focus_checks += 1;
                let id = self.next_script;
                self.next_script += 1;
                self.scripts
                    .insert(format!("/Scripting/Script{id}"), target);
                self.bus.reply(call, vec![Value::Int32(id)])
            }
            ("org.kde.kwin.Script", "run") => {
                self.bus.reply(call, Vec::new())?;
                let target = call
                    .path
                    .as_ref()
                    .and_then(|path| self.scripts.remove(path))
                    .expect("run must follow loadScript");
                let (id, class) = {
                    let mut state = self.state.lock().unwrap();
                    if state.focus.len() > 1 {
                        state.focus.remove(0)
                    } else {
                        state.focus[0].clone()
                    }
                };
                self.bus
                    .call(
                        &target,
                        "/",
                        "",
                        "result",
                        vec![Value::str(id), Value::str(class)],
                        SECOND * 2,
                    )
                    .map(|_| ())
            }
            ("org.kde.kwin.Scripting", "unloadScript") => {
                self.state.lock().unwrap().finished_focus_checks += 1;
                self.bus.reply(call, vec![Value::Bool(true)])
            }
            ("org.freedesktop.Notifications", "Notify") => {
                let text = |index: usize| call.body[index].as_str().unwrap().to_string();
                self.state
                    .lock()
                    .unwrap()
                    .notifications
                    .push((text(3), text(4)));
                self.bus.reply(call, vec![Value::Uint32(1)])
            }
            ("org.freedesktop.Notifications", "CloseNotification") => {
                self.bus.reply(call, Vec::new())
            }
            _ => self
                .bus
                .reply_error(call, "org.freedesktop.DBus.Error.UnknownMethod", member),
        }
    }
}

fn plasma_sandbox() -> Option<(Sandbox, MockDesktop)> {
    let mut sandbox = Sandbox::new();
    let desktop = MockDesktop::start(&mut sandbox)?;
    sandbox.configure(r#""hotkey":"auto","paste":"auto""#);
    Some((sandbox, desktop))
}

fn grant_paste(sandbox: &Sandbox) {
    fs::create_dir_all(sandbox.token_file().parent().unwrap()).unwrap();
    fs::write(sandbox.token_file(), "granted-earlier").unwrap();
}

fn wait_for_paste_session(desktop: &MockDesktop, sandbox: &Sandbox) {
    wait_until("the paste session", || {
        desktop.state().remote_session.is_some()
            && sandbox.status()["paste"]
                .as_str()
                .is_some_and(|text| text.starts_with("automatic"))
    });
}

#[test]
fn portal_shortcuts_are_requested_and_drive_hold_to_talk() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);
    assert_eq!(
        desktop.state().bound,
        [
            ("dictate".to_string(), "CTRL+LOGO+d".to_string()),
            ("hands-free".to_string(), "CTRL+LOGO+space".to_string()),
            ("cancel".to_string(), "CTRL+LOGO+BackSpace".to_string()),
        ]
    );
    assert_eq!(sandbox.status()["session"], "KDE Plasma on Wayland");

    desktop.press("dictate");
    sandbox.wait_for_state("recording");
    sandbox.wait_for_recording();
    desktop.release("dictate");
    sandbox.wait_for_clipboard(TRANSCRIPT);

    // Without `paste-setup` it never asks for keyboard access or sends keys.
    assert_eq!(desktop.state().remote_sessions, 0);
    assert!(desktop.state().keys.is_empty());
    assert_eq!(desktop.state().focus_checks, 0);
}

#[test]
fn plasmas_release_before_the_hands_free_press_keeps_the_recording() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);

    desktop.press("dictate");
    sandbox.wait_for_state("recording");
    sandbox.wait_for_recording();
    // Exactly what kglobalacceld does when the second shortcut fires.
    desktop.release("dictate");
    desktop.press("hands-free");
    sandbox.wait_for_state("recording-hands-free");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(sandbox.state(), "recording-hands-free");
    assert!(!sandbox.has("whisper-args"));

    // The hands-free shortcut's own release must not stop anything.
    desktop.release("hands-free");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(sandbox.state(), "recording-hands-free");

    desktop.press("hands-free");
    sandbox.wait_for_clipboard(TRANSCRIPT);
}

#[test]
fn plasmas_release_before_the_cancel_press_cancels_instead_of_transcribing() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);
    desktop.press("dictate");
    sandbox.wait_for_state("recording");
    sandbox.wait_for_recording();
    desktop.release("dictate");
    desktop.press("cancel");
    sandbox.wait_for_state("idle");
    thread::sleep(Duration::from_millis(200));
    assert!(!sandbox.has("whisper-args"));
    assert_eq!(sandbox.clipboard(), None);
}

#[test]
fn losing_the_portal_during_a_hold_cancels_and_cli_commands_keep_working() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);
    desktop.press("dictate");
    sandbox.wait_for_state("recording");
    desktop
        .commands
        .send(MockCommand::CloseShortcutSession)
        .unwrap();
    sandbox.wait_for_state("idle");
    assert!(!sandbox.has("whisper-args"));

    sandbox.ok(&["toggle"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["toggle"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
}

#[test]
fn a_cli_hold_is_not_cancelled_by_an_unavailable_portal() {
    // Nothing owns the portal name here, so the session keeps failing.
    let mut sandbox = Sandbox::new();
    let Some(bus) = PrivateBus::start(&sandbox.root) else {
        return;
    };
    sandbox.bus_address = Some(bus.address.clone());
    sandbox.desktop = "KDE";
    sandbox.configure(r#""hotkey":"auto","paste":"copy""#);
    sandbox.start_daemon(&[]);
    wait_until("the portal to be found missing", || {
        sandbox.status()["hotkey"]
            .as_str()
            .is_some_and(|text| text.contains("no portal shortcuts"))
    });
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    assert_eq!(sandbox.state(), "recording");
    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
}

#[test]
fn verified_focus_pastes_with_ctrl_v_and_restores_the_clipboard() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    grant_paste(&sandbox);
    fs::write(sandbox.root.join("clipboard"), "what I copied before").unwrap();
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);
    wait_for_paste_session(&desktop, &sandbox);

    // The earlier grant is restored, keyboard only, and its token rotated.
    {
        let state = desktop.state();
        assert_eq!(state.device_types, [1], "keyboard only");
        assert_eq!(state.restore_tokens, [Some("granted-earlier".to_string())]);
    }
    assert_eq!(fs::read_to_string(sandbox.token_file()).unwrap(), "token-1");
    assert_eq!(
        fs::metadata(sandbox.token_file()).unwrap().mode() & 0o777,
        0o600
    );

    desktop.press("dictate");
    sandbox.wait_for_state("recording");
    sandbox.wait_for_recording();
    desktop.wait_for_focus_check(1);
    desktop.release("dictate");

    wait_until("the keys", || desktop.state().keys.len() == 4);
    assert_eq!(
        desktop.state().keys,
        [(CONTROL_L, 1), (KEY_V, 1), (KEY_V, 0), (CONTROL_L, 0)]
    );
    // Focus was checked three times: when dictation began, and twice before keys.
    assert_eq!(desktop.state().focus_checks, 3);
    let copy_args = fs::read_to_string(sandbox.root.join("copy-args")).unwrap();
    assert!(copy_args.len() > 1);

    // After the target has had time to read it, the old contents come back.
    sandbox.wait_for_clipboard("what I copied before");
    assert_eq!(sandbox.ok(&["last"]).trim(), TRANSCRIPT);
}

#[test]
fn terminals_get_ctrl_shift_v() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    grant_paste(&sandbox);
    desktop.state().focus = vec![("{konsole}".to_string(), "org.kde.konsole".to_string())];
    sandbox.start_daemon(&[]);
    wait_for_paste_session(&desktop, &sandbox);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    desktop.wait_for_focus_check(1);
    sandbox.ok(&["release"]);
    wait_until("the keys", || desktop.state().keys.len() == 6);
    assert_eq!(
        desktop.state().keys,
        [
            (CONTROL_L, 1),
            (SHIFT_L, 1),
            (KEY_V, 1),
            (KEY_V, 0),
            (SHIFT_L, 0),
            (CONTROL_L, 0),
        ]
    );
}

#[test]
fn switching_windows_during_transcription_never_sends_keys() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    grant_paste(&sandbox);
    fs::write(sandbox.root.join("clipboard"), "old").unwrap();
    desktop.state().focus = vec![
        ("{window-a}".to_string(), "org.kde.kate".to_string()),
        ("{window-b}".to_string(), "firefox".to_string()),
    ];
    sandbox.start_daemon(&[]);
    wait_for_paste_session(&desktop, &sandbox);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    desktop.wait_for_focus_check(1);
    sandbox.ok(&["release"]);

    sandbox.wait_for_clipboard(TRANSCRIPT);
    sandbox.wait_for_state("idle");
    thread::sleep(Duration::from_millis(300));
    assert!(
        desktop.state().keys.is_empty(),
        "keys reached the wrong window"
    );
    assert_eq!(sandbox.clipboard().as_deref(), Some(TRANSCRIPT));
}

#[test]
fn a_focus_answer_that_comes_after_the_release_is_never_the_target() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    grant_paste(&sandbox);
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);
    wait_for_paste_session(&desktop, &sandbox);
    desktop.state().hold_focus = true;

    desktop.press("dictate");
    sandbox.wait_for_state("recording");
    sandbox.wait_for_recording();
    desktop.wait_for_held_focus_checks(1);
    desktop.release("dictate");
    wait_until("the recording to end", || sandbox.state() != "recording");

    // KWin finally answers, by which time the user has moved on. Every check
    // from here on would agree that this window is the one, so only the timing
    // of the first answer stands between the transcript and the wrong window.
    desktop.focus_on("{window-b}", "firefox");
    desktop.state().hold_focus = false;
    sandbox.wait_for_clipboard(TRANSCRIPT);
    sandbox.wait_for_state("idle");
    desktop.wait_for_focus_check(1);
    thread::sleep(Duration::from_millis(300));
    assert!(
        desktop.state().keys.is_empty(),
        "keys reached the wrong window"
    );
    assert_eq!(sandbox.clipboard().as_deref(), Some(TRANSCRIPT));
    assert_eq!(desktop.state().focus_checks, 1, "nothing to verify later");
}

#[test]
fn a_focus_answer_from_a_cancelled_recording_is_not_used() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    grant_paste(&sandbox);
    sandbox.start_daemon(&[]);
    wait_for_paste_session(&desktop, &sandbox);
    desktop.state().hold_focus = true;

    sandbox.ok(&["press"]);
    desktop.wait_for_held_focus_checks(1);
    sandbox.ok(&["cancel"]);
    sandbox.wait_for_state("idle");
    fs::remove_file(sandbox.root.join("recorder.pid")).unwrap();

    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    desktop.wait_for_held_focus_checks(2);

    // Only the cancelled recording's check is answered before this one ends.
    desktop.focus_on("{window-b}", "firefox");
    desktop.state().let_through = 1;
    desktop.wait_for_focus_check(1);
    sandbox.ok(&["release"]);
    desktop.state().hold_focus = false;

    sandbox.wait_for_clipboard(TRANSCRIPT);
    sandbox.wait_for_state("idle");
    desktop.wait_for_focus_check(2);
    thread::sleep(Duration::from_millis(300));
    assert!(
        desktop.state().keys.is_empty(),
        "keys reached the wrong window"
    );
    assert_eq!(sandbox.clipboard().as_deref(), Some(TRANSCRIPT));
}

#[test]
fn paste_setup_is_what_grants_access_and_a_refusal_stays_copy_only() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.start_daemon(&[]);
    desktop.wait_for_shortcuts(&sandbox);
    assert!(
        sandbox.status()["paste"]
            .as_str()
            .unwrap()
            .contains("paste-setup")
    );

    desktop.state().allow_remote = false;
    let output = sandbox.wispx(&["paste-setup"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not allow"));
    assert!(!sandbox.token_file().exists());

    desktop.state().allow_remote = true;
    let reply = sandbox.ok(&["paste-setup"]);
    assert!(reply.contains("Automatic paste is on"), "{reply}");
    assert!(sandbox.token_file().exists());
    assert_eq!(desktop.state().restore_tokens.last().unwrap(), &None);
    wait_for_paste_session(&desktop, &sandbox);

    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    desktop.wait_for_focus_check(1);
    sandbox.ok(&["release"]);
    wait_until("the keys", || desktop.state().keys.len() == 4);
}

#[test]
fn a_closed_paste_session_falls_back_to_copying_and_reconnects_in_the_background() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    grant_paste(&sandbox);
    sandbox.start_daemon(&[]);
    wait_for_paste_session(&desktop, &sandbox);
    desktop
        .commands
        .send(MockCommand::CloseRemoteSession)
        .unwrap();
    wait_until("the session to close", || {
        desktop.state().remote_session.is_none()
    });

    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    // The clipboard is set before the keys are tried, so wait for that attempt.
    wait_until("the lost session to be noticed", || {
        sandbox.daemon_log().contains("paste session lost")
    });
    assert!(desktop.state().keys.is_empty());
    assert_eq!(sandbox.clipboard().as_deref(), Some(TRANSCRIPT));

    wait_until("the reconnect", || desktop.state().remote_sessions == 2);
    wait_for_paste_session(&desktop, &sandbox);
}

#[test]
fn notifications_show_progress_but_never_the_transcript() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.configure(r#""hotkey":"none","paste":"copy","notifications":"all""#);
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    wait_until("the recording popup", || {
        desktop
            .state()
            .notifications
            .iter()
            .any(|(summary, _)| summary == "Recording")
    });
    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    wait_until("the final popup", || {
        desktop
            .state()
            .notifications
            .iter()
            .any(|(summary, _)| summary.starts_with("Copied"))
    });
    for (summary, body) in &desktop.state().notifications {
        assert!(!summary.contains("fake whisper"), "{summary}");
        assert!(!body.contains("fake whisper"), "{body}");
    }
}

#[test]
fn outside_plasma_nothing_touches_kwin_or_the_keyboard() {
    let Some((mut sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.desktop = "GNOME";
    grant_paste(&sandbox);
    sandbox.start_daemon(&[]);
    sandbox.ok(&["press"]);
    sandbox.wait_for_recording();
    sandbox.ok(&["release"]);
    sandbox.wait_for_clipboard(TRANSCRIPT);
    let state = desktop.state();
    assert_eq!(state.remote_sessions, 0);
    assert_eq!(state.focus_checks, 0);
    assert!(state.keys.is_empty());
    drop(state);
    let output = sandbox.wispx(&["paste-setup"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("KDE Plasma"));
}

#[test]
fn doctor_reports_plasma_support_using_read_only_queries() {
    let Some((sandbox, desktop)) = plasma_sandbox() else {
        return;
    };
    sandbox.configure(r#""paste":"auto","notifications":"all""#);
    let output = sandbox.wispx(&["doctor"]);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        text.contains("[ok]   session: KDE Plasma on Wayland"),
        "{text}"
    );
    assert!(
        text.contains("[ok]   shortcut portal: GlobalShortcuts version 2"),
        "{text}"
    );
    assert!(
        text.contains("[warn] automatic paste: not allowed yet"),
        "{text}"
    );
    assert!(text.contains("[ok]   notifications"), "{text}");
    let state = desktop.state();
    assert!(state.bound.is_empty(), "doctor bound shortcuts");
    assert_eq!(state.remote_sessions, 0, "doctor opened a session");
    assert_eq!(state.focus_checks, 0, "doctor loaded a KWin script");
}
