//! Gets a finished transcript to the user without ever typing into the wrong
//! window. Dictating takes seconds and people switch windows meanwhile, so the
//! rule is simple: paste only into the window that had focus as dictation began,
//! if it has focus again by the time the text is ready, and otherwise leave the
//! text on the clipboard and say so. Asking only once dictation has ended is not
//! safe: by the time KWin answers, focus may already have moved to another window,
//! which would then look like the target.

use super::FocusInfo;
use super::clipboard::Clipboard;
use crate::config::{PasteMode, PasteShortcut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const CONTROL_L: i32 = 0xffe3;
const SHIFT_L: i32 = 0xffe1;
const INSERT: i32 = 0xff63;
const KEY_V: i32 = 0x76;

/// Window classes that take Ctrl+Shift+V, matched against whole name parts so
/// `org.kde.konsole` and `gnome-terminal-server` hit while `footnotes` does not.
const TERMINALS: [&str; 18] = [
    "alacritty",
    "blackbox",
    "contour",
    "foot",
    "footclient",
    "ghostty",
    "kitty",
    "konsole",
    "ptyxis",
    "rio",
    "st",
    "terminal",
    "terminator",
    "tilix",
    "urxvt",
    "wezterm",
    "xterm",
    "yakuake",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    CtrlV,
    CtrlShiftV,
    ShiftInsert,
}

impl Keys {
    pub fn keysyms(self) -> &'static [i32] {
        match self {
            Keys::CtrlV => &[CONTROL_L, KEY_V],
            Keys::CtrlShiftV => &[CONTROL_L, SHIFT_L, KEY_V],
            Keys::ShiftInsert => &[SHIFT_L, INSERT],
        }
    }
}

fn is_terminal(class: &str) -> bool {
    class
        .to_ascii_lowercase()
        .split(['.', '-', '_', ' '])
        .any(|part| TERMINALS.contains(&part))
}

pub fn keys_for(shortcut: PasteShortcut, class: &str) -> Keys {
    match shortcut {
        PasteShortcut::Auto if is_terminal(class) => Keys::CtrlShiftV,
        PasteShortcut::Auto | PasteShortcut::CtrlV => Keys::CtrlV,
        PasteShortcut::CtrlShiftV => Keys::CtrlShiftV,
        PasteShortcut::ShiftInsert => Keys::ShiftInsert,
    }
}

