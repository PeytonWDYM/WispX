//! Global shortcuts through the XDG `GlobalShortcuts` portal.
//!
//! This is what makes hold-to-talk work on Plasma without reading the keyboard:
//! the desktop owns the keys, asks the user once to confirm them, and tells us
//! when each shortcut goes down and comes back up.

use crate::config::ShortcutsConfig;
use crate::dbus::{Connection, Error, Message, Value};
use crate::portal::{self, Response};
use crate::session::Input;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const HOLD: &str = "dictate";
pub const HANDS_FREE: &str = "hands-free";
pub const CANCEL: &str = "cancel";

const INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// Each failed attempt could put a dialog on screen, so they soon slow right down.
const RETRY_LIMIT: Duration = Duration::from_secs(300);
/// A session that lasted this long was working, so what ended it was not the
/// same failure happening again.
const HEALTHY_SESSION: Duration = Duration::from_secs(60);
/// Long enough for someone to read and answer the confirmation dialog.
const DIALOG_WAIT: Duration = Duration::from_secs(600);
const CALL_WAIT: Duration = Duration::from_secs(30);
const IDLE_WAIT: Duration = Duration::from_secs(1);
/// See [`Translator`].
const RELEASE_GRACE: Duration = Duration::from_millis(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    /// Shortcuts are bound; holds what the desktop says triggers them.
    Bound(String),
    /// The desktop has no such portal, so `wispx` commands must be bound instead.
    Unavailable(String),
    /// The user declined the shortcuts in the desktop's dialog.
    Declined,
    Disconnected(String),
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Status::Starting => "connecting to the GlobalShortcuts portal".to_string(),
            Status::Bound(triggers) => format!("portal shortcuts: {triggers}"),
            Status::Unavailable(reason) => format!("no portal shortcuts: {reason}"),
            Status::Declined => "portal shortcuts declined; assign them in System Settings > \
                                 Keyboard > Shortcuts > WispX"
                .to_string(),
            Status::Disconnected(reason) => format!("portal disconnected: {reason}"),
        }
    }
}

pub enum Event {
    Input(Input),
    Status(Status),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// Turns portal signals into dictation inputs.
///
/// Plasma releases the previous shortcut the moment another one fires. So going
/// from holding `dictate` to pressing `hands-free` (or `cancel`) first looks
/// exactly like letting go, which would end the recording right before the
/// press that was meant to keep it going or throw it away. Holding each release
/// back for a few milliseconds lets the following press replace it.
#[derive(Debug, Default)]
pub struct Translator {
    release_due: Option<Instant>,
    held: bool,
}

impl Translator {
    pub fn activated(&mut self, shortcut: &str) -> Option<Input> {
        let input = match shortcut {
            HOLD => Input::Press,
            HANDS_FREE => Input::Toggle,
            CANCEL => Input::Cancel,
            _ => return None,
        };
        self.release_due = None;
        self.held = shortcut == HOLD;
        Some(input)
    }

    pub fn deactivated(&mut self, shortcut: &str, now: Instant) {
        if shortcut == HOLD {
            self.release_due = Some(now + RELEASE_GRACE);
            self.held = false;
        }
    }

    /// Whether this source pressed the hold shortcut and has not released it.
    /// A release promised by the portal can no longer arrive once it is gone.
    pub fn awaiting_release(&self) -> bool {
        self.held || self.release_due.is_some()
    }

    /// The held-back release, once its grace period has passed.
    pub fn due(&mut self, now: Instant) -> Option<Input> {
        match self.release_due {
            Some(due) if now >= due => {
                self.release_due = None;
                Some(Input::Release)
            }
            _ => None,
        }
    }

