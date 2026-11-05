//! The app's brain. One thread owns all its state and handles messages in order,
//! as the daemon does, while anything that can block runs elsewhere and reports
//! back. So neither a slow tool nor a dialog waiting for an answer can freeze a
//! window, and the Qt side has nothing to do but draw.

use super::install;
use super::instance;
use super::link::{self, Connected, Exit, OwnedDaemon};
use super::models;
use super::overlay::{Kind, Mode, OverlayModel};
use super::view::{self, DownloadView, Model, Service};
use crate::audio;
use crate::config::{self, Backend, Config};
use crate::ipc::{self, ClientError, Request};
use crate::paths;
use crate::process::{self, Spec, Toolbox};
use crate::sys;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(50);
const RESTART_TRIES: u32 = 3;
const RESTART_PAUSE: Duration = Duration::from_secs(2);
/// A daemon that ran this long was working, so its end was not a startup failure.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
const STOP_LIMIT: Duration = Duration::from_secs(9);
const DEVICE_LIST_LIMIT: Duration = Duration::from_secs(3);
/// The most the first look at the settings waits for the shortcuts dialog, in case
/// it never came. Long enough to read it, and it isn't lost after this.
const SHORTCUT_QUESTION_WAIT: Duration = Duration::from_secs(20);

/// What the controller needs from the windows. Calls may come from any thread.
pub trait Ui: Send + Sync + 'static {
    fn set_overlay(&self, json: &str);
    fn set_state(&self, json: &str);
    fn set_tray(&self, json: &str);
    fn set_level(&self, level: f32);
    /// The token, if any, lets the desktop give the window focus.
    fn show_settings(&self, activation_token: Option<&str>);
    /// What is really on screen, as JSON, for `wispx-gui`'s `inspect` command.
    fn inspect(&self) -> String;
    fn quit(&self);
}

pub enum Msg {
    UiReady {
        overlay_available: bool,
        tray_available: bool,
    },
    Action {
        name: String,
        args: Value,
    },
    Instance {
        command: String,
        reply: Sender<Value>,
    },
    Connected {
        generation: u64,
        result: Option<Connected>,
    },
    Event {
        generation: u64,
        event: Value,
    },
    Closed {
        generation: u64,
    },
    ChildExit {
        generation: u64,
        exit: Exit,
    },
    Checks(Result<Vec<Value>, String>),
    Models(Vec<PathBuf>),
    Devices(Vec<(String, String)>),
    ActionDone(Result<String, String>),
    Copied(Result<String, String>),
    PasteDone(Result<String, String>),
    DownloadProgress(u64),
    DownloadDone(Result<PathBuf, String>),
    Stopped,
}

pub struct Options {
    pub config: Option<PathBuf>,
    pub background: bool,
}

pub struct Controller<U: Ui> {
    ui: Arc<U>,
    tx: Sender<Msg>,
    options: Options,
    model: Model,
    overlay: OverlayModel,
    generation: u64,
    owned: Option<Arc<OwnedDaemon>>,
    closer: Option<UnixStream>,
    connected_at: Option<Instant>,
    restarts: u32,
    restart_at: Option<Instant>,
    checks_running: bool,
    checks_again: bool,
    /// What the checks last depended on, so they rerun when it changes.
    checked_for: Option<Value>,
    /// Enough is known to decide whether the settings window should open by itself.
    first_facts_in: bool,
    first_checks_done: bool,
    shortcut_question_until: Option<Instant>,
    greeted: bool,
    download_cancel: Option<Arc<AtomicBool>>,
    quitting: bool,
    quit_replies: Vec<Sender<Value>>,
    shown_overlay: String,
    shown_state: String,
    shown_tray: String,
}

impl<U: Ui> Controller<U> {
    pub fn new(ui: Arc<U>, tx: Sender<Msg>, options: Options) -> Self {
        let mut model = Model::new(options.config.clone());
        model.autostart = install::autostart_enabled();
        Controller {
            ui,
            tx,
            options,
            model,
            overlay: OverlayModel::default(),
            generation: 0,
            owned: None,
            closer: None,
            connected_at: None,
            restarts: 0,
            restart_at: None,
            checks_running: false,
            checks_again: false,
            checked_for: None,
            first_facts_in: false,
            first_checks_done: false,
            shortcut_question_until: None,
            greeted: false,
            download_cancel: None,
            quitting: false,
            quit_replies: Vec::new(),
            shown_overlay: String::new(),
            shown_state: String::new(),
            shown_tray: String::new(),
        }
    }

