//! The desktop app itself, in a private headless Plasma session.
//!
//! REAL: `wispx-gui` and `wispx`, `kwin_wayland --virtual`, a real plasmashell with
//! its panel and system tray, the portals and their consent dialogs, wl-copy and
//! foot as the window being dictated into. Keys and clicks really travel through
//! KWin, from a RemoteDesktop session of the test's own. What is on screen is read
//! from the compositor, and what the windows believe is checked against it.
//! MOCKED: only the microphone and the transcription, which are scripts, except
//! in the last test, which runs real whisper.cpp on a public recording.
//!
//!     env -u DBUS_SESSION_BUS_ADDRESS -u WAYLAND_DISPLAY -u DISPLAY \
//!         cargo test --test gui_nested -- --ignored --nocapture --test-threads=1
//!
//! Every process has a cleared environment with a scratch HOME, XDG directories
//! and session bus, and connects only to the private compositor. Screenshots of
//! real states go to /tmp/wispx-ui-review/.

#[allow(dead_code)]
#[path = "support/nested.rs"]
mod nested;

use nested::*;
use serde_json::Value as Json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use wispx::dbus::{Connection, Value};

const REVIEW: &str = "/tmp/wispx-ui-review";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const KEY_A: i32 = 0x61;
const KEY_B: i32 = 0x62;
const HOLD: [i32; 3] = [CONTROL, META, KEY_D];
const HANDS_FREE: [i32; 3] = [CONTROL, META, SPACE];

fn app_command(nested: &Nested, command: &str) -> Option<Json> {
    let stream = UnixStream::connect(nested.root.join("run/wispx/app.sock")).ok()?;
    stream.set_read_timeout(Some(SECOND * 20)).ok()?;
    let mut writer = &stream;
    writer.write_all(format!("{command}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn inspect(nested: &Nested) -> Json {
    app_command(nested, "inspect").expect("the app did not answer")
}

/// A microphone that hears something changing, so the bars have a real level to
/// follow, delivered in real time for as long as it is recorded.
fn mock_mic(nested: &Nested) {
    let r = nested.root.display();
    let chunk = |amplitude: i16| -> Vec<u8> {
        (0..1600)
            .flat_map(|i| (if i % 2 == 0 { amplitude } else { -amplitude }).to_le_bytes())
            .collect()
    };
    fs::write(nested.root.join("loud.raw"), chunk(14_000)).unwrap();
    fs::write(nested.root.join("soft.raw"), chunk(5_000)).unwrap();
    nested.script(
        "pw-record",
        &format!(
            "while :; do cat {r}/loud.raw; sleep 0.1; cat {r}/loud.raw; sleep 0.1; \
             cat {r}/soft.raw; sleep 0.1; done"
        ),
    );
}

/// Transcription that can be told to take its time, or to fail.
fn mock_whisper(nested: &Nested) {
    let r = nested.root.display();
    nested.script(
        "whisper-cli",
        &format!(
            r#"[ -f {r}/slow ] && sleep 3
[ -f {r}/fail ] && {{ echo 'the model could not be read' >&2; exit 1; }}
echo x >> {r}/transcriptions
echo ' {TRANSCRIPT}'"#
        ),
    );
}

fn session(panel: bool) -> Option<Nested> {
    session_on_screens(panel, &[])
}

fn session_on_screens(panel: bool, screens: &[&str]) -> Option<Nested> {
    let mut nested = Nested::start_for_pictures(screens)?;
    require("KWin on the bus", SECOND * 20, || {
        nested
            .connect()
            .name_has_owner("org.kde.KWin", SECOND * 2)
            .unwrap_or(false)
    });
    nested.fake_audio();
    mock_mic(&nested);
    mock_whisper(&nested);
    if panel {
        let log = fs::File::create(nested.root.join("plasma.log")).unwrap();
        let shell = nested
            .command("plasmashell")
            .arg("--no-respawn")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        nested.windows.push(shell);
        require("the panel", SECOND * 30, || {
            nested.compositor()["work_area"]["height"]
                .as_i64()
                .is_some_and(|height| height < 800)
        });
    }
    Some(nested)
}

fn picture(nested: &Nested, name: &str, what_it_shows: &str) {
    fs::create_dir_all(REVIEW).unwrap();
    // The compositor's fade of a window that just closed isn't part of the picture.
    thread::sleep(Duration::from_millis(350));
    let path = Path::new(REVIEW).join(format!("{name}.png"));
    assert!(nested.screenshot(&path), "no screenshot for {name}");
    eprintln!("picture {}: {what_it_shows}", path.display());
    note_picture(name, what_it_shows);
    // The pill at a size a person can judge it.
    if let Some(window) = pill_window(&nested.compositor()) {
        let crop = format!(
            "{}x{}+{}+{}",
            window["width"], window["height"], window["x"], window["y"]
        );
        let closeup = Path::new(REVIEW).join(format!("{name}-pill.png"));
        let _ = Command::new("magick")
            .arg(&path)
            .args([
                "-crop", &crop, "+repage", "-filter", "point", "-resize", "200%",
            ])
            .arg(&closeup)
            .stderr(Stdio::null())
            .status();
    }
}

/// What each picture shows, for whoever looks through them afterwards.
fn note_picture(name: &str, what_it_shows: &str) {
    let path = Path::new(REVIEW).join("INDEX.txt");
    let heading = "None of these are demos. REAL means the real app, daemon, compositor, panel \
                   and portals, with the state the daemon reported at that moment. The microphone \
                   and whisper-cli are stand-ins unless a line says otherwise.\n\n";
    let before = fs::read_to_string(&path).unwrap_or_else(|_| heading.to_string());
    let prefix = format!("{name}.png:");
    let mut lines: Vec<String> = before
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(str::to_string)
        .collect();
    lines.push(format!("{prefix} {what_it_shows}"));
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn pill_window(dump: &Json) -> Option<&Json> {
    dump["windows"]
        .as_array()?
        .iter()
        .find(|window| window["name"] == "wispx-gui" && window["skip_taskbar"] == true)
}

fn window_of_class<'a>(dump: &'a Json, class: &str) -> Option<&'a Json> {
    dump["windows"]
        .as_array()?
        .iter()
        .find(|window| window["class"] == class)
}

/// The compositor lists its windows from the bottom up.
fn pill_is_above(dump: &Json, class: &str) -> bool {
    let windows = dump["windows"].as_array().unwrap();
    let place = |wanted: &dyn Fn(&Json) -> bool| windows.iter().position(wanted);
    let pill = place(&|window| window["name"] == "wispx-gui" && window["skip_taskbar"] == true);
    let other = place(&|window| window["class"] == class);
    pill.unwrap() > other.unwrap()
}

fn settings_window(dump: &Json) -> Option<&Json> {
    dump["windows"]
        .as_array()?
        .iter()
        .find(|window| window["class"] == WISPX_ID && window["skip_taskbar"] == false)
}

fn window_captioned<'a>(dump: &'a Json, caption: &str) -> Option<&'a Json> {
    dump["windows"]
        .as_array()?
        .iter()
        .find(|window| window["caption"].as_str().unwrap_or("").contains(caption))
}