    pub fn wait(&self, now: Instant) -> Duration {
        self.release_due
            .map_or(IDLE_WAIT, |due| due.saturating_duration_since(now))
    }
}

/// Runs the portal session on its own thread for the life of the daemon.
pub fn spawn(config: ShortcutsConfig, sink: Sink) {
    thread::spawn(move || {
        sink(Event::Status(Status::Starting));
        let mut delay = RETRY_AFTER;
        loop {
            let started = Instant::now();
            let mut translator = Translator::default();
            let stop = run_session(&config, &sink, &mut translator);
            if translator.awaiting_release() {
                sink(Event::Input(Input::CancelSilent));
            }
            match stop {
                // There is nothing to wait for; `wispx` commands still work.
                Stop::Unavailable(reason) => {
                    sink(Event::Status(Status::Unavailable(reason)));
                    return;
                }
                Stop::Lost(reason) => sink(Event::Status(Status::Disconnected(reason))),
            }
            delay = next_retry(delay, started.elapsed());
            thread::sleep(delay);
        }
    });
}

fn next_retry(previous: Duration, session_lasted: Duration) -> Duration {
    if session_lasted >= HEALTHY_SESSION {
        RETRY_AFTER
    } else {
        previous.saturating_mul(2).min(RETRY_LIMIT)
    }
}

enum Stop {
    Unavailable(String),
    Lost(String),
}

impl From<Error> for Stop {
    fn from(error: Error) -> Self {
        Stop::Lost(error.to_string())
    }
}

/// Only ever ends by losing the session, one way or another.
fn run_session(config: &ShortcutsConfig, sink: &Sink, translator: &mut Translator) -> Stop {
    match listen(config, sink, translator) {
        Ok(never) => match never {},
        Err(stop) => stop,
    }
}

fn listen(
    config: &ShortcutsConfig,
    sink: &Sink,
    translator: &mut Translator,
) -> Result<std::convert::Infallible, Stop> {
    let mut bus = Connection::session()?;
    if portal::interface_version(&mut bus, INTERFACE)?.is_none() {
        return Err(Stop::Unavailable(
            "this desktop's portal has no GlobalShortcuts interface".to_string(),
        ));
    }
    // Without the desktop entry the portal still works, under a generic name.
    let _ = portal::register_app(&mut bus);

    let session = create_session(&mut bus)?;
    bus.add_match(
        &format!(
            "type='signal',sender='{}',interface='{INTERFACE}'",
            portal::BUS_NAME
        ),
        CALL_WAIT,
    )?;
    bus.add_match(
        &format!(
            "type='signal',sender='{}',interface='{}',path='{session}'",
            portal::BUS_NAME,
            portal::SESSION_INTERFACE
        ),
        CALL_WAIT,
    )?;

    let status = bind(&mut bus, &session, config)?;
    sink(Event::Status(status));

    loop {
        let now = Instant::now();
        let message = bus.next_message(translator.wait(now))?;
        let now = Instant::now();
        if let Some(message) = message {
            if message.is_signal(portal::SESSION_INTERFACE, "Closed") {
                return Err(Stop::Lost("the portal closed the session".to_string()));
            }
            if let Some(input) = handle_signal(&message, &session, translator, now) {
                sink(Event::Input(input));
            }
        }
        if let Some(input) = translator.due(Instant::now()) {
            sink(Event::Input(input));
        }
    }
}

fn create_session(bus: &mut Connection) -> Result<String, Stop> {
    // A stable token gives Plasma a stable identity even without a registered
    // application, so shortcuts are not requested anew on every start.
    let options = vec![(
        "session_handle_token".to_string(),
        Value::str("wispx_shortcuts"),
    )];
    let response = portal::request(
        bus,
        INTERFACE,
        "CreateSession",
        Vec::new(),
        options,
        CALL_WAIT,
    )?;
    match response {
        Response::Success(results) => results
            .lookup("session_handle")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Stop::Lost("the portal returned no session".to_string())),
        other => Err(Stop::Unavailable(format!(
            "the portal refused a session ({other:?})"
        ))),
    }
}

/// The `a(sa{sv})` list describing WispX's shortcuts.
pub fn shortcut_list(config: &ShortcutsConfig) -> Value {
    let entry = |id: &str, description: &str, trigger: &str| {
        Value::Struct(vec![
            Value::str(id),
            Value::vardict(vec![
                ("description".to_string(), Value::str(description)),
                ("preferred_trigger".to_string(), Value::str(trigger)),
            ]),
        ])
    };
    Value::Array {
        element: "(sa{sv})".to_string(),
        items: vec![
            entry(HOLD, "Hold to dictate", &config.hold),
            entry(
                HANDS_FREE,
                "Dictate hands-free (start, keep going, or finish)",
                &config.hands_free,
            ),
            entry(CANCEL, "Cancel dictation", &config.cancel),
        ],
    }
}

