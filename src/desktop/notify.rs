//! Feedback: desktop notifications in place of the Windows overlay, plus cues.
//!
//! Each runs on its own thread, so a slow notification server or sound system
//! can never delay dictation. Transcripts never go into notifications, because
//! notification history is stored and may be shown on a lock screen.

use crate::config::NotifyLevel;
use crate::dbus::{Connection, Value};
use crate::paths;
use crate::process::{self, Spec, Toolbox};
use crate::sounds::Cue;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const BUS_NAME: &str = "org.freedesktop.Notifications";
const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Quick taps are not worth a popup; they usually end before this.
const RECORDING_DELAY: Duration = Duration::from_millis(400);
const RECONNECT_PAUSE: Duration = Duration::from_secs(5);
const SOUND_TIMEOUT: Duration = Duration::from_secs(3);
const IDLE: Duration = Duration::from_secs(3600);
/// Replaced or closed long before this. The cap only matters if the daemon dies
/// without getting to close the popup.
const UNTIL_REPLACED_MS: i32 = 30 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    Recording { locked: bool },
    Transcribing(String),
    Pasted,
    Done(String),
    Warn(String),
    Error(String),
    Clear,
}

impl Note {
    pub fn allowed(&self, level: NotifyLevel) -> bool {
        match level {
            NotifyLevel::All => true,
            NotifyLevel::Errors => matches!(self, Note::Error(_) | Note::Clear),
            NotifyLevel::None => matches!(self, Note::Clear),
        }
    }

    /// (summary, body, milliseconds on screen)
    fn content(&self) -> Option<(&str, &str, i32)> {
        match self {
            Note::Recording { locked: false } => Some(("Recording", "", UNTIL_REPLACED_MS)),
            Note::Recording { locked: true } => {
                Some(("Recording hands-free", "", UNTIL_REPLACED_MS))
            }
            Note::Transcribing(text) => Some((text, "", UNTIL_REPLACED_MS)),
            Note::Pasted => Some(("Pasted", "", 2500)),
            Note::Done(text) => Some((text, "", 2500)),
            Note::Warn(text) => Some(("WispX", text, 5000)),
            Note::Error(text) => Some(("WispX problem", text, 10000)),
            Note::Clear => None,
        }
    }
}

enum Command {
    Show(Note),
    /// Answers once everything sent earlier reached the notification server.
    Flush(Sender<()>),
}

pub struct Feedback {
    notes: Sender<Command>,
    sounds: Sender<Cue>,
}

impl Feedback {
    pub fn spawn(runtime: PathBuf, tools: &Toolbox) -> Self {
        let (notes, note_rx) = mpsc::channel();
        let (sounds, sound_rx) = mpsc::channel();
        thread::spawn(move || notification_loop(note_rx));
        let player = find_player(tools);
        thread::spawn(move || sound_loop(sound_rx, player, runtime));
        Feedback { notes, sounds }
    }

    pub fn note(&self, note: Note) {
        let _ = self.notes.send(Command::Show(note));
    }

    /// Takes down any popup before the process exits.
    pub fn clear_and_wait(&self, limit: Duration) {
        self.note(Note::Clear);
        let (done, finished) = mpsc::channel();
        if self.notes.send(Command::Flush(done)).is_ok() {
            let _ = finished.recv_timeout(limit);
        }
    }

    pub fn sound(&self, cue: Cue) {
        let _ = self.sounds.send(cue);
    }
}

/// Removes the cue files written for the sound player.
pub fn remove_cue_files(runtime: &Path) {
    for cue in Cue::ALL {
        let _ = fs::remove_file(runtime.join(cue.file_name()));
    }
}

/// A connection plus the id of the popup to keep updating in place.
#[derive(Default)]
struct Notifier {
    bus: Option<Connection>,
    current: u32,
    retry_at: Option<Instant>,
}

impl Notifier {
    fn connection(&mut self) -> Option<&mut Connection> {
        if self.bus.is_none() {
            if self.retry_at.is_some_and(|at| Instant::now() < at) {
                return None;
            }
            match Connection::session() {
                Ok(bus) => self.bus = Some(bus),
                Err(_) => {
                    self.retry_at = Some(Instant::now() + RECONNECT_PAUSE);
                    return None;
                }
            }
        }
        self.bus.as_mut()
    }