    pub fn run(mut self, inbox: &Receiver<Msg>) {
        loop {
            match inbox.recv_timeout(TICK) {
                // Level changes come 30 times a second and affect nothing else.
                Ok(Msg::Event { generation, event })
                    if generation == self.generation && event["event"] == "level" =>
                {
                    self.ui
                        .set_level(event["level"].as_f64().unwrap_or(0.0) as f32);
                }
                Ok(Msg::Stopped) => break,
                Ok(message) => {
                    self.handle(message);
                    self.publish();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
        for reply in self.quit_replies.drain(..) {
            let _ = reply.send(json!({ "ok": true }));
        }
        self.ui.quit();
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.overlay.tick(now);
        self.publish_overlay();
        self.open_settings_if_needed();
        if sys::shutdown_requested() {
            self.begin_quit();
        }
        if self.restart_at.is_some_and(|at| now >= at) {
            self.restart_at = None;
            self.connect(true);
        }
    }

    fn publish(&mut self) {
        self.publish_overlay();
        let state = view::settings(&self.model).to_string();
        if state != self.shown_state {
            self.ui.set_state(&state);
            self.shown_state = state;
        }
        let tray = view::tray(&self.model).to_string();
        if tray != self.shown_tray {
            self.ui.set_tray(&tray);
            self.shown_tray = tray;
        }
    }

    fn publish_overlay(&mut self) {
        let overlay = self.overlay.to_json();
        let text = overlay.to_string();
        if text != self.shown_overlay {
            self.ui.set_overlay(&text);
            self.shown_overlay = text;
        }
    }

    fn handle(&mut self, message: Msg) {
        match message {
            Msg::UiReady {
                overlay_available,
                tray_available,
            } => {
                self.model.overlay_available = overlay_available;
                self.model.tray_available = tray_available;
                self.scan_models();
                self.connect(true);
            }
            Msg::Action { name, args } => self.action(&name, &args),
            Msg::Instance { command, reply } => self.instance_command(&command, reply),
            Msg::Connected { generation, result } if generation == self.generation => {
                self.connected(result);
            }
            Msg::Event { generation, event } if generation == self.generation => {
                self.daemon_event(event);
            }
            Msg::Closed { generation } if generation == self.generation => self.closed(),
            Msg::ChildExit { generation, exit } if generation == self.generation => {
                self.child_exited(exit);
            }
            Msg::Connected { result, .. } => {
                // Replaced by a newer attempt while this one was under way.
                if let Some(Connected::Started(_, owned)) = result {
                    owned.ask_to_stop();
                }
            }
            Msg::Event { .. } | Msg::Closed { .. } | Msg::ChildExit { .. } => {}
            Msg::Checks(result) => self.checks_done(result),
            Msg::Models(found) => self.model.models = found,
            Msg::Devices(found) => self.model.devices = found,
            Msg::ActionDone(result) => {
                self.finish_action(result);
                self.refresh();
            }
            Msg::Copied(result) => match result {
                Ok(text) => self
                    .overlay
                    .show_message(Kind::Success, &text, Instant::now()),
                Err(error) => self.model.problem = Some(error),
            },
            Msg::PasteDone(result) => {
                self.model.paste_busy = false;
                self.finish_action(result);
                self.refresh();
            }
            Msg::DownloadProgress(done) => {
                if let Some(download) = &mut self.model.download {
                    download.done = done;
                }
            }
            Msg::DownloadDone(result) => self.download_done(result),
            Msg::Stopped => {}
        }
    }

    fn finish_action(&mut self, result: Result<String, String>) {
        self.model.problem = result.err();
    }

    fn connect(&mut self, may_start: bool) {
        self.generation += 1;
        let generation = self.generation;
        self.model.service = Service::Connecting;
        self.model.status = None;
        self.model.checks = None;
        self.checked_for = None;
        let config = self.options.config.clone();
        let presents = self.model.overlay_available;
        let tx = self.tx.clone();
        let exit_tx = self.tx.clone();
        thread::spawn(move || {
            let result = link::connect(config.as_deref(), presents, may_start, move |exit| {
                let _ = exit_tx.send(Msg::ChildExit { generation, exit });
            });
            let _ = tx.send(Msg::Connected { generation, result });
        });
    }

    fn connected(&mut self, result: Option<Connected>) {
        let generation = self.generation;
        let (watch, owned) = match result {
            Some(Connected::Attached(watch)) => (watch, None),
            Some(Connected::Started(watch, owned)) => (watch, Some(Arc::new(owned))),
            Some(Connected::Failed(error)) => {
                self.model.service = Service::Failed(error);
                self.first_facts_in = true;
                self.open_settings_if_needed();
                return;
            }
            None => {
                self.model.service = Service::Stopped;
                return;
            }
        };
        self.model.service = Service::Running {
            owned: owned.is_some(),
        };
        self.owned = owned;
        self.connected_at = Some(Instant::now());
        let events = self.tx.clone();
        let ended = self.tx.clone();
        self.closer = link::pump(
            watch,
            move |event| {
                let _ = events.send(Msg::Event { generation, event });
            },
            move || {
                let _ = ended.send(Msg::Closed { generation });
            },
        );
    }

    fn daemon_event(&mut self, event: Value) {
        let now = Instant::now();
        match event["event"].as_str() {
            Some("status") => {
                let mode = Mode::from_state(event["state"].as_str().unwrap_or_default());
                if mode != self.overlay.mode() {
                    self.overlay.set_mode(mode, now);
                    if mode != Mode::Recording && mode != Mode::Locked {
                        self.ui.set_level(0.0);
                    }
                }
                self.overlay
                    .set_idle_pill(event["settings"]["idle_pill"].as_bool().unwrap_or(true));
                self.model.status = Some(event);
                self.refresh_if_changed();
                self.greet();
            }
            Some("message") => {
                let kind = Kind::parse(event["kind"].as_str().unwrap_or_default());
                let text = event["text"].as_str().unwrap_or_default();
                self.overlay.show_message(kind, text, now);
            }
            _ => {}
        }
    }

    /// The checks describe the settings in use, so they follow any change to them.
    fn refresh_if_changed(&mut self) {
        let Some(status) = &self.model.status else {
            return;
        };
        let key = json!([status["backend"], status["settings"], status["paste_state"]]);
        if self.checked_for.as_ref() != Some(&key) {
            self.checked_for = Some(key);
            self.refresh();
        }
    }

    fn greet(&mut self) {
        if self.greeted || self.options.background {
            return;
        }
        if let Some(keys) = self.model.hold_keys() {
            let text = format!("Hold {keys} to dictate");
            self.greeted = true;
            self.overlay.show_message(Kind::Info, &text, Instant::now());
        }
    }

    fn closed(&mut self) {
        self.closer = None;
        self.model.status = None;
        self.overlay.set_mode(Mode::Idle, Instant::now());
        // Nothing can dictate now, so nothing should look ready to.
        self.overlay.set_idle_pill(false);
        self.ui.set_level(0.0);
        // One of ours is about to report how it ended. Someone else's is just gone.
        if self.owned.is_none() && !self.quitting {
            self.model.service = Service::Stopped;
        }
    }

    fn child_exited(&mut self, exit: Exit) {
        self.owned = None;
        if self.quitting {
            return;
        }
        let ran_for = self.connected_at.map(|at| at.elapsed());
        match after_exit(&exit, ran_for, &mut self.restarts) {
            // Another daemon got the lock first, and it's the one being watched.
            AfterExit::Attach if self.closer.is_some() => {
                self.model.service = Service::Running { owned: false };
            }
            AfterExit::Attach => self.connect(false),
            AfterExit::Stopped => self.model.service = Service::Stopped,
            AfterExit::RestartIn(pause) => {
                self.model.service = Service::Connecting;
                self.restart_at = Some(Instant::now() + pause);
            }
            AfterExit::GiveUp => {
                self.model.service = Service::Failed(if exit.message.is_empty() {
                    "the background service keeps stopping".to_string()
                } else {
                    exit.message
                });
            }
        }
    }

    /// Runs the checks unless they are running already, in which case they go
    /// again afterwards so the answer reflects the latest settings.
    fn refresh(&mut self) {
        self.scan_models();
        if !matches!(self.model.service, Service::Running { .. }) {
            return;
        }
        if self.checks_running {
            self.checks_again = true;
            return;
        }
        self.checks_running = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = match ipc::send(&paths::client_socket_path(), Request::Readiness) {
                Ok(reply) if reply["ok"] == true => {
                    Ok(reply["checks"].as_array().cloned().unwrap_or_default())
                }
                Ok(reply) => Err(reply["message"].as_str().unwrap_or_default().to_string()),
                Err(ClientError::NotRunning) => Err("not running".to_string()),
                Err(ClientError::Failed(error)) => Err(error),
            };
            let _ = tx.send(Msg::Checks(result));
        });
    }

    fn checks_done(&mut self, result: Result<Vec<Value>, String>) {
        self.checks_running = false;
        if let Ok(checks) = result {
            self.model.checks = Some(checks);
            self.first_facts_in = true;
            self.open_settings_if_needed();
        }
        if std::mem::take(&mut self.checks_again) {
            self.refresh();
        }
    }

    /// The first time the facts are in: an opened app shows what is left to do,
    /// and an app started at login only speaks up if dictation can't work. The
    /// desktop's question about shortcuts goes first, since a window opening now
    /// would land on top of it, and it could go unanswered.
    fn open_settings_if_needed(&mut self) {
        if self.first_checks_done || !self.first_facts_in {
            return;
        }
        if self.model.asking_about_shortcuts() {
            let now = Instant::now();
            let until = *self
                .shortcut_question_until
                .get_or_insert(now + SHORTCUT_QUESTION_WAIT);
            if now < until {
                return;
            }
        }
        self.first_checks_done = true;
        let wanted = if self.options.background {
            view::is_broken(&self.model)
        } else {
            view::needs_attention(&self.model)
        };
        if wanted {
            self.show_settings(None);
        }
    }

    fn show_settings(&mut self, activation_token: Option<&str>) {
        self.ui.show_settings(activation_token);
    }

    fn scan_models(&self) {
        let tx = self.tx.clone();
        let device_config = self.device_config();
        thread::spawn(move || {
            if let Ok(dir) = models_dir() {
                let _ = tx.send(Msg::Models(models::scan(&dir)));
            }
            let _ = tx.send(Msg::Devices(list_devices(&device_config)));
        });
    }

    fn device_config(&self) -> Config {
        let mut config = Config::defaults().unwrap_or_else(|_| fallback_config());
        if let Some(status) = &self.model.status
            && let Some(recorder) = status["settings"]["recorder"]
                .as_str()
                .and_then(config::RecorderChoice::parse)
        {
            config.audio.recorder = recorder;
        }
        config
    }

    fn instance_command(&mut self, command: &str, reply: Sender<Value>) {
        let (command, token) = match command.split_once(' ') {
            Some((command, token)) => (command, Some(token)),
            None => (command, None),
        };
        match command {
            "ping" => {
                let _ = reply.send(json!({ "ok": true }));
            }
            "show" => {
                self.show_settings(token);
                self.refresh();
                let _ = reply.send(json!({ "ok": true }));
            }
            "quit" => {
                self.quit_replies.push(reply);
                self.begin_quit();
            }
            "inspect" => {
                let ui = Arc::clone(&self.ui);
                let overlay = self.overlay.to_json();
                let settings = view::settings(&self.model);
                let tray = view::tray(&self.model);
                let owned = self.owned.is_some();
                thread::spawn(move || {
                    let windows: Value = serde_json::from_str(&ui.inspect()).unwrap_or(Value::Null);
                    let _ = reply.send(json!({
                        "ok": true,
                        "windows": windows,
                        "overlay": overlay,
                        "settings": settings,
                        "tray": tray,
                        "owns_daemon": owned,
                    }));
                });
            }
            _ => {
                let _ = reply.send(json!({ "ok": false, "message": "unknown command" }));
            }
        }
    }

    fn action(&mut self, name: &str, args: &Value) {
        let checked = args["checked"].as_bool().unwrap_or(false);
        match name {
            "show-settings" | "how-to-use" => {
                self.show_settings(None);
                self.refresh();
            }
            "settings-opened" => {
                // It's up already, so the first look has nothing to add.
                self.first_checks_done = true;
                self.refresh();
            }
            "recheck" => self.refresh(),
            "quit" => self.begin_quit(),
            "copy-last" => self.in_background(Msg::Copied, || {
                send_ok(Request::CopyLast).map(|_| "Copied".to_string())
            }),
            "set-sounds" => self.configure(json!({ "sounds": checked })),
            "set-idle-pill" => self.configure(json!({ "idle_pill": checked })),
            "set-language" => {
                self.configure(json!({ "language": args["code"].as_str().unwrap_or_default() }));
            }
            "set-device" => {
                let device = args["id"].as_str().filter(|id| !id.is_empty());
                self.configure(json!({ "audio": { "device": device } }));
            }
            "set-model" => {
                if let Some(path) = args["path"].as_str() {
                    self.use_model(Path::new(path));
                }
            }
            "set-backend" => self.set_backend(args["backend"].as_str().unwrap_or_default()),
            "set-autostart" => match install::set_autostart(checked) {
                Ok(_) => {
                    self.model.autostart = install::autostart_enabled();
                    self.model.problem = None;
                }
                Err(error) => self.model.problem = Some(error),
            },
            "enable-paste" => self.enable_paste(),
            "download-model" => self.start_download(args["id"].as_str().unwrap_or("base")),
            "cancel-download" => {
                if let Some(cancel) = &self.download_cancel {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            "open-shortcuts" => self.open_shortcuts(),
            "open-config" => self.open_config(),
            "start-service" => {
                self.restarts = 0;
                self.restart_at = None;
                self.model.problem = None;
                self.first_checks_done = true;
                self.connect(true);
            }
            "stop-service" => self.in_background(Msg::ActionDone, || {
                send_ok(Request::Quit).map(|_| "Stopped".to_string())
            }),
            "tray-status" => {
                self.model.tray_available = args["available"].as_bool().unwrap_or(false);
            }
            _ => {}
        }
    }

    fn in_background<T: Send + 'static>(
        &self,
        message: impl FnOnce(T) -> Msg + Send + 'static,
        work: impl FnOnce() -> T + Send + 'static,
    ) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(message(work()));
        });
    }

    /// Saves settings through the daemon, which applies them together. With none
    /// running, the file is what its next start will read.
    fn configure(&self, patch: Value) {
        let path = self.config_path();
        self.in_background(Msg::ActionDone, move || {
            match ipc::send(
                &paths::client_socket_path(),
                Request::Configure(patch.to_string()),
            ) {
                Ok(reply) if reply["ok"] == true => Ok("Saved".to_string()),
                Ok(reply) => Err(reply["message"].as_str().unwrap_or_default().to_string()),
                Err(ClientError::NotRunning) => {
                    let path = path?;
                    config::persist_patch(&path, &patch).map(|()| "Saved".to_string())
                }
                Err(ClientError::Failed(error)) => Err(error),
            }
        });
    }

    fn set_backend(&self, name: &str) {
        let Some(backend) = Backend::parse(name) else {
            return;
        };
        let path = self.config_path();
        self.in_background(Msg::ActionDone, move || {
            match ipc::send(&paths::client_socket_path(), Request::SetBackend(backend)) {
                Ok(reply) if reply["ok"] == true => Ok("Saved".to_string()),
                Ok(reply) => Err(reply["message"].as_str().unwrap_or_default().to_string()),
                Err(ClientError::NotRunning) => {
                    let path = path?;
                    config::persist_backend(&path, backend).map(|()| "Saved".to_string())
                }
                Err(ClientError::Failed(error)) => Err(error),
            }
        });
    }

    fn config_path(&self) -> Result<PathBuf, String> {
        match (&self.model.status, &self.options.config) {
            (Some(status), _) if status["config"].is_string() => {
                Ok(PathBuf::from(status["config"].as_str().unwrap_or_default()))
            }
            (_, Some(path)) => Ok(path.clone()),
            _ => paths::default_config_file(),
        }
    }

    /// An English-only model can't auto-detect, so it gets the language it needs.
    fn use_model(&self, path: &Path) {
        let mut patch = json!({ "whisper": { "model": path.to_string_lossy() } });
        let language = self
            .model
            .status
            .as_ref()
            .and_then(|status| status["settings"]["language"].as_str())
            .unwrap_or_default();
        if models::is_english_only(path) && language.is_empty() {
            patch["language"] = json!("en");
        }
        self.configure(patch);
    }

    fn enable_paste(&mut self) {
        if self.model.paste_busy {
            return;
        }
        self.model.paste_busy = true;
        self.model.problem = None;
        self.in_background(Msg::PasteDone, || {
            send_ok(Request::PasteSetup).map(|message| message.to_string())
        });
    }

    fn start_download(&mut self, id: &str) {
        if self.model.download.is_some() {
            return;
        }
        let Some(choice) = models::choice(id) else {
            return;
        };
        let dir = match models_dir() {
            Ok(dir) => dir,
            Err(error) => {
                self.model.problem = Some(error);
                return;
            }
        };
        self.model.problem = None;
        self.model.download = Some(DownloadView {
            label: choice.label.to_string(),
            bytes: choice.bytes,
            done: 0,
        });
        let cancel = Arc::new(AtomicBool::new(false));
        self.download_cancel = Some(Arc::clone(&cancel));
        let tx = self.tx.clone();
        let progress = self.tx.clone();
        thread::spawn(move || {
            let result =
                models::download(choice, &dir, &Toolbox::from_env(), &cancel, move |done| {
                    let _ = progress.send(Msg::DownloadProgress(done));
                });
            let _ = tx.send(Msg::DownloadDone(result));
        });
    }

    fn download_done(&mut self, result: Result<PathBuf, String>) {
        let cancelled = self
            .download_cancel
            .take()
            .is_some_and(|cancel| models::was_cancelled(&cancel));
        self.model.download = None;
        match result {
            Ok(path) => {
                self.use_model(&path);
                self.scan_models();
            }
            Err(_) if cancelled => {}
            Err(error) => self.model.problem = Some(error),
        }
    }

    fn open_shortcuts(&mut self) {
        let tools = Toolbox::from_env();
        let result = if let Some(program) = tools.find("systemsettings") {
            spawn_detached(&program, &["kcm_keys"])
        } else if let Some(program) = tools.find("kcmshell6") {
            spawn_detached(&program, &["kcm_keys"])
        } else {
            Err(
                "System Settings was not found. Change the keys in your desktop's settings"
                    .to_string(),
            )
        };
        self.model.problem = result.err();
    }

    fn open_config(&mut self) {
        let result = self.config_path().and_then(|path| {
            let program = Toolbox::from_env()
                .find("xdg-open")
                .ok_or_else(|| format!("Open {} in a text editor", path.display()))?;
            spawn_detached(&program, &[&path.to_string_lossy()])
        });
        self.model.problem = result.err();
    }

    /// The pill goes first, so quitting looks instant however long the rest takes.
    fn begin_quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.restart_at = None;
        if let Some(cancel) = &self.download_cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        self.overlay.set_idle_pill(false);
        self.overlay.set_mode(Mode::Idle, Instant::now());
        self.publish_overlay();
        if let Some(closer) = self.closer.take() {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        }
        let owned = self.owned.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            if let Some(owned) = owned {
                owned.stop_and_wait(STOP_LIMIT);
            }
            let _ = tx.send(Msg::Stopped);
        });
    }
}

