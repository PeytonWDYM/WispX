//! WispX against the real KDE Plasma stack, in a private headless session.
//!
//! REAL: `kwin_wayland --virtual`, its kglobalaccel, xdg-desktop-portal and its
//! KDE backend, the permission store, wl-copy, wl-paste and foot as the target
//! window. The shortcuts are really pressed, through a RemoteDesktop portal
//! session of this test's own, so they travel through KWin as key events.
//! MOCKED: only the microphone and the transcription, which are scripts.
//!
//! It is ignored by default because it starts a compositor:
//!
//!     cargo test --test plasma_nested -- --ignored --nocapture --test-threads=1
//!
//! Every process gets a cleared environment with a scratch HOME, XDG directories
//! and session bus, and connects only to the headless compositor. So it needs no
//! display, and cannot reach the desktop session it may be started from. Nothing
//! here uses the ambient session bus or the ambient Wayland display.

#[allow(dead_code)]
#[path = "../src/dbus.rs"]
mod dbus;
#[allow(dead_code)]
#[path = "../src/portal.rs"]
mod portal;

use dbus::{Connection, Value};
use portal::Response;
use serde_json::Value as Json;
use std::fs::{self, File, Permissions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SECOND: Duration = Duration::from_secs(1);
/// `timeout` wrapped around the bus and compositor stops them even if this test
/// is killed outright.
const SESSION_LIMIT: &str = "240";
const REMOTE: &str = "org.freedesktop.portal.RemoteDesktop";
const WISPX_ID: &str = "io.github.PeytonWDYM.WispX";
const DRIVER_ID: &str = "io.github.PeytonWDYM.WispX.TestDriver";
const TRANSCRIPT: &str = "hello from the nested desktop";

const CONTROL: i32 = 0xffe3;
const META: i32 = 0xffeb;
const KEY_D: i32 = 0x64;
const SPACE: i32 = 0x20;
const BACKSPACE: i32 = 0xff08;
const RETURN: i32 = 0xff0d;

#[track_caller]
fn wait_for(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !condition() {
        if Instant::now() >= deadline {
            eprintln!("gave up waiting for {what}");
            return false;
        }
        thread::sleep(Duration::from_millis(30));
    }
    true
}

#[track_caller]
fn require(what: &str, limit: Duration, condition: impl FnMut() -> bool) {
    assert!(wait_for(what, limit, condition), "timed out: {what}");
}

fn on_path(tool: &str) -> bool {
    ["/usr/bin", "/usr/lib", "/usr/libexec", "/bin"]
        .iter()
        .any(|dir| Path::new(dir).join(tool).exists())
}

struct Nested {
    root: PathBuf,
    env: Vec<(String, String)>,
    address: String,
    /// Stopped by hand, since a compositor's environment cannot be read back.
    session: Vec<Child>,
    windows: Vec<Child>,
    daemon: Option<Child>,
}

impl Nested {
    fn start() -> Option<Nested> {
        for tool in [
            "dbus-daemon",
            "kwin_wayland",
            "foot",
            "wl-copy",
            "wl-paste",
            "xdg-desktop-portal",
            "xdg-desktop-portal-kde",
        ] {
            if !on_path(tool) {
                eprintln!("skipped: {tool} is not installed");
                return None;
            }
        }
        let root = PathBuf::from(format!("/tmp/wxn-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        for dir in ["home", "config", "data", "cache", "state", "run", "bin"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        fs::set_permissions(root.join("run"), Permissions::from_mode(0o700)).unwrap();

        let path = |dir: &str| root.join(dir).to_str().unwrap().to_string();
        let socket = format!("wayland-wxn-{}", std::process::id());
        let mut env: Vec<(String, String)> = [
            ("HOME", path("home")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_DATA_HOME", path("data")),
            ("XDG_CACHE_HOME", path("cache")),
            ("XDG_STATE_HOME", path("state")),
            ("XDG_RUNTIME_DIR", path("run")),
            ("CODEX_HOME", path("home")),
            ("PATH", format!("{}:/usr/bin", path("bin"))),
            ("XDG_CURRENT_DESKTOP", "KDE".to_string()),
            ("XDG_SESSION_TYPE", "wayland".to_string()),
            ("KDE_FULL_SESSION", "true".to_string()),
            ("KDE_SESSION_VERSION", "6".to_string()),
            ("WAYLAND_DISPLAY", socket.clone()),
            ("QT_QPA_PLATFORM", "wayland".to_string()),
            // Software rendering all round, so no GPU is needed, and so Qt's
            // dialogs do not send buffers the software compositor cannot import.
            ("KWIN_COMPOSE", "Q".to_string()),
            ("QT_QUICK_BACKEND", "software".to_string()),
            ("QSG_RHI_BACKEND", "software".to_string()),
            ("LANG", "C.UTF-8".to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();

        let log = File::create(root.join("session.log")).unwrap();
        let mut bus = Command::new("timeout")
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .arg(SESSION_LIMIT)
            .args(["dbus-daemon", "--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}/bus", root.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_string();
        assert!(
            address.starts_with(&format!("unix:path={}/", root.display())),
            "unexpected bus {address}"
        );
        env.push(("DBUS_SESSION_BUS_ADDRESS".to_string(), address.clone()));

        let kwin = Command::new("timeout")
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .arg(SESSION_LIMIT)
            .args(["kwin_wayland", "--virtual", "--no-lockscreen"])
            .args(["--width", "1280", "--height", "800", "--socket", &socket])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();

        let nested = Nested {
            env,
            address,
            session: vec![bus, kwin],
            windows: Vec::new(),
            daemon: None,
            root,
        };
        let display = nested.root.join("run").join(socket);
        require("the headless compositor", SECOND * 30, || display.exists());
        Some(nested)
    }

    fn connect(&self) -> Connection {
        Connection::connect(&self.address).unwrap()
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null());
        command
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
    }

    /// Stand-ins for the microphone and speech recognition, so nothing listens.
    fn fake_audio(&self) {
        let r = self.root.display();
        let speech: Vec<u8> = (0..16_000)
            .flat_map(|i| (if i % 2 == 0 { 20_000i16 } else { -20_000 }).to_le_bytes())
            .collect();
        fs::write(self.root.join("speech.raw"), speech).unwrap();
        fs::write(self.root.join("model.bin"), b"fake model").unwrap();
        self.script("pw-record", &format!("cat {r}/speech.raw\nexec sleep 300"));
        self.script(
            "whisper-cli",
            &format!(
                r#"[ -f {r}/slow ] && sleep 3
echo x >> {r}/transcriptions
echo ' {TRANSCRIPT}'"#
            ),
        );
        let config = format!(
            r#"{{
                "sounds": false,
                "notifications": "none",
                "min_hold_ms": 50,
                "audio": {{ "recorder": "pw-record" }},
                "whisper": {{ "binary": "whisper-cli", "model": "{r}/model.bin", "threads": 1 }}
            }}"#
        );
        let dir = self.root.join("config/wispx");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.json"), config).unwrap();
    }

    fn wispx(&self, args: &[&str]) -> (bool, String) {
        let output = self
            .command(env!("CARGO_BIN_EXE_wispx"))
            .args(args)
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (success, text) = self.wispx(args);
        assert!(success, "wispx {args:?}: {text}");
        text
    }

    fn start_daemon(&mut self) {
        let log = File::create(self.root.join("daemon.log")).unwrap();
        let child = self
            .command(env!("CARGO_BIN_EXE_wispx"))
            .args(["daemon", "--quiet"])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        require("the daemon", SECOND * 10, || self.wispx(&["status"]).0);
    }

    fn quit_daemon(&mut self) {
        self.ok(&["quit"]);
        let daemon = self.daemon.as_mut().unwrap();
        let deadline = Instant::now() + SECOND * 10;
        let status = loop {
            if let Some(status) = daemon.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the daemon did not quit");
            thread::sleep(Duration::from_millis(30));
        };
        assert_eq!(status.code(), Some(0));
        self.daemon = None;
    }

    /// Adds a setting at the start of the config file.
    fn set_setting(&self, setting: &str) {
        let path = self.root.join("config/wispx/config.json");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replacen('{', &format!("{{ {setting}"), 1)).unwrap();
    }

    fn status(&self) -> Json {
        serde_json::from_str(&self.ok(&["status", "--json"])).unwrap()
    }

    fn state(&self) -> String {
        self.status()["state"].as_str().unwrap_or("?").to_string()
    }

    #[track_caller]
    fn wait_for_state(&self, wanted: &str) {
        require(&format!("state {wanted}"), SECOND * 15, || {
            self.state() == wanted
        });
    }

    fn set_clipboard(&self, text: &str) {
        let status = self
            .command("wl-copy")
            .arg(text)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "wl-copy failed");
    }

    fn clipboard(&self) -> String {
        let output = self
            .command("wl-paste")
            .arg("--no-newline")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Opens a terminal whose keystrokes and pastes land in `file`, unprocessed.
    fn open_target(&mut self, file: &str) -> PathBuf {
        let path = self.root.join(file);
        let child = self
            .command("foot")
            .args(["sh", "-c"])
            .arg(format!("stty raw -echo; exec cat > {}", path.display()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.windows.push(child);
        require("the target window", SECOND * 15, || path.exists());
        // The file appears as the shell runs; give KWin a moment to focus it.
        thread::sleep(Duration::from_millis(500));
        path
    }

    fn transcriptions(&self) -> usize {
        fs::read_to_string(self.root.join("transcriptions")).map_or(0, |text| text.lines().count())
    }

    /// Lets the test's own portal session in without a dialog, using the same
    /// store the KDE backend consults. Only ever touches this scratch session.
    fn allow_remote_desktop(&self, app: &str) {
        let mut bus = self.connect();
        bus.call(
            "org.freedesktop.impl.portal.PermissionStore",
            "/org/freedesktop/impl/portal/PermissionStore",
            "org.freedesktop.impl.portal.PermissionStore",
            "SetPermission",
            vec![
                Value::str("kde-authorized"),
                Value::Bool(true),
                Value::str("remote-desktop"),
                Value::str(app),
                Value::Array {
                    element: "s".to_string(),
                    items: vec![Value::str("yes")],
                },
            ],
            SECOND * 30,
        )
        .unwrap();
    }

    fn desktop_file(&self, id: &str) {
        let dir = self.root.join("data/applications");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{id}.desktop")),
            "[Desktop Entry]\nType=Application\nName=WispX test driver\nExec=true\nNoDisplay=true\n",
        )
        .unwrap();
    }

    fn logs(&self) -> String {
        let mut text = String::new();
        for name in ["daemon.log", "session.log"] {
            let content = fs::read_to_string(self.root.join(name)).unwrap_or_default();
            let lines: Vec<&str> = content.lines().collect();
            let tail = &lines[lines.len().saturating_sub(40)..];
            text.push_str(&format!("--- {name}\n{}\n", tail.join("\n")));
        }
        text
    }

    /// Everything started on this bus names the scratch directory in its
    /// environment, and activated services outlive a bus that goes away.
    fn sweep(&self) {
        let marker = format!("XDG_RUNTIME_DIR={}/run", self.root.display());
        for _ in 0..2 {
            let Ok(entries) = fs::read_dir("/proc") else {
                return;
            };
            for entry in entries.flatten() {
                let Ok(environ) = fs::read(entry.path().join("environ")) else {
                    continue;
                };
                let ours = environ
                    .split(|byte| *byte == 0)
                    .any(|item| item == marker.as_bytes());
                let pid = entry.file_name().to_string_lossy().into_owned();
                if ours && pid != std::process::id().to_string() {
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid])
                        .stderr(Stdio::null())
                        .status();
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
    }

    fn stop_child(child: &mut Child) {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + SECOND * 5;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("{}", self.logs());
        }
        if let Some(child) = self.daemon.as_mut() {
            Nested::stop_child(child);
        }
        for child in &mut self.windows {
            Nested::stop_child(child);
        }
        self.sweep();
        // `timeout` passes the signal on to the compositor, then the bus.
        for child in self.session.iter_mut().rev() {
            Nested::stop_child(child);
        }
        self.sweep();
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// This test's own keyboard: a RemoteDesktop session, so that keys reach KWin
/// as input events and take the same path as a person pressing them.
struct Keyboard {
    bus: Connection,
    session: String,
}

impl Keyboard {
    fn open(nested: &Nested) -> Keyboard {
        nested.desktop_file(DRIVER_ID);
        nested.allow_remote_desktop(DRIVER_ID);
        let mut bus = nested.connect();
        // The portal is already running here, as it would be for anyone running
        // `wispx setup`, and has to notice the new desktop entry.
        let written = Instant::now();
        require("the portal to know the desktop entry", SECOND * 30, || {
            let reply = bus.call(
                portal::BUS_NAME,
                portal::OBJECT_PATH,
                "org.freedesktop.host.portal.Registry",
                "Register",
                vec![Value::str(DRIVER_ID), Value::vardict(Vec::new())],
                SECOND * 30,
            );
            if let Err(error) = &reply {
                eprintln!("{:?} Register: {error}", written.elapsed());
                thread::sleep(Duration::from_millis(250));
            }
            reply.is_ok()
        });
        eprintln!("the portal knew the entry after {:?}", written.elapsed());
        let created = portal::request(
            &mut bus,
            REMOTE,
            "CreateSession",
            Vec::new(),
            vec![(
                "session_handle_token".to_string(),
                Value::str("wispx_test_driver"),
            )],
            SECOND * 30,
        )
        .unwrap();
        let Response::Success(results) = created else {
            panic!("CreateSession: {created:?}");
        };
        let session = results
            .lookup("session_handle")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let selected = portal::request(
            &mut bus,
            REMOTE,
            "SelectDevices",
            vec![Value::path(session.as_str())],
            vec![("types".to_string(), Value::Uint32(1))],
            SECOND * 30,
        )
        .unwrap();
        assert!(
            matches!(selected, Response::Success(_)),
            "SelectDevices: {selected:?}"
        );
        let started = portal::request(
            &mut bus,
            REMOTE,
            "Start",
            vec![Value::path(session.as_str()), Value::str("")],
            Vec::new(),
            SECOND * 30,
        )
        .unwrap();
        assert!(
            matches!(started, Response::Success(_)),
            "Start: {started:?}"
        );
        Keyboard { bus, session }
    }

    fn key(&mut self, keysym: i32, state: u32) {
        self.bus
            .call(
                portal::BUS_NAME,
                portal::OBJECT_PATH,
                REMOTE,
                "NotifyKeyboardKeysym",
                vec![
                    Value::path(self.session.as_str()),
                    Value::vardict(Vec::new()),
                    Value::Int32(keysym),
                    Value::Uint32(state),
                ],
                SECOND * 10,
            )
            .unwrap();
        thread::sleep(Duration::from_millis(40));
    }

    fn down(&mut self, keys: &[i32]) {
        for key in keys {
            self.key(*key, 1);
        }
    }

    fn up(&mut self, keys: &[i32]) {
        for key in keys.iter().rev() {
            self.key(*key, 0);
        }
    }

    fn tap(&mut self, keys: &[i32]) {
        self.down(keys);
        self.up(keys);
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn real_plasma_stack_dictation() {
    let Some(mut nested) = Nested::start() else {
        return;
    };
    nested.fake_audio();
    nested.ok(&["setup"]);
    assert!(
        nested
            .root
            .join(format!("data/applications/{WISPX_ID}.desktop"))
            .exists()
    );

    let doctor = nested.wispx(&["doctor"]).1;
    eprintln!("{doctor}");
    assert!(
        doctor.contains("[ok]   session: KDE Plasma on Wayland"),
        "{doctor}"
    );
    assert!(doctor.contains("GlobalShortcuts version"), "{doctor}");

    let mut keyboard = Keyboard::open(&nested);
    eprintln!("the test's keyboard session is open");

    // A person's first start: the desktop asks to confirm the shortcuts.
    nested.start_daemon();
    let bound = |nested: &Nested| {
        nested.status()["hotkey"]
            .as_str()
            .is_some_and(|text| text.starts_with("portal shortcuts:"))
    };
    if !wait_for("shortcuts without a dialog", SECOND * 5, || bound(&nested)) {
        eprintln!("hotkey status: {}", nested.status()["hotkey"]);
        eprintln!("answering the desktop's dialog with Return");
        keyboard.tap(&[RETURN]);
    }
    require("shortcuts to bind", SECOND * 20, || bound(&nested));
    eprintln!("hotkey status: {}", nested.status()["hotkey"]);

    // Paste permission, through the desktop's real consent dialog.
    let mut setup = nested
        .command(env!("CARGO_BIN_EXE_wispx"))
        .arg("paste-setup")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(SECOND * 2);
    assert!(
        setup.try_wait().unwrap().is_none(),
        "paste-setup did not ask"
    );
    keyboard.tap(&[RETURN]);
    require("the dialog to be answered", SECOND * 20, || {
        setup.try_wait().unwrap().is_some()
    });
    let answered = setup.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&answered.stdout).into_owned()
        + &String::from_utf8_lossy(&answered.stderr);
    eprintln!("paste-setup, answering the real dialog: {said:?}");
    assert!(answered.status.success(), "paste-setup: {said}");
    let token = nested.root.join("data/wispx/remote-desktop-token");
    assert!(!read(&token).trim().is_empty(), "no grant was stored");
    require("automatic paste", SECOND * 10, || {
        nested.status()["paste"]
            .as_str()
            .is_some_and(|text| text.starts_with("automatic"))
    });

    // Hold to talk, delivered into a real window through its own paste shortcut.
    let typed = nested.open_target("typed");
    nested.set_clipboard("what I copied earlier");
    require("the earlier clipboard", SECOND * 5, || {
        nested.clipboard() == "what I copied earlier"
    });
    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    thread::sleep(Duration::from_millis(700));
    keyboard.up(&[CONTROL, META, KEY_D]);
    require("the paste", SECOND * 15, || read(&typed) == TRANSCRIPT);
    eprintln!("hold to talk pasted into foot: {:?}", read(&typed));
    require("the clipboard to come back", SECOND * 10, || {
        nested.clipboard() == "what I copied earlier"
    });
    nested.wait_for_state("idle");

    // Going hands-free while the hold is down must keep the recording.
    let before = nested.transcriptions();
    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    thread::sleep(Duration::from_millis(400));
    keyboard.tap(&[SPACE]);
    nested.wait_for_state("recording-hands-free");
    keyboard.up(&[CONTROL, META, KEY_D]);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        nested.state(),
        "recording-hands-free",
        "letting go ended it"
    );
    assert_eq!(nested.transcriptions(), before);
    keyboard.tap(&[CONTROL, META, SPACE]);
    require("the hands-free paste", SECOND * 15, || {
        read(&typed) == format!("{TRANSCRIPT}{TRANSCRIPT}")
    });
    nested.wait_for_state("idle");
    eprintln!("hold, then hands-free, then finish: pasted once");

    // Cancel throws the recording away, from idle hands-free and from a hold.
    let before = nested.transcriptions();
    keyboard.tap(&[CONTROL, META, SPACE]);
    nested.wait_for_state("recording-hands-free");
    thread::sleep(Duration::from_millis(300));
    keyboard.tap(&[CONTROL, META, BACKSPACE]);
    nested.wait_for_state("idle");

    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    thread::sleep(Duration::from_millis(300));
    keyboard.tap(&[BACKSPACE]);
    nested.wait_for_state("idle");
    keyboard.up(&[CONTROL, META, KEY_D]);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(nested.state(), "idle");
    assert_eq!(
        nested.transcriptions(),
        before,
        "cancel must not transcribe"
    );
    eprintln!("cancel discarded both recordings");

    // A window that takes focus during transcription must not receive keys.
    let pasted = read(&typed);
    fs::write(nested.root.join("slow"), b"").unwrap();
    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    thread::sleep(Duration::from_millis(400));
    keyboard.up(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("transcribing");
    let other = nested.open_target("other");
    require("the transcript on the clipboard", SECOND * 15, || {
        nested.clipboard() == TRANSCRIPT
    });
    nested.wait_for_state("idle");
    thread::sleep(Duration::from_secs(1));
    assert_eq!(read(&other), "", "keys reached the new window");
    assert_eq!(read(&typed), pasted, "keys reached the old window");
    assert_eq!(
        nested.clipboard(),
        TRANSCRIPT,
        "the copy must stay for the user"
    );
    eprintln!("focus change: nothing pasted, transcript left on the clipboard");
    fs::remove_file(nested.root.join("slow")).unwrap();

    // The next login: both grants come back with no dialog and no key pressed.
    nested.quit_daemon();
    nested.start_daemon();
    require("shortcuts to come back unasked", SECOND * 10, || {
        bound(&nested)
    });
    require("paste to come back unasked", SECOND * 15, || {
        nested.status()["paste"]
            .as_str()
            .is_some_and(|text| text.starts_with("automatic"))
    });
    assert!(!read(&token).trim().is_empty(), "the grant was lost");
    eprintln!("after a restart both grants were restored without a dialog");

    // The window that has focus now gets the next dictation.
    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    thread::sleep(Duration::from_millis(400));
    keyboard.up(&[CONTROL, META, KEY_D]);
    require("the paste after restart", SECOND * 15, || {
        read(&other) == TRANSCRIPT
    });

    // Ctrl+V, what ordinary applications get. A raw terminal shows it as ^V.
    nested.set_setting(r#""paste_shortcut": "ctrl+v","#);
    nested.ok(&["reload"]);
    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    thread::sleep(Duration::from_millis(400));
    keyboard.up(&[CONTROL, META, KEY_D]);
    require("Ctrl+V to arrive", SECOND * 15, || {
        read(&other) == format!("{TRANSCRIPT}\u{16}")
    });
    eprintln!("Ctrl+V reached the focused window as a single chord");
    nested.wait_for_state("idle");

    nested.quit_daemon();
    let log = read(&nested.root.join("daemon.log"));
    assert!(
        !log.contains(TRANSCRIPT),
        "the log must not hold transcripts"
    );
}

/// Real whisper.cpp behind the real desktop stack, fed a public recording in
/// place of the microphone. Needs paths to a model and a 16 kHz mono WAV:
///
///     WISPX_TEST_MODEL=ggml-tiny.en.bin WISPX_TEST_WAV=jfk.wav \
///       cargo test --test plasma_nested -- --ignored --nocapture --test-threads=1
#[test]
#[ignore = "starts a private headless KWin and needs a whisper.cpp model"]
fn real_whisper_through_the_real_plasma_stack() {
    let (Some(model), Some(wav)) = (
        std::env::var_os("WISPX_TEST_MODEL"),
        std::env::var_os("WISPX_TEST_WAV"),
    ) else {
        eprintln!("skipped: WISPX_TEST_MODEL and WISPX_TEST_WAV are not set");
        return;
    };
    let Some(mut nested) = Nested::start() else {
        return;
    };
    let r = nested.root.display().to_string();
    let bytes = fs::read(&wav).unwrap();
    fs::write(nested.root.join("speech.raw"), &bytes[44..]).unwrap();
    nested.script("pw-record", &format!("cat {r}/speech.raw\nexec sleep 300"));
    let dir = nested.root.join("config/wispx");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.json"),
        format!(
            r#"{{
                "sounds": false,
                "notifications": "none",
                "language": "en",
                "audio": {{ "recorder": "pw-record" }},
                "whisper": {{ "model": "{}" }}
            }}"#,
            Path::new(&model).canonicalize().unwrap().display()
        ),
    )
    .unwrap();
    nested.ok(&["setup"]);
    nested.allow_remote_desktop(WISPX_ID);
    let mut keyboard = Keyboard::open(&nested);
    nested.start_daemon();
    thread::sleep(SECOND * 3);
    keyboard.tap(&[RETURN]);
    require("shortcuts to bind", SECOND * 20, || {
        nested.status()["hotkey"]
            .as_str()
            .is_some_and(|text| text.starts_with("portal shortcuts:"))
    });
    let setup = nested.wispx(&["paste-setup"]);
    assert!(setup.0, "paste-setup: {}", setup.1);
    let typed = nested.open_target("typed");

    keyboard.down(&[CONTROL, META, KEY_D]);
    nested.wait_for_state("recording");
    // Let the whole recording arrive through the pipe before letting go.
    thread::sleep(SECOND * 2);
    keyboard.up(&[CONTROL, META, KEY_D]);
    require("the real transcript", SECOND * 90, || {
        read(&typed)
            .to_lowercase()
            .contains("ask not what your country")
    });
    eprintln!("whisper.cpp, pasted into foot: {:?}", read(&typed));
    nested.wait_for_state("idle");
    nested.quit_daemon();
}
