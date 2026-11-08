//! UI-thread application host. Worker results always return through Slint's event loop.
use super::{
    clipboard, context,
    hook::{Hook, HookMessage},
    installer,
    instance::InstanceEvent,
    overlay::Overlay,
    sound, startup,
    tray::{Tray, TrayAction},
};
use crate::{
    app::{Controller, DictationError, Finish, MessageKind, Pill},
    audio::{opus, sounds::Cue},
    hotkey_machine::HotkeyEvent,
    settings::{Settings, SettingsStore},
    transcribe::{ChatGpt, auth},
    ui::{Options, OptionsHost},
};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};
use windows::{
    Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IDOK, MB_OKCANCEL, MessageBoxW},
    core::w,
};
thread_local! { static APP: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) }; }
pub enum Event {
    Hook(HookMessage),
    Instance(InstanceEvent),
    Tray(TrayAction),
    Complete(Result<(String, bool), DictationError>),
}
pub fn post(event: Event) {
    let _ = slint::invoke_from_event_loop(move || {
        APP.with(|app| {
            if let Some(app) = app.borrow().as_ref() {
                app.event(event);
            }
        })
    });
}
pub struct Runtime {
    controller: RefCell<Controller>,
    store: Rc<RefCell<SettingsStore>>,
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
            Arc::new(|message| post(Event::Hook(message))),
        )?;
        let tray = Tray::new(
            settings.clone(),
            Arc::new(|action| post(Event::Tray(action))),
        )?;
        let client = ChatGpt::default();
        client.warmup();
        let app = Rc::new(Self {
            controller: RefCell::new(Controller::default()),
            store: Rc::new(RefCell::new(store)),
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
                app.max_timer.stop();
            }
        });
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
            Event::Instance(InstanceEvent::Quit) | Event::Tray(TrayAction::Quit) => {
                let _ = slint::quit_event_loop();
            }
            Event::Tray(TrayAction::CopyLast) => {
                if let Some(text) = &self.controller.borrow().last_transcript {
                    let _ = clipboard::set_text(text, false);
                }
            }
            Event::Tray(TrayAction::Help) => {
                let hotkey = &self.store.borrow().value.hotkey;
                self.tray.balloon("WispX",&format!("Hold {hotkey} → talk → let go. Text is typed where your cursor is.\n{hotkey}, then Space → hands-free. {hotkey} again to finish.\nEsc → cancel."),7000);
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
                let pill = self.controller.borrow_mut().complete(result);
                self.tray.update(
                    &self.store.borrow().value,
                    self.controller.borrow().last_transcript.is_some(),
                );
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
                        let pasted = !text.is_empty() && clipboard::paste(&text, target);
                        Ok((text, pasted))
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
            .is_some_and(|a| a.controller.borrow().recording())
    }
    fn startup(&self) -> bool {
        startup::enabled()
    }
    fn set_startup(&self, enabled: bool) -> Result<(), String> {
        startup::set(enabled).map_err(|e| e.to_string())
    }
    fn settings_changed(&self, settings: &Settings) {
        if let Some(app) = self.0.upgrade() {
            app.hook.set_hotkey(settings.hotkey.clone());
            app.overlay.set_idle_enabled(settings.idle_pill);
            if !app.controller.borrow().recording() && app.controller.borrow().in_flight == 0 {
                app.show(Pill::Idle);
            }
            app.tray
                .update(settings, app.controller.borrow().last_transcript.is_some());
        }
    }
}