/// What to do about a daemon this app started that has ended.
#[derive(Debug, PartialEq, Eq)]
enum AfterExit {
    Attach,
    Stopped,
    RestartIn(Duration),
    GiveUp,
}

fn after_exit(exit: &Exit, ran_for: Option<Duration>, restarts: &mut u32) -> AfterExit {
    if exit.code == Some(crate::daemon::EXIT_ALREADY_RUNNING) {
        return AfterExit::Attach;
    }
    if exit.clean {
        return AfterExit::Stopped;
    }
    if ran_for.is_some_and(|ran_for| ran_for >= HEALTHY_RUN) {
        *restarts = 0;
    }
    *restarts += 1;
    if *restarts <= RESTART_TRIES {
        AfterExit::RestartIn(RESTART_PAUSE * *restarts)
    } else {
        AfterExit::GiveUp
    }
}

/// Sends a request and returns its message, or says why it did not work.
fn send_ok(request: Request) -> Result<String, String> {
    match ipc::send(&paths::client_socket_path(), request) {
        Ok(reply) if reply["ok"] == true => {
            Ok(reply["message"].as_str().unwrap_or_default().to_string())
        }
        Ok(reply) => Err(reply["message"].as_str().unwrap_or_default().to_string()),
        Err(ClientError::NotRunning) => Err("The background service is not running".to_string()),
        Err(ClientError::Failed(error)) => Err(error),
    }
}

