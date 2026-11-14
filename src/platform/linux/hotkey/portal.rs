//! Wayland shortcuts through the XDG GlobalShortcuts portal. Wayland doesn't let
//! clients watch the keyboard, so the desktop owns the keys: it confirms them with
//! the user once and reports each shortcut's press and release.
//!
//! Plasma's shortcut system can't bind a modifier-only chord such as Ctrl + Win,
//! so a modifier-only setting falls back to Ctrl + Win + D. Hands-free and cancel
//! are separate shortcuts because Space and Esc can't be claimed per dictation.
//! Listen has its own toggle; the cancel shortcut also discards a Listen capture.
use super::{DesktopStatus, HookMessage, Sink, keymap};
use crate::{hotkey::Hotkey, hotkey_machine::HotkeyEvent};
use ashpd::desktop::global_shortcuts::{
    BindShortcutsOptions, GlobalShortcuts, NewShortcut, Shortcut,
};
use futures_lite::{StreamExt, future};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub const HOLD: &str = "dictate";
pub const HANDS_FREE: &str = "hands-free";
pub const CANCEL: &str = "cancel";
pub const LISTEN: &str = "listen";
/// Plasma releases a held shortcut the moment another fires, so going from
/// holding `dictate` to pressing `hands-free` first looks exactly like letting go.
/// Holding each release back briefly lets the following press replace it.
pub const RELEASE_GRACE: Duration = Duration::from_millis(60);
const RETRY: Duration = Duration::from_secs(5);
const RETRY_LIMIT: Duration = Duration::from_secs(300);

/// Preferred triggers: the hold chord, then the same modifiers with Space and Backspace,
/// then Listen's own chord. Plain Esc stays with apps; Ctrl + Win + Esc is KWin's Kill Window.
pub fn triggers(hotkey: &Hotkey, listen: &Hotkey) -> [String; 4] {
    let hold = keymap::trigger(hotkey).unwrap_or_else(|| "CTRL+LOGO+d".into());
    let modifiers = hold
        .rsplit_once('+')
        .map_or("CTRL+LOGO", |(modifiers, _)| modifiers);
    [
        hold.clone(),
        format!("{modifiers}+space"),
        format!("{modifiers}+BackSpace"),
        keymap::trigger(listen)
            .or_else(|| keymap::trigger(&Hotkey::listen_default()))
            .expect("the default has a main key"),
    ]
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Holding,
    Locked,
}
/// Turns shortcut presses and releases into the Windows hook's dictation events.
#[derive(Debug, Default)]
pub struct Translator {
    phase: Phase,
    release_due: Option<Instant>,
    /// Listen is capturing, so the cancel shortcut discards it.
    listening: bool,
    /// Listen's shortcut is down; a portal may repeat Activated until Deactivated.
    listen_held: bool,
}
impl Translator {
    pub fn set_listening(&mut self, listening: bool) {
        self.listening = listening;
    }
    pub fn activated(&mut self, id: &str) -> Vec<HotkeyEvent> {
        if id == LISTEN {
            // A held key toggles once. Leaves a held dictation's pending release alone:
            // Plasma reports it first.
            if std::mem::replace(&mut self.listen_held, true) {
                return Vec::new();
            }
            return vec![HotkeyEvent::ListenToggle];
        }
        self.release_due = None;
        match (id, self.phase) {
            (HOLD | HANDS_FREE, Phase::Locked) => {
                self.phase = Phase::Idle;
                vec![HotkeyEvent::StopLocked]
            }
            (HOLD, Phase::Idle) => {
                self.phase = Phase::Holding;
                vec![HotkeyEvent::Start]
            }
            (HANDS_FREE, Phase::Holding) => {
                self.phase = Phase::Locked;
                vec![HotkeyEvent::Lock]
            }
            (HANDS_FREE, Phase::Idle) => {
                self.phase = Phase::Locked;
                vec![HotkeyEvent::Start, HotkeyEvent::Lock]
            }
            (CANCEL, Phase::Holding | Phase::Locked) => {
                self.phase = Phase::Idle;
                vec![HotkeyEvent::Cancel]
            }
            (CANCEL, Phase::Idle) if self.listening => {
                self.listening = false;
                vec![HotkeyEvent::ListenCancel]
            }
            _ => Vec::new(),
        }
    }
    pub fn deactivated(&mut self, id: &str, now: Instant) {
        if id == LISTEN {
            self.listen_held = false;
        }
        if id == HOLD && self.phase == Phase::Holding {
            self.release_due = Some(now + RELEASE_GRACE);
        }
    }
    /// The held-back release once its grace period has passed.
    pub fn due(&mut self, now: Instant) -> Option<HotkeyEvent> {
        let due = self.release_due?;
        if now < due {
            return None;
        }
        self.release_due = None;
        (self.phase == Phase::Holding).then(|| {
            self.phase = Phase::Idle;
            HotkeyEvent::Release
        })
    }
    pub fn due_at(&self) -> Option<Instant> {
        self.release_due
    }
    pub fn force_idle(&mut self) {
        self.phase = Phase::Idle;
        self.release_due = None;
    }
    /// The session is gone, so no further press or release will arrive. A hold
    /// already let go finishes as usual; any other live dictation, held or
    /// hands-free, is cancelled, since nothing could ever finish it.
    pub fn lost(&mut self) -> Option<HotkeyEvent> {
        let released = self.release_due.is_some();
        let phase = std::mem::take(&mut self.phase);
        self.listen_held = false;
        self.release_due = None;
        match phase {
            Phase::Idle => None,
            Phase::Holding if released => Some(HotkeyEvent::Release),
            Phase::Holding | Phase::Locked => Some(HotkeyEvent::CancelSilent),
        }
    }
}

