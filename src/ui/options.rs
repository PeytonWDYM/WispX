use super::{Action, ListenLine, ListenSource, ListenTask, OptionsWindow, blur, chart};
use crate::{
    audio::{
        capture::{self, Device},
        recorder::{self, Levels, Recorder},
    },
    hotkey::{Hotkey, listen_conflict},
    listen::{Listen, Stage, SummaryState, assembly::clock},
    settings::{Settings, SettingsStore},
    stats::{self, StatsStore},
    transcribe::{
        auth,
        cleanup::{MAX_DICTIONARY_ENTRIES, normalize_dictionary},
    },
};
use slint::{
    ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
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
const DEFAULT_OUTPUT: &str = "Windows default output";
#[cfg(not(windows))]
const DEFAULT_OUTPUT: &str = "System default output";
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
    /// Key names of the Listen toggle, or a single explanatory entry.
    pub listen: Vec<String>,
    pub listen_hint: String,
    /// The Listen trigger as the desktop shows it; empty when none is bound.
    pub listen_trigger: String,
}
/// What the Listen page asks of the host, which owns the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListenAction {
    Start,
    Stop,
    Discard,
    Save,
    Copy(String),
    NewSession,
    RetrySummary,
}

/// Host callbacks keep this UI usable without a Windows platform layer.
pub trait OptionsHost {
    /// Releases the closed window so the next open gets fresh pages and devices.
    fn closed(&self);
    fn begin_capture(&self) -> u64;
    fn cancel_capture(&self);
    /// Dictation or Listen holds the audio, so the microphone test must wait.
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
    /// The Listen session, which outlives this window.
    fn listen(&self) -> Option<Rc<RefCell<Listen>>> {
        None
    }
    fn listen_action(&self, _action: ListenAction) {}
}
pub struct Options {
    pub window: OptionsWindow,
    store: Rc<RefCell<SettingsStore>>,
    stats: Rc<RefCell<StatsStore>>,
    host: Option<Rc<dyn OptionsHost>>,
    test: RefCell<Option<Recorder>>,
    timer: Timer,
    capture_generation: Cell<Option<u64>>,
    /// Listen's choosable outputs and microphones, after the default entry.
    listen_devices: RefCell<(Vec<Device>, Vec<Device>)>,
    listen_levels: Rc<VecModel<f32>>,
    listen_timer: Timer,
    listen_stage: Cell<Option<Stage>>,
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
            window.set_cleanup_context(settings.cleanup_context);
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
            listen_devices: RefCell::default(),
            listen_levels: Rc::new(VecModel::default()),
            listen_timer: Timer::default(),
            listen_stage: Cell::new(None),
        });
        ui.window.set_listen_levels(ui.listen_levels.clone().into());
        ui.window
            .set_listen_include_microphone(store.borrow().value.listen_include_microphone);
        ui.refresh_account();
        ui.refresh_stats();
        ui.show_keys(false);
        ui.load_listen_devices();
        ui.refresh_listen();
        let weak = Rc::downgrade(&ui);
        ui.window.on_listen_output_selected(move |index| {
            if let Some(ui) = weak.upgrade() {
                let id = ui.listen_device(true, index);
                ui.store.borrow_mut().value.listen_output = id;
                ui.save();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.on_listen_microphone_selected(move |index| {
            if let Some(ui) = weak.upgrade() {
                let id = ui.listen_device(false, index);
                ui.store.borrow_mut().value.listen_microphone = id;
                ui.save();
            }
        });
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
                        let step = if modifiers.get().shift_key() { 6 } else { 1 };
                        ui.window.set_page((ui.window.get_page() + step) % 7);
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
    /// Switches to a page, as when the tray opens Listen.
    pub fn show_page(&self, page: i32) {
        self.window.set_page(page);
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
        self.window.set_signed_in(auth::login().is_some());
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
        let split = |key: &Hotkey| strings(key.to_string().split(" + ").map(str::to_owned));
        if let Some(shortcut) = self.host.as_ref().and_then(|h| h.desktop_shortcut()) {
            self.window.set_shortcut_capture(false);
            self.window.set_shortcut_hint(shortcut.hint.into());
            self.window.set_keys(strings(shortcut.keys));
            self.window
                .set_listen_shortcut_hint(shortcut.listen_hint.into());
            self.window
                .set_listen_shortcut(shortcut.listen_trigger.into());
            self.window.set_listen_keys(strings(shortcut.listen));
            self.window.set_listen_shortcut_error("".into());
        } else {
            let settings = &self.store.borrow().value;
            self.window.set_keys(split(&settings.hotkey));
            self.window.set_listen_keys(split(&settings.listen_hotkey));
            let conflict = listen_conflict(&settings.hotkey, &settings.listen_hotkey);
            self.window.set_listen_shortcut(match conflict {
                None => settings.listen_hotkey.to_string().into(),
                Some(_) => "".into(),
            });
            self.window.set_listen_shortcut_error(
                conflict
                    .map(|reason| format!("The Listen shortcut is off: {reason}"))
                    .unwrap_or_default()
                    .into(),
            );
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
        // Released before the host runs, since the host reads the same store.
        let saved = {
            let mut store = self.store.borrow_mut();
            store.save().map(|()| store.value.clone())
        };
        match saved {
            Ok(settings) => {
                if let Some(host) = &self.host {
                    host.settings_changed(&settings);
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
        let listen = self.window.get_capture_target() == 1;
        self.cancel_capture();
        let Some(key) = key else {
            return;
        };
        let rejected = {
            let settings = &self.store.borrow().value;
            if listen {
                listen_conflict(&settings.hotkey, &key).map(str::to_owned)
            } else if !key.is_valid() {
                Some(VALIDATION.to_owned())
            } else {
                listen_conflict(&key, &settings.listen_hotkey).map(|_| {
                    format!(
                        "That would clash with the Listen shortcut ({}). Change that first.",
                        settings.listen_hotkey
                    )
                })
            }
        };
        if let Some(reason) = rejected {
            if listen {
                self.window.set_listen_shortcut_error(reason.into());
            } else {
                self.window.set_error(reason.into());
            }
            return;
        }
        {
            let mut store = self.store.borrow_mut();
            if listen {
                store.value.listen_hotkey = key;
            } else {
                store.value.hotkey = key;
            }
        }
        self.save();
        self.show_keys(true);
    }
    fn change_shortcut(&self, target: i32) {
        if !self.window.get_shortcut_capture() {
            if let Some(host) = &self.host {
                host.configure_shortcut();
            }
        } else if self.window.get_capturing() {
            self.cancel_capture();
        } else if let Some(host) = &self.host {
            self.window.set_error("".into());
            self.window.set_listen_shortcut_error("".into());
            self.window.set_capture_target(target);
            self.window.set_capturing(true);
            self.capture_generation.set(Some(host.begin_capture()));
        }
    }
    /// Lists Listen's real devices, keeping a saved device that is unplugged now as
    /// a visible, unavailable choice rather than quietly switching to another.
    fn load_listen_devices(&self) {
        let (devices, error) = match capture::devices() {
            Ok(devices) => (devices, String::new()),
            Err(error) => (capture::Devices::default(), error.0),
        };
        let settings = &self.store.borrow().value;
        let choose = |mut list: Vec<Device>, saved: &str| -> (Vec<Device>, i32) {
            if saved.is_empty() {
                return (list, 0);
            }
            if let Some(index) = list.iter().position(|d| d.id == saved) {
                return (list, index as i32 + 1);
            }
            list.push(Device {
                id: saved.to_owned(),
                name: format!("{saved} (not connected)"),
            });
            let index = list.len() as i32;
            (list, index)
        };
        let (outputs, output) = choose(devices.outputs, &settings.listen_output);
        let (microphones, microphone) = choose(devices.microphones, &settings.listen_microphone);
        let names = |default: &str, list: &[Device]| {
            strings(std::iter::once(default.to_owned()).chain(list.iter().map(|d| d.name.clone())))
        };
        self.window
            .set_listen_outputs(names(DEFAULT_OUTPUT, &outputs));
        self.window.set_listen_output(output);
        self.window
            .set_listen_microphones(names(DEFAULT_MICROPHONE, &microphones));
        self.window.set_listen_microphone(microphone);
        self.window.set_listen_error(error.into());
        *self.listen_devices.borrow_mut() = (outputs, microphones);
    }
    fn listen_device(&self, output: bool, index: i32) -> String {
        let devices = self.listen_devices.borrow();
        let list = if output { &devices.0 } else { &devices.1 };
        usize::try_from(index - 1)
            .ok()
            .and_then(|i| list.get(i))
            .map(|d| d.id.clone())
            .unwrap_or_default()
    }
    /// Shows a host-side Listen error, such as a refused start.
    pub fn listen_error(&self, message: &str) {
        self.window.set_listen_error(message.into());
    }
    /// Redraws the Listen page from the host's session.
    pub fn refresh_listen(&self) {
        let Some(listen) = self.host.as_ref().and_then(|h| h.listen()) else {
            return;
        };
        let listen = listen.borrow();
        let window = &self.window;
        let stage = listen.stage();
        window.set_listen_stage(match stage {
            Stage::Setup => 0,
            Stage::Listening => 1,
            Stage::Finishing => 2,
            Stage::Formatting => 3,
            Stage::Summarizing => 4,
            Stage::Notes => 5,
        });
        let sources = listen.sources();
        window.set_listen_sources(
            Rc::new(VecModel::from(
                sources
                    .iter()
                    .enumerate()
                    .map(|(i, (device, _))| ListenSource {
                        label: if i == 0 {
                            "Computer audio"
                        } else {
                            "My microphone"
                        }
                        .into(),
                        device: device.into(),
                    })
                    .collect::<Vec<_>>(),
            ))
            .into(),
        );
        window.set_listen_lines(
            Rc::new(VecModel::from(
                listen
                    .draft()
                    .into_iter()
                    .map(|(time, text, gap)| ListenLine {
                        time: time.into(),
                        text: text.into(),
                        gap,
                    })
                    .collect::<Vec<_>>(),
            ))
            .into(),
        );
        window.set_listen_status(listen.status().into());
        let (progress, step) = listen.step();
        window.set_listen_progress(progress);
        window.set_listen_step(step.into());
        if let Some(notes) = listen.notes() {
            window.set_listen_title(notes.title().into());
            window.set_listen_details(notes.details.clone().into());
            window.set_listen_saved(
                notes
                    .saved
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
                    .into(),
            );
            window.set_listen_warning(notes.warning.clone().unwrap_or_default().into());
            let (state, error, summary) = match &notes.summary {
                SummaryState::Pending => (0, "", None),
                SummaryState::Ready(summary) => (1, "", Some(summary)),
                SummaryState::Failed(error) => (2, error.as_str(), None),
                SummaryState::NoSpeech => (3, "", None),
                SummaryState::NoTranscript(error) => (4, error.as_str(), None),
            };
            window.set_listen_summary_state(state);
            window.set_listen_summary_error(error.into());
            window.set_listen_tldr(summary.map(|s| s.tldr.clone()).unwrap_or_default().into());
            window.set_listen_coverage(
                summary
                    .and_then(|s| s.coverage.clone())
                    .unwrap_or_default()
                    .into(),
            );
            window.set_listen_decisions(strings(
                summary.map(|s| s.decisions.clone()).unwrap_or_default(),
            ));
            window.set_listen_tasks(
                Rc::new(VecModel::from(
                    summary
                        .map(|s| {
                            s.action_items
                                .iter()
                                .map(|a| ListenTask {
                                    owner: a.owner.clone().into(),
                                    task: a.task.clone().into(),
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                ))
                .into(),
            );
            window.set_listen_transcript(notes.transcript().into());
            window.set_listen_transcript_note(notes.transcript_note().unwrap_or_default().into());
            window.set_listen_raw(notes.raw.clone().into());
        }
        let changed = self.listen_stage.replace(Some(stage)) != Some(stage);
        if changed {
            window.set_listen_error("".into());
            if stage == Stage::Notes {
                window.set_listen_tab(0);
                window.set_listen_show_raw(false);
            }
            if stage == Stage::Setup {
                self.load_listen_devices();
            }
            // The page's buttons were just replaced; keep focus inside the window.
            window.invoke_focus_page();
        }
        self.listen_levels.set_vec(vec![0.0; sources.len()]);
        match (stage, listen.started()) {
            (Stage::Listening, Some(started)) => {
                let levels: Vec<Levels> = sources.into_iter().map(|(_, l)| l).collect();
                let weak = self.window.as_weak();
                let model = self.listen_levels.clone();
                let tick = move || {
                    if let Some(window) = weak.upgrade() {
                        window
                            .set_listen_elapsed(clock(started.elapsed().as_millis() as u64).into());
                        for (i, level) in levels.iter().enumerate() {
                            model.set_row_data(i, level.level());
                        }
                    }
                };
                tick();
                self.listen_timer
                    .start(TimerMode::Repeated, Duration::from_millis(100), tick);
            }
            _ => self.listen_timer.stop(),
        }
    }
    fn listen_copy_text(&self) -> String {
        let Some(listen) = self.host.as_ref().and_then(|h| h.listen()) else {
            return String::new();
        };
        let listen = listen.borrow();
        let Some(notes) = listen.notes() else {
            return String::new();
        };
        match (
            self.window.get_listen_tab(),
            self.window.get_listen_show_raw(),
        ) {
            (0, _) => notes.summary_text(),
            (_, true) => notes.raw.clone(),
            _ => notes.transcript().to_owned(),
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
            Action::ChangeShortcut => self.change_shortcut(0),
            Action::ChangeListenShortcut => self.change_shortcut(1),
            Action::ResetShortcut => {
                self.cancel_capture();
                self.window.set_error("".into());
                self.store.borrow_mut().value.hotkey = Hotkey::default();
                self.save();
                self.show_keys(true);
            }
            Action::ResetListenShortcut => {
                self.cancel_capture();
                self.store.borrow_mut().value.listen_hotkey = Hotkey::listen_default();
                self.save();
                self.show_keys(true);
            }
            Action::ListenMicrophone => {
                self.store.borrow_mut().value.listen_include_microphone =
                    self.window.get_listen_include_microphone();
                self.save();
            }
            Action::ListenStart
            | Action::ListenStop
            | Action::ListenDiscard
            | Action::ListenSave
            | Action::ListenCopy
            | Action::ListenNew
            | Action::ListenRetry => {
                let request = match action {
                    Action::ListenStart => {
                        self.stop_test();
                        self.window.set_listen_error("".into());
                        ListenAction::Start
                    }
                    Action::ListenStop => ListenAction::Stop,
                    Action::ListenDiscard => ListenAction::Discard,
                    Action::ListenSave => ListenAction::Save,
                    Action::ListenCopy => ListenAction::Copy(self.listen_copy_text()),
                    Action::ListenNew => ListenAction::NewSession,
                    _ => ListenAction::RetrySummary,
                };
                if let Some(host) = &self.host {
                    host.listen_action(request);
                }
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
                if self.window.get_page() == 4 && self.window.get_listen_stage() == 0 {
                    self.load_listen_devices();
                }
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
            self.window.set_test_error(
                "Finish dictating or listening before testing your microphone.".into(),
            );
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
