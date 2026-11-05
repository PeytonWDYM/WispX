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
#[path = "support/nested.rs"]
mod nested;

use nested::*;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::thread;
use std::time::Duration;

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
