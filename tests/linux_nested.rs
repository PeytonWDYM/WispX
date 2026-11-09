//! WispX's Linux paste and shortcut paths against a real, private Plasma stack.
//!
//! REAL: headless `kwin_wayland --virtual` with XWayland, its built-in global
//! shortcuts, xdg-desktop-portal with the KDE backend and permission store, the
//! data-control and X11 clipboards, foot and a small X11 window as paste targets,
//! and WispX's own `Hotkeys` and `Paster`. Keys are pressed through a RemoteDesktop
//! session belonging to this test, so they travel through KWin as input events.
//! MOCKED: the microphone and transcription. A finished dictation delivers a fixed
//! synthetic transcript, so no account or audio is involved.
//!
//! It starts a compositor, so it is ignored by default:
//!
//!     cargo test --test linux_nested -- --ignored --nocapture --test-threads=1
//!
//! Every process runs with a cleared environment: scratch HOME, XDG directories,
//! session bus, Wayland socket and X display, all deleted afterwards. Nothing
//! reaches the desktop session the test is started from.
#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

const ROLE: &str = "WISPX_NESTED_ROLE";
const WISPX_ID: &str = "io.github.PeytonWDYM.WispX";
const DRIVER_ID: &str = "io.github.PeytonWDYM.WispX.TestDriver";
const CONTROL: u32 = 0xffe3;
const META: u32 = 0xffeb;
const KEY_D: u32 = 0x64;
const SPACE: u32 = 0x20;
const RETURN: u32 = 0xff0d;
const BACKSPACE: u32 = 0xff08;
const SECOND: Duration = Duration::from_secs(1);