fn models_dir() -> Result<PathBuf, String> {
    Ok(paths::data_dir()?.join("models"))
}

fn fallback_config() -> Config {
    // Only reachable with neither HOME nor XDG directories set, and then the app
    // has bigger problems than which recorder it would list devices for.
    Config::parse("{}").unwrap_or_else(|_| panic!("the built-in defaults must load"))
}

/// Opens something for the user and lets go of it, reaping it when it ends.
fn spawn_detached(program: &Path, args: &[&str]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The microphones `audio.device` can name, read without opening any of them. The
/// recorder is what has to understand the name, so it decides whether to offer them.
fn list_devices(config: &Config) -> Vec<(String, String)> {
    let tools = Toolbox::from_env();
    let Ok(plan) = audio::resolve(&config.audio, &tools) else {
        return Vec::new();
    };
    let recorder = process::short_name(&plan.program);
    if recorder != "pw-record" && recorder != "parec" {
        return Vec::new();
    }
    let Some(pactl) = tools.find("pactl") else {
        return Vec::new();
    };
    let args: Vec<OsString> = vec!["--format=json".into(), "list".into(), "sources".into()];
    let Ok(output) = process::run(Spec::new(&pactl, &args, DEVICE_LIST_LIMIT)) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_devices(&output.stdout)
}

fn parse_devices(stdout: &[u8]) -> Vec<(String, String)> {
    let Ok(Value::Array(sources)) = serde_json::from_slice::<Value>(stdout) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|source| {
            let name = source["name"].as_str()?;
            if name.ends_with(".monitor") {
                return None;
            }
            let label = source["description"].as_str().unwrap_or(name);
            Some((name.to_string(), label.to_string()))
        })
        .collect()
}

