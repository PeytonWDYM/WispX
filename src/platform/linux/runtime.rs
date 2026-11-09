//! UI-thread application host, structured like the Windows runtime. Worker results
//! always return through Slint's event loop.
use super::{
    desktop,
    hotkey::{DesktopStatus, HookMessage, Hotkeys},
    instance::InstanceEvent,
    overlay::Overlay,
    paste::Paster,
    session::{Protocol, Session},
    sound,
    tray::{Tray, TrayAction},
};
use crate::{
    app::{Controller, Delivery, DictationError, Finish, MessageKind, Pill},
    audio::{opus, sounds::Cue},
    hotkey_machine::HotkeyEvent,
    settings::{Settings, SettingsStore},
    transcribe::{ChatGpt, auth},
    ui::{DesktopShortcut, Options, OptionsHost},
};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};
thread_local! { static APP: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) }; }
pub enum Event {
    Hook(HookMessage),
    Instance(InstanceEvent),
    Tray(TrayAction),
    Complete(Result<(String, Delivery), DictationError>),
    /// Copy last transcript failed; the reason for the pill.
    CopyFailed(String),
}
pub fn post(event: Event) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = APP.with(|app| app.borrow().clone()) {
            app.event(event);
        }
    });
}
pub struct Runtime {
    controller: RefCell<Controller>,
    store: Rc<RefCell<SettingsStore>>,
    options: RefCell<Option<Rc<Options>>>,
    hotkeys: Hotkeys,
    overlay: Overlay,
    tray: Tray,
    paster: Arc<Paster>,
    client: ChatGpt,
    max_timer: slint::Timer,
    /// What the desktop bound on Wayland; `None` until the portal answers.
    desktop: RefCell<Option<DesktopStatus>>,
    /// The welcome pill and first-run notice wait for the portal on Wayland.
    greeted: Cell<bool>,
    background: bool,
}
impl Runtime {
    pub fn start(
        store: SettingsStore,
        background: bool,
    ) -> Result<Rc<Self>, Box<dyn std::error::Error>> {
        let session = Session::detect();
        if session.protocol == Protocol::None {
            return Err("No graphical session: set WAYLAND_DISPLAY or DISPLAY".into());
        }
        let settings = store.value.clone();
        let overlay = Overlay::new(settings.idle_pill);
        let hotkeys = Hotkeys::start(
            &session,
            settings.hotkey.clone(),
            Arc::new(|message| post(Event::Hook(message))),
        );
        let paster = Paster::start(&session);
        let tray = Tray::new(
            settings.hotkey.to_string(),
            Arc::new(|action| post(Event::Tray(action))),
        );
        let client = ChatGpt::default();
        client.warmup();
        let app = Rc::new(Self {
            controller: RefCell::new(Controller::default()),
            store: Rc::new(RefCell::new(store)),
            options: RefCell::new(None),
            hotkeys,
            overlay,
            tray,
            paster,
            client,
            max_timer: slint::Timer::default(),
            desktop: RefCell::new(None),
            greeted: Cell::new(false),
            background,
        });
        APP.with(|slot| *slot.borrow_mut() = Some(app.clone()));
        if !app.hotkeys.desktop_owned() {
            app.greet()?;
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
                app.max_timer.stop();
            }
        });
    }
    /// The hold shortcut as the user must press it.
    fn hold_text(&self) -> String {
        match &*self.desktop.borrow() {
            Some(DesktopStatus::Bound { hold, .. }) => hold.clone(),
            _ => self.store.borrow().value.hotkey.to_string(),
        }
    }
    fn usage(&self) -> String {
        match &*self.desktop.borrow() {
            Some(DesktopStatus::Bound {
                hold,
                hands_free,
                cancel,
            }) => format!(
                "Hold {hold} → talk → let go. Text is typed where your cursor is.\n\
                 {hands_free} → hands-free. {hold} again to finish.\n{cancel} → cancel."
            ),
            _ => {
                let hotkey = &self.store.borrow().value.hotkey;
                format!(
                    "Hold {hotkey} → talk → let go. Text is typed where your cursor is.\n\
                     {hotkey}, then Space → hands-free. {hotkey} again to finish.\nEsc → cancel."
                )
            }
        }
    }
    fn greet(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.greeted.replace(true) {
            return Ok(());
        }
        let hold = self.hold_text();
        if !self.background {
            let pill = if auth::login().is_none() {
                Pill::message("Sign in to the Codex app first", MessageKind::Warn, 4.0)
            } else {
                Pill::message(format!("Hold {hold} to dictate"), MessageKind::Info, 2.8)
            };
            self.show(pill);
        }
        if !self.store.borrow().value.first_run_done {
            {
                let mut store = self.store.borrow_mut();
                store.value.first_run_done = true;
                store.save()?;
            }
            let mut message = self.usage();
            if !self.tray.available() {
                message.push_str("\nNo tray here: launch WispX again for Options.");
            }
            self.tray.balloon("WispX is running", &message, 6000);
        }
        Ok(())
    }
    fn show(&self, pill: Pill) {
        if let Err(error) = self.overlay.show(pill) {
            eprintln!("Overlay: {error}");
        }
    }
    fn cue(&self, cue: Cue) {
        if self.store.borrow().value.sounds {
            sound::play(cue);
        }
    }
    fn refresh_tray(&self) {
        self.tray.update(
            self.hold_text(),
            self.controller.borrow().last_transcript.is_some(),
        );
    }
    fn event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Hook(HookMessage::Dictation(event)) => self.hotkey(event),
            Event::Hook(HookMessage::Captured { generation, key }) => {
                if let Some(ui) = self.options.borrow().as_ref() {
                    ui.captured(generation, key);
                }
            }
            Event::Hook(HookMessage::Conflict(message)) => {
                // Never over a live recording's pill; the log keeps it either way.
                if !self.controller.borrow().recording() {
                    self.show(Pill::message(message, MessageKind::Warn, 4.0));
                }
            }
            Event::Hook(HookMessage::Desktop(status)) => {
                let unavailable = match &status {
                    DesktopStatus::Unavailable(reason) => Some(reason.clone()),
                    DesktopStatus::Bound { .. } => None,
                };
                *self.desktop.borrow_mut() = Some(status);
                self.refresh_tray();
                if let Some(ui) = self.options.borrow().as_ref() {
                    ui.show_keys(false);
                }
                match unavailable {
                    Some(reason) => {
                        eprintln!("Shortcuts: {reason}");
                        if !self.greeted.get() {
                            self.greeted.set(true);
                            self.show(Pill::message(
                                "Global shortcuts unavailable. See Options",
                                MessageKind::Warn,
                                4.0,
                            ));
                        }
                    }
                    None => {
                        if let Err(error) = self.greet() {
                            eprintln!("Settings: {error}");
                        }
                    }
                }
            }
            Event::Instance(InstanceEvent::Wake) | Event::Tray(TrayAction::Options) => {
                self.open_options()
            }
            Event::Instance(InstanceEvent::Quit) | Event::Tray(TrayAction::Quit) => {
                let _ = slint::quit_event_loop();
            }
            Event::Tray(TrayAction::CopyLast) => {
                if let Some(text) = self.controller.borrow().last_transcript.clone() {
                    // Waits for any paste that's borrowing the clipboard, so off the UI thread.
                    let paster = self.paster.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = paster.copy(&text) {
                            post(Event::CopyFailed(error));
                        }
                    });
                }
            }
            Event::CopyFailed(error) => {
                eprintln!("Clipboard: {error}");
                self.show(Pill::message(error, MessageKind::Error, 2.6));
            }
            Event::Tray(TrayAction::Help) => {
                self.tray.balloon("WispX", &self.usage(), 7000);
            }
            Event::Complete(result) => {
                let pill = self.controller.borrow_mut().complete(result);
                self.refresh_tray();
                if let Some(pill) = pill {
                    self.show(pill);
                }
            }
        }
    }
    fn open_options(self: &Rc<Self>) {
        if self.options.borrow().is_none() {
            let host = Rc::new(Host(Rc::downgrade(self)));
            match Options::new(self.store.clone(), Some(host), false) {
                Ok(ui) => *self.options.borrow_mut() = Some(ui),
                Err(error) => {
                    eprintln!("Options: {error}");
                    self.show(Pill::message(error.to_string(), MessageKind::Error, 2.6));
                    return;
                }
            }
        }
        if let Some(ui) = self.options.borrow().as_ref()
            && let Err(error) = ui.show()
        {
            eprintln!("Options: {error}");
            self.show(Pill::message(error.to_string(), MessageKind::Error, 2.6));
        }
    }
    fn hotkey(self: &Rc<Self>, event: HotkeyEvent) {
        match event {
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
                        // Linux has no accessibility reader for nearby text yet; cleanup
                        // runs without context rather than guessing at it.
                        self.show(pill);
                        self.cue(Cue::Start);
                        let weak = Rc::downgrade(self);
                        self.max_timer.start(
                            slint::TimerMode::SingleShot,
                            crate::app::MAX_RECORDING,
                            move || {
                                if let Some(app) = weak.upgrade() {
                                    app.hotkeys.force_idle();
                                    app.finish();
                                }
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.hotkeys.force_idle();
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
        let finished = self.controller.borrow_mut().finish();
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
                let paster = self.paster.clone();
                // The window focused as dictation ends is the only paste target.
                // Asking can take KWin a moment, so it runs beside the recorder
                // stopping rather than before it; no answer means copy only.
                let focus = {
                    let paster = paster.clone();
                    std::thread::spawn(move || paster.focused())
                };
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
                        let text = client.cleanup(text, settings.cleanup, "", &settings.dictionary);
                        let target = focus.join().ok().flatten();
                        let delivery = if text.is_empty() {
                            Delivery::Copied
                        } else {
                            paster.deliver(&text, target.as_ref())
                        };
                        Ok((text, delivery))
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
            .hotkeys
            .begin_capture()
    }
    fn cancel_capture(&self) {
        if let Some(app) = self.0.upgrade() {
            app.hotkeys.cancel_capture();
        }
    }
    fn dictating(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|a| a.controller.borrow().recording())
    }
    fn startup(&self) -> bool {
        desktop::startup_enabled()
    }
    fn set_startup(&self, enabled: bool) -> Result<(), String> {
        desktop::set_startup(enabled).map_err(|e| e.to_string())
    }
    fn settings_changed(&self, settings: &Settings) {
        if let Some(app) = self.0.upgrade() {
            app.hotkeys.set_hotkey(settings.hotkey.clone());
            app.overlay.set_idle_enabled(settings.idle_pill);
            if !app.controller.borrow().recording() && app.controller.borrow().in_flight == 0 {
                app.show(Pill::Idle);
            }
            app.refresh_tray();
        }
    }
    fn desktop_shortcut(&self) -> Option<DesktopShortcut> {
        let app = self.0.upgrade()?;
        if !app.hotkeys.desktop_owned() {
            return None;
        }
        Some(match &*app.desktop.borrow() {
            Some(status) => status.shortcut(),
            None => DesktopShortcut {
                keys: vec!["Waiting for your desktop…".into()],
                hint: "Confirm WispX's shortcuts in your desktop's dialog.".into(),
            },
        })
    }
    fn configure_shortcut(&self) {
        if let Some(app) = self.0.upgrade() {
            app.hotkeys.configure();
        }
    }
}
