//! The Options window rendered offscreen, without a display: keyboard checks of the
//! Listen page, and (on request) screenshots of each Listen state for review. The data
//! here is fixture text set straight on the window; it never reaches a real session.
use super::{Action, ListenLine, ListenSource, ListenTask, OptionsWindow, Palette};
use slint::{
    ComponentHandle, ModelRc, SharedString, VecModel,
    platform::{
        Key, Platform, WindowAdapter, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    time::Duration,
};

thread_local! {
    static ADAPTER: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    static NOW: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}
/// A platform for this test thread only, with a clock the test advances.
struct Headless;
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        ADAPTER.with(|slot| *slot.borrow_mut() = Some(window.clone()));
        Ok(window)
    }
    fn duration_since_start(&self) -> Duration {
        NOW.with(Cell::get)
    }
}
/// Lets animations finish.
fn settle() {
    NOW.with(|now| now.set(now.get() + Duration::from_secs(2)));
    slint::platform::update_timers_and_animations();
}
fn strings(values: &[&str]) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(
        values
            .iter()
            .map(|&v| v.into())
            .collect::<Vec<SharedString>>(),
    ))
    .into()
}
fn model<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(values)).into()
}
/// The Options window on the Listen page, with the actions it asked for.
fn options(width: u32, height: u32) -> (OptionsWindow, Rc<RefCell<Vec<Action>>>) {
    // Already set when this thread made a window before.
    let _ = slint::platform::set_platform(Box::new(Headless));
    let ui = OptionsWindow::new().expect("window");
    #[cfg(target_os = "linux")]
    crate::platform::linux::desktop::style_options(&ui);
    let adapter = ADAPTER.with(|slot| slot.borrow().clone()).expect("adapter");
    adapter.set_size(slint::PhysicalSize::new(width, height));
    ui.set_entered(true);
    ui.set_page_ready(true);
    ui.set_page(4);
    ui.set_signed_in(true);
    ui.set_cleanup_available(true);
    ui.set_listen_outputs(strings(&[
        "System default output",
        "USB Audio Analog Stereo",
    ]));
    ui.set_listen_microphones(strings(&["System default microphone"]));
    ui.set_listen_shortcut("Ctrl + Shift + F8".into());
    let actions = Rc::new(RefCell::new(Vec::new()));
    let seen = actions.clone();
    ui.on_action(move |action| seen.borrow_mut().push(action));
    ui.show().expect("show");
    settle();
    (ui, actions)
}
fn key(ui: &OptionsWindow, key: impl Into<SharedString>) {
    let text = key.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    settle();
}
fn tab(ui: &OptionsWindow) {
    key(ui, Key::Tab);
}
/// What the runtime does when the stage changes: new controls, focus back on the page.
fn stage(ui: &OptionsWindow, stage: i32) {
    ui.set_listen_stage(stage);
    settle();
    ui.invoke_focus_page();
}
/// Tabs forward until pressing Space asks for `action`, as a keyboard user would.
fn activate(ui: &OptionsWindow, actions: &RefCell<Vec<Action>>, action: Action, most: usize) {
    for _ in 0..most {
        tab(ui);
        actions.borrow_mut().clear();
        key(ui, " ");
        if actions.borrow().contains(&action) {
            return;
        }
    }
    panic!("{action:?} isn't reachable with Tab and Space");
}

fn listening(ui: &OptionsWindow, lines: usize) {
    ui.set_listen_elapsed("12:48".into());
    ui.set_listen_sources(model(vec![
        ListenSource {
            label: "Computer audio".into(),
            device: "USB Audio Analog Stereo".into(),
        },
        ListenSource {
            label: "My microphone".into(),
            device: "System default microphone".into(),
        },
    ]));
    ui.set_listen_levels(model(vec![0.6, 0.25]));
    let sentences = [
        "Okay so let's get started, the main thing today is the launch date for the new onboarding flow.",
        "I think Thursday still works if the profiling on the sign-up form comes back clean.",
        "Maya said she can run the profile tomorrow morning and share numbers before lunch.",
    ];
    let mut draft: Vec<ListenLine> = (0..lines)
        .map(|i| ListenLine {
            time: format!("{:02}:{:02}", i / 2, (i % 2) * 30).into(),
            text: sentences[i % sentences.len()].into(),
            gap: false,
        })
        .collect();
    if lines > 3 {
        draft.insert(
            2,
            ListenLine {
                time: "01:00".into(),
                text: "Not transcribed (01:00–01:30): No connection to ChatGPT".into(),
                gap: true,
            },
        );
    }
    ui.set_listen_lines(model(draft));
}
const TRANSCRIPT: &str = "[00:00] Okay, so let's get started. The main thing today is the launch date for the new onboarding flow.\n\n[00:30] I think Thursday still works if the profiling on the sign-up form comes back clean. Maya said she can run the profile tomorrow morning and share numbers before lunch.\n\n[01:00–01:30] (not transcribed: No connection to ChatGPT)\n\n[01:30] Then we keep Thursday, and Sam emails support about the change.";
fn notes(ui: &OptionsWindow, summary: i32) {
    ui.set_listen_title("Onboarding launch date".into());
    ui.set_listen_details(
        "2026-11-14 09:30 · 13 min · USB Audio Analog Stereo + System default microphone".into(),
    );
    ui.set_listen_warning(
        "Not transcribed: 01:00–01:30. The transcript and summary cover the rest.".into(),
    );
    ui.set_listen_summary_state(summary);
    ui.set_listen_summary_error("No connection to ChatGPT".into());
    ui.set_listen_tldr("The team keeps the Thursday launch for the new onboarding flow, pending a clean profile of the sign-up form.".into());
    ui.set_listen_decisions(strings(&["Launch stays on Thursday."]));
    ui.set_listen_tasks(model(vec![
        ListenTask {
            owner: "Maya".into(),
            task: "Profile the sign-up form and share numbers before lunch".into(),
        },
        ListenTask {
            owner: "".into(),
            task: "Email support about the change".into(),
        },
    ]));
    ui.set_listen_transcript(TRANSCRIPT.into());
    ui.set_listen_raw(TRANSCRIPT.to_lowercase().into());
}

