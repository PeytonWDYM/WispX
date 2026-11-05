//! The background process: owns the dictation session and connects the shortcut
//! source and control socket to the recorder, transcription and delivery.
//!
//! One thread owns all state and handles events in order, so there is no locking
//! to get wrong. Anything slow (transcribing, clipboard tools, popups, sounds)
//! runs elsewhere and reports back with a message, which keeps the microphone
//! responsive no matter how a tool behaves.

use crate::audio::{self, RecorderPlan, Recording, wav};
use crate::config::{Backend, Config, HotkeyMode, Overrides, PasteMode};
use crate::desktop::SessionKind;
use crate::desktop::clipboard::{Clipboard, ToolClipboard};
use crate::desktop::kwin::FocusInfo;
use crate::desktop::notify::{self, Feedback, Note};
use crate::desktop::paste::{self, CopyReason, Delivered, Delivery, Injector, Timing};
use crate::desktop::remote::{self, PlasmaInjector, SetupOutcome};
use crate::ipc::{self, Incoming, Request};
use crate::paths;
use crate::process::Toolbox;
use crate::session::{Action, Input, Mode, Session, Verdict, classify};
use crate::shortcuts;
use crate::sounds::Cue;
use crate::sys;
use crate::transcribe::{self, TranscribeError};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const EXIT_ALREADY_RUNNING: i32 = 4;
const TICK: Duration = Duration::from_millis(50);
/// Extra room so the recorder is still delivering when the limit is applied.
const CAPTURE_MARGIN_SECS: u64 = 5;
const SHUTDOWN_JOB_WAIT: Duration = Duration::from_millis(1500);
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);
const NOTE_FLUSH: Duration = Duration::from_millis(500);
const JOB_PREFIX: &str = "job-";

pub struct Options {
    pub config_path: PathBuf,
    pub overrides: Overrides,
    pub quiet: bool,
}

enum Event {
    Ipc(Incoming),
    Shortcut(shortcuts::Event),
    /// Stored before delivery starts, so it survives a failed paste.
    Transcript(String),
    JobDone(u64, JobResult),
}

enum JobResult {
    Cancelled,
    Empty,
    Failed(String),
    Delivered(Delivery),
}

struct Job {
    id: u64,
    recording: Recording,
    config: Arc<Config>,
    cancel: Arc<AtomicBool>,
    focus: Option<JoinHandle<Option<FocusInfo>>>,
}

