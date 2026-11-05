//! What the pill should be showing. It only ever follows the daemon's real state
//! and messages; the animation itself lives in `ui/Overlay.qml`.

use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MESSAGE_TIME: Duration = Duration::from_millis(2600);
/// Long enough to read a sentence and act on it.
const WARNING_TIME: Duration = Duration::from_millis(3500);
const ERROR_TIME: Duration = Duration::from_millis(5000);
/// About what fits in the pill at its widest. Qt elides anything longer.
const PILL_CHARACTERS: usize = 54;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Recording,
    Locked,
    Transcribing,
}

impl Mode {
    pub fn from_state(state: &str) -> Mode {
        match state {
            "recording" => Mode::Recording,
            "recording-hands-free" => Mode::Locked,
            "transcribing" => Mode::Transcribing,
            _ => Mode::Idle,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Mode::Idle => "idle",
            Mode::Recording => "recording",
            Mode::Locked => "locked",
            Mode::Transcribing => "transcribing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Info,
    Success,
    Warn,
    Error,
}

impl Kind {
    pub fn parse(text: &str) -> Kind {
        match text {
            "success" => Kind::Success,
            "warn" => Kind::Warn,
            "error" => Kind::Error,
            _ => Kind::Info,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::Info => "info",
            Kind::Success => "success",
            Kind::Warn => "warn",
            Kind::Error => "error",
        }
    }

    fn time(self) -> Duration {
        match self {
            Kind::Info | Kind::Success => MESSAGE_TIME,
            Kind::Warn => WARNING_TIME,
            Kind::Error => ERROR_TIME,
        }
    }
}

struct Message {
    kind: Kind,
    text: String,
    /// None until it is on screen, which waits for transcribing to finish.
    until: Option<Instant>,
}

pub struct OverlayModel {
    mode: Mode,
    message: Option<Message>,
    idle_pill: bool,
}

impl Default for OverlayModel {
    fn default() -> Self {
        OverlayModel {
            mode: Mode::Idle,
            message: None,
            // Whether it's wanted is known once the daemon says, and then it appears.
            idle_pill: false,
        }
    }
}

impl OverlayModel {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_idle_pill(&mut self, shown: bool) {
        self.idle_pill = shown;
    }

    pub fn set_mode(&mut self, mode: Mode, now: Instant) {
        self.mode = mode;
        match mode {
            // The microphone is live, so the pill must say so, whatever it was
            // in the middle of saying.
            Mode::Recording | Mode::Locked => self.message = None,
            Mode::Transcribing => {}
            Mode::Idle => self.start_message(now),
        }
    }

    /// A result that arrives after a new recording began is not allowed to cover
    /// it. One that arrives just before the daemon reports idle waits for that.
    pub fn show_message(&mut self, kind: Kind, text: &str, now: Instant) {
        if matches!(self.mode, Mode::Recording | Mode::Locked) {
            return;
        }
        self.message = Some(Message {
            kind,
            text: first_sentence(text),
            until: None,
        });
        if self.mode == Mode::Idle {
            self.start_message(now);
        }
    }

    fn start_message(&mut self, now: Instant) {
        if let Some(message) = &mut self.message
            && message.until.is_none()
        {
            message.until = Some(now + message.kind.time());
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self
            .message
            .as_ref()
            .is_some_and(|message| message.until.is_some_and(|until| now >= until))
        {
            self.message = None;
        }
    }

    pub fn to_json(&self) -> Value {
        let shown = self.message.as_ref().filter(|m| m.until.is_some());
        json!({
            "mode": self.mode.as_str(),
            "message": shown.map_or("", |m| m.text.as_str()),
            "kind": shown.map_or("info", |m| m.kind.as_str()),
            "idle_pill": self.idle_pill,
        })
    }
}

/// What fits on the pill's one line. A short message is shown whole, and a long one
/// is cut at its first sentence. All of it stays in the settings window.
fn first_sentence(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= PILL_CHARACTERS {
        return line.to_string();
    }
    match line.find(". ") {
        Some(end) => line[..=end].trim_end().to_string(),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(model: &OverlayModel) -> String {
        model.to_json()["message"].as_str().unwrap().to_string()
    }

    #[test]
    fn modes_follow_the_daemon_states() {
        assert_eq!(Mode::from_state("recording"), Mode::Recording);
        assert_eq!(Mode::from_state("recording-hands-free"), Mode::Locked);
        assert_eq!(Mode::from_state("transcribing"), Mode::Transcribing);
        assert_eq!(Mode::from_state("idle"), Mode::Idle);
        assert_eq!(Mode::from_state("something new"), Mode::Idle);
    }

    #[test]
    fn a_message_lasts_for_its_time_and_then_the_pill_returns_to_idle() {
        let start = Instant::now();
        let mut model = OverlayModel::default();
        model.show_message(Kind::Warn, "Check your mic?", start);
        assert_eq!(shown(&model), "Check your mic?");
        assert_eq!(model.to_json()["kind"], "warn");
        model.tick(start + WARNING_TIME - Duration::from_millis(1));
        assert_eq!(shown(&model), "Check your mic?");
        model.tick(start + WARNING_TIME);
        assert_eq!(shown(&model), "");
        assert_eq!(model.to_json()["mode"], "idle");
    }

    #[test]
    fn a_result_cannot_cover_a_new_recording() {
        let start = Instant::now();
        let mut model = OverlayModel::default();
        model.set_mode(Mode::Locked, start);
        model.show_message(Kind::Error, "An older dictation failed", start);
        assert_eq!(shown(&model), "");
        model.set_mode(Mode::Idle, start);
        assert_eq!(shown(&model), "");
    }

    #[test]
    fn starting_to_record_clears_a_message_at_once() {
        let start = Instant::now();
        let mut model = OverlayModel::default();
        model.show_message(Kind::Info, "Hold Meta+Ctrl+D to dictate", start);
        model.set_mode(Mode::Recording, start);
        assert_eq!(shown(&model), "");
        assert_eq!(model.to_json()["mode"], "recording");
    }

    #[test]
    fn a_result_just_before_idle_waits_and_gets_its_full_time() {
        let start = Instant::now();
        let mut model = OverlayModel::default();
        model.set_mode(Mode::Transcribing, start);
        model.show_message(Kind::Warn, "Copied", start);
        assert_eq!(shown(&model), "", "transcribing is still on screen");
        model.tick(start + Duration::from_secs(60));
        let later = start + Duration::from_secs(60);
        model.set_mode(Mode::Idle, later);
        assert_eq!(shown(&model), "Copied");
        model.tick(later + WARNING_TIME);
        assert_eq!(shown(&model), "");
    }

    #[test]
    fn long_errors_keep_their_first_sentence() {
        assert_eq!(
            first_sentence(
                "whisper-cli was not found. Install whisper.cpp and put it on your PATH"
            ),
            "whisper-cli was not found."
        );
        // Short enough to show whole, and the second half is the useful part.
        assert_eq!(
            first_sentence("Copied. You switched windows, so press Ctrl + V"),
            "Copied. You switched windows, so press Ctrl + V"
        );
        assert_eq!(
            first_sentence("Backend: local\nSaved in x"),
            "Backend: local"
        );
        assert_eq!(first_sentence("v1.5 works"), "v1.5 works");
    }
}