pub trait Injector: Send + Sync {
    /// Whether a paste could be sent right now.
    fn ready(&self) -> bool;
    fn current_focus(&self) -> Result<Option<FocusInfo>, String>;
    fn send_paste(&self, keys: Keys) -> Result<(), String>;
    /// Waits until keys can be sent cleanly. An error leaves the text on the clipboard.
    fn prepare(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Modifiers of the shortcut that ended dictation may still be down, and
    /// Wayland offers no way to look, so give the fingers a moment.
    pub before_inject: Duration,
    /// Time for the clipboard's new owner to be announced to the target.
    pub settle: Duration,
    /// How long the target gets to read the clipboard before it is restored.
    pub before_restore: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            before_inject: Duration::from_millis(150),
            settle: Duration::from_millis(40),
            before_restore: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyReason {
    /// `paste` is `copy`, so this is what was asked for.
    Requested,
    FocusChanged,
    /// There was no way to be sure which window would get the keys.
    CannotVerify,
    PasteFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    Pasted,
    Copied(CopyReason),
    /// Kept only for `wispx last` and `wispx copy-last`.
    Stored,
    Failed(String),
}

pub struct Delivered<'a> {
    pub text: &'a str,
    pub mode: PasteMode,
    pub shortcut: PasteShortcut,
    pub restore_clipboard: bool,
    /// The window seen to have focus before dictation ended, if KWin answered.
    pub target: Option<&'a FocusInfo>,
    pub clipboard: Result<&'a dyn Clipboard, String>,
    pub injector: Option<&'a dyn Injector>,
    pub timing: Timing,
    pub cancel: &'a AtomicBool,
}

pub fn deliver(request: Delivered<'_>) -> Delivery {
    if request.mode == PasteMode::None {
        return Delivery::Stored;
    }
    let clipboard = match request.clipboard {
        Ok(clipboard) => clipboard,
        Err(ref message) => return Delivery::Failed(message.clone()),
    };
    let copy_because = |reason: CopyReason| match clipboard.write_text(request.text, false) {
        Ok(()) => Delivery::Copied(reason),
        Err(message) => Delivery::Failed(message),
    };
    if request.mode == PasteMode::Copy {
        return copy_because(CopyReason::Requested);
    }

    let (Some(injector), Some(target)) = (request.injector, request.target) else {
        return copy_because(CopyReason::CannotVerify);
    };
    if !injector.ready() {
        return copy_because(CopyReason::CannotVerify);
    }
    if let Some(reason) = focus_problem(injector, target) {
        return copy_because(reason);
    }

    // A clipboard that holds anything but plain text is not ours to restore.
    let backup = if request.restore_clipboard {
        clipboard.read_text().ok().flatten()
    } else {
        None
    };
    if let Err(message) = clipboard.write_text(request.text, true) {
        return Delivery::Failed(message);
    }
    thread::sleep(request.timing.settle + request.timing.before_inject);
    if let Err(message) = injector.prepare() {
        return copy_because(CopyReason::PasteFailed(message));
    }

    // Time has passed and the keys are about to fire, so look once more.
    if request.cancel.load(Ordering::SeqCst) {
        return copy_because(CopyReason::Requested);
    }
    if let Some(reason) = focus_problem(injector, target) {
        return copy_because(reason);
    }
    if let Err(message) = injector.send_paste(keys_for(request.shortcut, &target.class)) {
        return copy_because(CopyReason::PasteFailed(message));
    }

    if let Some(backup) = backup {
        thread::sleep(request.timing.before_restore);
        // If something else was copied meanwhile, that wins.
        if clipboard.read_text().ok().flatten().as_deref() == Some(request.text) {
            let _ = clipboard.write_text(&backup, false);
        }
    }
    Delivery::Pasted
}

fn focus_problem(injector: &dyn Injector, target: &FocusInfo) -> Option<CopyReason> {
    match injector.current_focus() {
        Ok(Some(now)) if now == *target => None,
        Ok(Some(_)) => Some(CopyReason::FocusChanged),
        Ok(None) => Some(CopyReason::FocusChanged),
        Err(_) => Some(CopyReason::CannotVerify),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeClipboard {
        text: Mutex<Option<String>>,
        writes: Mutex<Vec<(String, bool)>>,
        fail_writes: bool,
    }

    impl Clipboard for FakeClipboard {
        fn read_text(&self) -> Result<Option<String>, String> {
            Ok(self.text.lock().unwrap().clone())
        }

        fn write_text(&self, text: &str, transient: bool) -> Result<(), String> {
            if self.fail_writes {
                return Err("no clipboard".to_string());
            }
            *self.text.lock().unwrap() = Some(text.to_string());
            self.writes
                .lock()
                .unwrap()
                .push((text.to_string(), transient));
            Ok(())
        }
    }

    struct FakeInjector {
        ready: bool,
        /// Answers to successive focus queries; the last one repeats.
        focus: Mutex<VecDeque<Result<Option<FocusInfo>, String>>>,
        sent: Mutex<Vec<Keys>>,
        send_error: Option<String>,
    }

    impl FakeInjector {
        fn focused(answers: Vec<Result<Option<FocusInfo>, String>>) -> Self {
            FakeInjector {
                ready: true,
                focus: Mutex::new(answers.into()),
                sent: Mutex::new(Vec::new()),
                send_error: None,
            }
        }
    }

    impl Injector for FakeInjector {
        fn ready(&self) -> bool {
            self.ready
        }

        fn current_focus(&self) -> Result<Option<FocusInfo>, String> {
            let mut focus = self.focus.lock().unwrap();
            if focus.len() > 1 {
                focus.pop_front().unwrap()
            } else {
                focus.front().unwrap().clone()
            }
        }

        fn send_paste(&self, keys: Keys) -> Result<(), String> {
            self.sent.lock().unwrap().push(keys);
            match &self.send_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    fn window(id: &str, class: &str) -> FocusInfo {
        FocusInfo {
            id: id.to_string(),
            class: class.to_string(),
        }
    }

    fn run(
        clipboard: &FakeClipboard,
        injector: Option<&FakeInjector>,
        target: Option<&FocusInfo>,
        mode: PasteMode,
        restore: bool,
    ) -> Delivery {
        deliver(Delivered {
            text: "hello world",
            mode,
            shortcut: PasteShortcut::Auto,
            restore_clipboard: restore,
            target,
            clipboard: Ok(clipboard),
            injector: injector.map(|i| i as &dyn Injector),
            timing: Timing {
                before_inject: Duration::ZERO,
                settle: Duration::ZERO,
                before_restore: Duration::ZERO,
            },
            cancel: &AtomicBool::new(false),
        })
    }

    #[test]
    fn verified_focus_pastes_and_restores_the_old_clipboard() {
        let clipboard = FakeClipboard::default();
        *clipboard.text.lock().unwrap() = Some("old contents".to_string());
        let target = window("{a}", "firefox");
        let injector = FakeInjector::focused(vec![Ok(Some(target.clone()))]);

        let result = run(
            &clipboard,
            Some(&injector),
            Some(&target),
            PasteMode::Auto,
            true,
        );
        assert_eq!(result, Delivery::Pasted);
        assert_eq!(*injector.sent.lock().unwrap(), [Keys::CtrlV]);
        assert_eq!(
            *clipboard.writes.lock().unwrap(),
            [
                ("hello world".to_string(), true),
                ("old contents".to_string(), false),
            ]
        );
    }

    #[test]
    fn switching_windows_before_delivery_never_touches_that_window() {
        let clipboard = FakeClipboard::default();
        let target = window("{a}", "firefox");
        let injector = FakeInjector::focused(vec![Ok(Some(window("{b}", "firefox")))]);

        let result = run(
            &clipboard,
            Some(&injector),
            Some(&target),
            PasteMode::Auto,
            true,
        );
        assert_eq!(result, Delivery::Copied(CopyReason::FocusChanged));
        assert!(injector.sent.lock().unwrap().is_empty());
        assert_eq!(
            *clipboard.writes.lock().unwrap(),
            [("hello world".to_string(), false)]
        );
    }

    #[test]
    fn switching_windows_in_the_last_moment_also_stops_the_keys() {
        let clipboard = FakeClipboard::default();
        *clipboard.text.lock().unwrap() = Some("old".to_string());
        let target = window("{a}", "firefox");
        let injector = FakeInjector::focused(vec![
            Ok(Some(target.clone())),
            Ok(Some(window("{b}", "kate"))),
        ]);

        let result = run(
            &clipboard,
            Some(&injector),
            Some(&target),
            PasteMode::Auto,
            true,
        );
        assert_eq!(result, Delivery::Copied(CopyReason::FocusChanged));
        assert!(injector.sent.lock().unwrap().is_empty());
        // The transcript stays available instead of the old contents.
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn no_focused_window_or_a_failing_probe_means_copy_only() {
        let target = window("{a}", "firefox");
        for (answer, expected) in [
            (Ok(None), CopyReason::FocusChanged),
            (Err("no kwin".to_string()), CopyReason::CannotVerify),
        ] {
            let clipboard = FakeClipboard::default();
            let injector = FakeInjector::focused(vec![answer]);
            let result = run(
                &clipboard,
                Some(&injector),
                Some(&target),
                PasteMode::Auto,
                true,
            );
            assert_eq!(result, Delivery::Copied(expected));
            assert!(injector.sent.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn without_consent_or_a_recorded_target_nothing_is_sent() {
        let target = window("{a}", "firefox");
        let clipboard = FakeClipboard::default();

        // Permission was never granted.
        assert_eq!(
            run(&clipboard, None, Some(&target), PasteMode::Auto, true),
            Delivery::Copied(CopyReason::CannotVerify)
        );

        // The session is not open.
        let mut injector = FakeInjector::focused(vec![Ok(Some(target.clone()))]);
        injector.ready = false;
        assert_eq!(
            run(
                &clipboard,
                Some(&injector),
                Some(&target),
                PasteMode::Auto,
                true
            ),
            Delivery::Copied(CopyReason::CannotVerify)
        );
        assert!(injector.sent.lock().unwrap().is_empty());

        // Focus could not be determined before dictation ended.
        injector.ready = true;
        assert_eq!(
            run(&clipboard, Some(&injector), None, PasteMode::Auto, true),
            Delivery::Copied(CopyReason::CannotVerify)
        );
        assert!(injector.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_key_send_leaves_the_transcript_on_the_clipboard() {
        let clipboard = FakeClipboard::default();
        *clipboard.text.lock().unwrap() = Some("old".to_string());
        let target = window("{a}", "firefox");
        let mut injector = FakeInjector::focused(vec![Ok(Some(target.clone()))]);
        injector.send_error = Some("session closed".to_string());

        let result = run(
            &clipboard,
            Some(&injector),
            Some(&target),
            PasteMode::Auto,
            true,
        );
        assert_eq!(
            result,
            Delivery::Copied(CopyReason::PasteFailed("session closed".to_string()))
        );
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn something_copied_meanwhile_is_not_overwritten_by_the_restore() {
        struct ChangesAfterPaste(FakeClipboard);
        impl Clipboard for ChangesAfterPaste {
            fn read_text(&self) -> Result<Option<String>, String> {
                let writes = self.0.writes.lock().unwrap().len();
                if writes > 0 {
                    return Ok(Some("copied by the user".to_string()));
                }
                self.0.read_text()
            }
            fn write_text(&self, text: &str, transient: bool) -> Result<(), String> {
                self.0.write_text(text, transient)
            }
        }
        let inner = FakeClipboard::default();
        *inner.text.lock().unwrap() = Some("old".to_string());
        let clipboard = ChangesAfterPaste(inner);
        let target = window("{a}", "firefox");
        let injector = FakeInjector::focused(vec![Ok(Some(target.clone()))]);
        let result = deliver(Delivered {
            text: "hello world",
            mode: PasteMode::Auto,
            shortcut: PasteShortcut::CtrlV,
            restore_clipboard: true,
            target: Some(&target),
            clipboard: Ok(&clipboard),
            injector: Some(&injector),
            timing: Timing {
                before_inject: Duration::ZERO,
                settle: Duration::ZERO,
                before_restore: Duration::ZERO,
            },
            cancel: &AtomicBool::new(false),
        });
        assert_eq!(result, Delivery::Pasted);
        assert_eq!(clipboard.0.writes.lock().unwrap().len(), 1);
    }

    #[test]
    fn restore_can_be_turned_off() {
        let clipboard = FakeClipboard::default();
        *clipboard.text.lock().unwrap() = Some("old".to_string());
        let target = window("{a}", "firefox");
        let injector = FakeInjector::focused(vec![Ok(Some(target.clone()))]);
        let result = run(
            &clipboard,
            Some(&injector),
            Some(&target),
            PasteMode::Auto,
            false,
        );
        assert_eq!(result, Delivery::Pasted);
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn copy_and_none_modes_never_send_keys() {
        let target = window("{a}", "firefox");
        let injector = FakeInjector::focused(vec![Ok(Some(target.clone()))]);

        let clipboard = FakeClipboard::default();
        assert_eq!(
            run(
                &clipboard,
                Some(&injector),
                Some(&target),
                PasteMode::Copy,
                true
            ),
            Delivery::Copied(CopyReason::Requested)
        );
        assert_eq!(
            clipboard.read_text().unwrap().as_deref(),
            Some("hello world")
        );

        let clipboard = FakeClipboard::default();
        assert_eq!(
            run(
                &clipboard,
                Some(&injector),
                Some(&target),
                PasteMode::None,
                true
            ),
            Delivery::Stored
        );
        assert_eq!(clipboard.read_text().unwrap(), None);
        assert!(injector.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn clipboard_problems_are_failures_not_silent_losses() {
        let clipboard = FakeClipboard {
            fail_writes: true,
            ..FakeClipboard::default()
        };
        assert_eq!(
            run(&clipboard, None, None, PasteMode::Copy, true),
            Delivery::Failed("no clipboard".to_string())
        );

        let result = deliver(Delivered {
            text: "x",
            mode: PasteMode::Copy,
            shortcut: PasteShortcut::Auto,
            restore_clipboard: true,
            target: None,
            clipboard: Err("install wl-clipboard".to_string()),
            injector: None,
            timing: Timing::default(),
            cancel: &AtomicBool::new(false),
        });
        assert_eq!(result, Delivery::Failed("install wl-clipboard".to_string()));
    }

    #[test]
    fn terminals_get_their_own_paste_shortcut() {
        for class in [
            "org.kde.konsole",
            "org.kde.yakuake",
            "foot",
            "kitty",
            "Alacritty",
            "org.wezfurlong.wezterm",
            "com.mitchellh.ghostty",
            "org.gnome.Terminal",
            "gnome-terminal-server",
            "xfce4-terminal",
        ] {
            assert_eq!(
                keys_for(PasteShortcut::Auto, class),
                Keys::CtrlShiftV,
                "{class}"
            );
        }
        for class in ["firefox", "org.kde.kate", "footnotes", "", "google-chrome"] {
            assert_eq!(keys_for(PasteShortcut::Auto, class), Keys::CtrlV, "{class}");
        }
        // An explicit choice beats detection.
        assert_eq!(keys_for(PasteShortcut::CtrlV, "foot"), Keys::CtrlV);
        assert_eq!(
            keys_for(PasteShortcut::ShiftInsert, "firefox"),
            Keys::ShiftInsert
        );
    }

    #[test]
    fn shortcuts_map_to_x11_keysym_values() {
        assert_eq!(Keys::CtrlV.keysyms(), [0xffe3, 0x76]);
        assert_eq!(Keys::CtrlShiftV.keysyms(), [0xffe3, 0xffe1, 0x76]);
        assert_eq!(Keys::ShiftInsert.keysyms(), [0xffe1, 0xff63]);
    }
}
