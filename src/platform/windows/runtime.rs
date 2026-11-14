//! UI-thread application host. Worker results always return through Slint's event loop.
use super::{
    clipboard, context, files,
    hook::{Hook, HookMessage},
    installer,
    instance::InstanceEvent,
    overlay::Overlay,
    sound, startup,
    tray::{Tray, TrayAction},
};
use crate::{
    app::{Controller, Delivery, Dictation, DictationError, Finish, MessageKind, Pill},
    audio::{duck, opus, sounds::Cue},
    hotkey_machine::HotkeyEvent,
    listen::{self, Listen, Stage},
    settings::{Settings, SettingsStore},
    stats::StatsStore,
    transcribe::{ChatGpt, auth},
    ui::{ListenAction, Options, OptionsHost},
};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};
use windows::{
    Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IDOK, MB_OKCANCEL, MessageBoxW},
    core::w,
};
thread_local! { static APP: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) }; }
/// Options' Listen page, for opening it from the tray or after Stop.
const LISTEN_PAGE: i32 = 4;
pub enum Event {
    Hook(HookMessage),
    Instance(InstanceEvent),
    Tray(TrayAction),
    Complete(Result<Dictation, DictationError>),
    Listen(u64, listen::Update),
}
pub fn post(event: Event) {
    let _ = slint::invoke_from_event_loop(move || {
        // Clone out of the slot: nested message loops (MessageBox, menus) dispatch more events.
        if let Some(app) = APP.with(|app| app.borrow().clone()) {
            app.event(event);
        }
    });
}
pub struct Runtime {
    controller: RefCell<Controller>,
    listen: Rc<RefCell<Listen>>,
    /// Ends a Listen capture at `listen::MAX_SESSION`.
    listen_timer: slint::Timer,
    /// Brings the Listen pill back after a message shown over it.
    pill_restore: slint::Timer,
    store: Rc<RefCell<SettingsStore>>,
    stats: Rc<RefCell<StatsStore>>,
    options: RefCell<Option<Rc<Options>>>,
    hook: Hook,
    overlay: Overlay,
    tray: Tray,
    client: ChatGpt,
    max_timer: slint::Timer,
    recording_context: RefCell<Option<context::Capture>>,
}
impl Runtime {
    pub fn start(
        store: SettingsStore,
        background: bool,
    ) -> Result<Rc<Self>, Box<dyn std::error::Error>> {
        let settings = store.value.clone();
        let overlay = Overlay::new(settings.idle_pill)?;
        let hook = Hook::start(
            settings.hotkey.clone(),
            settings.listen_hotkey.clone(),
            Arc::new(|message| post(Event::Hook(message))),
        )?;
        let tray = Tray::new(
            settings.clone(),
            Arc::new(|action| post(Event::Tray(action))),
        )?;
        let client = ChatGpt::default();
        client.warmup();
        // Restores playback a crashed WispX left lowered.
        duck::start();
        let listen = Listen::new(
            Arc::new(client.clone()),
            Arc::new(|generation, update| post(Event::Listen(generation, update))),
        );
        let app = Rc::new(Self {
            controller: RefCell::new(Controller::default()),
            listen: Rc::new(RefCell::new(listen)),
            listen_timer: slint::Timer::default(),
            pill_restore: slint::Timer::default(),
            store: Rc::new(RefCell::new(store)),
            stats: Rc::new(RefCell::new(StatsStore::load()?)),
            options: RefCell::new(None),
            hook,
            overlay,
            tray,
            client,
            max_timer: slint::Timer::default(),
            recording_context: RefCell::new(None),
        });
        APP.with(|slot| *slot.borrow_mut() = Some(app.clone()));
        if !background {
            let pill = if auth::login().is_none() {
                Pill::message("Sign in to the Codex app first", MessageKind::Warn, 4.0)
            } else {
                Pill::message(
                    format!("Hold {} to dictate", settings.hotkey),
                    MessageKind::Info,
                    2.8,
                )
            };
            app.show(pill);
        }
        if !settings.first_run_done {
            {
                let mut store = app.store.borrow_mut();
                store.value.first_run_done = true;
                store.save()?;
            }
            app.tray.balloon(
                "WispX is running",
                &format!(
                    "Hold {} and talk. Let go to type it.\n{} + Space = hands-free. Esc = cancel.",
                    settings.hotkey, settings.hotkey
                ),
                6000,
            );
        }
        Ok(app)
    }
    pub fn shutdown() {
        APP.with(|slot| {
            if let Some(app) = slot.borrow_mut().take() {
                if let Some(ui) = app.options.borrow_mut().take() {
                    ui.cleanup();
                }
                app.controller.borrow_mut().cancel();
                // Stops capture, uploads and notes requests; nothing of it is written anywhere.
                app.listen.borrow_mut().shutdown();
                app.max_timer.stop();
                app.listen_timer.stop();
                // Give lowered playback a moment to return before the process exits.
                duck::flush(Duration::from_secs(3));
            }
        });
    }
    /// Shows a pill, keeping Listen's visible while it captures: dictation's idle or
    /// finished states give way to it, and a message gives it back once it expires.
    fn show(&self, pill: Pill) {
        let listening = self.listen.borrow().pill();
        let pill = match (listening, pill) {
            (Some(listening), Pill::Idle | Pill::Done | Pill::Transcribing)
                if !self.controller.borrow().recording() =>
            {
                listening
            }
            (
                Some(_),
                Pill::Message {
                    text,
                    kind,
                    seconds,
                },
            ) => {
                self.pill_restore.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_secs_f64(seconds),
                    || {
                        if let Some(app) = APP.with(|app| app.borrow().clone())
                            && let Some(pill) = app.listen.borrow().pill()
                        {
                            app.show(pill);
                        }
                    },
                );
                Pill::Message {
                    text,
                    kind,
                    seconds,
                }
            }
            (_, pill) => pill,
        };
        if let Err(error) = self.overlay.show(pill) {
            eprintln!("Overlay: {error}");
        }
    }
    fn usage(&self) -> String {
        let settings = &self.store.borrow().value;
        let hotkey = &settings.hotkey;
        let listen = settings
            .listen_shortcut()
            .map_or("Tray → Start listening".into(), |key| key.to_string());
        format!(
            "Hold {hotkey} → talk → let go. Text is typed where your cursor is.\n\
             {hotkey}, then Space → hands-free. {hotkey} again to finish.\nEsc → cancel.\n\
             {listen} → listen to computer audio and write notes."
        )
    }
    fn refresh_tray(&self) {
        self.tray.update(
            &self.store.borrow().value,
            self.controller.borrow().last_transcript.is_some(),
            self.listen.borrow().tray_item(),
        );
    }
    /// After any Listen change: the shortcut's Esc, the tray item, and the page.
    fn sync_listen(&self) {
        let capturing = self.listen.borrow().capturing();
        self.hook.set_listening(capturing);
        if !capturing {
            self.listen_timer.stop();
        }
        self.refresh_tray();
        if let Some(ui) = self.options.borrow().as_ref() {
            ui.refresh_listen();
        }
    }
    fn listen_refused(self: &Rc<Self>, message: &str) {
        self.show(Pill::message(message, MessageKind::Warn, 3.5));
        if let Some(ui) = self.options.borrow().as_ref() {
            ui.listen_error(message);
        }
    }
    fn start_listen(self: &Rc<Self>) {
        if auth::login().is_none() {
            self.listen_refused("Sign in to the Codex app first");
            return;
        }
        if self.controller.borrow().recording() {
            self.listen_refused("Finish dictating first, then start Listen.");
            return;
        }
        if let Some(ui) = self.options.borrow().as_ref() {
            ui.stop_test();
        }
        let request = listen::Request::from_settings(&self.store.borrow().value);
        let result = self.listen.borrow_mut().start(request);
        match result {
            Ok(()) => {
                self.cue(Cue::Start);
                let weak = Rc::downgrade(self);
                self.listen_timer.start(
                    slint::TimerMode::SingleShot,
                    listen::MAX_SESSION,
                    move || {
                        if let Some(app) = weak.upgrade() {
                            app.stop_listen();
                        }
                    },
                );
                if let Some(pill) = self.listen.borrow().pill() {
                    self.pill_restore.stop();
                    if let Err(error) = self.overlay.show(pill) {
                        eprintln!("Overlay: {error}");
                    }
                }
            }
            Err(error) => {
                self.listen_refused(&error);
                if error == listen::UNSAVED {
                    self.open_options_page(LISTEN_PAGE);
                }
            }
        }
        self.sync_listen();
    }
    fn stop_listen(self: &Rc<Self>) {
        if self.listen.borrow_mut().stop() {
            self.cue(Cue::Stop);
            self.sync_listen();
            if !self.controller.borrow().recording() {
                self.show(Pill::Transcribing);
            }
            self.open_options_page(LISTEN_PAGE);
        }
    }
    fn toggle_listen(self: &Rc<Self>) {
        let stage = self.listen.borrow().stage();
        match stage {
            Stage::Listening => self.stop_listen(),
            Stage::Setup | Stage::Notes => self.start_listen(),
            _ => self.listen_refused("Listen is still writing your notes."),
        }
    }
    fn discard_listen(self: &Rc<Self>, cue: bool) {
        if self.listen.borrow_mut().discard() {
            if cue {
                self.cue(Cue::Cancel);
            }
            self.sync_listen();
            if !self.controller.borrow().recording() && self.controller.borrow().in_flight == 0 {
                self.show(Pill::Idle);
            }
        }
    }
    fn listen_action(self: &Rc<Self>, action: ListenAction) {
        match action {
            ListenAction::Start => self.start_listen(),
            ListenAction::Stop => self.stop_listen(),
            ListenAction::Discard => self.discard_listen(false),
            ListenAction::NewSession => {
                self.listen.borrow_mut().close_notes();
                self.sync_listen();
            }
            ListenAction::RetrySummary => {
                self.listen.borrow_mut().retry_summary();
                self.sync_listen();
            }
            ListenAction::Copy(text) => {
                if !text.is_empty() {
                    self.show(match clipboard::set_text(&text, false) {
                        Ok(()) => Pill::message("Copied", MessageKind::Success, 1.6),
                        Err(error) => Pill::message(error.message(), MessageKind::Error, 2.6),
                    });
                }
            }
            ListenAction::Save => {
                let Some(request) = self.listen.borrow().save_request() else {
                    return;
                };
                // SAFETY: only reads the foreground window handle, the Options window that was clicked.
                let owner = unsafe { GetForegroundWindow() };
                // The dialog runs a message loop, so a summary may arrive before it returns.
                let result = files::choose_save_path(owner, &request.name).and_then(|path| {
                    let Some(path) = path else {
                        return Ok(None);
                    };
                    crate::settings::atomic_write(&path, request.text.as_bytes())
                        .map(|()| Some(path))
                        .map_err(|e| format!("Couldn't save the notes: {e}"))
                });
                match result {
                    Ok(Some(path)) => {
                        let marked = self.listen.borrow_mut().mark_saved(
                            request.generation,
                            request.revision,
                            path,
                        );
                        self.sync_listen();
                        if !marked
                            && request.generation == self.listen.borrow().generation()
                            && let Some(ui) = self.options.borrow().as_ref()
                        {
                            ui.listen_error(listen::CHANGED_WHILE_SAVING);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if let Some(ui) = self.options.borrow().as_ref() {
                            ui.listen_error(&error);
                        }
                    }
                }
            }
        }
    }
    fn request_quit(self: &Rc<Self>) {
        let blocked = self.listen.borrow().blocks_quit();
        if blocked {
            self.open_options_page(LISTEN_PAGE);
            self.listen_refused("Save or discard your Listen session before quitting.");
        } else {
            let _ = slint::quit_event_loop();
        }
    }
    fn cue(&self, cue: Cue) {
        if self.store.borrow().value.sounds {
            sound::play(cue);
        }
    }
    fn event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Hook(HookMessage::Dictation(event)) => self.hotkey(event),
            Event::Hook(HookMessage::Captured { generation, key }) => {
                if let Some(ui) = self.options.borrow().as_ref() {
                    ui.captured(generation, key);
                }
            }
            Event::Instance(InstanceEvent::Wake) | Event::Tray(TrayAction::Options) => {
                self.open_options()
            }
            Event::Tray(TrayAction::Listen) => self.toggle_listen(),
            Event::Tray(TrayAction::ListenPage) => self.open_options_page(LISTEN_PAGE),
            Event::Listen(generation, update) => {
                let pill = self.listen.borrow_mut().update(generation, update);
                self.sync_listen();
                if let Some(pill) = pill
                    && !self.controller.borrow().recording()
                {
                    self.show(pill);
                }
            }
            Event::Instance(InstanceEvent::Quit) | Event::Tray(TrayAction::Quit) => {
                self.request_quit();
            }
            Event::Tray(TrayAction::CopyLast) => {
                if let Some(text) = &self.controller.borrow().last_transcript {
                    let _ = clipboard::set_text(text, false);
                }
            }
            Event::Tray(TrayAction::Help) => {
                self.tray.balloon("WispX", &self.usage(), 7000);
            }
            Event::Tray(TrayAction::Uninstall) => {
                // SAFETY: a standard modal dialog uses constant strings and no borrowed callback state.
                if unsafe {
                    MessageBoxW(
                        None,
                        w!("Remove WispX from this PC?\n\nYour Codex/ChatGPT login isn't touched."),
                        w!("Uninstall WispX"),
                        MB_OKCANCEL,
                    )
                } == IDOK
                {
                    if let Err(error) = installer::uninstall() {
                        self.show(Pill::message(error.to_string(), MessageKind::Error, 2.6));
                    } else {
                        let _ = slint::quit_event_loop();
                    }
                }
            }
            Event::Complete(result) => {
                if let Ok(dictation) = &result
                    && let Err(error) = self
                        .stats
                        .borrow_mut()
                        .record(&dictation.text, dictation.spoken)
                {
                    eprintln!("Statistics: {error}");
                }
                if let Some(ui) = self.options.borrow().as_ref() {
                    ui.refresh_stats();
                }
                let pill = self.controller.borrow_mut().complete(result);
                self.refresh_tray();
                if let Some(pill) = pill {
                    self.show(pill);
                }
            }
        }
    }
    fn open_options_page(self: &Rc<Self>, page: i32) {
        self.open_options();
        if let Some(ui) = self.options.borrow().as_ref() {
            ui.show_page(page);
        }
    }
    fn open_options(self: &Rc<Self>) {
        if self.options.borrow().is_none() {
            let host = Rc::new(Host(Rc::downgrade(self)));
            match Options::new(self.store.clone(), self.stats.clone(), Some(host), false) {
                Ok(ui) => *self.options.borrow_mut() = Some(ui),
                Err(error) => {
                    self.show(Pill::message(error.to_string(), MessageKind::Error, 2.6));
                    return;
                }
            }
        }
        if let Some(ui) = self.options.borrow().as_ref()
            && let Err(error) = ui.show()
        {
            self.show(Pill::message(error.to_string(), MessageKind::Error, 2.6));
        }
    }
    fn hotkey(self: &Rc<Self>, event: HotkeyEvent) {
        match event {
            HotkeyEvent::ListenToggle => self.toggle_listen(),
            HotkeyEvent::ListenCancel => self.discard_listen(true),
            HotkeyEvent::Start if self.listen.borrow().capturing() => {
                self.hook.force_idle();
                let stop = self
                    .store
                    .borrow()
                    .value
                    .listen_shortcut()
                    .map_or("Stop it in Options".into(), |key| {
                        format!("Press {key} to stop it")
                    });
                self.show(Pill::message(
                    format!("Listen is running. {stop}"),
                    MessageKind::Warn,
                    3.0,
                ));
            }
            HotkeyEvent::Start => {
                if let Some(ui) = self.options.borrow().as_ref() {
                    ui.stop_test();
                }
                let result = self
                    .controller
                    .borrow_mut()
                    .start(&self.store.borrow().value);
                match result {
                    Ok(Some(pill)) => {
                        let settings = &self.store.borrow().value;
                        if settings.cleanup
                            && settings.cleanup_context
                            && auth::login()
                                .is_some_and(|login| login.mode == auth::AuthMode::ChatGpt)
                        {
                            *self.recording_context.borrow_mut() = context::begin();
                        }
                        self.show(pill);
                        self.cue(Cue::Start);
                        let weak = Rc::downgrade(self);
                        self.max_timer.start(
                            slint::TimerMode::SingleShot,
                            crate::app::MAX_RECORDING,
                            move || {
                                if let Some(app) = weak.upgrade() {
                                    app.hook.force_idle();
                                    app.finish();
                                }
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.hook.force_idle();
                        let (kind, seconds) = match error {
                            crate::app::StartError::NoLogin => (MessageKind::Warn, 3.5),
                            crate::app::StartError::Microphone(_) => (MessageKind::Error, 2.6),
                        };
                        self.show(Pill::message(error.to_string(), kind, seconds));
                    }
                }
            }
            HotkeyEvent::Lock => {
                if self.controller.borrow().recording() {
                    self.show(Pill::Locked);
                }
            }
            HotkeyEvent::Release | HotkeyEvent::StopLocked => self.finish(),
            HotkeyEvent::Cancel | HotkeyEvent::CancelSilent => {
                self.recording_context.borrow_mut().take();
                self.max_timer.stop();
                let pill = self.controller.borrow_mut().cancel();
                if let Some(pill) = pill {
                    if event == HotkeyEvent::Cancel {
                        self.cue(Cue::Cancel);
                    }
                    self.show(pill);
                }
            }
        }
    }
    fn finish(&self) {
        self.max_timer.stop();
        // SAFETY: the current HWND is stored as an opaque value and compared only by the paste worker.
        let target = unsafe { GetForegroundWindow() }.0 as isize;
        let finished = self.controller.borrow_mut().finish();
        let surrounding = if finished.is_some() {
            self.recording_context.borrow_mut().take()
        } else {
            None
        };
        match finished {
            None => {}
            Some(Finish::Quiet) => {
                if self.controller.borrow().in_flight == 0 {
                    self.show(Pill::Idle);
                }
            }
            Some(Finish::TooQuiet) => {
                self.cue(Cue::Stop);
                self.show(Pill::message(
                    "Didn't hear anything. Check your mic?",
                    MessageKind::Warn,
                    2.6,
                ));
            }
            Some(Finish::Upload(recorder)) => {
                self.cue(Cue::Stop);
                self.show(Pill::Transcribing);
                let client = self.client.clone();
                let settings = self.store.borrow().value.clone();
                std::thread::spawn(move || {
                    let result = (|| {
                        let audio = recorder.stop()?;
                        let text = client
                            .transcribe(
                                &opus::encode(&audio.pcm)
                                    .map_err(crate::transcribe::TranscribeError::from)?,
                                audio.duration_ms,
                                &settings.language,
                            )?
                            .trim()
                            .to_owned();
                        let surrounding = surrounding
                            .filter(|_| settings.cleanup_context)
                            .and_then(context::Capture::finish)
                            .unwrap_or_default();
                        let text = client.cleanup(
                            text,
                            settings.cleanup,
                            &surrounding,
                            &settings.dictionary,
                        );
                        let text = crate::transcribe::cleanup::join_context(text, &surrounding);
                        let delivery = if !text.is_empty() && clipboard::paste(&text, target) {
                            Delivery::Pasted
                        } else {
                            Delivery::FocusChanged
                        };
                        Ok(Dictation {
                            text,
                            delivery,
                            spoken: Duration::from_millis(audio.duration_ms),
                        })
                    })();
                    post(Event::Complete(result));
                });
            }
        }
    }
}
struct Host(Weak<Runtime>);
impl OptionsHost for Host {
    fn closed(&self) {
        if let Some(app) = self.0.upgrade() {
            app.options.borrow_mut().take();
        }
    }
    fn begin_capture(&self) -> u64 {
        self.0
            .upgrade()
            .expect("Options belongs to the live app")
            .hook
            .begin_capture()
    }
    fn cancel_capture(&self) {
        if let Some(app) = self.0.upgrade() {
            app.hook.cancel_capture();
        }
    }
    fn dictating(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|a| a.controller.borrow().recording() || a.listen.borrow().capturing())
    }
    fn startup(&self) -> bool {
        startup::enabled()
    }
    fn set_startup(&self, enabled: bool) -> Result<(), String> {
        startup::set(enabled).map_err(|e| e.to_string())
    }
    fn settings_changed(&self, settings: &Settings) {
        if let Some(app) = self.0.upgrade() {
            app.hook
                .set_hotkeys(settings.hotkey.clone(), settings.listen_hotkey.clone());
            app.overlay.set_idle_enabled(settings.idle_pill);
            if !app.controller.borrow().recording() && app.controller.borrow().in_flight == 0 {
                app.show(Pill::Idle);
            }
            app.refresh_tray();
        }
    }
    fn listen(&self) -> Option<Rc<RefCell<Listen>>> {
        Some(self.0.upgrade()?.listen.clone())
    }
    fn listen_action(&self, action: ListenAction) {
        if let Some(app) = self.0.upgrade() {
            // Deferred: the page's own callback is still on the stack and may be replaced.
            slint::Timer::single_shot(Duration::ZERO, move || app.listen_action(action));
        }
    }
}