#[track_caller]
fn wait_for_active_caption(nested: &Nested, caption: &str) {
    require(&format!("{caption} to have focus"), SECOND * 25, || {
        window_captioned(&nested.compositor(), caption).is_some_and(|w| w["active"] == true)
    });
}

/// Waits for the pill to reach a size, which only the real state can have asked for.
#[track_caller]
fn wait_for_pill(nested: &Nested, width: f64, height: f64) {
    let close =
        |value: &Json, target: f64| value.as_f64().is_some_and(|v| (v - target).abs() < 1.0);
    require(&format!("a {width}x{height} pill"), SECOND * 15, || {
        let state = inspect(nested);
        let pill = &state["windows"]["overlay"]["items"]["pill"];
        close(&pill["width"], width) && close(&pill["height"], height)
    });
}

/// The window the words should go to must never lose focus to the pill.
#[track_caller]
fn assert_focus_stays(nested: &Nested, target_class: &str) {
    let dump = nested.compositor();
    let pill = pill_window(&dump).expect("the pill is not mapped");
    assert_eq!(pill["active"], false, "the pill took focus: {pill}");
    assert_eq!(pill["skip_taskbar"], true, "{pill}");
    assert_eq!(pill["skip_switcher"], true, "{pill}");
    let active = dump["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["active"] == true)
        .unwrap_or_else(|| panic!("nothing has focus: {dump}"));
    assert_eq!(active["class"], target_class, "focus moved to {active}");
}

/// The pill sits centered in the work area, clear of the panel.
#[track_caller]
fn assert_placed_in_work_area(nested: &Nested) {
    let dump = nested.compositor();
    let pill = pill_window(&dump).expect("the pill is not mapped");
    let area = &dump["work_area"];
    let number = |value: &Json| value.as_i64().unwrap();
    assert_eq!(
        number(&pill["x"]) + number(&pill["width"]) / 2,
        number(&area["x"]) + number(&area["width"]) / 2,
        "not centered: {pill} in {area}"
    );
    assert_eq!(
        number(&pill["y"]) + number(&pill["height"]),
        number(&area["y"]) + number(&area["height"]),
        "not on the work area's bottom edge: {pill} in {area}"
    );
}

/// Clicks a named control with the real pointer, scrolling to it first.
fn click_item(nested: &Nested, keyboard: &mut Keyboard, name: &str) {
    let dump = nested.compositor();
    let window = settings_window(&dump)
        .unwrap_or_else(|| panic!("no settings window: {dump}"))
        .clone();
    let number = |value: &Json| value.as_f64().unwrap();
    let (left, top) = (number(&window["client_x"]), number(&window["client_y"]));
    let height = number(&window["height"]);
    keyboard.move_pointer(left + 200.0, top + 200.0);
    let mut item = Json::Null;
    for _ in 0..30 {
        let state = inspect(nested);
        item = state["windows"]["settings"]["items"][name].clone();
        assert!(item.is_object(), "no control called {name}: {state}");
        let middle = number(&item["y"]) + number(&item["height"]) / 2.0;
        if middle < 30.0 {
            keyboard.scroll(-3);
        } else if middle > height - 60.0 {
            keyboard.scroll(3);
        } else {
            break;
        }
    }
    let x = left + number(&item["x"]) + number(&item["width"]) / 2.0;
    let y = top + number(&item["y"]) + number(&item["height"]) / 2.0;
    keyboard.click(x, y);
    let cursor = nested.compositor()["cursor"].clone();
    let off = (number(&cursor["x"]) - x).abs() + (number(&cursor["y"]) - y).abs();
    assert!(off < 4.0, "the pointer went to {cursor}, not {x},{y}");
}

fn config(nested: &Nested) -> Json {
    let path = nested.root.join("config/wispx/config.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Reads a D-Bus property as a string or list of strings.
fn property(bus: &mut Connection, service: &str, path: &str, interface: &str, name: &str) -> Value {
    let reply = bus
        .call(
            service,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            vec![Value::str(interface), Value::str(name)],
            SECOND * 5,
        )
        .unwrap();
    match reply.into_iter().next() {
        Some(Value::Variant(inner)) => *inner,
        other => panic!("{name}: {other:?}"),
    }
}

struct TrayItem {
    service: String,
    menu: String,
}

/// Finds this app's icon the way a panel does: through the real watcher.
fn find_tray_item(nested: &Nested) -> Option<TrayItem> {
    let mut bus = nested.connect();
    let Value::Array { items, .. } = property(
        &mut bus,
        WATCHER,
        "/StatusNotifierWatcher",
        WATCHER,
        "RegisteredStatusNotifierItems",
    ) else {
        return None;
    };
    for item in items {
        let text = item.as_str()?.to_string();
        let (service, tail) = text.split_once('/')?;
        let path = format!("/{tail}");
        let interface = "org.kde.StatusNotifierItem";
        let title = property(&mut bus, service, &path, interface, "Title");
        if title.as_str() != Some("WispX") {
            continue;
        }
        let menu = property(&mut bus, service, &path, interface, "Menu");
        return Some(TrayItem {
            service: service.to_string(),
            menu: menu.as_str()?.to_string(),
        });
    }
    None
}

/// The entries a panel would show, as (id, label, enabled, toggled).
fn tray_menu(nested: &Nested, item: &TrayItem) -> Vec<(i32, String, bool, bool)> {
    let mut bus = nested.connect();
    let reply = bus
        .call(
            &item.service,
            &item.menu,
            "com.canonical.dbusmenu",
            "GetLayout",
            vec![
                Value::Int32(0),
                Value::Int32(-1),
                Value::Array {
                    element: "s".to_string(),
                    items: Vec::new(),
                },
            ],
            SECOND * 5,
        )
        .unwrap();
    let Some(Value::Struct(root)) = reply.get(1) else {
        panic!("unexpected layout: {reply:?}");
    };
    let Some(Value::Array {
        items: children, ..
    }) = root.get(2)
    else {
        panic!("no children: {root:?}");
    };
    children
        .iter()
        .filter_map(|child| {
            let Value::Variant(child) = child else {
                return None;
            };
            let Value::Struct(fields) = child.as_ref() else {
                return None;
            };
            let id = fields.first()?.as_i32()?;
            let properties = fields.get(1)?;
            let read = |key: &str| match properties.lookup(key) {
                Some(Value::Variant(inner)) => Some(inner.as_ref().clone()),
                Some(other) => Some(other.clone()),
                None => None,
            };
            let label = read("label")
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default();
            let enabled = !matches!(read("enabled"), Some(Value::Bool(false)));
            let toggled = matches!(read("toggle-state"), Some(Value::Int32(1)));
            Some((id, label, enabled, toggled))
        })
        .filter(|entry| !entry.1.is_empty())
        .collect()
}

fn click_tray_entry(nested: &Nested, item: &TrayItem, label: &str) {
    let menu = tray_menu(nested, item);
    let entry = menu
        .iter()
        .find(|entry| entry.1 == label)
        .unwrap_or_else(|| panic!("no {label} in {menu:?}"));
    let mut bus = nested.connect();
    bus.call(
        &item.service,
        &item.menu,
        "com.canonical.dbusmenu",
        "Event",
        vec![
            Value::Int32(entry.0),
            Value::str("clicked"),
            Value::Variant(Box::new(Value::Int32(0))),
            Value::Uint32(0),
        ],
        SECOND * 5,
    )
    .unwrap();
}

/// Starts it from a terminal that then goes away: a process group of its own that
/// gets the hangup a closing terminal sends.
fn launch_and_close_terminal(nested: &Nested) {
    use std::os::unix::process::CommandExt;
    let mut launcher = nested
        .command(env!("CARGO_BIN_EXE_wispx"))
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let group = launcher.id();
    let deadline = std::time::Instant::now() + SECOND * 25;
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the launcher did not return"
        );
        thread::sleep(Duration::from_millis(30));
    };
    assert!(status.success(), "launch failed: {status}");
    let _ = Command::new("kill")
        .args(["-HUP", "--", &format!("-{group}")])
        .stderr(Stdio::null())
        .status();
    thread::sleep(Duration::from_millis(300));
    assert!(
        app_command(nested, "ping").is_some(),
        "the app went with its terminal\n{}",
        nested.logs()
    );
}

fn answer_shortcuts_dialog(nested: &Nested, keyboard: &mut Keyboard) {
    wait_for_active_caption(nested, "Global Shortcuts");
    keyboard.tap(&[RETURN]);
    require("shortcuts to bind", SECOND * 20, || {
        nested.status()["hotkey_state"] == "bound"
    });
}

#[test]
#[ignore = "starts a private headless KWin, plasmashell and real portals"]
fn the_whole_app_as_a_person_uses_it() {
    let Some(mut nested) = session(true) else {
        return;
    };
    let mut keyboard = Keyboard::open(&nested);

    // Someone coming from an earlier build has an entry that's hidden from the
    // menu and only starts the daemon.
    let entry_path = nested
        .root
        .join(format!("data/applications/{WISPX_ID}.desktop"));
    fs::create_dir_all(entry_path.parent().unwrap()).unwrap();
    fs::write(
        &entry_path,
        "[Desktop Entry]\nType=Application\nName=WispX\nExec=\"/old/wispx\" daemon --quiet\n\
         Icon=audio-input-microphone\nNoDisplay=true\n",
    )
    .unwrap();

    // Opening it: no daemon to start, no terminal to keep open, and the desktop
    // entry and icon are right before anything asks for them.
    launch_and_close_terminal(&nested);
    let entry = fs::read_to_string(&entry_path).unwrap();
    assert!(!entry.contains("NoDisplay"), "{entry}");
    assert!(!entry.contains("/old/wispx"), "{entry}");
    assert!(entry.contains(" app\n"), "{entry}");
    require("the background service", SECOND * 10, || {
        nested.wispx(&["status"]).0
    });

    // First run: the desktop's question comes first, with nothing on top of it.
    wait_for_active_caption(&nested, "Global Shortcuts");
    assert!(
        settings_window(&nested.compositor()).is_none(),
        "the settings window covers the question"
    );
    answer_shortcuts_dialog(&nested, &mut keyboard);

    // Then what's left to do is on screen, and a second start finds it.
    require("the settings window", SECOND * 15, || {
        settings_window(&nested.compositor()).is_some()
    });
    assert_placed_in_work_area(&nested);
    assert_eq!(inspect(&nested)["owns_daemon"], true);
    wait_for_active_caption(&nested, "WispX");
    require("the checks", SECOND * 10, || {
        inspect(&nested)["settings"]["rows"][3]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Not allowed yet"))
    });

    let state = inspect(&nested);
    eprintln!("settings on first run: {:#}", state["settings"]["rows"]);
    assert_eq!(state["settings"]["shortcuts"][0]["keys"], "Meta+Ctrl+D");
    assert_eq!(state["windows"]["tray"]["available"], true, "{state}");
    assert_eq!(state["settings"]["summary"]["level"], "warn");
    picture(
        &nested,
        "1-first-run-settings",
        "REAL: first-run settings beside a real panel and tray, paste not allowed yet",
    );

    // The real button, the real consent dialog.
    click_item(&nested, &mut keyboard, "button-enable-paste");
    wait_for_active_caption(&nested, "Remote");
    picture(
        &nested,
        "2-paste-consent",
        "REAL: the desktop's own consent dialog, from the button",
    );
    keyboard.tap(&[RETURN]);
    require("automatic paste", SECOND * 20, || {
        nested.status()["paste_state"] == "automatic"
    });
    require("the settings window to agree", SECOND * 10, || {
        inspect(&nested)["settings"]["summary"]["level"] == "ok"
    });
    let token = nested.root.join("data/wispx/remote-desktop-token");
    assert!(
        !fs::read_to_string(&token).unwrap().trim().is_empty(),
        "consent left no restore token"
    );
    assert!(
        inspect(&nested)["settings"]["rows"][3]["buttons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    wait_for_active_caption(&nested, "WispX");
    thread::sleep(Duration::from_millis(400));
    picture(&nested, "3-ready-settings", "REAL: everything set up");

    // The real settings: a switch clicked with the real pointer changes the file,
    // and the daemon's report comes back into the window.
    assert_eq!(config(&nested)["sounds"], false);
    click_item(&nested, &mut keyboard, "sounds");
    require("sounds to be saved", SECOND * 10, || {
        config(&nested)["sounds"] == true
    });
    require("the daemon to agree", SECOND * 10, || {
        inspect(&nested)["settings"]["sounds"] == true
    });

    // Dictating into another window.
    let typed = nested.open_target("typed");
    wait_for_active_caption(&nested, "foot");
    let target = window_captioned(&nested.compositor(), "foot")
        .map(|window| window["class"].as_str().unwrap().to_string())
        .unwrap();
    require("the pill to rest", SECOND * 15, || {
        inspect(&nested)["overlay"]["message"] == ""
    });
    wait_for_pill(&nested, 44.0, 8.0);
    assert_focus_stays(&nested, &target);
    assert_placed_in_work_area(&nested);
    thread::sleep(Duration::from_millis(500));
    let opacity = inspect(&nested)["windows"]["overlay"]["items"]["pill"]["opacity"]
        .as_f64()
        .unwrap();
    assert!((opacity - 0.55).abs() < 0.02, "idle opacity {opacity}");
    picture(&nested, "4-idle", "REAL: idle pill, foot has focus");

    // At rest the pill stays out of a video's way, and comes forward to speak.
    nested.set_active_fullscreen(true);
    require("the target to fill the screen", SECOND * 5, || {
        let dump = nested.compositor();
        window_of_class(&dump, &target).is_some_and(|window| window["fullscreen"] == true)
    });
    require("the pill to be behind it", SECOND * 5, || {
        !pill_is_above(&nested.compositor(), &target)
    });
    assert_focus_stays(&nested, &target);
    picture(
        &nested,
        "13-fullscreen-idle",
        "REAL: a fullscreen window covers the idle pill",
    );
    nested.ok(&["start"]);
    nested.wait_for_state("recording-hands-free");
    require("the pill to come forward", SECOND * 5, || {
        pill_is_above(&nested.compositor(), &target)
    });
    assert_focus_stays(&nested, &target);
    picture(
        &nested,
        "14-fullscreen-recording",
        "REAL: the pill above a fullscreen window while recording",
    );
    nested.ok(&["cancel"]);
    nested.wait_for_state("idle");
    nested.set_active_fullscreen(false);
    require("the target to stop filling the screen", SECOND * 5, || {
        let dump = nested.compositor();
        window_of_class(&dump, &target).is_some_and(|window| window["fullscreen"] == false)
    });
    assert_focus_stays(&nested, &target);

    // Hold to talk: real level, real focus, real paste after release.
    keyboard.down(&HOLD);
    nested.wait_for_state("recording");
    wait_for_pill(&nested, 156.0, 38.0);
    assert_focus_stays(&nested, &target);
    assert_placed_in_work_area(&nested);
    require("the bars to follow the microphone", SECOND * 5, || {
        let state = inspect(&nested);
        let overlay = &state["windows"]["overlay"];
        overlay["level"].as_f64().unwrap_or(0.0) > 0.05
            && overlay["items"]["bar-7"]["height"].as_f64().unwrap_or(0.0) > 8.0
    });
    let state = inspect(&nested);
    let middle = state["windows"]["overlay"]["items"]["bar-7"]["height"]
        .as_f64()
        .unwrap();
    let edge = state["windows"]["overlay"]["items"]["bar-0"]["height"]
        .as_f64()
        .unwrap();
    assert!(middle > edge, "the bars have no shape: {middle} vs {edge}");
    picture(
        &nested,
        "5-recording",
        "REAL: recording, bars driven by the mock mic's level",
    );
    keyboard.up(&HOLD);
    require("the paste", SECOND * 15, || read(&typed) == TRANSCRIPT);
    nested.wait_for_state("idle");
    assert_focus_stays(&nested, &target);
    eprintln!("hold to talk pasted into foot: {:?}", read(&typed));

    // Hands-free: the cursor stays in the target, so typing still reaches it.
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("recording-hands-free");
    wait_for_pill(&nested, 176.0, 38.0);
    assert_focus_stays(&nested, &target);
    let state = inspect(&nested);
    assert_eq!(
        state["windows"]["overlay"]["items"]["lock"]["visible"],
        true
    );
    assert_eq!(
        state["windows"]["overlay"]["items"]["rec-dot"]["visible"],
        true
    );
    keyboard.tap(&[KEY_A]);
    keyboard.tap(&[KEY_B]);
    require("the typing to arrive", SECOND * 5, || {
        read(&typed) == format!("{TRANSCRIPT}ab")
    });
    picture(
        &nested,
        "6-hands-free",
        "REAL: hands-free, lock and pulsing dot",
    );

    // Transcribing: still focused, and the words arrive afterwards.
    fs::write(nested.root.join("slow"), b"").unwrap();
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("transcribing");
    wait_for_pill(&nested, 120.0, 38.0);
    assert_focus_stays(&nested, &target);
    thread::sleep(Duration::from_millis(400));
    picture(
        &nested,
        "7-transcribing",
        "REAL: transcribing, traveling wave and glow",
    );
    require("the second paste", SECOND * 20, || {
        read(&typed) == format!("{TRANSCRIPT}ab{TRANSCRIPT}")
    });
    fs::remove_file(nested.root.join("slow")).unwrap();
    nested.wait_for_state("idle");
    wait_for_pill(&nested, 44.0, 8.0);

    // A failure says so where the person is looking, then gets out of the way.
    fs::write(nested.root.join("fail"), b"").unwrap();
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("recording-hands-free");
    thread::sleep(Duration::from_millis(500));
    keyboard.tap(&HANDS_FREE);
    require("the error", SECOND * 15, || {
        inspect(&nested)["overlay"]["kind"] == "error"
    });
    thread::sleep(Duration::from_millis(500));
    let state = inspect(&nested);
    eprintln!("error in the pill: {}", state["overlay"]["message"]);
    assert!(
        state["overlay"]["message"]
            .as_str()
            .unwrap()
            .contains("model could not be read")
    );
    assert_eq!(
        state["windows"]["overlay"]["items"]["message"]["opacity"],
        1.0
    );
    assert_focus_stays(&nested, &target);
    picture(
        &nested,
        "8-error",
        "REAL: transcription failed, red text in the expanded pill",
    );
    require("the pill to go back to rest", SECOND * 10, || {
        inspect(&nested)["overlay"]["message"] == ""
    });
    wait_for_pill(&nested, 44.0, 8.0);
    fs::remove_file(nested.root.join("fail")).unwrap();

    // Switching windows before the text is ready: it waits on the clipboard, and
    // the pill says so without ever touching the new window.
    let before = read(&typed);
    fs::write(nested.root.join("slow"), b"").unwrap();
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("recording-hands-free");
    thread::sleep(Duration::from_millis(400));
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("transcribing");
    let other = nested.open_target("other");
    require("the warning", SECOND * 15, || {
        inspect(&nested)["overlay"]["kind"] == "warn"
    });
    thread::sleep(Duration::from_millis(500));
    picture(
        &nested,
        "9-warning",
        "REAL: focus changed during transcribing, text kept on the clipboard",
    );
    assert_eq!(read(&typed), before, "the old window got the text");
    assert_eq!(read(&other), "", "the new window got the text");
    assert_eq!(nested.clipboard(), TRANSCRIPT);
    fs::remove_file(nested.root.join("slow")).unwrap();
    require("the pill to go back to rest", SECOND * 10, || {
        inspect(&nested)["overlay"]["message"] == ""
    });

    // Turning off the idle pill unmaps it, as the compositor sees it.
    let item = find_tray_item(&nested).expect("the app has no tray icon");
    click_tray_entry(&nested, &item, "Show idle pill");
    require("the idle pill to go", SECOND * 10, || {
        pill_window(&nested.compositor()).is_none()
    });
    assert_eq!(config(&nested)["idle_pill"], false);
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("recording-hands-free");
    require("the pill to come back when needed", SECOND * 10, || {
        pill_window(&nested.compositor()).is_some()
    });
    wait_for_pill(&nested, 176.0, 38.0);
    keyboard.tap(&[CONTROL, META, BACKSPACE]);
    nested.wait_for_state("idle");
    require("the pill to go again", SECOND * 10, || {
        pill_window(&nested.compositor()).is_none()
    });
    click_tray_entry(&nested, &item, "Show idle pill");
    require("the idle pill to return", SECOND * 10, || {
        pill_window(&nested.compositor()).is_some()
    });

    // The tray, as a panel reads it.
    let menu = tray_menu(&nested, &item);
    eprintln!("tray menu: {menu:?}");
    let labels: Vec<&str> = menu.iter().map(|entry| entry.1.as_str()).collect();
    assert_eq!(
        labels,
        [
            "WispX · hold Meta+Ctrl+D",
            "Ready",
            "Copy last transcript",
            "Sounds",
            "Show idle pill",
            "Start at login",
            "Settings…",
            "How to use",
            "Quit",
        ]
    );
    let entry = |label: &str| menu.iter().find(|entry| entry.1 == label).unwrap();
    assert!(!entry("Ready").2, "the status line is not a button");
    assert!(
        entry("Copy last transcript").2,
        "there is a transcript to copy"
    );
    assert!(entry("Sounds").3, "sounds were turned on above");
    assert!(!entry("Start at login").3);

    nested.set_clipboard("something else");
    click_tray_entry(&nested, &item, "Copy last transcript");
    require("the transcript on the clipboard", SECOND * 10, || {
        nested.clipboard() == TRANSCRIPT
    });

    click_tray_entry(&nested, &item, "Start at login");
    let autostart = nested
        .root
        .join(format!("config/autostart/{WISPX_ID}.desktop"));
    require("autostart", SECOND * 5, || autostart.exists());
    let text = fs::read_to_string(&autostart).unwrap();
    assert!(text.contains(" app --background"), "{text}");
    click_tray_entry(&nested, &item, "Start at login");
    require("autostart to go", SECOND * 5, || !autostart.exists());

    // Opening it again finds the running app and shows its window, on top.
    let first_pid = app_pid(&nested);
    let window = settings_window(&nested.compositor()).unwrap().clone();
    assert_eq!(window["active"], false, "foot has focus before this");
    let (ok, said) = nested.wispx(&[]);
    assert!(ok, "second launch: {said}");
    assert_eq!(app_pid(&nested), first_pid, "a second copy started");
    require("settings to come up", SECOND * 10, || {
        let dump = nested.compositor();
        let on_top = dump["windows"]
            .as_array()
            .unwrap()
            .iter()
            .rfind(|window| window["skip_taskbar"] == false && window["normal"] == true)
            .cloned();
        on_top.is_some_and(|window| window["class"] == WISPX_ID)
    });

    // Quitting from the tray stops what the app started, and nothing else remains.
    nested.ok(&["status"]);
    keyboard.tap(&HANDS_FREE);
    nested.wait_for_state("recording-hands-free");
    require("the recorder", SECOND * 5, || nested.running("pw-record"));
    click_tray_entry(&nested, &item, "Quit");
    require("the app to quit", SECOND * 15, || {
        !nested.running("wispx-gui")
    });
    assert!(!nested.running("--app-child"), "its daemon was left behind");
    assert!(!nested.running("pw-record"), "the recorder was left behind");
    assert!(!nested.wispx(&["status"]).0, "something still answers");
    assert!(app_command(&nested, "ping").is_none());
    assert_eq!(
        read(&other),
        "",
        "quitting during a recording pasted something"
    );
    drop(keyboard);
    nested.windows.clear();
}

fn app_pid(nested: &Nested) -> u32 {
    *nested
        .pids("wispx-gui")
        .first()
        .expect("the app is not running")
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn the_pill_follows_the_screen_in_use_at_any_scale() {
    let Some(mut nested) = session_on_screens(false, &["--output-count", "2", "--scale", "2"])
    else {
        return;
    };
    let mut keyboard = Keyboard::open(&nested);
    let (ok, said) = nested.wispx(&["app", "--background"]);
    assert!(ok, "launch: {said}");
    answer_shortcuts_dialog(&nested, &mut keyboard);
    nested.open_target("typed");
    wait_for_active_caption(&nested, "foot");
    let target = window_captioned(&nested.compositor(), "foot")
        .map(|window| window["class"].as_str().unwrap().to_string())
        .unwrap();

    let dictate_and_measure = |nested: &Nested| {
        nested.ok(&["start"]);
        nested.wait_for_state("recording-hands-free");
        wait_for_pill(nested, 176.0, 38.0);
        // A window that has just closed stays listed while it fades.
        require("only the new pill to remain", SECOND * 5, || {
            let dump = nested.compositor();
            let windows = dump["windows"].as_array().unwrap();
            windows.iter().filter(|w| w["name"] == "wispx-gui").count() == 1
        });
        assert_focus_stays(nested, &target);
        assert_placed_in_work_area(nested);
        let dump = nested.compositor();
        let pill = pill_window(&dump).unwrap();
        let center = pill["x"].as_i64().unwrap() + pill["width"].as_i64().unwrap() / 2;
        (center, dump["work_area"].clone())
    };

    let (first_center, first_area) = dictate_and_measure(&nested);
    nested.ok(&["cancel"]);
    nested.wait_for_state("idle");

    nested.send_active_to_screen(1);
    require("the target to move", SECOND * 5, || {
        nested.compositor()["work_area"] != first_area
    });
    let (second_center, second_area) = dictate_and_measure(&nested);
    assert_ne!(first_area, second_area);
    assert!(
        second_center > first_center,
        "the pill stayed on the first screen: {first_center} then {second_center}"
    );
    let left = second_area["x"].as_i64().unwrap();
    let width = second_area["width"].as_i64().unwrap();
    assert!((left..left + width).contains(&second_center));

    // At twice the scale, the pill has the same size in the original's units.
    let state = inspect(&nested);
    assert_eq!(state["windows"]["overlay"]["width"], 500);
    picture(
        &nested,
        "17-second-screen-scaled",
        "REAL: two screens at scale 2, dictating on the second",
    );
    nested.ok(&["cancel"]);
    nested.wait_for_state("idle");
    assert_eq!(app_command(&nested, "quit").unwrap()["ok"], true);
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn sending_audio_online_needs_a_yes_from_the_window() {
    let Some(nested) = session(false) else {
        return;
    };
    let mut keyboard = Keyboard::open(&nested);
    let config_path = nested.root.join("config/wispx/config.json");
    let (ok, said) = nested.wispx(&[]);
    assert!(ok, "launch: {said}");
    answer_shortcuts_dialog(&nested, &mut keyboard);
    wait_for_active_caption(&nested, "WispX");
    require("the controls to be ready", SECOND * 10, || {
        inspect(&nested)["settings"]["ready"] == true
    });
    assert_eq!(nested.status()["backend"], "local");
    let before = fs::read_to_string(&config_path).unwrap();
    // The combo's list opens over the window, and its rows split it evenly.
    let choose_backend = |keyboard: &mut Keyboard, row: f64| {
        click_item(&nested, keyboard, "backend");
        let mut list = Json::Null;
        require("the list to open", SECOND * 5, || {
            list = inspect(&nested)["windows"]["settings"]["items"]["Menu"].clone();
            list["visible"] == true
        });
        let number = |value: &Json| value.as_f64().unwrap();
        let dump = nested.compositor();
        let window = settings_window(&dump).unwrap();
        keyboard.click(
            number(&window["client_x"]) + number(&list["x"]) + number(&list["width"]) / 2.0,
            number(&window["client_y"]) + number(&list["y"]) + number(&list["height"]) * row,
        );
    };
    let choose_chatgpt = |keyboard: &mut Keyboard| {
        choose_backend(keyboard, 0.75);
        require("the question", SECOND * 5, || {
            inspect(&nested)["windows"]["settings"]["items"]["chatgpt-accept"]["visible"] == true
        });
    };

    choose_chatgpt(&mut keyboard);
    picture(
        &nested,
        "15-chatgpt-consent",
        "REAL: what is asked before any audio could go online",
    );
    // Asking changed nothing, and declining changes nothing.
    assert_eq!(nested.status()["backend"], "local");
    click_item(&nested, &mut keyboard, "chatgpt-decline");
    thread::sleep(SECOND);
    assert_eq!(nested.status()["backend"], "local");
    assert_eq!(fs::read_to_string(&config_path).unwrap(), before);

    // Saying yes switches the daemon and the file together.
    choose_chatgpt(&mut keyboard);
    click_item(&nested, &mut keyboard, "chatgpt-accept");
    require("the daemon to switch", SECOND * 5, || {
        nested.status()["backend"] == "chatgpt"
    });
    assert_eq!(config(&nested)["backend"], "chatgpt");
    require("the window to say where audio goes", SECOND * 10, || {
        let state = inspect(&nested);
        let rows = state["settings"]["rows"].as_array().unwrap().clone();
        rows.iter().any(|row| {
            row["id"] == "speech" && row["detail"].as_str().unwrap().contains("chatgpt.com")
        })
    });

    // And back, without a question.
    choose_backend(&mut keyboard, 0.25);
    require("the daemon to come back", SECOND * 5, || {
        nested.status()["backend"] == "local"
    });
    assert_eq!(config(&nested)["backend"], "local");

    // The rest of the window, which the other pictures don't reach.
    keyboard.scroll(30);
    thread::sleep(Duration::from_millis(400));
    picture(
        &nested,
        "16-settings-lower",
        "REAL: the lower half of the settings window",
    );

    assert_eq!(app_command(&nested, "quit").unwrap()["ok"], true);
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn a_daemon_started_separately_is_left_running() {
    let Some(mut nested) = session(false) else {
        return;
    };
    let mut keyboard = Keyboard::open(&nested);
    nested.start_daemon();
    let (ok, said) = nested.wispx(&["app", "--background"]);
    assert!(ok, "launch: {said}");
    wait_for_active_caption(&nested, "Global Shortcuts");
    keyboard.tap(&[RETURN]);
    require("shortcuts to bind", SECOND * 20, || {
        nested.status()["hotkey_state"] == "bound"
    });
    require("the app to attach", SECOND * 10, || {
        inspect(&nested)["settings"]["ready"] == true
    });
    let state = inspect(&nested);
    assert_eq!(state["owns_daemon"], false);
    let rows = state["settings"]["rows"].as_array().unwrap();
    let service = rows.iter().find(|row| row["id"] == "service").unwrap();
    assert!(
        service["detail"]
            .as_str()
            .unwrap()
            .contains("keeps running"),
        "{service}"
    );

    // The app shows the daemon's settings, and changes them through the daemon.
    assert_eq!(state["settings"]["backend"], "local");

    let quit = app_command(&nested, "quit").unwrap();
    assert_eq!(quit["ok"], true);
    require("the app to quit", SECOND * 10, || {
        !nested.running("wispx-gui")
    });
    assert!(
        nested.wispx(&["status"]).0,
        "the daemon stopped with the app"
    );
    assert!(
        nested
            .daemon
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    nested.quit_daemon();
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn a_daemon_that_crashes_comes_back_and_a_bad_config_is_explained() {
    let Some(nested) = session(false) else {
        return;
    };
    let mut keyboard = Keyboard::open(&nested);
    let config_path = nested.root.join("config/wispx/config.json");
    let good = fs::read_to_string(&config_path).unwrap();

    // A broken file: said out loud, and left exactly as it was.
    let broken = good.replacen('{', "{ \"no_such_setting\": 1,", 1);
    fs::write(&config_path, &broken).unwrap();
    let (ok, said) = nested.wispx(&[]);
    assert!(ok, "launch: {said}");
    require("the failure to be shown", SECOND * 20, || {
        inspect(&nested)["settings"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["id"] == "service"
                    && row["level"] == "fail"
                    && row["detail"].as_str().unwrap().contains("no_such_setting")
            })
    });
    require("the settings window", SECOND * 10, || {
        settings_window(&nested.compositor()).is_some()
    });
    thread::sleep(Duration::from_millis(400));
    picture(
        &nested,
        "10-bad-config",
        "REAL: an invalid settings file, with what to do about it",
    );
    assert_eq!(fs::read_to_string(&config_path).unwrap(), broken);
    assert!(
        pill_window(&nested.compositor()).is_none(),
        "nothing can dictate"
    );

    // Fixed by hand, then the real Try again button.
    fs::write(&config_path, &good).unwrap();
    wait_for_active_caption(&nested, "WispX");
    click_item(&nested, &mut keyboard, "button-start-service");
    wait_for_active_caption(&nested, "Global Shortcuts");
    keyboard.tap(&[RETURN]);
    require("shortcuts to bind", SECOND * 20, || {
        nested.status()["hotkey_state"] == "bound"
    });
    require("the app to own the daemon", SECOND * 10, || {
        inspect(&nested)["owns_daemon"] == true
    });

    // A crash: back without being asked.
    let crashed = nested.pids("--app-child");
    assert_eq!(crashed.len(), 1, "one daemon: {crashed:?}");
    let killed = Command::new("kill")
        .args(["-KILL", &crashed[0].to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    require("the service to come back", SECOND * 20, || {
        let now = nested.pids("--app-child");
        !now.is_empty()
            && !now.contains(&crashed[0])
            && nested.wispx(&["status"]).0
            && inspect(&nested)["owns_daemon"] == true
    });
    eprintln!("the daemon came back after being killed");

    let quit = app_command(&nested, "quit").unwrap();
    assert_eq!(quit["ok"], true);
    require("everything to stop", SECOND * 15, || {
        !nested.running("wispx-gui") && !nested.running("--app-child")
    });
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn a_missing_model_can_be_fetched_from_the_window() {
    let Some(nested) = session(false) else {
        return;
    };
    let mut keyboard = Keyboard::open(&nested);
    // A fresh install: no settings file and no model, but whisper-cli is there.
    fs::remove_file(nested.root.join("config/wispx/config.json")).unwrap();
    let r = nested.root.display();
    // Stands in for the network: a slow file with the right magic number.
    nested.script(
        "curl",
        &format!(
            r#"while [ $# -gt 1 ]; do [ "$1" = --output ] && out=$2; shift; done
echo "$1" > {r}/curl-url
printf lmgg > "$out"
for i in 1 2 3 4 5 6 7 8; do head -c 400000 /dev/zero >> "$out"; sleep 0.25; done"#
        ),
    );
    let (ok, said) = nested.wispx(&[]);
    assert!(ok, "launch: {said}");
    wait_for_active_caption(&nested, "Global Shortcuts");
    keyboard.tap(&[RETURN]);
    require("shortcuts to bind", SECOND * 20, || {
        nested.status()["hotkey_state"] == "bound"
    });
    wait_for_active_caption(&nested, "WispX");
    require("the model to be missed", SECOND * 10, || {
        inspect(&nested)["settings"]["rows"][0]["level"] == "fail"
    });
    thread::sleep(Duration::from_millis(300));
    picture(
        &nested,
        "11-missing-model",
        "REAL: no model yet, with a download button",
    );

    click_item(&nested, &mut keyboard, "button-download-model");
    require("the download to show", SECOND * 5, || {
        inspect(&nested)["settings"]["download"].is_object()
    });
    thread::sleep(Duration::from_millis(600));
    picture(
        &nested,
        "12-downloading",
        "REAL: progress from the file growing (curl is a stand-in)",
    );
    let model = nested.root.join("data/wispx/models/ggml-base.bin");
    require("the model", SECOND * 20, || model.exists());
    assert!(read(&nested.root.join("curl-url")).contains("ggml-base.bin"));
    require("the speech check to pass", SECOND * 10, || {
        inspect(&nested)["settings"]["rows"][0]["level"] == "ok"
    });
    let saved = config(&nested);
    assert_eq!(saved["whisper"]["model"], model.to_str().unwrap());

    // What's there already is never fetched or replaced again.
    let offered = inspect(&nested)["settings"]["downloads"].to_string();
    assert!(!offered.contains("\"base\""), "{offered}");
    let before = fs::metadata(&model).unwrap().modified().unwrap();
    assert_eq!(fs::metadata(&model).unwrap().modified().unwrap(), before);
    let quit = app_command(&nested, "quit").unwrap();
    assert_eq!(quit["ok"], true);
}

#[test]
#[ignore = "starts a private headless KWin and real portals"]
fn real_whisper_through_the_real_app() {
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
                "language": "en",
                "audio": {{ "recorder": "pw-record" }},
                "whisper": {{ "model": "{}" }}
            }}"#,
            Path::new(&model).canonicalize().unwrap().display()
        ),
    )
    .unwrap();
    let mut keyboard = Keyboard::open(&nested);
    nested.allow_remote_desktop(WISPX_ID);
    let (ok, said) = nested.wispx(&[]);
    assert!(ok, "launch: {said}");
    wait_for_active_caption(&nested, "Global Shortcuts");
    keyboard.tap(&[RETURN]);
    require("shortcuts to bind", SECOND * 20, || {
        nested.status()["hotkey_state"] == "bound"
    });
    let setup = nested.wispx(&["paste-setup"]);
    assert!(setup.0, "paste-setup: {}", setup.1);
    let typed = nested.open_target("typed");

    keyboard.down(&HOLD);
    nested.wait_for_state("recording");
    wait_for_pill(&nested, 156.0, 38.0);
    thread::sleep(SECOND * 2);
    keyboard.up(&HOLD);
    wait_for_pill(&nested, 120.0, 38.0);
    require("the real transcript", SECOND * 90, || {
        read(&typed)
            .to_lowercase()
            .contains("ask not what your country")
    });
    eprintln!(
        "whisper.cpp, through the app, into foot: {:?}",
        read(&typed)
    );
    let quit = app_command(&nested, "quit").unwrap();
    assert_eq!(quit["ok"], true);
    nested.windows.clear();
}