pub fn run(options: Options) -> i32 {
    sys::install_shutdown_handlers();
    let runtime = match paths::runtime_dir() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("wispx: {error}");
            return 1;
        }
    };
    // Held for the whole run; the OS releases it however the process ends.
    let _lock = match acquire_lock(&paths::lock_path(&runtime)) {
        Ok(Some(file)) => file,
        Ok(None) => {
            eprintln!("wispx: already running. Use `wispx status` or `wispx quit`");
            return EXIT_ALREADY_RUNNING;
        }
        Err(error) => {
            eprintln!("wispx: {error}");
            return 1;
        }
    };
    let config = match load_config(&options.config_path, &options.overrides) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("wispx: {error}");
            return 1;
        }
    };
    remove_job_files(&runtime);

    let socket = paths::socket_path(&runtime);
    let listener = match ipc::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("wispx: cannot create {}: {error}", socket.display());
            return 1;
        }
    };

    let (events, inbox) = mpsc::channel();
    let connections = {
        let events = events.clone();
        ipc::serve(listener, move |incoming| {
            let _ = events.send(Event::Ipc(incoming));
        })
    };

    let tools = Toolbox::from_env();
    let desktop = SessionKind::detect();
    let feedback = Feedback::spawn(runtime.clone(), &tools);
    let injector = if desktop.is_plasma() {
        match remote::token_path() {
            Ok(token_file) => Some(PlasmaInjector::new(runtime.clone(), token_file)),
            Err(error) => {
                eprintln!("wispx: automatic paste is off: {error}");
                None
            }
        }
    } else {
        None
    };
    if let Some(injector) = &injector
        && config.paste == PasteMode::Auto
    {
        injector.restore_in_background();
    }

    let hotkey = if wants_portal(config.hotkey, &desktop) {
        let events = events.clone();
        let sink: shortcuts::Sink = Arc::new(move |event| {
            let _ = events.send(Event::Shortcut(event));
        });
        shortcuts::spawn(config.shortcuts.clone(), sink);
        shortcuts::Status::Starting
    } else {
        shortcuts::Status::Unavailable(match config.hotkey {
            HotkeyMode::None => "turned off by the hotkey setting".to_string(),
            _ => format!(
                "needs a Wayland session; this is {}. Bind `wispx press` and `wispx release` \
                 in your window manager",
                desktop.describe()
            ),
        })
    };

    let jobs = {
        let worker = Worker {
            runtime: runtime.clone(),
            tools: tools.clone(),
            desktop: desktop.clone(),
            injector: injector.clone(),
            events: events.clone(),
        };
        let (jobs, job_rx) = mpsc::channel();
        thread::spawn(move || worker.run(job_rx));
        jobs
    };

    let mut daemon = Daemon {
        config_path: options.config_path,
        overrides: options.overrides,
        config: Arc::new(config),
        tools,
        desktop,
        runtime: runtime.clone(),
        session: Session::default(),
        recording: None,
        plan: None,
        feedback,
        jobs,
        in_flight: Vec::new(),
        next_job: 1,
        last_transcript: None,
        last_error: None,
        hotkey,
        injector,
        quit: false,
    };

    daemon.greet(options.quiet);
    daemon.run_loop(&inbox);
    daemon.shutdown(&inbox);

    let _ = fs::remove_file(&socket);
    connections.drain(SHUTDOWN_DRAIN);
    0
}

fn wants_portal(mode: HotkeyMode, desktop: &SessionKind) -> bool {
    match mode {
        HotkeyMode::Portal => true,
        HotkeyMode::Auto => desktop.is_wayland(),
        HotkeyMode::None => false,
    }
}

/// `Ok(None)` means another daemon holds it. Being a lock rather than a pid file,
/// it can't go stale, however the last daemon ended.
fn acquire_lock(path: &Path) -> Result<Option<File>, String> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(format!("cannot lock {}: {error}", path.display())),
    }
}

fn load_config(path: &Path, overrides: &Overrides) -> Result<Config, String> {
    let mut config = Config::load(path)?;
    overrides.apply(&mut config)?;
    Ok(config)
}