    fn show(&mut self, summary: &str, body: &str, timeout_ms: i32) {
        let replaces = self.current;
        let Some(bus) = self.connection() else {
            return;
        };
        let reply = bus.call(
            BUS_NAME,
            OBJECT_PATH,
            BUS_NAME,
            "Notify",
            vec![
                Value::str("WispX"),
                Value::Uint32(replaces),
                Value::str("audio-input-microphone"),
                Value::str(summary),
                Value::str(body),
                Value::Array {
                    element: "s".to_string(),
                    items: Vec::new(),
                },
                Value::vardict(vec![
                    (
                        "desktop-entry".to_string(),
                        Value::str(crate::portal::APP_ID),
                    ),
                    ("transient".to_string(), Value::Bool(true)),
                ]),
                Value::Int32(timeout_ms),
            ],
            CALL_TIMEOUT,
        );
        match reply {
            Ok(values) => {
                self.current = values.first().and_then(Value::as_u32).unwrap_or(0);
            }
            Err(error) => self.lost(&error),
        }
    }

    fn close(&mut self) {
        let current = std::mem::take(&mut self.current);
        if current == 0 {
            return;
        }
        let Some(bus) = self.connection() else {
            return;
        };
        if let Err(error) = bus.call(
            BUS_NAME,
            OBJECT_PATH,
            BUS_NAME,
            "CloseNotification",
            vec![Value::Uint32(current)],
            CALL_TIMEOUT,
        ) {
            self.lost(&error);
        }
    }

    fn lost(&mut self, error: &crate::dbus::Error) {
        // A bus without a notification server keeps the connection: the server
        // may start later, and the failed call did no harm.
        if !matches!(error, crate::dbus::Error::Remote { .. }) {
            self.bus = None;
            self.retry_at = Some(Instant::now() + RECONNECT_PAUSE);
        }
        self.current = 0;
    }

    fn apply(&mut self, note: &Note) {
        match note.content() {
            Some((summary, body, timeout)) => self.show(summary, body, timeout),
            None => self.close(),
        }
    }
}

