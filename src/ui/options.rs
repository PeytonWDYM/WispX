use super::{Action, OptionsWindow, blur, chart};
use crate::{
    audio::recorder::{self, Recorder},
    hotkey::Hotkey,
    settings::{Settings, SettingsStore},
    stats::{self, StatsStore},
    transcribe::{
        auth,
        cleanup::{MAX_DICTIONARY_ENTRIES, normalize_dictionary},
    },
};
use slint::{
    ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel,
    winit_030::{
        EventResult, WinitWindowAccessor,
        winit::{
            event::WindowEvent,
            keyboard::{KeyCode, ModifiersState, PhysicalKey},
        },
    },
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

const VALIDATION: &str = "Use a modifier plus a key (like Ctrl + Shift + D), two modifiers, Right Ctrl or Right Alt alone, or an F key.";
const LANGUAGES: &[(&str, &str)] = &[
    ("Auto-detect", ""),
    ("English", "en"),
    ("Spanish", "es"),
    ("French", "fr"),
    ("German", "de"),
    ("Italian", "it"),
    ("Portuguese", "pt"),
    ("Dutch", "nl"),
    ("Polish", "pl"),
    ("Russian", "ru"),
    ("Ukrainian", "uk"),
    ("Turkish", "tr"),
    ("Arabic", "ar"),
    ("Hindi", "hi"),
    ("Japanese", "ja"),
    ("Korean", "ko"),
    ("Chinese", "zh"),
];

#[cfg(windows)]
const DEFAULT_MICROPHONE: &str = "Windows default";
#[cfg(not(windows))]
const DEFAULT_MICROPHONE: &str = "System default";
#[cfg(windows)]
const MICROPHONE_ERROR: &str = "Couldn't open this microphone. Check its Windows settings.";
#[cfg(not(windows))]
const MICROPHONE_ERROR: &str =
    "Couldn't open this microphone. Check your sound settings and microphone permissions.";

/// A shortcut the desktop binds on WispX's behalf, as on Wayland.
pub struct DesktopShortcut {
    /// Key names of the hold-to-talk shortcut, or a single explanatory entry.
    pub keys: Vec<String>,
    /// How to go hands-free, cancel and change the keys on this desktop.
    pub hint: String,
}

/// Host callbacks keep this UI usable without a Windows platform layer.
pub trait OptionsHost {
    /// Releases the closed window so the next open gets fresh pages and devices.
    fn closed(&self);
    fn begin_capture(&self) -> u64;
    fn cancel_capture(&self);
    fn dictating(&self) -> bool;
    fn startup(&self) -> bool;
    fn set_startup(&self, enabled: bool) -> Result<(), String>;
    fn settings_changed(&self, settings: &Settings);
    /// Present when the desktop, not WispX, owns the global shortcut.
    fn desktop_shortcut(&self) -> Option<DesktopShortcut> {
        None
    }
    /// Opens the desktop's own shortcut settings for a desktop-owned shortcut.
    fn configure_shortcut(&self) {}
}
pub struct Options {
    pub window: OptionsWindow,
    store: Rc<RefCell<SettingsStore>>,
    stats: Rc<RefCell<StatsStore>>,
    host: Option<Rc<dyn OptionsHost>>,
    test: RefCell<Option<Recorder>>,
    timer: Timer,
    capture_generation: Cell<Option<u64>>,
}
fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
    .into()
}
/// Rebuilds device-resolution masks on account refresh and monitor scale changes.
fn refresh_email_frames(window: &OptionsWindow) {
    let scale = window.window().scale_factor();
    let frames = blur::text(
        window.get_email().as_str(),
        window.get_text_font().as_str(),
        15.0,
        scale,
    );
    let (images, pad) = frames.map_or_else(|| (Vec::new(), 0.0), |f| (f.images, f.pad));
    window.set_email_frames(Rc::new(VecModel::from(images)).into());
    window.set_email_pad(pad);
    window.set_email_scale(scale);
}
impl Options {
    pub fn new(
        store: Rc<RefCell<SettingsStore>>,
        stats: Rc<RefCell<StatsStore>>,
        host: Option<Rc<dyn OptionsHost>>,
        tour: bool,
    ) -> Result<Rc<Self>, slint::PlatformError> {
        let window = OptionsWindow::new()?;
        #[cfg(windows)]
        {
            let (text, icons) = crate::platform::windows::fonts::options_families()
                .map_err(|e| slint::PlatformError::Other(e.to_string()))?;
            window.set_text_font(text.into());
            window.set_icon_font(icons.into());
        }
        #[cfg(target_os = "linux")]
        crate::platform::linux::desktop::style_options(&window);
        window.set_tour(tour);
        {
            let settings = &store.borrow().value;
            window.set_sounds(settings.sounds);
            window.set_lower_audio(settings.lower_audio);
            window.set_cleanup(settings.cleanup);
            // Unsupported hosts show it off without changing the saved choice.
            window.set_cleanup_context(settings.cleanup_context && window.get_context_supported());
            window.set_dictionary(strings(settings.dictionary.clone()));
            window.set_dictionary_limit(MAX_DICTIONARY_ENTRIES as i32);
            window.set_version(env!("CARGO_PKG_VERSION").into());
            window.set_idle_pill(settings.idle_pill);
            window.set_startup(host.as_ref().is_some_and(|h| h.startup()));
            let devices = recorder::input_devices().unwrap_or_default();
            let selected = devices
                .iter()
                .position(|d| d == &settings.microphone)
                .or_else(|| {
                    devices.iter().position(|d| {
                        !settings.microphone.is_empty() && d.starts_with(&settings.microphone)
                    })
                });
            window.set_microphone(selected.map_or(0, |i| i as i32 + 1));
            window.set_microphones(strings(
                std::iter::once(DEFAULT_MICROPHONE.into()).chain(devices),
            ));
            window.set_languages(strings(LANGUAGES.iter().map(|&(n, _)| n.to_owned())));
            window.set_language(
                LANGUAGES
                    .iter()
                    .position(|&(_, code)| code == settings.language)
                    .map_or(0, |i| i as i32),
            );
        }
        let ui = Rc::new(Self {
            window,
            store: store.clone(),
            stats,
            host,
            test: RefCell::new(None),
            timer: Timer::default(),
            capture_generation: Cell::new(None),
        });
        ui.refresh_account();
        ui.refresh_stats();
        ui.show_keys(false);
        let weak = Rc::downgrade(&ui);
        ui.window.on_action(move |action| {
            if let Some(ui) = weak.upgrade() {
                ui.action(action);
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.on_microphone_selected(move |index| {
            if let Some(ui) = weak.upgrade() {
                ui.stop_test();
                use slint::Model;
                ui.store.borrow_mut().value.microphone = if index == 0 {
                    String::new()
                } else {
                    ui.window
                        .get_microphones()
                        .row_data(index as usize)
                        .unwrap_or_default()
                        .to_string()
                };
                ui.save();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.on_language_selected(move |index| {
            if let Some(ui) = weak.upgrade() {
                ui.store.borrow_mut().value.language = LANGUAGES[index as usize].1.to_owned();
                ui.save();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.on_dictionary_add(move |entry| {
            if let Some(ui) = weak.upgrade() {
                let entries = {
                    let mut store = ui.store.borrow_mut();
                    store.value.dictionary = normalize_dictionary(
                        store
                            .value
                            .dictionary
                            .iter()
                            .map(String::as_str)
                            .chain(std::iter::once(entry.as_str())),
                    );
                    store.value.dictionary.clone()
                };
                ui.window.set_dictionary(strings(entries));
                ui.save();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.on_dictionary_remove(move |index| {
            if let Some(ui) = weak.upgrade() {
                let entries = {
                    let mut store = ui.store.borrow_mut();
                    store.value.dictionary.remove(index as usize);
                    store.value.dictionary.clone()
                };
                ui.window.set_dictionary(strings(entries));
                ui.save();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                ui.cleanup();
                ui.window.set_entered(false);
                if let Some(host) = &ui.host {
                    host.closed();
                }
            }
            slint::CloseRequestResponse::HideWindow
        });
        let modifiers = Cell::new(ModifiersState::empty());
        let weak = Rc::downgrade(&ui);
        ui.window.window().on_winit_window_event(move |_, event| {
            if tour
                && matches!(
                    event,
                    WindowEvent::KeyboardInput { .. }
                        | WindowEvent::MouseInput { .. }
                        | WindowEvent::MouseWheel { .. }
                        | WindowEvent::Touch(_)
                )
            {
                return EventResult::PreventDefault;
            }
            if let Some(ui) = weak.upgrade() {
                match event {
                    WindowEvent::ScaleFactorChanged { .. } => {
                        // Read Slint's effective scale after its backend consumes the event.
                        let weak = Rc::downgrade(&ui);
                        Timer::single_shot(Duration::ZERO, move || {
                            if let Some(ui) = weak.upgrade() {
                                refresh_email_frames(&ui.window);
                            }
                        });
                    }
                    WindowEvent::Focused(false) => ui.cleanup(),
                    WindowEvent::ModifiersChanged(m) => modifiers.set(m.state()),
                    WindowEvent::KeyboardInput { event, .. }
                        if event.state.is_pressed()
                            && event.physical_key == PhysicalKey::Code(KeyCode::Tab)
                            && modifiers.get().control_key() =>
                    {
                        let step = if modifiers.get().shift_key() { 5 } else { 1 };
                        ui.window.set_page((ui.window.get_page() + step) % 6);
                        return EventResult::PreventDefault;
                    }
                    _ => {}
                }
            }
            EventResult::Propagate
        });
        Ok(ui)
    }
    pub fn show(&self) -> Result<(), slint::PlatformError> {
        self.refresh_account();
        self.refresh_stats();
        let opening = !self.window.window().is_visible();
        self.window.show()?;
        if opening {
            let weak = self.window.as_weak();
            Timer::single_shot(Duration::from_millis(16), move || {
                if let Some(window) = weak.upgrade() {
                    // Winit realizes a newly shown window on the event loop, acquiring its DPI.
                    refresh_email_frames(&window);
                    window.set_entered(true);
                    window.set_page_ready(true);
                }
            });
        }
        self.window.window().set_minimized(false);
        self.window.window().with_winit_window(|w| w.focus_window());
        Ok(())
    }
    /// Updates totals and chart geometry after dictation, reset, or opening Options.
    pub fn refresh_stats(&self) {
        let today = stats::today();
        let summary = self.stats.borrow().value.summary(today);
        self.window.set_stats_totals(strings(
            [
                summary.total_words,
                summary.total_dictations,
                summary.average_wpm,
                summary.words_today,
            ]
            .map(chart::number),
        ));
        self.window.set_has_stats(summary.total_dictations > 0);
        let chart = chart::chart(&summary.daily);
        self.window.set_stats_line(chart.line.into());
        self.window.set_stats_area(chart.area.into());
        self.window
            .set_stats_maximum(chart::number(chart.top).into());
        self.window.set_stats_last_y(chart.last_y);
        let first = today - time::Duration::days(29);
        let month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ][first.month() as usize - 1];
        self.window
            .set_stats_first_date(format!("{month} {}", first.day()).into());
    }
    fn refresh_account(&self) {
        self.window.set_cleanup_available(
            auth::login().is_some_and(|login| login.mode == auth::AuthMode::ChatGpt),
        );
        let email = auth::email();
        self.window
            .set_email(email.as_deref().unwrap_or_default().into());
        refresh_email_frames(&self.window);
        self.window.set_account_detail(
            if email.is_some() {
                "Using your ChatGPT login from the Codex app"
            } else {
                "Open the Codex app and sign in"
            }
            .into(),
        );
    }
    /// Shows the current shortcut; the runtime calls this when the desktop rebinds it.
    pub fn show_keys(&self, animate: bool) {
        self.window.set_keycaps_ready(!animate);
        if let Some(shortcut) = self.host.as_ref().and_then(|h| h.desktop_shortcut()) {
            self.window.set_shortcut_capture(false);
            self.window.set_shortcut_hint(shortcut.hint.into());
            self.window.set_keys(strings(shortcut.keys));
        } else {
            self.window.set_keys(strings(
                self.store
                    .borrow()
                    .value
                    .hotkey
                    .to_string()
                    .split(" + ")
                    .map(str::to_owned),
            ));
        }
        if animate {
            let weak = self.window.as_weak();
            Timer::single_shot(Duration::from_millis(16), move || {
                if let Some(window) = weak.upgrade() {
                    window.set_keycaps_ready(true);
                }
            });
        }
    }
    fn save(&self) {
        let mut store = self.store.borrow_mut();
        match store.save() {
            Ok(()) => {
                if let Some(host) = &self.host {
                    host.settings_changed(&store.value);
                }
            }
            Err(error) => self.window.set_error(error.to_string().into()),
        }
    }
    pub fn stop_test(&self) {
        self.timer.stop();
        self.test.borrow_mut().take();
        self.window.set_testing(false);
        self.window.set_level(0.0);
    }
    pub fn cleanup(&self) {
        self.stop_test();
        self.cancel_capture();
    }
    fn cancel_capture(&self) {
        self.capture_generation.set(None);
        if self.window.get_capturing() {
            self.window.set_capturing(false);
            if let Some(host) = &self.host {
                host.cancel_capture();
            }
            self.show_keys(false);
        }
    }
    /// Called on the UI thread after the global hook finishes capture.
    pub fn captured(&self, generation: u64, key: Option<Hotkey>) {
        if self.capture_generation.get() != Some(generation) {
            return;
        }
        self.cancel_capture();
        if let Some(key) = key {
            if key.is_valid() {
                self.store.borrow_mut().value.hotkey = key;
                self.save();
                self.show_keys(true);
            } else {
                self.window.set_error(VALIDATION.into());
            }
        }
    }
    fn action(self: &Rc<Self>, action: Action) {
        match action {
            Action::CheckAccount => {
                self.refresh_account();
                self.window.set_account_ready(false);
                let weak = self.window.as_weak();
                Timer::single_shot(Duration::from_millis(16), move || {
                    if let Some(window) = weak.upgrade() {
                        window.set_account_ready(true);
                    }
                });
            }
            Action::ChangeShortcut => {
                if !self.window.get_shortcut_capture() {
                    if let Some(host) = &self.host {
                        host.configure_shortcut();
                    }
                } else if self.window.get_capturing() {
                    self.cancel_capture();
                } else if let Some(host) = &self.host {
                    self.window.set_error("".into());
                    self.window.set_capturing(true);
                    self.capture_generation.set(Some(host.begin_capture()));
                }
            }
            Action::ResetShortcut => {
                self.cancel_capture();
                self.window.set_error("".into());
                self.store.borrow_mut().value.hotkey = Hotkey::default();
                self.save();
                self.show_keys(true);
            }
            Action::TestMicrophone => self.toggle_test(),
            Action::ResetStats => {
                let result = self.stats.borrow_mut().reset();
                match result {
                    Ok(()) => {
                        self.window.set_stats_error("".into());
                        self.refresh_stats();
                    }
                    Err(error) => self
                        .window
                        .set_stats_error(format!("Couldn't reset statistics: {error}").into()),
                }
            }
            Action::Cleanup => {
                self.store.borrow_mut().value.cleanup = self.window.get_cleanup();
                self.save();
            }
            Action::CleanupContext => {
                self.store.borrow_mut().value.cleanup_context = self.window.get_cleanup_context();
                self.save();
            }
            Action::Sounds => {
                self.store.borrow_mut().value.sounds = self.window.get_sounds();
                self.save();
            }
            Action::LowerAudio => {
                self.store.borrow_mut().value.lower_audio = self.window.get_lower_audio();
                self.save();
            }
            Action::IdlePill => {
                self.store.borrow_mut().value.idle_pill = self.window.get_idle_pill();
                self.save();
            }
            Action::Startup => {
                if let Some(host) = &self.host {
                    if let Err(error) = host.set_startup(self.window.get_startup()) {
                        self.window.set_error(error.into());
                    } else {
                        self.save();
                    }
                }
            }
            Action::PageChanged => {
                self.cleanup();
                self.refresh_stats();
                self.window.set_page_ready(false);
                let weak = self.window.as_weak();
                Timer::single_shot(Duration::from_millis(16), move || {
                    if let Some(window) = weak.upgrade() {
                        window.set_page_ready(true);
                    }
                });
            }
            Action::Close => {
                if self.window.get_capturing() {
                    self.cancel_capture();
                } else {
                    self.cleanup();
                    self.window.set_entered(false);
                    let _ = self.window.hide();
                    if let Some(host) = &self.host {
                        host.closed();
                    }
                }
            }
        }
    }
    fn toggle_test(self: &Rc<Self>) {
        if !self.window.get_testing() {
            self.stop_test();
            return;
        }
        if self.host.as_ref().is_some_and(|h| h.dictating()) {
            self.stop_test();
            self.window
                .set_test_error("Finish dictating before testing your microphone.".into());
            return;
        }
        match Recorder::start(&self.store.borrow().value.microphone) {
            Ok(recorder) => {
                self.window.set_test_error("".into());
                let level = recorder.levels.clone();
                *self.test.borrow_mut() = Some(recorder);
                let weak = Rc::downgrade(self);
                self.timer
                    .start(TimerMode::Repeated, Duration::from_millis(33), move || {
                        if let Some(ui) = weak.upgrade() {
                            if ui.host.as_ref().is_some_and(|h| h.dictating()) {
                                ui.stop_test();
                            } else {
                                ui.window.set_level(level.level());
                            }
                        }
                    });
            }
            Err(_) => {
                self.stop_test();
                self.window.set_test_error(MICROPHONE_ERROR.into());
            }
        }
    }
}
