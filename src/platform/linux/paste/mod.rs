//! Pastes a transcript only into the window that was focused when dictation
//! ended, like the Windows host. X11 checks `_NET_ACTIVE_WINDOW` and types with
//! XTEST. Wayland asks KWin for focus and types through the RemoteDesktop portal;
//! where focus can't be verified, the text is only copied.
//!
//! A paste borrows the clipboard: back it up, put the transcript there, press
//! Ctrl + V, put the backup back. Transcripts finish on independent threads, so
//! the whole exchange is one transaction under a lock, and focus and modifiers
//! are checked again at the last moment before any key is sent.
mod portal;
mod x11;

use super::{
    clipboard::{self, Backup},
    session::{Protocol, Session},
};
use crate::app::Delivery;
use portal::PortalKeys;
use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};
use x11::X11;

const CONTROL_L: u32 = 0xffe3;
const SHIFT_L: u32 = 0xffe1;
const KEY_V: u32 = 0x76;
/// Classes that paste with Ctrl + Shift + V, matched against whole name parts.
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
pub fn is_terminal(class: &str) -> bool {
    class
        .to_ascii_lowercase()
        .split(['.', '-', '_', ' '])
        .any(|part| TERMINALS.contains(&part))
}
fn paste_keys(class: &str) -> &'static [u32] {
    if is_terminal(class) {
        &[CONTROL_L, SHIFT_L, KEY_V]
    } else {
        &[CONTROL_L, KEY_V]
    }
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The focused window when dictation ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub id: String,
    pub class: String,
}

/// Why no key reached the target. Every variant means nothing was typed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refused {
    FocusChanged,
    ModifiersHeld,
    Failed(String),
}

/// The desktop's clipboard, behind a seam so delivery ordering can be tested.
pub trait Clip: Send + Sync {
    fn set_text(&self, text: &str, transient: bool) -> Result<(), String>;
    fn text(&self) -> Option<String>;
    /// `None` when the clipboard is empty, too large, too slow or unreadable.
    fn backup(&self) -> Option<Backup>;
    fn restore(&self, backup: Backup) -> Result<(), String>;
}
struct SystemClip;
impl Clip for SystemClip {
    fn set_text(&self, text: &str, transient: bool) -> Result<(), String> {
        clipboard::set_text(text, transient)
    }
    fn text(&self) -> Option<String> {
        clipboard::text()
    }
    fn backup(&self) -> Option<Backup> {
        clipboard::backup()
    }
    fn restore(&self, backup: Backup) -> Result<(), String> {
        clipboard::restore(backup)
    }
}

/// Focus queries and key injection for one display protocol.
pub trait Keyboard: Send + Sync {
    /// Whether keys can be sent at all right now.
    fn ready(&self) -> bool;
    fn describe(&self) -> String;
    fn focused(&self) -> Option<Target>;
    /// `Some(held)` where key state is visible (X11); `None` on Wayland.
    fn modifier_held(&self) -> Option<bool>;
    /// Sends `keysyms` only if, immediately before the first key, `target` still
    /// has focus and no modifier is held. `Err` guarantees nothing was sent.
    fn paste(&self, keysyms: &[u32], target: &Target) -> Result<(), Refused>;
}
struct NoKeyboard;
impl Keyboard for NoKeyboard {
    fn ready(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "copy only".into()
    }
    fn focused(&self) -> Option<Target> {
        None
    }
    fn modifier_held(&self) -> Option<bool> {
        None
    }
    fn paste(&self, _: &[u32], _: &Target) -> Result<(), Refused> {
        Err(Refused::Failed("no keyboard injection".into()))
    }
}

#[derive(Clone, Copy, Debug)]
struct Timing {
    /// How long a held modifier from the finishing shortcut may take to come up.
    modifier_wait: Duration,
    poll: Duration,
    /// Wayland can't report key state; the hold shortcut released before this ran.
    blind_release: Duration,
    /// Lets the clipboard owner announce the new offer before Ctrl + V.
    settle: Duration,
    /// Lets the target read the clipboard before the backup goes back.
    restore_after: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            modifier_wait: Duration::from_millis(2250),
            poll: Duration::from_millis(15),
            blind_release: Duration::from_millis(150),
            settle: Duration::from_millis(40),
            restore_after: Duration::from_millis(500),
        }
    }
}