fn on_path(tool: &str) -> bool {
    ["/usr/bin", "/usr/lib", "/bin"]
        .iter()
        .any(|dir| Path::new(dir).join(tool).exists())
}
fn wait_for(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !condition() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// A private bus and headless KWin; every child gets only `env`.
struct Nested {
    root: PathBuf,
    env: Vec<(String, String)>,
    children: Vec<Child>,
}
impl Nested {
    fn start() -> Option<Self> {
        for tool in [
            "dbus-daemon",
            "kwin_wayland",
            "Xwayland",
            "foot",
            "wl-copy",
            "wl-paste",
            "xdg-desktop-portal",
            "xdg-desktop-portal-kde",
            "xdg-permission-store",
            "timeout",
        ] {
            if !on_path(tool) {
                eprintln!("skipped: {tool} is not installed");
                return None;
            }
        }
        let root = PathBuf::from(format!("/tmp/wispx-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        for dir in ["home", "config", "data/applications", "cache", "run"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        for (id, name) in [(WISPX_ID, "WispX"), (DRIVER_ID, "WispX test driver")] {
            fs::write(
                root.join(format!("data/applications/{id}.desktop")),
                format!(
                    "[Desktop Entry]\nType=Application\nName={name}\nExec=true\nNoDisplay=true\n"
                ),
            )
            .unwrap();
        }
        // KWin asks before an XWayland client may emulate input (XTEST through
        // libei). This private session answers yes up front, standing in for the
        // user's consent; a real Xorg session has no such prompt.
        fs::write(
            root.join("config/kwinrc"),
            "[Xwayland]\nXwaylandEisNoPrompt=true\n",
        )
        .unwrap();
        let path = |dir: &str| root.join(dir).display().to_string();
        let socket = format!("wayland-wispx-nested-{}", std::process::id());
        let mut env: Vec<(String, String)> = [
            ("HOME", path("home")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_DATA_HOME", path("data")),
            ("XDG_CACHE_HOME", path("cache")),
            ("XDG_RUNTIME_DIR", path("run")),
            ("CODEX_HOME", path("home")),
            ("PATH", "/usr/bin:/bin".into()),
            ("LANG", "C.UTF-8".into()),
            ("XDG_CURRENT_DESKTOP", "KDE".into()),
            ("XDG_SESSION_TYPE", "wayland".into()),
            ("KDE_FULL_SESSION", "true".into()),
            ("KDE_SESSION_VERSION", "6".into()),
            ("WAYLAND_DISPLAY", socket.clone()),
            ("QT_QPA_PLATFORM", "wayland".into()),
            ("KWIN_COMPOSE", "Q".into()),
            ("QT_QUICK_BACKEND", "software".into()),
            ("QSG_RHI_BACKEND", "software".into()),
            ("XWAYLAND_NO_GLAMOR", "1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let log = fs::File::create(root.join("session.log")).unwrap();
        let mut bus = Command::new("timeout")
            .env_clear()
            .envs(env.clone())
            .args([
                "600",
                "dbus-daemon",
                "--session",
                "--nofork",
                "--print-address=1",
            ])
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
        let address = address.trim().to_owned();
        assert!(address.starts_with(&format!("unix:path={}/", root.display())));
        env.push(("DBUS_SESSION_BUS_ADDRESS".into(), address));
        // KWin's child learns XWayland's display and its authority file.
        let probe = root.join("x11-env.sh");
        fs::write(
            &probe,
            format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n' \"$DISPLAY\" \"$XAUTHORITY\" > {}/x11-env.tmp\nmv {0}/x11-env.tmp {0}/x11-env\nexec sleep 600\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        let kwin = Command::new("timeout")
            .env_clear()
            .envs(env.clone())
            .args([
                "600",
                "kwin_wayland",
                "--virtual",
                "--xwayland",
                "--no-lockscreen",
            ])
            .args(["--width", "1280", "--height", "800", "--socket", &socket])
            .arg(&probe)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let nested = Self {
            env,
            children: vec![bus, kwin],
            root,
        };
        let display = nested.root.join("run").join(&socket);
        assert!(
            wait_for(SECOND * 30, || display.exists()),
            "headless KWin didn't start"
        );
        Some(nested)
    }
    /// The XWayland half: `DISPLAY` and `XAUTHORITY` without `WAYLAND_DISPLAY`.
    fn x11_env(&self) -> Vec<(String, String)> {
        let file = self.root.join("x11-env");
        assert!(
            wait_for(SECOND * 30, || file.exists()),
            "XWayland didn't start"
        );
        let text = read(&file);
        let mut lines = text.lines();
        let display = lines.next().unwrap_or_default().to_owned();
        let authority = lines.next().unwrap_or_default().to_owned();
        assert!(display.starts_with(':'), "unexpected DISPLAY {display:?}");
        let mut env: Vec<_> = self
            .env
            .iter()
            .filter(|(k, _)| k != "WAYLAND_DISPLAY")
            .cloned()
            .collect();
        env.push(("DISPLAY".into(), display));
        if !authority.is_empty() {
            env.push(("XAUTHORITY".into(), authority));
        }
        env.push(("XDG_SESSION_TYPE".into(), "x11".into()));
        env
    }
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(self.env.clone())
            .stdin(Stdio::null());
        command
    }
    fn clipboard(&self) -> String {
        let output = self
            .command("timeout")
            .args(["5", "wl-paste", "--no-newline"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
    fn set_clipboard(&self, text: &str) {
        let status = self
            .command("wl-copy")
            .arg(text)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    /// A foot terminal whose pastes land in `file`, unprocessed.
    fn foot(&mut self, file: &str) -> PathBuf {
        let path = self.root.join(file);
        let child = self
            .command("foot")
            .args(["sh", "-c"])
            .arg(format!("stty raw -echo; exec cat > {}", path.display()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.children.push(child);
        assert!(wait_for(SECOND * 15, || path.exists()), "foot didn't start");
        thread::sleep(Duration::from_millis(700));
        path
    }
    fn role(&self, role: &str, env: Option<Vec<(String, String)>>, extra: &[(&str, &str)]) -> Role {
        let log = fs::File::create(self.root.join(format!("{role}.log"))).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "role",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .envs(env.unwrap_or_else(|| self.env.clone()))
            .env(ROLE, role)
            .env("WISPX_NESTED_ROOT", &self.root)
            .envs(extra.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let name = role.to_owned();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                eprintln!("[{name}] {line}");
                record.lock().unwrap().push(line.clone());
                // libtest may print "test role ... " ahead of the first report.
                if let Some(at) = line.find("@ ") {
                    let _ = tx.send(line[at + 2..].to_owned());
                }
            }
        });
        Role {
            pending: Mutex::new(Vec::new()),
            stdin: child.stdin.take().unwrap(),
            child,
            lines,
            name: role.to_owned(),
            log: self.root.join(format!("{role}.log")),
        }
    }
    fn logs(&self) -> String {
        let mut text = String::new();
        for entry in fs::read_dir(&self.root).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "log") {
                let content = read(&path);
                let lines: Vec<_> = content.lines().collect();
                let tail = &lines[lines.len().saturating_sub(30)..];
                text.push_str(&format!("--- {}\n{}\n", path.display(), tail.join("\n")));
            }
        }
        text
    }
}
impl Drop for Nested {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("{}", self.logs());
        }
        // TERM, not KILL: `timeout` passes TERM on to KWin and the bus, while a
        // KILL would orphan them.
        for child in self.children.iter_mut().rev() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            if !wait_for(SECOND * 5, || matches!(child.try_wait(), Ok(Some(_)))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        // Whatever is left belongs to this session: it names the scratch
        // directory in its environment or, where that's unreadable (KWin raises
        // its capabilities), on its command line.
        let root = self.root.display().to_string();
        let marker = format!("XDG_RUNTIME_DIR={root}/run");
        for _ in 0..2 {
            for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
                let pid = entry.file_name().to_string_lossy().into_owned();
                if pid.parse::<u32>().is_err() || pid == std::process::id().to_string() {
                    continue;
                }
                let environ = fs::read(entry.path().join("environ")).unwrap_or_default();
                let command = fs::read(entry.path().join("cmdline")).unwrap_or_default();
                let ours = environ
                    .split(|&b| b == 0)
                    .any(|item| item == marker.as_bytes())
                    || String::from_utf8_lossy(&command).contains(&root);
                if ours {
                    let _ = Command::new("kill").args(["-TERM", &pid]).status();
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Role {
    pending: Mutex<Vec<String>>,
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    name: String,
    log: PathBuf,
}
impl Role {
    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }
    /// The next report starting with `prefix`, skipping others.
    fn expect(&self, prefix: &str, limit: Duration) -> String {
        self.wait(prefix, limit).unwrap_or_else(|| {
            panic!(
                "{}: no '{prefix}' within {limit:?}; stderr:\n{}",
                self.name,
                read(&self.log)
            )
        })
    }
    /// Reports skipped while waiting for another stay available, in order.
    fn wait(&self, prefix: &str, limit: Duration) -> Option<String> {
        let mut pending = self.pending.lock().unwrap();
        if let Some(at) = pending.iter().position(|l| l.starts_with(prefix)) {
            let line = pending.remove(at);
            return Some(line[prefix.len()..].trim().to_owned());
        }
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let line = self.lines.recv_timeout(left).ok()?;
            match line.strip_prefix(prefix) {
                Some(rest) => return Some(rest.trim().to_owned()),
                None => pending.push(line),
            }
        }
    }
    fn keys(&mut self, verb: &str, keys: &[u32]) {
        let keys: Vec<_> = keys.iter().map(|k| format!("{k:#x}")).collect();
        self.send(&format!("{verb} {}", keys.join(" ")));
        self.expect("ok", SECOND * 10);
    }
}
impl Drop for Role {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "starts a private headless KWin, XWayland and real portals"]
fn plasma_paste_and_shortcuts() {
    let Some(mut nested) = Nested::start() else {
        return;
    };
    let mut driver = nested.role("driver", None, &[]);
    driver.expect("driver-ready", SECOND * 60);

    // Wayland: real GlobalShortcuts, KWin focus, RemoteDesktop keys, data control.
    let typed = nested.foot("typed");
    nested.set_clipboard("earlier clipboard");
    assert!(wait_for(SECOND * 5, || nested.clipboard() == "earlier clipboard"));
    let mut app = nested.role("app", None, &[]);
    app.expect("app-ready", SECOND * 20);
    if app.wait("desktop Bound", SECOND * 6).is_none() {
        // The desktop's first-bind confirmation; Return accepts it.
        eprintln!("answering the shortcut dialog with Return");
        driver.keys("tap", &[RETURN]);
        app.expect("desktop Bound", SECOND * 20);
    }
    app.expect(
        "paste RemoteDesktop portal with KWin focus checks",
        SECOND * 30,
    );
    let focused = {
        app.send("focused");
        app.expect("focused", SECOND * 5)
    };
    assert!(
        focused.contains("foot"),
        "foot should have focus: {focused}"
    );

    // Hold to talk: press, release, paste into foot, clipboard comes back.
    driver.keys("down", &[CONTROL, META, KEY_D]);
    app.expect("event Start", SECOND * 5);
    thread::sleep(Duration::from_millis(400));
    driver.keys("up", &[CONTROL, META, KEY_D]);
    app.expect("event Release", SECOND * 5);
    assert_eq!(app.expect("delivered 1", SECOND * 15), "Pasted");
    assert!(
        wait_for(SECOND * 5, || read(&typed) == "synthetic transcript 1"),
        "foot received {:?}",
        read(&typed)
    );
    assert!(
        wait_for(SECOND * 5, || nested.clipboard() == "earlier clipboard"),
        "clipboard is {:?}",
        nested.clipboard()
    );

    // Hands-free from the hold, finished by the hold shortcut again.
    driver.keys("down", &[CONTROL, META, KEY_D]);
    app.expect("event Start", SECOND * 5);
    driver.keys("tap", &[SPACE]);
    app.expect("event Lock", SECOND * 5);
    driver.keys("up", &[CONTROL, META, KEY_D]);
    assert!(
        app.wait("event Release", Duration::from_millis(800))
            .is_none()
    );
    driver.keys("tap", &[CONTROL, META, KEY_D]);
    app.expect("event StopLocked", SECOND * 5);
    assert_eq!(app.expect("delivered 2", SECOND * 15), "Pasted");
    assert!(
        wait_for(SECOND * 5, || read(&typed)
            == "synthetic transcript 1synthetic transcript 2"),
        "foot received {:?}",
        read(&typed)
    );

    // Cancel while hands-free: no delivery.
    driver.keys("tap", &[CONTROL, META, SPACE]);
    app.expect("event Start", SECOND * 5);
    driver.keys("tap", &[CONTROL, META, BACKSPACE]);
    app.expect("event Cancel", SECOND * 5);

    // Two transcripts finishing together each land once, and the clipboard
    // from before both comes back.
    app.send("remember");
    assert!(app.expect("remembered", SECOND * 5).contains("foot"));
    app.send("deliver-twice overlap-A overlap-B");
    assert_eq!(app.expect("delivered-twice", SECOND * 20), "Pasted Pasted");
    let text = read(&typed);
    assert!(
        text.ends_with("overlap-Aoverlap-B") || text.ends_with("overlap-Boverlap-A"),
        "foot received {text:?}"
    );
    assert!(
        wait_for(SECOND * 5, || nested.clipboard() == "earlier clipboard"),
        "clipboard is {:?}",
        nested.clipboard()
    );

    // Focus moves to another window before delivery: copy, never type.
    let other = nested.foot("other");
    app.send("deliver-remembered moved-away");
    assert_eq!(
        app.expect("delivered-remembered", SECOND * 15),
        "FocusChanged"
    );
    thread::sleep(Duration::from_millis(500));
    assert!(!read(&typed).contains("moved-away"));
    assert!(!read(&other).contains("moved-away"));
    assert_eq!(nested.clipboard(), "moved-away");
    app.send("copy copied-last");
    assert_eq!(app.expect("copied", SECOND * 5), "ok");
    assert_eq!(nested.clipboard(), "copied-last");

    // A non-text clipboard comes back byte for byte after a paste.
    let image: Vec<u8> = b"\x89PNG\r\n\x1a\nsynthetic image bytes \x00\xff"
        .iter()
        .copied()
        .chain((0..=255u8).cycle().take(50_000))
        .collect();
    let mut copy = nested
        .command("wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    copy.stdin.take().unwrap().write_all(&image).unwrap();
    assert!(copy.wait().unwrap().success());
    let paste_image = || {
        nested
            .command("timeout")
            .args(["5", "wl-paste", "--type", "image/png"])
            .output()
            .unwrap()
            .stdout
    };
    assert!(wait_for(SECOND * 5, || paste_image() == image));
    app.send("remember");
    assert!(app.expect("remembered", SECOND * 5).contains("foot"));
    app.send("deliver-remembered over-an-image");
    assert_eq!(app.expect("delivered-remembered", SECOND * 15), "Pasted");
    assert!(wait_for(SECOND * 5, || read(&other).contains("over-an-image")));
    assert!(
        wait_for(SECOND * 5, || paste_image() == image),
        "the image didn't come back"
    );
    drop(app);

    // X11 under XWayland: XInput2 shortcuts, _NET_ACTIVE_WINDOW, XTEST, X selection.
    let x11 = nested.x11_env();
    let mut target = nested.role("x11-target", Some(x11.clone()), &[]);
    target.expect("x11-target-ready", SECOND * 15);
    thread::sleep(SECOND);
    let pasted = nested.root.join("x11-pasted");
    let mut app = nested.role("app", Some(x11.clone()), &[]);
    app.expect("app-ready", SECOND * 20);
    app.expect("paste XTEST", SECOND * 5);
    app.send("focused");
    let focused = app.expect("focused", SECOND * 5);
    assert!(focused.contains("wispx-x11-target"), "{focused}");
    app.send("copy x11 earlier");
    assert_eq!(app.expect("copied", SECOND * 5), "ok");
    target.send("read");
    assert_eq!(target.expect("clipboard", SECOND * 5), "x11 earlier");
    driver.keys("down", &[CONTROL, META]);
    app.expect("event Start", SECOND * 5);
    thread::sleep(Duration::from_millis(400));
    driver.keys("up", &[CONTROL, META]);
    app.expect("event Release", SECOND * 5);
    assert_eq!(app.expect("delivered 1", SECOND * 15), "Pasted");
    assert!(
        wait_for(SECOND * 5, || read(&pasted) == "synthetic transcript 1"),
        "X11 window received {:?}",
        read(&pasted)
    );
    thread::sleep(SECOND);
    target.send("read");
    assert_eq!(target.expect("clipboard", SECOND * 5), "x11 earlier");

    // Focus moves to a Wayland window: the X11 paste is refused and copied.
    app.send("remember");
    app.expect("remembered", SECOND * 5);
    let _third = nested.foot("third");
    app.send("deliver-remembered x11-moved");
    assert_eq!(
        app.expect("delivered-remembered", SECOND * 15),
        "FocusChanged"
    );
    thread::sleep(Duration::from_millis(500));
    assert!(!read(&pasted).contains("x11-moved"));
    drop(app);

    // A shortcut another X11 client already grabbed is reported, not claimed.
    target.send("grab-ctrl-shift-d");
    target.expect("grabbed", SECOND * 5);
    let app = nested.role("app", Some(x11), &[("WISPX_NESTED_HOTKEY", "Ctrl+Shift+D")]);
    app.expect("app-ready", SECOND * 20);
    let conflict = app.expect("conflict", SECOND * 5);
    assert!(conflict.contains("Ctrl"), "{conflict}");
}

/// The processes the test above starts; does nothing when run directly.
#[test]
#[ignore = "a helper role for plasma_paste_and_shortcuts"]
fn role() {
    match std::env::var(ROLE).as_deref() {
        Ok("driver") => roles::driver(),
        Ok("app") => roles::app(),
        Ok("x11-target") => roles::x11_target(),
        _ => {}
    }
}

mod roles {
    use super::*;
    use ashpd::desktop::remote_desktop::{
        DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions, StartOptions,
    };
    use futures_lite::future;
    use std::io::BufRead;
    use wispx::{
        app::Delivery,
        hotkey::Hotkey,
        platform::linux::{
            hotkey::{HookMessage, Hotkeys},
            paste::{Paster, Target},
            session::Session,
        },
    };

    fn say(line: impl AsRef<str>) {
        println!("@ {}", line.as_ref());
        let _ = std::io::stdout().flush();
    }
    fn stdin_lines() -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        rx
    }

    /// The test's own keyboard. Pre-authorizes remote control for itself and
    /// WispX in this private session's permission store, like answering yes.
    pub fn driver() {
        future::block_on(async {
            let bus = zbus::Connection::session().await.unwrap();
            for app in [DRIVER_ID, WISPX_ID] {
                bus.call_method(
                    Some("org.freedesktop.impl.portal.PermissionStore"),
                    "/org/freedesktop/impl/portal/PermissionStore",
                    Some("org.freedesktop.impl.portal.PermissionStore"),
                    "SetPermission",
                    &("kde-authorized", true, "remote-desktop", app, vec!["yes"]),
                )
                .await
                .unwrap();
            }
            let id = ashpd::AppID::try_from(DRIVER_ID).unwrap();
            let deadline = Instant::now() + SECOND * 30;
            while let Err(error) = ashpd::register_host_app(id.clone()).await {
                assert!(Instant::now() < deadline, "register: {error}");
                async_io::Timer::after(Duration::from_millis(250)).await;
            }
            let portal = RemoteDesktop::new().await.unwrap();
            let session = portal.create_session(Default::default()).await.unwrap();
            portal
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(ashpd::enumflags2::BitFlags::from_flag(DeviceType::Keyboard)),
                )
                .await
                .unwrap()
                .response()
                .unwrap();
            portal
                .start(&session, None, StartOptions::default())
                .await
                .unwrap()
                .response()
                .unwrap();
            say("driver-ready");
            let lines = stdin_lines();
            while let Ok(line) = lines.recv() {
                let mut words = line.split_whitespace();
                let verb = words.next().unwrap_or_default();
                let keys: Vec<i32> = words
                    .map(|k| i32::from_str_radix(k.trim_start_matches("0x"), 16).unwrap())
                    .collect();
                let send = async |key: i32, state| {
                    portal
                        .notify_keyboard_keysym(&session, key, state, Default::default())
                        .await
                        .unwrap();
                    async_io::Timer::after(Duration::from_millis(40)).await;
                };
                if matches!(verb, "down" | "tap") {
                    for &key in &keys {
                        send(key, KeyState::Pressed).await;
                    }
                }
                if matches!(verb, "up" | "tap") {
                    for &key in keys.iter().rev() {
                        send(key, KeyState::Released).await;
                    }
                }
                say("ok");
            }
        });
    }

    /// WispX's shortcuts and paste, with a synthetic transcript per dictation.
    pub fn app() {
        let session = Session::detect();
        let paster = Paster::start(&session);
        let hotkey: Hotkey = std::env::var("WISPX_NESTED_HOTKEY")
            .ok()
            .and_then(|h| h.parse().ok())
            .unwrap_or_default();
        let (tx, hooks) = mpsc::channel();
        let tx = Mutex::new(tx);
        let _hotkeys = Hotkeys::start(
            &session,
            hotkey,
            Arc::new(move |message| {
                let _ = tx.lock().unwrap().send(message);
            }),
        );
        {
            let paster = paster.clone();
            thread::spawn(move || {
                let mut last = String::new();
                loop {
                    let now = paster.describe();
                    if now != last {
                        say(format!("paste {now}"));
                        last = now;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            });
        }
        let commands = stdin_lines();
        say("app-ready");
        let mut finished = 0;
        let mut remembered: Option<Target> = None;
        loop {
            if let Ok(message) = hooks.recv_timeout(Duration::from_millis(20)) {
                match message {
                    HookMessage::Desktop(status) => say(format!("desktop {status:?}")),
                    HookMessage::Conflict(message) => say(format!("conflict {message}")),
                    HookMessage::Captured { .. } => {}
                    HookMessage::Dictation(event) => {
                        say(format!("event {event:?}"));
                        use wispx::hotkey_machine::HotkeyEvent::{Release, StopLocked};
                        if matches!(event, Release | StopLocked) {
                            // As the runtime does: focus is asked for beside the
                            // recorder stopping, then the transcript is delivered.
                            let focus = {
                                let paster = paster.clone();
                                thread::spawn(move || paster.focused())
                            };
                            finished += 1;
                            let target = focus.join().ok().flatten();
                            let text = format!("synthetic transcript {finished}");
                            let delivery = paster.deliver(&text, target.as_ref());
                            say(format!("delivered {finished} {delivery:?}"));
                        }
                    }
                }
            }
            let Ok(line) = commands.try_recv() else {
                continue;
            };
            let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match verb {
                "focused" => say(format!("focused {:?}", paster.focused())),
                "remember" => {
                    remembered = paster.focused();
                    say(format!("remembered {remembered:?}"));
                }
                "deliver-remembered" => {
                    let delivery = paster.deliver(rest, remembered.as_ref());
                    say(format!("delivered-remembered {delivery:?}"));
                }
                "deliver-twice" => {
                    let workers: Vec<_> = rest
                        .split(' ')
                        .map(|text| {
                            let (paster, text, target) =
                                (paster.clone(), text.to_owned(), remembered.clone());
                            thread::spawn(move || paster.deliver(&text, target.as_ref()))
                        })
                        .collect();
                    let results: Vec<Delivery> =
                        workers.into_iter().map(|w| w.join().unwrap()).collect();
                    let words: Vec<_> = results.iter().map(|d| format!("{d:?}")).collect();
                    say(format!("delivered-twice {}", words.join(" ")));
                }
                "copy" => say(match paster.copy(rest) {
                    Ok(()) => "copied ok".to_owned(),
                    Err(error) => format!("copied {error}"),
                }),
                _ => {}
            }
        }
    }

    /// A plain X11 window that pastes CLIPBOARD into a file on Ctrl + V.
    pub fn x11_target() {
        use x11rb::{
            connection::Connection,
            protocol::{
                Event,
                xproto::{self, ConnectionExt as _, EventMask, ModMask},
            },
            wrapper::ConnectionExt as _,
        };
        let root_dir = PathBuf::from(std::env::var("WISPX_NESTED_ROOT").unwrap());
        let (conn, screen) = x11rb::connect(None).unwrap();
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            100,
            100,
            500,
            300,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            0,
            &xproto::CreateWindowAux::new()
                .background_pixel(conn.setup().roots[screen].white_pixel)
                .event_mask(EventMask::KEY_PRESS | EventMask::PROPERTY_CHANGE),
        )
        .unwrap();
        let atom = |name: &str| {
            conn.intern_atom(false, name.as_bytes())
                .unwrap()
                .reply()
                .unwrap()
                .atom
        };
        let (clipboard, utf8, pasted, peeked) = (
            atom("CLIPBOARD"),
            atom("UTF8_STRING"),
            atom("WISPX_PASTED"),
            atom("WISPX_PEEKED"),
        );
        conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_CLASS,
            xproto::AtomEnum::STRING,
            b"wispx-x11-target\0wispx-x11-target\0",
        )
        .unwrap();
        conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_NAME,
            xproto::AtomEnum::STRING,
            b"WispX X11 paste target",
        )
        .unwrap();
        conn.map_window(window).unwrap();
        conn.flush().unwrap();
        let mapping = {
            let setup = conn.setup();
            let (min, max) = (setup.min_keycode, setup.max_keycode);
            let reply = conn
                .get_keyboard_mapping(min, max - min + 1)
                .unwrap()
                .reply()
                .unwrap();
            let per = usize::from(reply.keysyms_per_keycode);
            let v = reply
                .keysyms
                .chunks(per)
                .position(|syms| syms.contains(&0x76))
                .unwrap();
            let d = reply
                .keysyms
                .chunks(per)
                .position(|syms| syms.contains(&0x64))
                .unwrap();
            (min + v as u8, min + d as u8)
        };
        let (key_v, key_d) = mapping;
        {
            let conn = conn.clone();
            let lines = stdin_lines();
            thread::spawn(move || {
                while let Ok(line) = lines.recv() {
                    match line.as_str() {
                        "read" => {
                            conn.convert_selection(
                                window,
                                clipboard,
                                utf8,
                                peeked,
                                x11rb::CURRENT_TIME,
                            )
                            .unwrap();
                        }
                        "grab-ctrl-shift-d" => {
                            for lock in [0_u16, 2, 16, 18] {
                                conn.grab_key(
                                    false,
                                    root,
                                    ModMask::CONTROL | ModMask::SHIFT | ModMask::from(lock),
                                    key_d,
                                    xproto::GrabMode::ASYNC,
                                    xproto::GrabMode::ASYNC,
                                )
                                .unwrap()
                                .check()
                                .unwrap();
                            }
                            say("grabbed");
                        }
                        _ => {}
                    }
                    conn.flush().unwrap();
                }
            });
        }
        say("x11-target-ready");
        loop {
            let event = conn.wait_for_event().unwrap();
            if let Event::KeyPress(e) = &event {
                eprintln!("x11-target key {} state {:?}", e.detail, e.state);
            }
            match event {
                Event::KeyPress(e)
                    if e.detail == key_v
                        && u16::from(e.state) & u16::from(ModMask::CONTROL) != 0 =>
                {
                    conn.convert_selection(window, clipboard, utf8, pasted, e.time)
                        .unwrap();
                    conn.flush().unwrap();
                }
                Event::SelectionNotify(e) if e.property != x11rb::NONE => {
                    let value = conn
                        .get_property(true, window, e.property, xproto::AtomEnum::ANY, 0, 1 << 20)
                        .unwrap()
                        .reply()
                        .unwrap()
                        .value;
                    let text = String::from_utf8_lossy(&value).into_owned();
                    if e.property == pasted {
                        let path = root_dir.join("x11-pasted");
                        let mut file = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .unwrap();
                        file.write_all(text.as_bytes()).unwrap();
                    } else {
                        say(format!("clipboard {text}"));
                    }
                }
                Event::SelectionNotify(_) => say("clipboard <none>"),
                _ => {}
            }
        }
    }
}