fn bind(bus: &mut Connection, session: &str, config: &ShortcutsConfig) -> Result<Status, Stop> {
    let arguments = vec![Value::path(session), shortcut_list(config), Value::str("")];
    let response = portal::request(
        bus,
        INTERFACE,
        "BindShortcuts",
        arguments,
        Vec::new(),
        DIALOG_WAIT,
    )?;
    Ok(match response {
        Response::Success(results) => Status::Bound(describe_triggers(&results)),
        // Plasma records declined shortcuts without keys, so they can still be
        // assigned in System Settings, and keep listening for that.
        Response::Cancelled => Status::Declined,
        Response::Failed(code) => {
            return Err(Stop::Lost(format!("binding shortcuts failed ({code})")));
        }
    })
}

/// "hands-free: Meta+Ctrl+Space, ..." from a `BindShortcuts` result.
fn describe_triggers(results: &Value) -> String {
    let Some(Value::Array { items, .. }) = results.lookup("shortcuts") else {
        return "bound".to_string();
    };
    let mut parts: Vec<String> = items
        .iter()
        .filter_map(|item| {
            let Value::Struct(fields) = item else {
                return None;
            };
            let id = fields.first()?.as_str()?;
            let trigger = fields
                .get(1)?
                .lookup("trigger_description")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("not assigned");
            Some(format!("{id}: {trigger}"))
        })
        .collect();
    parts.sort();
    if parts.is_empty() {
        "bound".to_string()
    } else {
        parts.join(", ")
    }
}