#[test]
fn listen_is_usable_from_the_keyboard() {
    let (ui, actions) = options(820, 560);
    // Setup → Start, then Listening → Stop.
    stage(&ui, 0);
    activate(&ui, &actions, Action::ListenStart, 6);
    listening(&ui, 3);
    stage(&ui, 1);
    activate(&ui, &actions, Action::ListenStop, 3);
    // Discard asks first; its confirmation is reachable and discards.
    stage(&ui, 1);
    activate(&ui, &actions, Action::ListenDiscard, 6);

    // Notes: Tab reaches the selected tab, and the arrows move between tabs.
    notes(&ui, 1);
    stage(&ui, 5);
    ui.set_listen_tab(0);
    tab(&ui);
    key(&ui, Key::RightArrow);
    assert_eq!(ui.get_listen_tab(), 1, "→ selects and focuses Transcript");
    key(&ui, Key::LeftArrow);
    assert_eq!(ui.get_listen_tab(), 0, "← goes back to Summary");
    key(&ui, Key::RightArrow);
    key(&ui, " ");
    assert_eq!(
        ui.get_listen_tab(),
        1,
        "Space keeps the focused tab selected"
    );
    // Only the selected tab is a Tab stop; the next stop is the transcript's own button.
    tab(&ui);
    key(&ui, " ");
    assert!(ui.get_listen_show_raw(), "Show original was next");
    activate(&ui, &actions, Action::ListenSave, 3);
    // Shift+Tab walks back to the selected tab, which keeps working.
    for _ in 0..8 {
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Shift.into(),
        });
        tab(&ui);
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Shift.into(),
        });
        key(&ui, Key::LeftArrow);
        if ui.get_listen_tab() == 0 {
            break;
        }
    }
    assert_eq!(
        ui.get_listen_tab(),
        0,
        "the tab list is reachable backwards"
    );
    // A failed summary's Try again is reachable.
    notes(&ui, 2);
    stage(&ui, 5);
    activate(&ui, &actions, Action::ListenRetry, 4);
    // No recovered transcript needs neither a summary retry nor an unsaved-notes prompt.
    notes(&ui, 4);
    ui.set_listen_tab(0);
    stage(&ui, 5);
    activate(&ui, &actions, Action::ListenNew, 3);
    // Unsaved notes ask before closing; the confirmation is reachable too.
    notes(&ui, 1);
    ui.set_listen_tab(0);
    stage(&ui, 5);
    activate(&ui, &actions, Action::ListenNew, 8);
}

/// Renders every Listen state for review:
///
///     WISPX_LISTEN_SHOTS=artifacts/listen-shots cargo test --lib ui::tests -- --ignored
#[test]
#[ignore = "writes screenshots to WISPX_LISTEN_SHOTS"]
fn listen_screenshots() {
    let dir = std::env::var_os("WISPX_LISTEN_SHOTS").expect("WISPX_LISTEN_SHOTS");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).expect("directory");
    for (scheme, name) in [
        (slint::language::ColorScheme::Dark, "dark"),
        (slint::language::ColorScheme::Light, "light"),
    ] {
        for (width, height) in [(820, 560), (680, 1100)] {
            let (ui, _) = options(width, height);
            ui.global::<Palette>().set_color_scheme(scheme);
            let shot = |state: &str| {
                settle();
                let pixels = ui.window().take_snapshot().expect("snapshot");
                let path = dir.join(format!("{name}-{width}x{height}-{state}.png"));
                let mut encoder = png::Encoder::new(
                    std::fs::File::create(&path).expect("file"),
                    pixels.width(),
                    pixels.height(),
                );
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .and_then(|mut w| w.write_image_data(pixels.as_bytes()))
                    .expect("png");
            };
            stage(&ui, 0);
            shot("1-setup");
            listening(&ui, 3);
            stage(&ui, 1);
            ui.set_listen_status("Transcription is behind: 5 parts waiting. If the queue fills, the gap is marked in your notes.".into());
            shot("2-listening");
            listening(&ui, 24);
            ui.set_listen_status("".into());
            shot("3-listening-long");
            listening(&ui, 3);
            for (stage_number, state, step, progress) in [
                (2, "4-finishing", "Finishing transcript…", 0.6),
                (3, "5-formatting", "Formatting transcript…", 0.35),
                (4, "6-summarizing", "Writing summary…", 0.5),
            ] {
                stage(&ui, stage_number);
                ui.set_listen_step(step.into());
                ui.set_listen_progress(progress);
                shot(state);
            }
            notes(&ui, 1);
            stage(&ui, 5);
            ui.set_listen_tab(0);
            shot("7-notes-summary");
            ui.set_listen_tab(1);
            shot("8-notes-transcript");
            ui.set_listen_tab(0);
            notes(&ui, 2);
            shot("9-summary-failed");
            // Keyboard focus on the selected tab shows its focus ring.
            ui.invoke_focus_page();
            tab(&ui);
            shot("10-tab-focus");
            notes(&ui, 4);
            ui.set_listen_summary_error("No audio parts could be transcribed.".into());
            ui.invoke_focus_page();
            shot("11-no-transcript");
        }
    }
}