type Bound = (GlobalShortcuts, ashpd::desktop::Session<GlobalShortcuts>);
pub struct Portal {
    translator: Arc<Mutex<Translator>>,
    bound: Arc<Mutex<Option<Arc<Bound>>>>,
}
impl Portal {
    pub fn start(hotkey: &Hotkey, listen_key: &Hotkey, sink: Sink) -> Self {
        let translator = Arc::new(Mutex::new(Translator::default()));
        let bound = Arc::new(Mutex::new(None));
        let triggers = triggers(hotkey, listen_key);
        let (worker_translator, worker_bound) = (translator.clone(), bound.clone());
        thread::Builder::new()
            .name("portal-shortcuts".into())
            .spawn(move || {
                super::super::register_app();
                let mut delay = RETRY;
                loop {
                    let started = Instant::now();
                    let result = future::block_on(listen(
                        &triggers,
                        &sink,
                        &worker_translator,
                        &worker_bound,
                    ));
                    worker_bound.lock().expect("portal lock").take();
                    let lost = worker_translator.lock().expect("portal lock").lost();
                    if let Some(event) = lost {
                        sink(HookMessage::Dictation(event));
                    }
                    match result {
                        Err(Stop::Unavailable(reason)) => {
                            sink(HookMessage::Desktop(DesktopStatus::Unavailable(reason)));
                            return;
                        }
                        Err(Stop::Lost(reason)) => {
                            sink(HookMessage::Desktop(DesktopStatus::Unavailable(format!(
                                "{reason}; retrying"
                            ))));
                        }
                    }
                    delay = if started.elapsed() > Duration::from_secs(60) {
                        RETRY
                    } else {
                        (delay * 2).min(RETRY_LIMIT)
                    };
                    thread::sleep(delay);
                }
            })
            .expect("spawn portal thread");
        Self { translator, bound }
    }
    pub fn force_idle(&self) {
        self.translator.lock().expect("portal lock").force_idle();
    }
    pub fn set_listening(&self, listening: bool) {
        self.translator
            .lock()
            .expect("portal lock")
            .set_listening(listening);
    }
    /// Opens the desktop's shortcut settings for this session (portal version 2).
    pub fn configure(&self) {
        let Some(bound) = self.bound.lock().expect("portal lock").clone() else {
            return;
        };
        thread::spawn(move || {
            let (portal, session) = &*bound;
            let result = if portal.version() >= 2 {
                future::block_on(portal.configure_shortcuts(session, None, Default::default()))
                    .map_err(|e| e.to_string())
            } else {
                // Older portals have no configuration call; Plasma's settings page works.
                std::process::Command::new("systemsettings")
                    .arg("kcm_keys")
                    .spawn()
                    .map(drop)
                    .map_err(|e| e.to_string())
            };
            if let Err(error) = result {
                eprintln!("Shortcut settings: {error}");
            }
        });
    }
}

