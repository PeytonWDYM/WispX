//! Real endpoint exercise with a timed tour of every pill state, without hooks/tray/install.
use super::overlay::Overlay;
use crate::{
    app::{MessageKind, Pill},
    audio::opus,
    transcribe::ChatGpt,
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
enum Step {
    Show(Pill),
    Preview,
    Snapshot(PathBuf),
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
    result.recv()?.map_err(io::Error::other)?;
    Ok(())
}