fn notification_loop(notes: Receiver<Command>) {
    let mut notifier = Notifier::default();
    let mut delayed = Delayed::default();
    loop {
        match notes.recv_timeout(delayed.wait(Instant::now())) {
            Ok(Command::Show(note)) => {
                if let Some(ready) = delayed.offer(note, Instant::now()) {
                    notifier.apply(&ready);
                }
            }
            Ok(Command::Flush(done)) => {
                let _ = done.send(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(note) = delayed.due(Instant::now()) {
            notifier.apply(&note);
        }
    }
}

/// Holds a "recording" popup back until it is clear the press was not a tap.
#[derive(Default)]
struct Delayed {
    pending: Option<(Note, Instant)>,
}

impl Delayed {
    /// What to show right away, if anything.
    fn offer(&mut self, note: Note, now: Instant) -> Option<Note> {
        if matches!(note, Note::Recording { .. }) {
            // Locking keeps the original deadline, so the popup is never later.
            let due = self
                .pending
                .take()
                .map_or(now + RECORDING_DELAY, |(_, due)| due);
            self.pending = Some((note, due));
            return None;
        }
        self.pending = None;
        Some(note)
    }

    fn due(&mut self, now: Instant) -> Option<Note> {
        match &self.pending {
            Some((_, due)) if now >= *due => self.pending.take().map(|(note, _)| note),
            _ => None,
        }
    }

    fn wait(&self, now: Instant) -> Duration {
        self.pending
            .as_ref()
            .map_or(IDLE, |(_, due)| due.saturating_duration_since(now))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Player {
    program: PathBuf,
    args: Vec<OsString>,
}

/// Which tool plays the cues, for `wispx doctor`.
pub fn player_name(tools: &Toolbox) -> Option<String> {
    find_player(tools).map(|player| process::short_name(&player.program))
}

fn find_player(tools: &Toolbox) -> Option<Player> {
    let candidates: [(&str, &[&str]); 3] = [("pw-play", &[]), ("paplay", &[]), ("aplay", &["-q"])];
    candidates.iter().find_map(|(name, args)| {
        tools.find(name).map(|program| Player {
            program,
            args: args.iter().map(OsString::from).collect(),
        })
    })
}

fn sound_loop(cues: Receiver<Cue>, player: Option<Player>, runtime: PathBuf) {
    while let Ok(mut cue) = cues.recv() {
        // Falling behind must not leave a string of late clicks.
        while let Ok(newer) = cues.try_recv() {
            cue = newer;
        }
        if let Some(player) = &player {
            let _ = play(player, cue, &runtime);
        }
    }
}

fn play(player: &Player, cue: Cue, runtime: &Path) -> Result<(), String> {
    let path = runtime.join(cue.file_name());
    if !path.exists() {
        let mut file = paths::create_private_file(&path).map_err(|error| error.to_string())?;
        file.write_all(&cue.wav())
            .map_err(|error| error.to_string())?;
    }
    let mut args = player.args.clone();
    args.push(path.into_os_string());
    let mut spec = Spec::new(&player.program, &args, SOUND_TIMEOUT);
    spec.capture = false;
    process::run(spec)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn levels_filter_notes_but_always_allow_clearing() {
        let error = Note::Error("x".to_string());
        let done = Note::Done("x".to_string());
        assert!(error.allowed(NotifyLevel::All));
        assert!(done.allowed(NotifyLevel::All));
        assert!(error.allowed(NotifyLevel::Errors));
        assert!(!done.allowed(NotifyLevel::Errors));
        assert!(!error.allowed(NotifyLevel::None));
        assert!(Note::Clear.allowed(NotifyLevel::None));
    }

    #[test]
    fn a_tap_never_produces_a_recording_popup() {
        let start = Instant::now();
        let mut delayed = Delayed::default();
        assert_eq!(
            delayed.offer(Note::Recording { locked: false }, start),
            None
        );
        assert_eq!(delayed.due(start), None);
        assert_eq!(delayed.offer(Note::Clear, start), Some(Note::Clear));
        assert_eq!(delayed.due(start + RECORDING_DELAY * 2), None);
    }

    #[test]
    fn a_longer_recording_shows_after_the_delay_with_its_latest_state() {
        let start = Instant::now();
        let mut delayed = Delayed::default();
        delayed.offer(Note::Recording { locked: false }, start);
        assert_eq!(delayed.wait(start), RECORDING_DELAY);
        delayed.offer(
            Note::Recording { locked: true },
            start + Duration::from_millis(200),
        );
        assert_eq!(
            delayed.due(start + RECORDING_DELAY),
            Some(Note::Recording { locked: true })
        );
        assert_eq!(delayed.wait(start + RECORDING_DELAY), IDLE);
    }

    #[test]
    fn other_notes_appear_at_once_and_replace_a_pending_popup() {
        let start = Instant::now();
        let mut delayed = Delayed::default();
        delayed.offer(Note::Recording { locked: false }, start);
        let note = Note::Transcribing("Transcribing".to_string());
        assert_eq!(delayed.offer(note.clone(), start), Some(note));
        assert_eq!(delayed.due(start + RECORDING_DELAY), None);
    }

    #[test]
    fn players_are_tried_in_order() {
        let temp = TempDir::new();
        let tools = Toolbox::with_path(temp.path());
        assert_eq!(find_player(&tools), None);

        temp.script("aplay", "exit 0");
        let found = find_player(&tools).unwrap();
        assert_eq!(found.args, [OsString::from("-q")]);

        let pw = temp.script("pw-play", "exit 0");
        assert_eq!(find_player(&tools).unwrap().program, pw);
    }

    #[test]
    fn cues_are_private_valid_wavs_handed_to_the_player() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new();
        let log = temp.path().join("played");
        let program = temp.script("pw-play", &format!(r#"echo "$1" >> {}"#, log.display()));
        let player = Player {
            program,
            args: Vec::new(),
        };
        play(&player, Cue::Start, temp.path()).unwrap();
        play(&player, Cue::Start, temp.path()).unwrap();

        let file = temp.path().join(Cue::Start.file_name());
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 2);
        let meta = fs::metadata(&file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        crate::audio::wav::parse(&fs::read(&file).unwrap()).unwrap();

        remove_cue_files(temp.path());
        assert!(!file.exists());
    }
}