enum Stop {
    Unavailable(String),
    Lost(String),
}
impl From<ashpd::Error> for Stop {
    fn from(error: ashpd::Error) -> Self {
        Self::Lost(error.to_string())
    }
}
fn status(shortcuts: &[Shortcut]) -> DesktopStatus {
    let find = |id: &str| {
        shortcuts
            .iter()
            .find(|s| s.id() == id)
            .map(|s| s.trigger_description().trim().to_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| super::NOT_ASSIGNED.to_owned())
    };
    DesktopStatus::Bound {
        hold: find(HOLD),
        hands_free: find(HANDS_FREE),
        cancel: find(CANCEL),
        listen: find(LISTEN),
    }
}
enum Next {
    Activated(String),
    Deactivated(String),
    Changed(Vec<Shortcut>),
    Due,
    Closed,
}
async fn listen(
    triggers: &[String; 4],
    sink: &Sink,
    translator: &Mutex<Translator>,
    bound: &Mutex<Option<Arc<Bound>>>,
) -> Result<std::convert::Infallible, Stop> {
    let portal = GlobalShortcuts::new()
        .await
        .map_err(|e| Stop::Unavailable(format!("no GlobalShortcuts portal ({e})")))?;
    let session = portal.create_session(Default::default()).await?;
    let wanted = [
        NewShortcut::new(HOLD, "Hold to dictate").preferred_trigger(triggers[0].as_str()),
        NewShortcut::new(HANDS_FREE, "Dictate hands-free (start or finish)")
            .preferred_trigger(triggers[1].as_str()),
        NewShortcut::new(CANCEL, "Cancel dictation, or discard Listen")
            .preferred_trigger(triggers[2].as_str()),
        NewShortcut::new(LISTEN, "Listen: start, or stop and write notes")
            .preferred_trigger(triggers[3].as_str()),
    ];
    let mut activated = portal.receive_activated().await?;
    let mut deactivated = portal.receive_deactivated().await?;
    let mut changed = portal.receive_shortcuts_changed().await?;
    // The first bind shows Plasma's confirmation dialog, so allow time to answer it.
    let response = portal
        .bind_shortcuts(&session, &wanted, None, BindShortcutsOptions::default())
        .await?
        .response();
    match response {
        Ok(result) => sink(HookMessage::Desktop(status(result.shortcuts()))),
        // Declined shortcuts stay listed, unassigned, in the desktop's settings.
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
            sink(HookMessage::Desktop(status(&[])))
        }
        Err(error) => return Err(error.into()),
    }
    let bound_pair = Arc::new((portal, session));
    *bound.lock().expect("portal lock") = Some(bound_pair.clone());
    let mut closed = bound_pair.1.receive_closed().await?;
    loop {
        let due_at = translator.lock().expect("portal lock").due_at();
        let signals = async {
            future::or(
                async {
                    match activated.next().await {
                        Some(a) => Next::Activated(a.shortcut_id().to_owned()),
                        None => Next::Closed,
                    }
                },
                future::or(
                    async {
                        match deactivated.next().await {
                            Some(d) => Next::Deactivated(d.shortcut_id().to_owned()),
                            None => Next::Closed,
                        }
                    },
                    future::or(
                        async {
                            // The portal signals only the session's owner: this one.
                            match changed.next().await {
                                Some(c) => Next::Changed(c.shortcuts().to_vec()),
                                None => Next::Closed,
                            }
                        },
                        async {
                            closed.next().await;
                            Next::Closed
                        },
                    ),
                ),
            )
            .await
        };
        let next = match due_at {
            Some(at) => {
                future::or(signals, async {
                    async_io::Timer::at(at).await;
                    Next::Due
                })
                .await
            }
            None => signals.await,
        };
        let now = Instant::now();
        let events = {
            let mut t = translator.lock().expect("portal lock");
            match next {
                Next::Activated(id) => t.activated(&id),
                Next::Deactivated(id) => {
                    t.deactivated(&id, now);
                    Vec::new()
                }
                Next::Changed(shortcuts) => {
                    if !shortcuts.is_empty() {
                        sink(HookMessage::Desktop(status(&shortcuts)));
                    }
                    Vec::new()
                }
                Next::Due => t.due(now).into_iter().collect(),
                Next::Closed => return Err(Stop::Lost("the portal closed the session".into())),
            }
        };
        for event in events {
            sink(HookMessage::Dictation(event));
        }
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: Plasma's release-before-next-press ends a recording that
    // should go hands-free; a second hold press doesn't finish hands-free;
    // cancel fires while idle; releases leak after a forced stop; a hands-free
    // dictation keeps recording, unreachable, after the portal session is lost.
    use super::*;

    #[test]
    fn hold_then_release_after_grace() {
        let mut t = Translator::default();
        let now = Instant::now();
        assert_eq!(t.activated(HOLD), vec![HotkeyEvent::Start]);
        t.deactivated(HOLD, now);
        assert_eq!(t.due(now), None);
        assert_eq!(t.due(now + RELEASE_GRACE), Some(HotkeyEvent::Release));
        assert_eq!(t.due_at(), None);
    }

    #[test]
    fn hands_free_replaces_the_pending_release() {
        let mut t = Translator::default();
        let now = Instant::now();
        t.activated(HOLD);
        t.deactivated(HOLD, now);
        assert_eq!(t.activated(HANDS_FREE), vec![HotkeyEvent::Lock]);
        assert_eq!(t.due(now + RELEASE_GRACE * 2), None);
        // Either shortcut finishes hands-free.
        assert_eq!(t.activated(HOLD), vec![HotkeyEvent::StopLocked]);
        assert_eq!(
            t.activated(HANDS_FREE),
            vec![HotkeyEvent::Start, HotkeyEvent::Lock]
        );
        assert_eq!(t.activated(HANDS_FREE), vec![HotkeyEvent::StopLocked]);
    }

    #[test]
    fn cancel_and_forced_idle() {
        let mut t = Translator::default();
        assert!(t.activated(CANCEL).is_empty());
        t.activated(HOLD);
        assert_eq!(t.activated(CANCEL), vec![HotkeyEvent::Cancel]);
        t.activated(HOLD);
        t.deactivated(HOLD, Instant::now());
        t.force_idle();
        assert_eq!(t.due(Instant::now() + RELEASE_GRACE), None);
        assert!(t.activated(CANCEL).is_empty());
    }

    #[test]
    fn a_lost_session_ends_every_live_dictation() {
        let now = Instant::now();
        // Held: no release can come from a dead session, so cancel.
        let mut t = Translator::default();
        t.activated(HOLD);
        assert_eq!(t.lost(), Some(HotkeyEvent::CancelSilent));
        // Hands-free, reached from a hold or started directly.
        for start in [&[HOLD, HANDS_FREE][..], &[HANDS_FREE]] {
            let mut t = Translator::default();
            for id in start {
                t.activated(id);
            }
            assert_eq!(t.lost(), Some(HotkeyEvent::CancelSilent), "{start:?}");
            // The translator is idle again, so the next press starts afresh.
            assert_eq!(t.activated(HOLD), vec![HotkeyEvent::Start]);
        }
        // Released just before the loss: the user already finished.
        let mut t = Translator::default();
        t.activated(HOLD);
        t.deactivated(HOLD, now);
        assert_eq!(t.lost(), Some(HotkeyEvent::Release));
        assert_eq!(t.due(now + RELEASE_GRACE), None);
        // Nothing live, nothing to end.
        assert_eq!(Translator::default().lost(), None);
    }

    #[test]
    fn triggers_follow_the_setting_where_the_spec_allows() {
        let listen = Hotkey::listen_default();
        assert_eq!(
            triggers(&Hotkey::default(), &listen),
            [
                "CTRL+LOGO+d",
                "CTRL+LOGO+space",
                "CTRL+LOGO+BackSpace",
                "CTRL+SHIFT+F8"
            ]
        );
        assert_eq!(
            triggers(
                &"Ctrl+Shift+D".parse().expect("chord"),
                &"Alt+F9".parse().expect("chord")
            ),
            [
                "CTRL+SHIFT+d",
                "CTRL+SHIFT+space",
                "CTRL+SHIFT+BackSpace",
                "ALT+F9"
            ]
        );
        assert_eq!(
            triggers(
                &"F8".parse().expect("f8"),
                &"Ctrl+Alt".parse().expect("modifiers")
            ),
            [
                "F8",
                "CTRL+LOGO+space",
                "CTRL+LOGO+BackSpace",
                "CTRL+SHIFT+F8"
            ]
        );
    }

    #[test]
    fn listen_toggles_and_cancel_discards_only_while_it_captures() {
        let now = Instant::now();
        let mut t = Translator::default();
        assert_eq!(t.activated(LISTEN), vec![HotkeyEvent::ListenToggle]);
        // Repeated activations of a held key never stop what the press started.
        assert!(t.activated(LISTEN).is_empty());
        assert!(t.activated(LISTEN).is_empty());
        t.deactivated(LISTEN, now);
        assert_eq!(t.activated(LISTEN), vec![HotkeyEvent::ListenToggle]);
        t.deactivated(LISTEN, now);
        // A session lost while it was held forgets the press.
        t.activated(LISTEN);
        assert_eq!(t.lost(), None);
        assert_eq!(t.activated(LISTEN), vec![HotkeyEvent::ListenToggle]);
        t.deactivated(LISTEN, now);
        assert!(
            t.activated(CANCEL).is_empty(),
            "cancel does nothing while idle"
        );
        t.set_listening(true);
        assert_eq!(t.activated(CANCEL), vec![HotkeyEvent::ListenCancel]);
        assert!(t.activated(CANCEL).is_empty());
        // Dictation keeps the cancel shortcut first, and Listen's press doesn't swallow
        // a held dictation's pending release.
        t.set_listening(true);
        t.activated(HOLD);
        t.deactivated(HOLD, now);
        assert_eq!(t.activated(LISTEN), vec![HotkeyEvent::ListenToggle]);
        t.deactivated(LISTEN, now);
        assert_eq!(t.due(now + RELEASE_GRACE), Some(HotkeyEvent::Release));
        t.activated(HOLD);
        assert_eq!(t.activated(CANCEL), vec![HotkeyEvent::Cancel]);
        assert_eq!(t.activated(CANCEL), vec![HotkeyEvent::ListenCancel]);
    }
}
