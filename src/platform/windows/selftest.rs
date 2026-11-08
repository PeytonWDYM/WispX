//! Real endpoint exercise with a timed tour of every pill state, without hooks/tray/install.
use super::{context, overlay::Overlay};
use crate::{
    app::{MessageKind, Pill},
    audio::opus,
    transcribe::{
        ChatGpt,
        auth::{self, AuthMode},
    },
};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
thread_local! {static OVERLAY: std::cell::RefCell<Option<Rc<Overlay>>> = const {std::cell::RefCell::new(None)};}
thread_local! {static CONTEXT_WINDOW: std::cell::RefCell<Option<ContextWindow>> = const {std::cell::RefCell::new(None)};}
const CONTEXT_BEFORE: &str = "Before the cursor.\r\nWispX ";
const CONTEXT_AFTER: &str = "fits here.\r\nAfter the cursor.";
#[derive(Clone, Copy)]
enum ContextField {
    Multiline,
    Password,
}
struct ContextWindow(windows::Win32::Foundation::HWND);
impl Drop for ContextWindow {
    fn drop(&mut self) {
        // SAFETY: selftest owns this window and destroys it on its creating UI thread.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}
impl ContextWindow {
    fn open(field: ContextField) -> windows::core::Result<(Self, isize)> {
        use windows::{
            Win32::{
                Foundation::{LPARAM, WPARAM},
                UI::{
                    Controls::EM_SETSEL,
                    Input::KeyboardAndMouse::SetFocus,
                    WindowsAndMessaging::{
                        CreateWindowExW, ES_MULTILINE, ES_PASSWORD, SendMessageW, WINDOW_STYLE,
                        WS_CHILD, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
                    },
                },
            },
            core::{PCWSTR, w},
        };
        let (text, style, caret) = match field {
            ContextField::Multiline => (
                format!("{CONTEXT_BEFORE}{CONTEXT_AFTER}"),
                ES_MULTILINE,
                CONTEXT_BEFORE.encode_utf16().count(),
            ),
            ContextField::Password => ("fixed selftest password".into(), ES_PASSWORD, 0),
        };
        let text = super::files::wide(text);
        // SAFETY: registered system classes, owned parent/child handles, and a NUL-terminated
        // text buffer valid for creation. Messages target only our own EDIT control.
        unsafe {
            let window = Self(CreateWindowExW(
                WS_EX_TOPMOST,
                w!("STATIC"),
                w!("WispX context selftest"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                80,
                80,
                520,
                200,
                None,
                None,
                None,
                None,
            )?);
            let edit = CreateWindowExW(
                Default::default(),
                w!("EDIT"),
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style as u32),
                12,
                12,
                470,
                130,
                Some(window.0),
                None,
                None,
                None,
            )?;
            SendMessageW(
                edit,
                EM_SETSEL,
                Some(WPARAM(caret)),
                Some(LPARAM(caret as isize)),
            );
            SetFocus(Some(edit))?;
            Ok((window, edit.0 as isize))
        }
    }
}
enum Step {
    Show(Pill),
    Preview,
    Snapshot(PathBuf),
    ContextOpen(ContextField, std::sync::mpsc::SyncSender<isize>),
    ContextClose,
}
fn step(step: Step) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    slint::invoke_from_event_loop(move || {
        OVERLAY.with(|o| {
            let overlay = o.borrow();
            let overlay = overlay
                .as_ref()
                .expect("selftest overlay lives through the worker");
            let result = match step {
                Step::Show(pill) => overlay.show(pill).map_err(|e| e.to_string()),
                Step::Preview => overlay.preview_recording().map_err(|e| e.to_string()),
                Step::Snapshot(path) => overlay.snapshot(&path).map_err(|e| e.to_string()),
                Step::ContextOpen(field, sender) => ContextWindow::open(field)
                    .map(|(window, edit)| {
                        CONTEXT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
                        let _ = sender.send(edit);
                    })
                    .map_err(|e| e.to_string()),
                Step::ContextClose => {
                    CONTEXT_WINDOW.with(|slot| slot.borrow_mut().take());
                    Ok(())
                }
            };
            let _ = tx.send(result);
        })
    })?;
    rx.recv()?.map_err(io::Error::other)?;
    Ok(())
}
pub fn run(wav: PathBuf, out: PathBuf, language: String) -> Result<(), Box<dyn std::error::Error>> {
    let upload_path = out.with_extension("webm");
    let overlay = Rc::new(Overlay::new(true)?);
    OVERLAY.with(|o| *o.borrow_mut() = Some(overlay));
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let frames = out
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("pill");
    fs::create_dir_all(&frames)?;
    let log = Arc::new(std::sync::Mutex::new(fs::File::create(out)?));
    let worker_log = log.clone();
    let (finished, result) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let log = worker_log;
        let mark = |message: &str| -> io::Result<()> {
            let mut log = log.lock().expect("log writer does not panic");
            writeln!(log, "{} {message}", time::OffsetDateTime::now_utc())?;
            log.flush()
        };
        let run = || -> Result<(), Box<dyn std::error::Error>> {
            thread::sleep(Duration::from_millis(1500));
            mark("idle")?;
            step(Step::Snapshot(frames.join("1-idle.png")))?;
            thread::sleep(Duration::from_millis(1500));
            step(Step::Preview)?;
            mark("recording")?;
            thread::sleep(Duration::from_secs(2));
            step(Step::Snapshot(frames.join("2-recording.png")))?;
            step(Step::Show(Pill::Locked))?;
            mark("locked")?;
            thread::sleep(Duration::from_secs(2));
            step(Step::Snapshot(frames.join("3-locked.png")))?;
            step(Step::Show(Pill::Transcribing))?;
            mark("transcribing")?;
            let start = Instant::now();
            let reader = hound::WavReader::open(wav)?;
            let spec = reader.spec();
            if spec.channels != 1
                || spec.sample_rate != 16_000
                || spec.bits_per_sample != 16
                || spec.sample_format != hound::SampleFormat::Int
            {
                return Err("Selftest input must be 16 kHz mono signed 16-bit PCM WAV".into());
            }
            let pcm = reader
                .into_samples::<i16>()
                .collect::<Result<Vec<_>, _>>()?;
            let duration_ms = pcm.len() as u64 * 1000 / 16_000;
            let bytes = opus::encode(&pcm)?;
            fs::write(&upload_path, &bytes)?;
            mark(&format!(
                "UPLOAD: {} bytes WebM vs {} bytes equivalent WAV; {duration_ms} ms; {}",
                bytes.len(),
                44 + pcm.len() * 2,
                upload_path.display()
            ))?;
            let client = ChatGpt::default();
            client.warmup();
            let transcription = client.transcribe(&bytes, duration_ms, &language);
            match &transcription {
                Ok(text) => mark(&format!(
                    "TRANSCRIPT ({} ms): {text}",
                    start.elapsed().as_millis()
                ))?,
                Err(error) => mark(&format!("ERROR: {error}"))?,
            }
            step(Step::Snapshot(frames.join("4-transcribing.png")))?;
            let raw = transcription
                .as_ref()
                .map_err(|error| io::Error::other(error.to_string()))?;
            if !auth::login().is_some_and(|login| login.mode == AuthMode::ChatGpt) {
                return Err("Cleanup E2E needs a ChatGPT Codex login".into());
            }
            let context = "WispX is a Windows dictation app.\n[cursor]\nThis is a repeatable test.";
            let dictionary = ["WispX".to_owned()];
            let cleanup_start = Instant::now();
            let cleaned = client.cleanup(raw.clone(), true, context, &dictionary);
            mark(&format!(
                "CLEANUP ({} ms): raw={raw:?}; cleaned={cleaned:?}",
                cleanup_start.elapsed().as_millis()
            ))?;
            if !cleaned.contains("WispX") {
                return Err("Cleanup did not preserve the canonical WispX spelling".into());
            }
            // A deliberate historical recognition error proves the response wasn't a silent fallback.
            let cleanup_start = Instant::now();
            let corrected = client.cleanup(
                "Wispecs is a dictation app.".into(),
                true,
                context,
                &dictionary,
            );
            mark(&format!(
                "DICTIONARY ({} ms): raw=\"Wispecs is a dictation app.\"; cleaned={corrected:?}",
                cleanup_start.elapsed().as_millis()
            ))?;
            if !corrected.contains("WispX") {
                return Err("Cleanup dictionary correction failed or fell back".into());
            }
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            step(Step::ContextOpen(ContextField::Multiline, tx))?;
            let edit = rx.recv()?;
            let context_start = Instant::now();
            let captured = context::begin_selftest(edit).and_then(context::Capture::finish);
            step(Step::ContextClose)?;
            mark(&format!(
                "CONTEXT ({} ms): {captured:?}",
                context_start.elapsed().as_millis()
            ))?;
            // Native EDIT exposes only ValuePattern; its caret comes from EM_GETSEL.
            let expected = format!("{CONTEXT_BEFORE}[cursor]{CONTEXT_AFTER}");
            if captured.as_deref() != Some(expected.as_str()) {
                return Err("Context did not mark the caret in the fixed selftest EDIT".into());
            }
            mark("CONTEXT: caret position matched")?;
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            step(Step::ContextOpen(ContextField::Password, tx))?;
            let password = context::begin_selftest(rx.recv()?).and_then(context::Capture::finish);
            step(Step::ContextClose)?;
            if password.is_some() {
                return Err("Password control was not excluded from context".into());
            }
            mark("CONTEXT: password field excluded")?;
            thread::sleep(Duration::from_millis(1200));
            step(Step::Show(Pill::Done))?;
            mark("done")?;
            thread::sleep(Duration::from_millis(80));
            step(Step::Snapshot(frames.join("5-done.png")))?;
            thread::sleep(Duration::from_millis(1420));
            step(Step::Show(Pill::message(
                "Copied. You switched windows, so press Ctrl + V",
                MessageKind::Info,
                3.0,
            )))?;
            mark("message")?;
            thread::sleep(Duration::from_millis(2500));
            step(Step::Snapshot(frames.join("6-message.png")))?;
            step(Step::Show(Pill::message(
                "Sign in to the Codex app first",
                MessageKind::Warn,
                3.0,
            )))?;
            mark("warn")?;
            thread::sleep(Duration::from_millis(2500));
            step(Step::Snapshot(frames.join("7-warn.png")))?;
            step(Step::Show(Pill::message(
                "Transcription failed",
                MessageKind::Error,
                2.6,
            )))?;
            mark("error")?;
            thread::sleep(Duration::from_millis(1500));
            step(Step::Snapshot(frames.join("8-error.png")))?;
            mark("end")?;
            transcription?;
            Ok(())
        };
        let outcome = run().map_err(|error| error.to_string());
        if let Err(error) = &outcome {
            let _ = mark(&format!("SELFTEST FAILED: {error}"));
        }
        let _ = finished.send(outcome);
        let _ = slint::quit_event_loop();
    });
    slint::run_event_loop_until_quit()?;
    OVERLAY.with(|o| o.borrow_mut().take());
    CONTEXT_WINDOW.with(|slot| slot.borrow_mut().take());
    result.recv()?.map_err(io::Error::other)?;
    Ok(())
}
