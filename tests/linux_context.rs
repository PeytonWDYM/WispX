//! Nearby text over a real, private AT-SPI stack.
//!
//! REAL: a private session bus, headless `kwin_wayland --virtual`, the
//! accessibility bus and registry it activates, GTK 3 and GTK 4 text fields, KWin's
//! report of the focused window's process, and WispX's own `context::begin`.
//! The fields hold only fixture text. Nothing is pasted or typed.
//!
//! It starts a compositor, so it is ignored by default:
//!
//!     cargo test --test linux_context -- --ignored --nocapture --test-threads=1
//!
//! Every process runs with a cleared environment in one process group, and the
//! scratch directory is deleted afterwards. Nothing reaches the desktop session
//! the test is started from.
#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const ROLE: &str = "WISPX_CONTEXT_ROLE";
const SECRET: &str = "hunter2-fixture-secret";
const SECOND: Duration = Duration::from_secs(1);

/// Each command focuses one field, sets its text and caret, and answers `ready`.
const FIXTURE: &str = r#"
import json, sys, gi
version = sys.argv[1]
gi.require_version("Gtk", version)
from gi.repository import GLib, Gtk

def field(kind):
    if kind == "view":
        return Gtk.TextView()
    if kind == "password" and version == "4.0":
        return Gtk.PasswordEntry()
    entry = Gtk.Entry()
    if kind == "password":
        entry.set_visibility(False)
    return entry

fields = {kind: field(kind) for kind in ["entry", "password", "view"]}
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
for widget in fields.values():
    box.append(widget) if version == "4.0" else box.pack_start(widget, False, False, 0)
window = Gtk.Window(title="WispX context fixture " + version)
if version == "4.0":
    window.set_child(box)
    window.present()
else:
    window.add(box)
    window.show_all()

def apply(command):
    widget = fields[command["focus"]]
    start, end = command.get("select", [command["caret"], command["caret"]])
    if isinstance(widget, Gtk.TextView):
        buffer = widget.get_buffer()
        buffer.set_text(command["text"])
        widget.grab_focus()
        buffer.select_range(buffer.get_iter_at_offset(end), buffer.get_iter_at_offset(start))
    else:
        widget.set_text(command["text"])
        if version == "4.0":
            widget.grab_focus()
        else:
            widget.grab_focus_without_selecting()
        widget.select_region(start, end)
    GLib.timeout_add(300, lambda: print("ready", flush=True) and False)
    return False

def stdin(channel, condition):
    line = channel.readline()
    if not line:
        loop.quit()
        return False
    GLib.idle_add(apply, json.loads(line))
    return True

GLib.io_add_watch(GLib.IOChannel.unix_new(0), GLib.IO_IN | GLib.IO_HUP, stdin)
print("ready", flush=True)
loop = GLib.MainLoop()
loop.run()
"#;

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