/// Private audio from a run that died must not linger.
fn remove_job_files(runtime: &Path) {
    let Ok(entries) = fs::read_dir(runtime) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(JOB_PREFIX) && name.ends_with(".wav") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

struct Daemon {
    config_path: PathBuf,
    overrides: Overrides,
    config: Arc<Config>,
    tools: Toolbox,
    desktop: SessionKind,
    runtime: PathBuf,
    session: Session,
    recording: Option<Recording>,
    /// Finding a recorder runs a tool, so it is only done once per config.
    plan: Option<RecorderPlan>,
    feedback: Feedback,
    jobs: Sender<Job>,
    in_flight: Vec<(u64, Arc<AtomicBool>)>,
    next_job: u64,
    last_transcript: Option<String>,
    last_error: Option<String>,
    hotkey: shortcuts::Status,
    injector: Option<Arc<PlasmaInjector>>,
    quit: bool,
}

impl Daemon {
    fn greet(&mut self, quiet: bool) {
        if let Err(problem) = transcribe::precheck(&self.config, &self.tools) {
            eprintln!("wispx: not ready to transcribe: {problem}");
            self.note(Note::Warn(problem));
        } else if !quiet {
            self.note(Note::Done("WispX is running".to_string()));
        }
    }

    fn run_loop(&mut self, inbox: &Receiver<Event>) {
        while !self.quit && !sys::shutdown_requested() {
            match inbox.recv_timeout(TICK) {
                Ok(event) => self.handle(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.tick();
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Ipc(incoming) => self.handle_request(incoming),
            Event::Shortcut(shortcuts::Event::Input(input)) => {
                // Failures reach the user as a notification; there is no caller.
                let _ = self.input(input);
            }
            Event::Shortcut(shortcuts::Event::Status(status)) => {
                if status != self.hotkey {
                    eprintln!("wispx: {}", status.describe());
                    if status == shortcuts::Status::Declined {
                        self.note(Note::Warn(status.describe()));
                    }
                }
                self.hotkey = status;
            }
            Event::Transcript(text) => self.last_transcript = Some(text),
            Event::JobDone(id, result) => self.job_done(id, result),
        }
    }

    fn tick(&mut self) {
        let Some(elapsed) = self.session.elapsed(Instant::now()) else {
            return;
        };
        let input = if elapsed >= Duration::from_secs(self.config.max_recording_secs) {
            Input::MaxDuration
        } else if self.recording.as_ref().is_some_and(Recording::input_closed) {
            Input::RecorderClosed
        } else {
            return;
        };
        let _ = self.input(input);
    }

    fn note(&self, note: Note) {
        if note.allowed(self.config.notifications) {
            self.feedback.note(note);
        }
    }

    fn cue(&self, cue: Cue) {
        if self.config.sounds {
            self.feedback.sound(cue);
        }
    }

    /// Runs one gesture, returning the message for whoever asked.
    fn input(&mut self, input: Input) -> Result<String, String> {
        match self.session.apply(input, Instant::now()) {
            Action::Begin(mode) => self.begin(mode),
            Action::Locked => {
                self.note(Note::Recording { locked: true });
                Ok("Recording hands-free".to_string())
            }
            Action::Finish { held } => self.finish(held, input),
            Action::Cancel { sound } => {
                self.recording = None;
                if sound {
                    self.cue(Cue::Cancel);
                }
                self.note_idle(Note::Clear);
                Ok("Cancelled".to_string())
            }
            Action::None if input == Input::Cancel && !self.in_flight.is_empty() => {
                for (_, cancel) in &self.in_flight {
                    cancel.store(true, Ordering::SeqCst);
                }
                self.cue(Cue::Cancel);
                self.note(Note::Clear);
                Ok("Cancelled the transcription".to_string())
            }
            Action::None => Ok(match self.session.mode() {
                Some(_) => "Already recording".to_string(),
                None => "Nothing to do".to_string(),
            }),
        }
    }

    fn begin(&mut self, mode: Mode) -> Result<String, String> {
        match self.open_recorder() {
            Ok(recording) => {
                self.recording = Some(recording);
                self.last_error = None;
                self.cue(Cue::Start);
                let locked = mode == Mode::Locked;
                self.note(Note::Recording { locked });
                Ok(if locked {
                    "Recording hands-free. Finish with `wispx toggle`".to_string()
                } else {
                    "Recording".to_string()
                })
            }
            Err(message) => {
                self.session.abort();
                Err(self.report(message))
            }
        }
    }

    /// Checks the backend first so nobody dictates a paragraph into a setup
    /// problem, and only reads the login when the ChatGPT backend is selected.
    fn open_recorder(&mut self) -> Result<Recording, String> {
        transcribe::precheck(&self.config, &self.tools)?;
        if self.plan.is_none() {
            self.plan = Some(audio::resolve(&self.config.audio, &self.tools)?);
        }
        let plan = self.plan.as_ref().ok_or("no recorder available")?;
        let seconds = self.config.max_recording_secs + CAPTURE_MARGIN_SECS;
        let max_bytes = wav::BYTES_PER_SECOND.saturating_mul(seconds);
        let recording = Recording::start(plan, max_bytes as usize);
        if recording.is_err() {
            // The tool may have been removed; look it up again next time.
            self.plan = None;
        }
        recording
    }

    fn report(&mut self, message: String) -> String {
        eprintln!("wispx: {message}");
        self.last_error = Some(message.clone());
        self.note(Note::Error(message.clone()));
        message
    }

    fn finish(&mut self, held: Duration, input: Input) -> Result<String, String> {
        let Some(mut recording) = self.recording.take() else {
            return Ok("Nothing to do".to_string());
        };
        let verdict = classify(
            held,
            recording.peak(),
            Duration::from_millis(self.config.min_hold_ms),
            self.config.silence_threshold,
        );
        if input == Input::RecorderClosed && verdict != Verdict::Speech {
            let detail = recording
                .last_error()
                .unwrap_or_else(|| "it stopped without saying why".to_string());
            let message = format!("{} stopped recording: {detail}", recording.name());
            return Err(self.report(message));
        }
        match verdict {
            Verdict::TooShort => {
                self.note_idle(Note::Clear);
                Ok("Too short, discarded".to_string())
            }
            Verdict::Silent => {
                self.cue(Cue::Stop);
                let message = "Didn't hear anything. Check your mic?".to_string();
                self.note_idle(Note::Warn(message.clone()));
                Ok(message)
            }
            Verdict::Speech => {
                // The microphone goes off at once, however long the backend takes.
                recording.request_stop();
                self.cue(Cue::Stop);
                self.enqueue(recording);
                Ok("Transcribing".to_string())
            }
        }
    }

    fn enqueue(&mut self, recording: Recording) {
        let id = self.next_job;
        self.next_job += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.in_flight.push((id, Arc::clone(&cancel)));
        let focus = self.capture_focus();
        self.note(Note::Transcribing(
            match self.config.backend {
                Backend::Local => "Transcribing",
                Backend::ChatGpt => "Sending to ChatGPT",
            }
            .to_string(),
        ));
        let job = Job {
            id,
            recording,
            config: Arc::clone(&self.config),
            cancel,
            focus,
        };
        if self.jobs.send(job).is_err() {
            self.in_flight.retain(|(job_id, _)| *job_id != id);
            self.report("the transcription thread stopped".to_string());
        }
    }

    /// Remembers which window dictation was for. On its own thread, since asking
    /// KWin must not delay anything.
    fn capture_focus(&self) -> Option<JoinHandle<Option<FocusInfo>>> {
        if self.config.paste != PasteMode::Auto {
            return None;
        }
        let injector = Arc::clone(self.injector.as_ref()?);
        if !injector.ready() {
            return None;
        }
        Some(thread::spawn(move || {
            injector.current_focus().ok().flatten()
        }))
    }

    /// Shows a note unless more important work is still on screen.
    fn note_idle(&self, note: Note) {
        if self.session.mode().is_none() && self.in_flight.is_empty() {
            self.note(note);
        }
    }

    fn job_done(&mut self, id: u64, result: JobResult) {
        self.in_flight.retain(|(job_id, _)| *job_id != id);
        match result {
            JobResult::Cancelled => self.note_idle(Note::Clear),
            JobResult::Empty => {
                self.note_idle(Note::Warn("Didn't catch that".to_string()));
            }
            JobResult::Failed(message) => {
                self.report(message);
            }
            JobResult::Delivered(delivery) => self.announce(delivery),
        }
    }

    fn announce(&mut self, delivery: Delivery) {
        match delivery {
            Delivery::Pasted => self.note_idle(Note::Done("Pasted".to_string())),
            Delivery::Copied(CopyReason::Requested | CopyReason::CannotVerify) => {
                self.note_idle(Note::Done("Copied. Press Ctrl + V to paste".to_string()));
            }
            Delivery::Copied(CopyReason::FocusChanged) => {
                self.note_idle(Note::Warn(
                    "Copied. You switched windows, so press Ctrl + V".to_string(),
                ));
            }
            Delivery::Copied(CopyReason::PasteFailed(error)) => {
                eprintln!("wispx: paste failed: {error}");
                self.note_idle(Note::Warn(
                    "Couldn't paste, so it's copied. Press Ctrl + V".to_string(),
                ));
            }
            Delivery::Stored => {
                self.note_idle(Note::Done(
                    "Done. Use `wispx copy-last` to get the text".to_string(),
                ));
            }
            Delivery::Failed(error) => {
                self.report(format!(
                    "Transcribed, but the text couldn't be copied: {error}. `wispx last` \
                     prints it"
                ));
            }
        }
    }

    fn handle_request(&mut self, incoming: Incoming) {
        let Incoming { request, reply } = incoming;
        let mut transcript = None;
        let result = match request {
            Request::Press => self.input(Input::Press),
            Request::Release => self.input(Input::Release),
            Request::Start => self.input(Input::Start),
            Request::Stop => self.input(Input::Stop),
            Request::Toggle => self.input(Input::Toggle),
            Request::Cancel => self.input(Input::Cancel),
            Request::Status => Ok(String::new()),
            Request::Last => match &self.last_transcript {
                Some(text) => {
                    transcript = Some(text.clone());
                    Ok(String::new())
                }
                None => Err("Nothing transcribed yet".to_string()),
            },
            Request::CopyLast => {
                self.copy_last(reply);
                return;
            }
            Request::Reload => self.reload(),
            Request::SetBackend(backend) => {
                // The file was already updated, and it must now win.
                self.overrides.backend = None;
                self.reload()
                    .map(|_| format!("Backend: {}", backend.as_str()))
            }
            Request::PasteSetup => {
                self.paste_setup(reply);
                return;
            }
            Request::Quit => {
                self.quit = true;
                Ok("Stopping".to_string())
            }
        };
        let _ = reply.send(self.reply(result, transcript));
    }

    fn reply(&self, result: Result<String, String>, transcript: Option<String>) -> Value {
        let now = Instant::now();
        let (ok, message) = match result {
            Ok(message) => (true, message),
            Err(message) => (false, message),
        };
        let state = match self.session.mode() {
            Some(Mode::Hold) => "recording",
            Some(Mode::Locked) => "recording-hands-free",
            None if !self.in_flight.is_empty() => "transcribing",
            None => "idle",
        };
        let mut value = json!({
            "ok": ok,
            "message": message,
            "state": state,
            "elapsed_ms": self.session.elapsed(now).map(|e| e.as_millis() as u64),
            "level": self.recording.as_ref().map(Recording::level),
            "transcribing": self.in_flight.len(),
            "backend": self.config.backend.as_str(),
            "session": self.desktop.describe(),
            "hotkey": self.hotkey.describe(),
            "paste": self.paste_status(),
            "has_transcript": self.last_transcript.is_some(),
            "last_error": self.last_error,
        });
        if let Some(text) = transcript {
            value["transcript"] = json!(text);
        }
        value
    }

    fn paste_status(&self) -> String {
        match (self.config.paste, &self.injector) {
            (PasteMode::None, _) => "off: transcripts are only kept for `wispx last`".to_string(),
            (PasteMode::Copy, _) => "copy to the clipboard".to_string(),
            (PasteMode::Auto, None) => {
                "copy to the clipboard (automatic paste needs KDE Plasma)".to_string()
            }
            (PasteMode::Auto, Some(injector)) if injector.ready() => {
                "automatic, into the window that had focus".to_string()
            }
            (PasteMode::Auto, Some(_)) => {
                "copy to the clipboard (run `wispx paste-setup` to paste automatically)".to_string()
            }
        }
    }

    fn copy_last(&self, reply: Sender<Value>) {
        let Some(text) = self.last_transcript.clone() else {
            let _ = reply.send(ipc::failure("Nothing transcribed yet"));
            return;
        };
        let (desktop, tools) = (self.desktop.clone(), self.tools.clone());
        // Runs a clipboard tool, which may be slow.
        thread::spawn(move || {
            let result = ToolClipboard::detect(&desktop, &tools)
                .and_then(|clipboard| clipboard.write_text(&text, false));
            let _ = reply.send(match result {
                Ok(()) => json!({ "ok": true, "message": "Copied" }),
                Err(error) => ipc::failure(error),
            });
        });
    }

    fn reload(&mut self) -> Result<String, String> {
        // A bad file changes nothing, including what a recording will use.
        let config = load_config(&self.config_path, &self.overrides)?;
        self.config = Arc::new(config);
        self.plan = None;
        if let Some(injector) = &self.injector
            && self.config.paste == PasteMode::Auto
            && !injector.ready()
        {
            injector.restore_in_background();
        }
        Ok("Settings reloaded. Shortcut suggestions apply on the next start".to_string())
    }

    fn paste_setup(&self, reply: Sender<Value>) {
        let Some(injector) = self.injector.clone() else {
            let _ = reply.send(ipc::failure(
                "Automatic paste is only available on KDE Plasma, where WispX can check the \
                 focused window first. Transcripts are copied to the clipboard instead",
            ));
            return;
        };
        // Blocks until the user answers the desktop's dialog.
        thread::spawn(move || {
            let _ = reply.send(match injector.setup() {
                Ok(SetupOutcome::Granted) => {
                    json!({ "ok": true, "message": "Automatic paste is on" })
                }
                Ok(SetupOutcome::Declined) => ipc::failure(
                    "The desktop did not allow it, so transcripts stay on the clipboard",
                ),
                Err(error) => ipc::failure(format!("Could not set up automatic paste: {error}")),
            });
        });
    }

    fn shutdown(&mut self, inbox: &Receiver<Event>) {
        self.session.abort();
        self.recording = None;
        for (_, cancel) in &self.in_flight {
            cancel.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now() + SHUTDOWN_JOB_WAIT;
        while !self.in_flight.is_empty() && Instant::now() < deadline {
            match inbox.recv_timeout(TICK) {
                Ok(Event::JobDone(id, _)) => self.in_flight.retain(|(job, _)| *job != id),
                Ok(Event::Ipc(incoming)) => {
                    let _ = incoming.reply.send(ipc::failure("WispX is stopping"));
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.feedback.clear_and_wait(NOTE_FLUSH);
        remove_job_files(&self.runtime);
        notify::remove_cue_files(&self.runtime);
    }
}

/// Runs jobs one at a time, so overlapping dictations land in the order spoken.
struct Worker {
    runtime: PathBuf,
    tools: Toolbox,
    desktop: SessionKind,
    injector: Option<Arc<PlasmaInjector>>,
    events: Sender<Event>,
}

impl Worker {
    fn run(self, jobs: Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            let id = job.id;
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| self.process(job)));
            let result = outcome.unwrap_or_else(|_| {
                JobResult::Failed("Transcription stopped unexpectedly".to_string())
            });
            let _ = self.events.send(Event::JobDone(id, result));
        }
    }

    fn process(&self, job: Job) -> JobResult {
        let Job {
            id,
            recording,
            config,
            cancel,
            focus,
        } = job;
        let captured = recording.finish();
        if cancel.load(Ordering::SeqCst) {
            return JobResult::Cancelled;
        }
        let bytes = wav::encode_mono(&captured.pcm, wav::SAMPLE_RATE);
        drop(captured.pcm);
        let audio = match PrivateAudio::create(&self.runtime, id, &bytes) {
            Ok(audio) => audio,
            Err(error) => return JobResult::Failed(format!("cannot store the audio: {error}")),
        };
        drop(bytes);

        let result = transcribe::transcribe(
            &config,
            &self.tools,
            &audio.path,
            captured.duration_ms,
            &cancel,
        );
        drop(audio);

        let text = match result {
            Ok(text) => text,
            Err(TranscribeError::Cancelled) => return JobResult::Cancelled,
            Err(TranscribeError::Failed(message)) => return JobResult::Failed(message),
        };
        if text.is_empty() {
            return JobResult::Empty;
        }
        let _ = self.events.send(Event::Transcript(text.clone()));
        if cancel.load(Ordering::SeqCst) {
            return JobResult::Cancelled;
        }

        let target = focus.and_then(|handle| handle.join().ok()).flatten();
        let clipboard = ToolClipboard::detect(&self.desktop, &self.tools);
        let clipboard: Result<&dyn Clipboard, String> = match &clipboard {
            Ok(clipboard) => Ok(clipboard),
            Err(error) => Err(error.clone()),
        };
        let injector = self.injector.as_ref().map(|i| i as &dyn Injector);
        JobResult::Delivered(paste::deliver(Delivered {
            text: &text,
            mode: config.paste,
            shortcut: config.paste_shortcut,
            restore_clipboard: config.restore_clipboard,
            target: target.as_ref(),
            clipboard,
            injector,
            timing: Timing::default(),
            cancel: &cancel,
        }))
    }
}

/// A WAV file only this user can read, removed when dropped.
struct PrivateAudio {
    path: PathBuf,
}

impl PrivateAudio {
    fn create(runtime: &Path, id: u64, bytes: &[u8]) -> std::io::Result<Self> {
        let path = runtime.join(format!("{JOB_PREFIX}{id}.wav"));
        let audio = PrivateAudio { path };
        let mut file = paths::create_private_file(&audio.path)?;
        file.write_all(bytes)?;
        Ok(audio)
    }
}

impl Drop for PrivateAudio {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn hotkeys_use_the_portal_on_wayland_only_unless_forced() {
        let wayland = SessionKind::Wayland {
            plasma: true,
            desktop: "KDE".to_string(),
        };
        assert!(wants_portal(HotkeyMode::Auto, &wayland));
        assert!(!wants_portal(HotkeyMode::Auto, &SessionKind::X11));
        assert!(wants_portal(HotkeyMode::Portal, &SessionKind::X11));
        assert!(!wants_portal(HotkeyMode::None, &wayland));
    }

    #[test]
    fn the_instance_lock_excludes_a_second_holder_until_released() {
        let temp = TempDir::new();
        let path = temp.path().join("lock");
        let first = acquire_lock(&path).unwrap().expect("first holder");
        assert!(acquire_lock(&path).unwrap().is_none());
        drop(first);
        // Until it execs, a child that another test is forking right now shares
        // the lock, so the release can take a moment to show.
        let deadline = Instant::now() + Duration::from_secs(5);
        while acquire_lock(&path).unwrap().is_none() {
            assert!(Instant::now() < deadline, "the lock was never released");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn audio_files_are_private_and_removed_even_if_writing_fails() {
        let temp = TempDir::new();
        let audio = PrivateAudio::create(temp.path(), 7, b"RIFF").unwrap();
        let path = audio.path.clone();
        assert_eq!(fs::read(&path).unwrap(), b"RIFF");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(audio);
        assert!(!path.exists());

        // An existing file (a symlink planted here, say) is refused, not followed.
        fs::write(temp.path().join("job-8.wav"), b"old").unwrap();
        assert!(PrivateAudio::create(temp.path(), 8, b"new").is_err());
    }

    #[test]
    fn leftover_audio_is_swept_but_nothing_else() {
        let temp = TempDir::new();
        fs::write(temp.path().join("job-1.wav"), b"x").unwrap();
        fs::write(temp.path().join("wispx.lock"), b"").unwrap();
        fs::write(temp.path().join("job-notes.txt"), b"x").unwrap();
        remove_job_files(temp.path());
        assert!(!temp.path().join("job-1.wav").exists());
        assert!(temp.path().join("wispx.lock").exists());
        assert!(temp.path().join("job-notes.txt").exists());
    }
}
