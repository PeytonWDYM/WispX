//! The private headless Plasma session the opt-in tests run in. See `plasma_nested.rs`
//! and `gui_nested.rs` for what is real and what is not. Every process here gets a
//! cleared environment, and connects only to this session's own compositor and bus.

use serde_json::Value as Json;
use std::fs::{self, File, Permissions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use wispx::dbus::{Connection, Value};
use wispx::portal::{self, Response};

pub const SECOND: Duration = Duration::from_secs(1);
/// `timeout` wrapped around the bus and compositor stops them even if this test
/// is killed outright.
pub const SESSION_LIMIT: &str = "240";
pub const REMOTE: &str = "org.freedesktop.portal.RemoteDesktop";
pub const WISPX_ID: &str = "io.github.PeytonWDYM.WispX";
pub const DRIVER_ID: &str = "io.github.PeytonWDYM.WispX.TestDriver";
pub const TRANSCRIPT: &str = "hello from the nested desktop";

pub const CONTROL: i32 = 0xffe3;
pub const META: i32 = 0xffeb;
pub const KEY_D: i32 = 0x64;
pub const SPACE: i32 = 0x20;
pub const BACKSPACE: i32 = 0xff08;
pub const RETURN: i32 = 0xff0d;
const BTN_LEFT: i32 = 272;

#[track_caller]
pub fn wait_for(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
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
pub fn require(what: &str, limit: Duration, condition: impl FnMut() -> bool) {
    assert!(wait_for(what, limit, condition), "timed out: {what}");
}

pub fn on_path(tool: &str) -> bool {
    ["/usr/bin", "/usr/lib", "/usr/libexec", "/bin"]
        .iter()
        .any(|dir| Path::new(dir).join(tool).exists())
}

pub struct Nested {
    pub root: PathBuf,
    pub env: Vec<(String, String)>,
    pub address: String,
    /// Stopped by hand, since a compositor's environment cannot be read back.
    pub session: Vec<Child>,
    pub windows: Vec<Child>,
    pub daemon: Option<Child>,
}

impl Nested {
    pub fn start() -> Option<Nested> {
        Nested::start_with(&[])
    }

    /// KWin can only take screenshots when compositing with OpenGL. It uses a render
    /// node or Mesa's software driver, with no display, so this stays just as private.
    /// With `WISPX_TEST_QT_GPU` set, the app also draws on the GPU, as on a real
    /// desktop, rather than in software.
    pub fn start_for_pictures(screens: &[&str]) -> Option<Nested> {
        if std::env::var_os("WISPX_TEST_QT_GPU").is_some() {
            return Nested::start_on_screens(
                &[
                    ("KWIN_COMPOSE", "O2"),
                    ("QT_QUICK_BACKEND", ""),
                    ("QSG_RHI_BACKEND", ""),
                ],
                screens,
            );
        }
        Nested::start_on_screens(&[("KWIN_COMPOSE", "O2")], screens)
    }

    pub fn start_with(settings: &[(&str, &str)]) -> Option<Nested> {
        Nested::start_on_screens(settings, &[])
    }

    /// `screens` are further options for the compositor, such as how many it has.
    pub fn start_on_screens(settings: &[(&str, &str)], screens: &[&str]) -> Option<Nested> {
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

        for (name, value) in settings {
            env.retain(|(existing, _)| existing != name);
            env.push(((*name).to_string(), (*value).to_string()));
        }

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
            .args(screens)
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

    pub fn connect(&self) -> Connection {
        Connection::connect(&self.address).unwrap()
    }

    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null());
        command
    }

    pub fn script(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
    }

    /// Stand-ins for the microphone and speech recognition, so nothing listens.
    pub fn fake_audio(&self) {
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

    pub fn wispx(&self, args: &[&str]) -> (bool, String) {
        let output = self
            .command(env!("CARGO_BIN_EXE_wispx"))
            .args(args)
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let (success, text) = self.wispx(args);
        assert!(success, "wispx {args:?}: {text}");
        text
    }

    pub fn start_daemon(&mut self) {
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

    pub fn quit_daemon(&mut self) {
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
    pub fn set_setting(&self, setting: &str) {
        let path = self.root.join("config/wispx/config.json");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replacen('{', &format!("{{ {setting}"), 1)).unwrap();
    }

    pub fn status(&self) -> Json {
        serde_json::from_str(&self.ok(&["status", "--json"])).unwrap()
    }

    pub fn state(&self) -> String {
        self.status()["state"].as_str().unwrap_or("?").to_string()
    }

    #[track_caller]
    pub fn wait_for_state(&self, wanted: &str) {
        require(&format!("state {wanted}"), SECOND * 15, || {
            self.state() == wanted
        });
    }

    pub fn set_clipboard(&self, text: &str) {
        let status = self
            .command("wl-copy")
            .arg(text)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "wl-copy failed");
    }

    pub fn clipboard(&self) -> String {
        let output = self
            .command("wl-paste")
            .arg("--no-newline")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Opens a terminal whose keystrokes and pastes land in `file`, unprocessed.
    pub fn open_target(&mut self, file: &str) -> PathBuf {
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

    pub fn transcriptions(&self) -> usize {
        fs::read_to_string(self.root.join("transcriptions")).map_or(0, |text| text.lines().count())
    }

    /// Lets the test's own portal session in without a dialog, using the same
    /// store the KDE backend consults. Only ever touches this scratch session.
    pub fn allow_remote_desktop(&self, app: &str) {
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

    pub fn desktop_file(&self, id: &str) {
        let dir = self.root.join("data/applications");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{id}.desktop")),
            "[Desktop Entry]\nType=Application\nName=WispX test driver\nExec=true\nNoDisplay=true\n",
        )
        .unwrap();
    }

    /// What the compositor itself says about its windows and the pointer, from a
    /// short script. This is the truth about focus, whatever a client believes.
    pub fn compositor(&self) -> Json {
        const SOURCE: &str = r#"
const list = workspace.stackingOrder.map(w => ({
    id: String(w.internalId),
    class: String(w.resourceClass),
    name: String(w.resourceName),
    caption: String(w.caption),
    active: w.active,
    fullscreen: w.fullScreen,
    skip_taskbar: w.skipTaskbar,
    skip_switcher: w.skipSwitcher,
    normal: w.normalWindow,
    layer: w.layer,
    x: w.frameGeometry.x,
    y: w.frameGeometry.y,
    width: w.frameGeometry.width,
    height: w.frameGeometry.height,
    client_x: w.clientGeometry.x,
    client_y: w.clientGeometry.y,
}));
callDBus("UNIQUE", "/", "", "result", JSON.stringify({
    active: workspace.activeWindow ? String(workspace.activeWindow.internalId) : "",
    cursor: { x: workspace.cursorPos.x, y: workspace.cursorPos.y },
    work_area: (() => {
        const a = workspace.clientArea(KWin.MaximizeArea, workspace.activeScreen, workspace.currentDesktop);
        return { x: a.x, y: a.y, width: a.width, height: a.height };
    })(),
    windows: list,
}));
"#;
        self.kwin_script(SOURCE)
    }

    /// Makes the window with focus fill the screen, or stop.
    pub fn set_active_fullscreen(&self, on: bool) {
        self.kwin_script(&format!(
            r#"workspace.activeWindow.fullScreen = {on};
callDBus("UNIQUE", "/", "", "result", "{{}}");"#
        ));
    }

    /// Moves the window with focus to another screen.
    pub fn send_active_to_screen(&self, screen: usize) {
        self.kwin_script(&format!(
            r#"workspace.sendClientToScreen(workspace.activeWindow, workspace.screens[{screen}]);
callDBus("UNIQUE", "/", "", "result", "{{}}");"#
        ));
    }

    /// Runs a script in the compositor. It answers by sending JSON to `UNIQUE`.
    fn kwin_script(&self, source: &str) -> Json {
        let mut bus = self.connect();
        let unique = bus.unique_name().to_string();
        let name = format!("wxn-dump-{:?}", Instant::now());
        let name: String = name.chars().filter(char::is_ascii_alphanumeric).collect();
        let path = self.root.join("run").join(format!("{name}.js"));
        fs::write(&path, source.replace("UNIQUE", &unique)).unwrap();
        let scripting = ("org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting");
        let reply = bus
            .call(
                scripting.0,
                scripting.1,
                scripting.2,
                "loadScript",
                vec![
                    Value::str(path.to_string_lossy()),
                    Value::str(name.as_str()),
                ],
                SECOND * 5,
            )
            .unwrap();
        let id = reply.first().and_then(Value::as_i32).unwrap();
        bus.call(
            scripting.0,
            &format!("/Scripting/Script{id}"),
            "org.kde.kwin.Script",
            "run",
            Vec::new(),
            SECOND * 5,
        )
        .unwrap();
        let deadline = Instant::now() + SECOND * 5;
        let answer = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(message) = bus.next_message(left).unwrap() else {
                panic!("KWin's script did not answer");
            };
            if message.member.as_deref() == Some("result") {
                let text = message.body.first().and_then(Value::as_str).unwrap_or("{}");
                let parsed = serde_json::from_str(text).unwrap();
                let _ = bus.reply(&message, Vec::new());
                break parsed;
            }
        };
        let _ = bus.call(
            scripting.0,
            scripting.1,
            scripting.2,
            "unloadScript",
            vec![Value::str(name.as_str())],
            SECOND * 5,
        );
        let _ = fs::remove_file(&path);
        answer
    }

    /// Takes a picture of the whole private screen.
    pub fn screenshot(&self, path: &Path) -> bool {
        let _ = fs::remove_file(path);
        let status = self
            .command("timeout")
            .args([
                "20",
                "spectacle",
                "--background",
                "--nonotify",
                "--fullscreen",
                "--output",
            ])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        status.is_ok_and(|status| status.success()) && path.exists()
    }

    pub fn logs(&self) -> String {
        let mut text = String::new();
        for name in ["daemon.log", "session.log", "run/wispx/app.log"] {
            let content = fs::read_to_string(self.root.join(name)).unwrap_or_default();
            let lines: Vec<&str> = content.lines().collect();
            let tail = &lines[lines.len().saturating_sub(40)..];
            text.push_str(&format!("--- {name}\n{}\n", tail.join("\n")));
        }
        text
    }

    /// Whether a process of this session has `text` in its command line.
    /// Processes of this session only, told apart by the scratch directory in
    /// their environment, whose command line mentions `text`.
    pub fn pids(&self, text: &str) -> Vec<u32> {
        let marker = format!("XDG_RUNTIME_DIR={}/run", self.root.display());
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
                let environ = fs::read(entry.path().join("environ")).ok()?;
                let ours = environ
                    .split(|byte| *byte == 0)
                    .any(|item| item == marker.as_bytes());
                let command = fs::read(entry.path().join("cmdline")).ok()?;
                let command = String::from_utf8_lossy(&command).replace('\0', " ");
                (ours && command.contains(text)).then_some(pid)
            })
            .collect()
    }

    pub fn running(&self, text: &str) -> bool {
        !self.pids(text).is_empty()
    }

    /// Everything started on this bus names the scratch directory in its
    /// environment, and activated services outlive a bus that goes away.
    pub fn sweep(&self) {
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

    pub fn stop_child(child: &mut Child) {
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
pub struct Keyboard {
    pub bus: Connection,
    pub session: String,
}

impl Keyboard {
    pub fn open(nested: &Nested) -> Keyboard {
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
            // Keyboard and pointer, so tests can use a window the way a person does.
            vec![("types".to_string(), Value::Uint32(3))],
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

    pub fn key(&mut self, keysym: i32, state: u32) {
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

    pub fn down(&mut self, keys: &[i32]) {
        for key in keys {
            self.key(*key, 1);
        }
    }

    pub fn up(&mut self, keys: &[i32]) {
        for key in keys.iter().rev() {
            self.key(*key, 0);
        }
    }

    pub fn tap(&mut self, keys: &[i32]) {
        self.down(keys);
        self.up(keys);
    }

    fn pointer(&mut self, method: &str, args: Vec<Value>) {
        let mut all = vec![
            Value::path(self.session.as_str()),
            Value::vardict(Vec::new()),
        ];
        all.extend(args);
        self.bus
            .call(
                portal::BUS_NAME,
                portal::OBJECT_PATH,
                REMOTE,
                method,
                all,
                SECOND * 10,
            )
            .unwrap();
    }

    /// Moves in relative steps, which is all this portal session can do, from a
    /// corner the pointer can always be pushed into.
    pub fn move_pointer(&mut self, x: f64, y: f64) {
        self.pointer(
            "NotifyPointerMotion",
            vec![Value::Double(-5000.0), Value::Double(-5000.0)],
        );
        thread::sleep(Duration::from_millis(40));
        self.pointer(
            "NotifyPointerMotion",
            vec![Value::Double(x), Value::Double(y)],
        );
        thread::sleep(Duration::from_millis(80));
    }

    /// Turns the wheel. Positive steps scroll down.
    pub fn scroll(&mut self, steps: i32) {
        self.pointer(
            "NotifyPointerAxisDiscrete",
            vec![Value::Uint32(0), Value::Int32(steps)],
        );
        thread::sleep(Duration::from_millis(150));
    }

    pub fn click(&mut self, x: f64, y: f64) {
        self.move_pointer(x, y);
        self.pointer(
            "NotifyPointerButton",
            vec![Value::Int32(BTN_LEFT), Value::Uint32(1)],
        );
        thread::sleep(Duration::from_millis(40));
        self.pointer(
            "NotifyPointerButton",
            vec![Value::Int32(BTN_LEFT), Value::Uint32(0)],
        );
        thread::sleep(Duration::from_millis(120));
    }
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