/// A private bus and headless KWin; every process shares one process group.
struct Session {
    root: PathBuf,
    env: Vec<(String, String)>,
    group: Option<u32>,
    children: Vec<Child>,
}
impl Session {
    fn start() -> Option<Self> {
        for tool in [
            "dbus-daemon",
            "kwin_wayland",
            "python3",
            "at-spi-bus-launcher",
        ] {
            if !on_path(tool) {
                eprintln!("skipped: {tool} is not installed");
                return None;
            }
        }
        for version in ["3.0", "4.0"] {
            let probe = format!("import gi; gi.require_version('Gtk', '{version}')");
            let gtk = Command::new("python3").args(["-c", &probe]).status();
            if !gtk.is_ok_and(|s| s.success()) {
                eprintln!("skipped: PyGObject with GTK {version} is not installed");
                return None;
            }
        }
        let root = PathBuf::from(format!("/tmp/wispx-context-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "config", "data", "cache", "run"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("fixture.py"), FIXTURE).unwrap();
        let path = |dir: &str| root.join(dir).display().to_string();
        let socket = format!("wayland-wispx-context-{}", std::process::id());
        let env: Vec<(String, String)> = [
            ("HOME", path("home")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_DATA_HOME", path("data")),
            ("XDG_CACHE_HOME", path("cache")),
            ("XDG_RUNTIME_DIR", path("run")),
            ("PATH", "/usr/bin:/bin".into()),
            ("LANG", "C.UTF-8".into()),
            ("XDG_CURRENT_DESKTOP", "KDE".into()),
            ("XDG_SESSION_TYPE", "wayland".into()),
            ("WAYLAND_DISPLAY", socket.clone()),
            ("GDK_BACKEND", "wayland".into()),
            ("GSK_RENDERER", "cairo".into()),
            // GTK 4 would otherwise start portals in this session.
            ("GDK_DEBUG", "no-portals".into()),
            ("QT_QPA_PLATFORM", "wayland".into()),
            ("KWIN_COMPOSE", "Q".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let mut session = Self {
            root,
            env,
            group: None,
            children: Vec::new(),
        };
        let mut bus = session
            .command("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!(
                "--address=unix:path={}/bus",
                session.root.display()
            ))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        session.group = Some(bus.id());
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        session.children.push(bus);
        let address = address.trim().to_owned();
        assert!(address.starts_with(&format!("unix:path={}/", session.root.display())));
        session
            .env
            .push(("DBUS_SESSION_BUS_ADDRESS".into(), address));
        let kwin = session
            .command("kwin_wayland")
            .args([
                "--virtual",
                "--no-lockscreen",
                "--width",
                "1024",
                "--height",
                "768",
            ])
            .args(["--socket", &socket])
            .spawn()
            .unwrap();
        session.children.push(kwin);
        let display = session.root.join("run").join(&socket);
        assert!(
            wait_for(SECOND * 30, || display.exists()),
            "headless KWin didn't start"
        );
        // Without an accessibility bus there's no context, and asking doesn't start one.
        assert_eq!(session.capture("1", false), "pid=1 context=None");
        assert!(!session.has_owner(None, "org.a11y.Bus"));
        // A desktop session activates these through systemd, which this private
        // bus lacks, so start the real launcher and registry directly.
        let launcher = session
            .command("/usr/lib/at-spi-bus-launcher")
            .arg("--launch-immediately")
            .spawn()
            .unwrap();
        session.children.push(launcher);
        assert!(
            wait_for(SECOND * 10, || session.has_owner(None, "org.a11y.Bus")),
            "the accessibility bus didn't start"
        );
        let address = session.send(None, "/org/a11y/bus", "org.a11y.Bus.GetAddress", &[]);
        let address = address.split('"').nth(1).expect("bus address").to_owned();
        let registry = session
            .command("/usr/lib/at-spi2-registryd")
            .spawn()
            .unwrap();
        session.children.push(registry);
        assert!(
            wait_for(SECOND * 10, || {
                session.has_owner(Some(&address), "org.a11y.atspi.Registry")
            }),
            "the accessibility registry didn't start"
        );
        Some(session)
    }
    /// A `dbus-send` reply on the session bus or the bus at `address`.
    fn send(&self, address: Option<&str>, path: &str, method: &str, args: &[&str]) -> String {
        let (destination, bus) = match address {
            Some(address) => ("org.freedesktop.DBus", format!("--bus={address}")),
            None if method.starts_with("org.a11y") => ("org.a11y.Bus", "--session".into()),
            None => ("org.freedesktop.DBus", "--session".into()),
        };
        let output = self
            .command("dbus-send")
            .args([bus.as_str(), "--print-reply", "--reply-timeout=2000"])
            .arg(format!("--dest={destination}"))
            .args([path, method])
            .args(args)
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
    fn has_owner(&self, address: Option<&str>, name: &str) -> bool {
        self.send(
            address,
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            &[&format!("string:{name}")],
        )
        .contains("boolean true")
    }
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("session.log"))
            .unwrap();
        command
            .env_clear()
            .envs(self.env.clone())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .process_group(self.group.unwrap_or(0) as i32);
        command
    }
    fn fixture(&mut self, version: &str) -> Fixture {
        let mut child = self
            .command("python3")
            .arg(self.root.join("fixture.py"))
            .arg(version)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut fixture = Fixture {
            pid: child.id(),
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
        };
        self.children.push(child);
        fixture.ready();
        fixture
    }
    /// Runs the capture role in this session: `pid` is a number or `kwin`.
    fn capture(&self, pid: &str, redact: bool) -> String {
        let output = self
            .command(std::env::current_exe().unwrap().to_str().unwrap())
            .args([
                "--exact",
                "role",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, "capture")
            .env("WISPX_CONTEXT_PID", pid)
            .env("WISPX_CONTEXT_REDACT", if redact { "1" } else { "" })
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.split_once("@ ").map(|(_, rest)| rest.to_owned()))
            .unwrap_or_else(|| panic!("no report from the capture role:\n{stdout}"))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if thread::panicking() {
            let log = fs::read_to_string(self.root.join("session.log")).unwrap_or_default();
            let lines: Vec<_> = log.lines().collect();
            eprintln!("{}", lines[lines.len().saturating_sub(40)..].join("\n"));
        }
        // The bus's activated services (the accessibility bus and registry) are in
        // the same group as everything this test started.
        if let Some(group) = self.group {
            for signal in ["-TERM", "-KILL"] {
                let _ = Command::new("kill")
                    .args([signal, "--", &format!("-{group}")])
                    .stderr(Stdio::null())
                    .status();
                thread::sleep(Duration::from_millis(500));
            }
        }
        for child in &mut self.children {
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Fixture {
    pid: u32,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}
impl Fixture {
    fn ready(&mut self) {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready", "fixture didn't start");
    }
    fn set(&mut self, command: &str) {
        writeln!(self.stdin, "{command}").unwrap();
        self.ready();
    }
}

#[test]
#[ignore = "starts a private headless KWin and accessibility bus"]
fn nearby_text_from_gtk_fields() {
    let Some(mut session) = Session::start() else {
        return;
    };
    for version in ["3.0", "4.0"] {
        let mut fixture = session.fixture(version);
        let pid = fixture.pid.to_string();
        let check = |session: &Session, expected: &str| {
            // KWin's report ties the field to the window that would get the paste.
            let report = session.capture("kwin", false);
            assert_eq!(
                report,
                format!("pid={pid} context={expected:?}"),
                "GTK {version}"
            );
        };

        fixture.set(r#"{"focus": "entry", "text": "Dear team,  the release", "caret": 10}"#);
        check(&session, "Dear team,[cursor]  the release");

        fixture.set(r#"{"focus": "entry", "text": "naïve 🦀 café", "caret": 7}"#);
        check(&session, "naïve 🦀[cursor] café");

        // A selection ending at the caret will be replaced, so it's left out. GTK 4
        // entries report neither the selection nor its end as the caret, only its
        // start, so the selected word is still read as text after the caret.
        fixture.set(
            r#"{"focus": "entry", "text": "Hello big world", "caret": 10, "select": [6, 10]}"#,
        );
        check(
            &session,
            if version == "3.0" {
                "Hello [cursor]world"
            } else {
                "Hello [cursor]big world"
            },
        );

        fixture
            .set(r#"{"focus": "view", "text": "First line\n  second ✓ line\nthird", "caret": 14}"#);
        check(&session, "First line\n  s[cursor]econd ✓ line\nthird");

        let long = format!("{}|{}", "a".repeat(3_000), "b".repeat(3_000));
        fixture.set(&format!(
            r#"{{"focus": "view", "text": "{long}", "caret": 3000}}"#
        ));
        let report = session.capture(&pid, false);
        let context = report.split_once("context=").unwrap().1;
        let expected = format!(
            "{}[cursor]{}",
            "a".repeat(1_500),
            "|".to_owned() + &"b".repeat(491)
        );
        assert_eq!(context, format!("{expected:?}"), "GTK {version}: bounded");

        fixture.set(&format!(
            r#"{{"focus": "password", "text": "{SECRET}", "caret": 4}}"#
        ));
        assert_eq!(
            session.capture(&pid, true),
            format!("pid={pid} context=none"),
            "GTK {version}: password"
        );
        assert_eq!(
            session.capture("kwin", true),
            format!("pid={pid} context=none")
        );

        // Another process, even with the same text, isn't the target.
        fixture.set(r#"{"focus": "entry", "text": "Dear team", "caret": 4}"#);
        let other = (fixture.pid + 100_000).to_string();
        assert_eq!(
            session.capture(&other, false),
            format!("pid={other} context=None")
        );
    }
    // The GTK 4 window opened last, so the GTK 3 one is no longer active: its
    // still-focused field must not supply text for the window that has focus now.
    let first = session
        .children
        .iter()
        .rev()
        .nth(1)
        .unwrap()
        .id()
        .to_string();
    assert_eq!(
        session.capture(&first, false),
        format!("pid={first} context=None")
    );
}

/// The capture role; does nothing when run directly.
#[test]
#[ignore = "a helper role for nearby_text_from_gtk_fields"]
fn role() {
    use wispx::platform::linux::{context, kwin};
    if std::env::var(ROLE).as_deref() != Ok("capture") {
        return;
    }
    let pid = match std::env::var("WISPX_CONTEXT_PID").unwrap().as_str() {
        "kwin" => kwin::active_window().unwrap().unwrap().pid.unwrap(),
        pid => pid.parse().unwrap(),
    };
    let context = context::begin(pid).and_then(context::Capture::finish);
    if std::env::var("WISPX_CONTEXT_REDACT").is_ok_and(|v| !v.is_empty()) {
        let found = if context.is_some() { "present" } else { "none" };
        println!("@ pid={pid} context={found}");
    } else {
        let found = context.map_or("None".into(), |text| format!("{text:?}"));
        println!("@ pid={pid} context={found}");
    }
}