fn handle_signal(
    message: &Message,
    session: &str,
    translator: &mut Translator,
    now: Instant,
) -> Option<Input> {
    let activated = message.is_signal(INTERFACE, "Activated");
    if !activated && !message.is_signal(INTERFACE, "Deactivated") {
        return None;
    }
    if message.body.first()?.as_str()? != session {
        return None;
    }
    let shortcut = message.body.get(1)?.as_str()?;
    if activated {
        translator.activated(shortcut)
    } else {
        translator.deactivated(shortcut, now);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_slow_down_until_a_session_has_worked() {
        let quick = Duration::from_secs(1);
        let first = next_retry(RETRY_AFTER, quick);
        assert!(first > RETRY_AFTER);
        let mut delay = first;
        for _ in 0..20 {
            delay = next_retry(delay, quick);
        }
        assert_eq!(delay, RETRY_LIMIT);
        assert_eq!(next_retry(delay, HEALTHY_SESSION), RETRY_AFTER);
    }

    fn config() -> ShortcutsConfig {
        ShortcutsConfig {
            hold: "CTRL+LOGO+d".to_string(),
            hands_free: "CTRL+LOGO+space".to_string(),
            cancel: "CTRL+LOGO+BackSpace".to_string(),
        }
    }

    #[test]
    fn hold_release_is_delivered_after_the_grace_period() {
        let start = Instant::now();
        let mut translator = Translator::default();
        assert_eq!(translator.activated(HOLD), Some(Input::Press));
        translator.deactivated(HOLD, start);
        assert_eq!(translator.due(start), None);
        assert_eq!(translator.wait(start), RELEASE_GRACE);
        assert_eq!(translator.due(start + RELEASE_GRACE), Some(Input::Release));
        assert_eq!(translator.due(start + RELEASE_GRACE), None);
        assert_eq!(translator.wait(start + RELEASE_GRACE), IDLE_WAIT);
    }

    #[test]
    fn plasma_releasing_hold_for_hands_free_does_not_end_the_recording() {
        // Plasma sends Deactivated(dictate) and then Activated(hands-free).
        let start = Instant::now();
        let mut translator = Translator::default();
        translator.activated(HOLD);
        translator.deactivated(HOLD, start);
        assert_eq!(translator.activated(HANDS_FREE), Some(Input::Toggle));
        assert_eq!(translator.due(start + RELEASE_GRACE * 2), None);
    }

    #[test]
    fn plasma_releasing_hold_for_cancel_cancels_instead_of_transcribing() {
        let start = Instant::now();
        let mut translator = Translator::default();
        translator.activated(HOLD);
        translator.deactivated(HOLD, start);
        assert_eq!(translator.activated(CANCEL), Some(Input::Cancel));
        assert_eq!(translator.due(start + RELEASE_GRACE * 2), None);
    }

    #[test]
    fn a_hold_in_progress_is_known_so_only_it_is_cancelled_on_disconnect() {
        let start = Instant::now();
        let mut translator = Translator::default();
        assert!(!translator.awaiting_release());
        translator.activated(HOLD);
        assert!(translator.awaiting_release());
        translator.deactivated(HOLD, start);
        assert!(
            translator.awaiting_release(),
            "the release is still pending"
        );
        translator.due(start + RELEASE_GRACE);
        assert!(!translator.awaiting_release());

        // A hands-free recording needs no release, and neither does a press that
        // came from `wispx press` rather than this portal session.
        translator.activated(HANDS_FREE);
        assert!(!translator.awaiting_release());
    }

    #[test]
    fn releases_of_other_shortcuts_and_unknown_ids_are_ignored() {
        let start = Instant::now();
        let mut translator = Translator::default();
        translator.deactivated(HANDS_FREE, start);
        translator.deactivated(CANCEL, start);
        assert_eq!(translator.due(start + RELEASE_GRACE), None);
        assert_eq!(translator.activated("unknown"), None);
    }

    fn signal(member: &str, session: &str, id: &str) -> Message {
        Message::for_tests(
            INTERFACE,
            member,
            vec![
                Value::path(session),
                Value::str(id),
                Value::Uint64(0),
                Value::vardict(Vec::new()),
            ],
        )
    }

    #[test]
    fn only_signals_for_our_session_count() {
        let now = Instant::now();
        let mut translator = Translator::default();
        let ours = signal("Activated", "/s/mine", HOLD);
        assert_eq!(
            handle_signal(&ours, "/s/mine", &mut translator, now),
            Some(Input::Press)
        );
        let theirs = signal("Activated", "/s/other", HOLD);
        assert_eq!(
            handle_signal(&theirs, "/s/mine", &mut translator, now),
            None
        );

        let release = signal("Deactivated", "/s/mine", HOLD);
        assert_eq!(
            handle_signal(&release, "/s/mine", &mut translator, now),
            None
        );
        assert_eq!(translator.due(now + RELEASE_GRACE), Some(Input::Release));

        let other = signal("ShortcutsChanged", "/s/mine", HOLD);
        assert_eq!(handle_signal(&other, "/s/mine", &mut translator, now), None);
    }

    #[test]
    fn requested_shortcuts_use_xdg_trigger_syntax() {
        let Value::Array { element, items } = shortcut_list(&config()) else {
            panic!("not an array");
        };
        assert_eq!(element, "(sa{sv})");
        let found: Vec<(String, String)> = items
            .iter()
            .map(|item| {
                let Value::Struct(fields) = item else {
                    panic!("not a struct");
                };
                (
                    fields[0].as_str().unwrap().to_string(),
                    fields[1]
                        .lookup("preferred_trigger")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                (HOLD.to_string(), "CTRL+LOGO+d".to_string()),
                (HANDS_FREE.to_string(), "CTRL+LOGO+space".to_string()),
                (CANCEL.to_string(), "CTRL+LOGO+BackSpace".to_string()),
            ]
        );
    }

    #[test]
    fn the_bound_triggers_are_described_for_status_output() {
        let results = Value::vardict(vec![(
            "shortcuts".to_string(),
            Value::Array {
                element: "(sa{sv})".to_string(),
                items: vec![
                    Value::Struct(vec![
                        Value::str(HOLD),
                        Value::vardict(vec![(
                            "trigger_description".to_string(),
                            Value::str("Ctrl+Meta+D"),
                        )]),
                    ]),
                    Value::Struct(vec![
                        Value::str(CANCEL),
                        Value::vardict(vec![("trigger_description".to_string(), Value::str(""))]),
                    ]),
                ],
            },
        )]);
        assert_eq!(
            describe_triggers(&results),
            "cancel: not assigned, dictate: Ctrl+Meta+D"
        );
        assert_eq!(describe_triggers(&Value::vardict(Vec::new())), "bound");
    }
}