/// Helps the launcher hand a later launch over to the running app.
pub fn serve_instance(runtime: &Path, tx: Sender<Msg>) -> Result<(), String> {
    instance::serve(runtime, move |command, reply| {
        let _ = tx.send(Msg::Instance { command, reply });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc;

    #[derive(Default)]
    struct FakeUi {
        overlays: Mutex<Vec<Value>>,
        states: Mutex<Vec<Value>>,
        levels: Mutex<Vec<f32>>,
        settings_shown: Mutex<Vec<Option<String>>>,
        quits: Mutex<u32>,
    }

    impl Ui for FakeUi {
        fn set_overlay(&self, json: &str) {
            let value = serde_json::from_str(json).unwrap();
            self.overlays.lock().unwrap().push(value);
        }
        fn set_state(&self, json: &str) {
            let value = serde_json::from_str(json).unwrap();
            self.states.lock().unwrap().push(value);
        }
        fn set_tray(&self, _json: &str) {}
        fn set_level(&self, level: f32) {
            self.levels.lock().unwrap().push(level);
        }
        fn show_settings(&self, activation_token: Option<&str>) {
            let token = activation_token.map(str::to_string);
            self.settings_shown.lock().unwrap().push(token);
        }
        fn inspect(&self) -> String {
            "{}".to_string()
        }
        fn quit(&self) {
            *self.quits.lock().unwrap() += 1;
        }
    }

    fn controller(
        background: bool,
    ) -> (Controller<FakeUi>, Arc<FakeUi>, Sender<Msg>, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let ui = Arc::new(FakeUi::default());
        let options = Options {
            config: None,
            background,
        };
        let controller = Controller::new(Arc::clone(&ui), tx.clone(), options);
        (controller, ui, tx, rx)
    }

    /// A status the controller has already run its checks for, so this test stays
    /// on this thread and asks nothing of the real machine.
    fn status_event(controller: &mut Controller<FakeUi>, state: &str) -> Value {
        let event = json!({
            "event": "status",
            "state": state,
            "backend": "local",
            "settings": { "idle_pill": true },
            "paste_state": "automatic",
        });
        controller.checked_for = Some(json!([
            event["backend"],
            event["settings"],
            event["paste_state"]
        ]));
        event
    }

    fn crash() -> Exit {
        Exit {
            clean: false,
            code: Some(1),
            message: String::new(),
        }
    }

    #[test]
    fn a_crash_is_retried_a_few_times_then_explained() {
        let mut restarts = 0;
        let quick = Some(Duration::from_secs(1));
        for pause in [2, 4, 6] {
            assert_eq!(
                after_exit(&crash(), quick, &mut restarts),
                AfterExit::RestartIn(Duration::from_secs(pause))
            );
        }
        assert_eq!(
            after_exit(&crash(), quick, &mut restarts),
            AfterExit::GiveUp
        );

        // A daemon that had been working has earned a fresh start.
        assert_eq!(
            after_exit(&crash(), Some(HEALTHY_RUN), &mut restarts),
            AfterExit::RestartIn(RESTART_PAUSE)
        );
        assert_eq!(restarts, 1);
    }

    #[test]
    fn losing_the_race_attaches_and_a_clean_stop_stays_stopped() {
        let mut restarts = 0;
        let lost = Exit {
            clean: false,
            code: Some(crate::daemon::EXIT_ALREADY_RUNNING),
            message: String::new(),
        };
        assert_eq!(after_exit(&lost, None, &mut restarts), AfterExit::Attach);
        let clean = Exit {
            clean: true,
            code: Some(0),
            message: String::new(),
        };
        assert_eq!(after_exit(&clean, None, &mut restarts), AfterExit::Stopped);
        assert_eq!(restarts, 0, "neither counts as a failure");
    }

    #[test]
    fn the_pill_follows_the_daemon_and_ignores_an_old_connection() {
        let (mut controller, ui, tx, rx) = controller(true);
        let recording = status_event(&mut controller, "recording");
        let stale = status_event(&mut controller, "transcribing");
        let idle = status_event(&mut controller, "idle");
        let events = [
            (0, recording),
            (9, stale),
            (0, json!({ "event": "level", "level": 0.5 })),
            (9, json!({ "event": "level", "level": 0.9 })),
            (0, idle),
            (
                0,
                json!({ "event": "message", "kind": "error", "text": "It broke" }),
            ),
        ];
        for (generation, event) in events {
            tx.send(Msg::Event { generation, event }).unwrap();
        }
        tx.send(Msg::Stopped).unwrap();
        controller.run(&rx);

        let overlays = ui.overlays.lock().unwrap();
        assert!(overlays.iter().any(|o| o["mode"] == "recording"));
        assert!(!overlays.iter().any(|o| o["mode"] == "transcribing"));
        let last = overlays.last().unwrap();
        assert_eq!(last["message"], "It broke");
        assert_eq!(last["kind"], "error");
        // The level is set back to nothing when recording ends.
        assert_eq!(*ui.levels.lock().unwrap(), [0.5, 0.0]);
        assert_eq!(*ui.quits.lock().unwrap(), 1);
    }

    #[test]
    fn a_service_that_cannot_start_speaks_up_even_at_login() {
        let (mut controller, ui, _tx, _rx) = controller(true);
        let failed = || Msg::Connected {
            generation: 0,
            result: Some(Connected::Failed("bad settings file".to_string())),
        };
        controller.handle(failed());
        controller.publish();
        assert_eq!(*ui.settings_shown.lock().unwrap(), [None]);
        let states = ui.states.lock().unwrap().clone();
        let rows = states.last().unwrap()["rows"].as_array().unwrap().clone();
        assert!(rows.iter().any(|row| {
            row["level"] == "fail"
                && row["detail"]
                    .as_str()
                    .unwrap()
                    .contains("bad settings file")
        }));

        // Said once. A person who closed the window isn't asked again.
        controller.handle(failed());
        assert_eq!(ui.settings_shown.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_first_look_waits_for_the_desktops_question_but_not_forever() {
        let (mut controller, ui, _tx, _rx) = controller(false);
        controller.first_facts_in = true;
        controller.model.service = Service::Running { owned: true };
        let status = |hotkey: &str| {
            Some(json!({
                "state": "idle",
                "hotkey_state": hotkey,
                "paste_state": "needs-permission",
            }))
        };

        controller.model.status = status("starting");
        controller.open_settings_if_needed();
        assert!(ui.settings_shown.lock().unwrap().is_empty());

        // Answered: there it is, once.
        controller.model.status = status("bound");
        controller.open_settings_if_needed();
        controller.open_settings_if_needed();
        assert_eq!(*ui.settings_shown.lock().unwrap(), [None]);

        // A question that never comes doesn't keep it shut.
        let (mut controller, ui, _tx, _rx) = super::tests::controller(false);
        controller.first_facts_in = true;
        controller.model.service = Service::Running { owned: true };
        controller.model.status = status("starting");
        controller.shortcut_question_until = Some(Instant::now());
        controller.open_settings_if_needed();
        assert_eq!(*ui.settings_shown.lock().unwrap(), [None]);
    }

    #[test]
    fn quitting_hides_the_pill_answers_and_closes_the_windows() {
        let (mut controller, ui, tx, rx) = controller(true);
        controller.overlay.set_idle_pill(true);
        let (reply, answer) = mpsc::channel();
        tx.send(Msg::Instance {
            command: "quit".to_string(),
            reply,
        })
        .unwrap();
        controller.run(&rx);

        assert_eq!(answer.recv().unwrap()["ok"], true);
        assert_eq!(*ui.quits.lock().unwrap(), 1);
        let overlays = ui.overlays.lock().unwrap();
        assert_eq!(overlays.last().unwrap()["idle_pill"], false);
    }

    #[test]
    fn an_unknown_command_is_refused() {
        let (mut controller, ui, _tx, _rx) = controller(true);
        let (reply, answer) = mpsc::channel();
        controller.instance_command("ping", reply.clone());
        assert_eq!(answer.recv().unwrap()["ok"], true);
        controller.instance_command("format", reply);
        assert_eq!(answer.recv().unwrap()["ok"], false);
        assert!(ui.settings_shown.lock().unwrap().is_empty());
    }

    #[test]
    fn device_lists_leave_out_monitors_and_survive_junk() {
        let text = br#"[
            {"name":"alsa_output.monitor","description":"Monitor of speakers"},
            {"name":"alsa_input.usb","description":"USB microphone"},
            {"name":"bare"}
        ]"#;
        assert_eq!(
            parse_devices(text),
            vec![
                ("alsa_input.usb".to_string(), "USB microphone".to_string()),
                ("bare".to_string(), "bare".to_string()),
            ]
        );
        assert!(parse_devices(b"not json").is_empty());
        assert!(parse_devices(b"{}").is_empty());
    }
}