pub struct Paster {
    keys: Box<dyn Keyboard>,
    clip: Box<dyn Clip>,
    /// Held for a whole backup → paste → restore exchange, and for plain copies,
    /// so overlapping transcripts never interleave on the clipboard.
    transaction: Mutex<()>,
    timing: Timing,
}
impl Paster {
    pub fn start(session: &Session) -> Arc<Self> {
        let keys: Box<dyn Keyboard> = match session.protocol {
            Protocol::X11 => X11::connect().map_or_else(
                |error| {
                    eprintln!("Paste: {error}");
                    Box::new(NoKeyboard) as Box<dyn Keyboard>
                },
                |x| Box::new(x),
            ),
            // Without a focus query a paste could land in another window.
            Protocol::Wayland if session.kde() => Box::new(PortalKeys::start()),
            _ => Box::new(NoKeyboard),
        };
        Arc::new(Self::with(keys, Box::new(SystemClip), Timing::default()))
    }
    fn with(keys: Box<dyn Keyboard>, clip: Box<dyn Clip>, timing: Timing) -> Self {
        Self {
            keys,
            clip,
            transaction: Mutex::new(()),
            timing,
        }
    }
    /// What can be verified about focus right now; `None` means copy only.
    /// Blocking (KWin can take up to two seconds): never call on the UI thread.
    pub fn focused(&self) -> Option<Target> {
        self.keys.ready().then(|| self.keys.focused()).flatten()
    }
    pub fn describe(&self) -> String {
        self.keys.describe()
    }
    /// Copies `text` as an ordinary clipboard entry, after any paste in progress.
    pub fn copy(&self, text: &str) -> Result<(), String> {
        let _transaction = lock(&self.transaction);
        self.clip.set_text(text, false)
    }
    /// Blocking: runs on the transcription worker, never the UI thread.
    pub fn deliver(&self, text: &str, target: Option<&Target>) -> Delivery {
        let _transaction = lock(&self.transaction);
        let copy = |delivery| match self.clip.set_text(text, false) {
            Ok(()) => delivery,
            Err(error) => {
                eprintln!("Clipboard: {error}");
                Delivery::Failed
            }
        };
        let Some(target) = target.filter(|_| self.keys.ready()) else {
            return copy(Delivery::Copied);
        };
        // Modifiers from the finishing shortcut would turn Ctrl + V into another chord.
        if !self.modifiers_released() {
            eprintln!("Paste: a modifier key stayed down; copying instead");
            return copy(Delivery::Copied);
        }
        // Cheap early check, so a lost target never disturbs the clipboard.
        if self.keys.focused().as_ref() != Some(target) {
            return copy(Delivery::FocusChanged);
        }
        let backup = self.clip.backup();
        if let Err(error) = self.clip.set_text(text, true) {
            eprintln!("Clipboard: {error}");
            return copy(Delivery::Copied);
        }
        thread::sleep(self.timing.settle);
        // Focus and modifiers are checked again inside, right before the keys.
        if let Err(refused) = self.keys.paste(paste_keys(&target.class), target) {
            if let Refused::Failed(error) = &refused {
                eprintln!("Paste: {error}");
            }
            // Re-offered as a normal entry: a transcript the user must paste by
            // hand shouldn't be hidden from clipboard history.
            return copy(match refused {
                Refused::FocusChanged => Delivery::FocusChanged,
                Refused::ModifiersHeld | Refused::Failed(_) => Delivery::Copied,
            });
        }
        thread::sleep(self.timing.restore_after);
        // Only while the clipboard still holds the transcript: anything else is
        // newer and belongs to the user.
        if let Some(backup) = backup
            && self.clip.text().as_deref() == Some(text)
            && let Err(error) = self.clip.restore(backup)
        {
            eprintln!("Clipboard restore: {error}");
        }
        Delivery::Pasted
    }
    fn modifiers_released(&self) -> bool {
        let deadline = Instant::now() + self.timing.modifier_wait;
        loop {
            match self.keys.modifier_held() {
                None => {
                    thread::sleep(self.timing.blind_release);
                    return true;
                }
                Some(false) => return true,
                Some(true) if Instant::now() >= deadline => return false,
                Some(true) => thread::sleep(self.timing.poll),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: "footnotes" or "testing" count as terminals; terminals get
    // Ctrl + V, which they treat as a literal control character; two transcripts
    // finishing together paste each other's text or restore the wrong clipboard;
    // focus moving during the clipboard backup still pastes; a failed copy
    // reports success.
    use super::test_helpers::target;
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn terminals_paste_with_shift() {
        for class in [
            "org.kde.konsole",
            "kitty",
            "gnome-terminal-server",
            "XTerm",
            "foot",
        ] {
            assert!(is_terminal(class), "{class}");
            assert_eq!(paste_keys(class), &[CONTROL_L, SHIFT_L, KEY_V]);
        }
        for class in ["firefox", "footnotes", "org.kde.kate", "testing", ""] {
            assert!(!is_terminal(class), "{class}");
            assert_eq!(paste_keys(class), &[CONTROL_L, KEY_V]);
        }
    }

    fn quick() -> Timing {
        Timing {
            modifier_wait: Duration::from_millis(30),
            poll: Duration::from_millis(1),
            blind_release: Duration::ZERO,
            settle: Duration::ZERO,
            restore_after: Duration::from_millis(5),
        }
    }
    /// A clipboard holding one text entry; `transient` marks hidden entries.
    #[derive(Default)]
    struct FakeClip {
        content: Mutex<Option<(String, bool)>>,
        broken: bool,
        /// Runs during `backup`, standing in for a slow clipboard owner.
        during_backup: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl FakeClip {
        fn holding(text: &str) -> Arc<Self> {
            Arc::new(Self {
                content: Mutex::new(Some((text.into(), false))),
                ..Self::default()
            })
        }
        fn now(&self) -> Option<(String, bool)> {
            lock(&self.content).clone()
        }
    }
    impl Clip for Arc<FakeClip> {
        fn set_text(&self, text: &str, transient: bool) -> Result<(), String> {
            if self.broken {
                return Err("No clipboard is available".into());
            }
            *lock(&self.content) = Some((text.into(), transient));
            Ok(())
        }
        fn text(&self) -> Option<String> {
            self.now().map(|(text, _)| text)
        }
        fn backup(&self) -> Option<Backup> {
            if let Some(hook) = lock(&self.during_backup).take() {
                hook();
            }
            self.text().map(Backup::Text)
        }
        fn restore(&self, backup: Backup) -> Result<(), String> {
            let Backup::Text(text) = backup else {
                unreachable!()
            };
            *lock(&self.content) = Some((text, false));
            Ok(())
        }
    }

    /// A keyboard whose focused window and modifier state the test controls.
    /// Each paste records what the target would have received.
    struct FakeKeys {
        clip: Arc<FakeClip>,
        focus: Mutex<String>,
        modifier: Mutex<Option<bool>>,
        pasted: Mutex<Vec<(String, String)>>,
        /// When set, a paste waits here until the test lets it finish.
        gate: Option<Arc<Barrier>>,
        /// How long each paste takes, so concurrent deliveries would overlap.
        hold: Duration,
        active: AtomicUsize,
        most_active: AtomicUsize,
    }
    impl FakeKeys {
        fn new(clip: &Arc<FakeClip>, focus: &str) -> Arc<Self> {
            Arc::new(Self {
                clip: clip.clone(),
                focus: Mutex::new(focus.into()),
                modifier: Mutex::new(Some(false)),
                pasted: Mutex::new(Vec::new()),
                gate: None,
                hold: Duration::ZERO,
                active: AtomicUsize::new(0),
                most_active: AtomicUsize::new(0),
            })
        }
    }
    impl Keyboard for Arc<FakeKeys> {
        fn ready(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "fake".into()
        }
        fn focused(&self) -> Option<Target> {
            Some(target(&lock(&self.focus)))
        }
        fn modifier_held(&self) -> Option<bool> {
            *lock(&self.modifier)
        }
        fn paste(&self, _: &[u32], target: &Target) -> Result<(), Refused> {
            if self.modifier_held() == Some(true) {
                return Err(Refused::ModifiersHeld);
            }
            if self.focused().as_ref() != Some(target) {
                return Err(Refused::FocusChanged);
            }
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_active.fetch_max(now, Ordering::SeqCst);
            let received = self.clip.text().unwrap_or_default();
            lock(&self.pasted).push((target.id.clone(), received));
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            thread::sleep(self.hold);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }
    fn paster(keys: &Arc<FakeKeys>, clip: &Arc<FakeClip>) -> Arc<Paster> {
        Arc::new(Paster::with(
            Box::new(keys.clone()),
            Box::new(clip.clone()),
            quick(),
        ))
    }

    #[test]
    fn paste_restores_the_previous_clipboard() {
        let clip = FakeClip::holding("user's text");
        let keys = FakeKeys::new(&clip, "a");
        let delivery = paster(&keys, &clip).deliver("transcript", Some(&target("a")));
        assert_eq!(delivery, Delivery::Pasted);
        assert_eq!(
            *lock(&keys.pasted),
            vec![("a".to_owned(), "transcript".to_owned())]
        );
        assert_eq!(clip.now(), Some(("user's text".into(), false)));
    }

    #[test]
    fn overlapping_deliveries_paste_their_own_text_and_restore_the_original() {
        let clip = FakeClip::holding("original");
        let mut keys = FakeKeys::new(&clip, "a");
        Arc::get_mut(&mut keys).expect("unshared").hold = Duration::from_millis(40);
        let paster = paster(&keys, &clip);
        let start = Arc::new(Barrier::new(3));
        let workers: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(|text| {
                let (paster, start) = (paster.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    paster.deliver(text, Some(&target("a")))
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().expect("delivery"), Delivery::Pasted);
        }
        assert_eq!(
            keys.most_active.load(Ordering::SeqCst),
            1,
            "pastes overlapped"
        );
        let mut received: Vec<_> = lock(&keys.pasted)
            .iter()
            .map(|(_, text)| text.clone())
            .collect();
        received.sort();
        assert_eq!(received, ["first", "second", "third"]);
        // Each backup saw the original, not another transcript.
        assert_eq!(clip.now(), Some(("original".into(), false)));
    }

    #[test]
    fn copy_last_waits_for_a_paste_in_progress() {
        let clip = FakeClip::holding("original");
        let mut keys = FakeKeys::new(&clip, "a");
        let gate = Arc::new(Barrier::new(2));
        Arc::get_mut(&mut keys).expect("unshared").gate = Some(gate.clone());
        let paster = paster(&keys, &clip);
        let delivering = {
            let paster = paster.clone();
            thread::spawn(move || paster.deliver("new", Some(&target("a"))))
        };
        // Wait until the paste is underway, then ask for the copy.
        while lock(&keys.pasted).is_empty() {
            thread::yield_now();
        }
        let copying = {
            let paster = paster.clone();
            thread::spawn(move || paster.copy("older transcript"))
        };
        thread::sleep(Duration::from_millis(20));
        // The copy hasn't run yet: the paste still owns the clipboard.
        assert_eq!(clip.text().as_deref(), Some("new"));
        gate.wait();
        assert_eq!(delivering.join().expect("deliver"), Delivery::Pasted);
        copying.join().expect("copy").expect("copied");
        // The copy came last and the restore didn't overwrite it.
        assert_eq!(clip.now(), Some(("older transcript".into(), false)));
    }

    #[test]
    fn focus_change_during_the_backup_copies_instead_of_pasting() {
        let clip = FakeClip::holding("original");
        let keys = FakeKeys::new(&clip, "a");
        let moved = keys.clone();
        *lock(&clip.during_backup) = Some(Box::new(move || *lock(&moved.focus) = "b".into()));
        let delivery = paster(&keys, &clip).deliver("transcript", Some(&target("a")));
        assert_eq!(delivery, Delivery::FocusChanged);
        assert!(lock(&keys.pasted).is_empty(), "nothing may be typed");
        // Left copied as a normal entry, not a hidden transient one.
        assert_eq!(clip.now(), Some(("transcript".into(), false)));
    }

    #[test]
    fn a_modifier_that_stays_down_copies_instead_of_pasting() {
        let clip = FakeClip::holding("original");
        let keys = FakeKeys::new(&clip, "a");
        *lock(&keys.modifier) = Some(true);
        let delivery = paster(&keys, &clip).deliver("transcript", Some(&target("a")));
        assert_eq!(delivery, Delivery::Copied);
        assert!(lock(&keys.pasted).is_empty());
        assert_eq!(clip.now(), Some(("transcript".into(), false)));
    }

    #[test]
    fn a_newer_clipboard_is_not_overwritten_by_the_restore() {
        let clip = FakeClip::holding("original");
        let keys = FakeKeys::new(&clip, "a");
        let paster = Paster::with(
            Box::new(keys.clone()),
            Box::new(clip.clone()),
            Timing {
                restore_after: Duration::from_millis(80),
                ..quick()
            },
        );
        let copier = {
            let clip = clip.clone();
            let keys = keys.clone();
            thread::spawn(move || {
                while lock(&keys.pasted).is_empty() {
                    thread::yield_now();
                }
                *lock(&clip.content) = Some(("user copied this".into(), false));
            })
        };
        assert_eq!(
            paster.deliver("transcript", Some(&target("a"))),
            Delivery::Pasted
        );
        copier.join().expect("copier");
        assert_eq!(clip.now(), Some(("user copied this".into(), false)));
    }

    #[test]
    fn no_clipboard_reports_failure_instead_of_a_copy() {
        let clip = Arc::new(FakeClip {
            broken: true,
            ..FakeClip::default()
        });
        let keys = FakeKeys::new(&clip, "a");
        let paster = paster(&keys, &clip);
        assert_eq!(paster.deliver("transcript", None), Delivery::Failed);
        assert_eq!(
            paster.deliver("transcript", Some(&target("a"))),
            Delivery::Failed
        );
        assert!(lock(&keys.pasted).is_empty());
        assert!(paster.copy("transcript").is_err());
    }
}

#[cfg(test)]
mod test_helpers {
    use super::Target;

    pub(super) fn target(id: &str) -> Target {
        Target {
            id: id.into(),
            class: "editor".into(),
        }
    }
}
