//! Portable dictation controller. Platform hosts execute the returned effects.
use crate::{
    audio::recorder::{Levels, RecordError, Recorder},
    settings::Settings,
    transcribe::{TranscribeError, auth},
};
use std::time::{Duration, Instant};

pub const MAX_RECORDING: Duration = Duration::from_secs(600);
#[derive(Clone, Copy, Debug)]
pub enum MessageKind {
    Info,
    Warn,
    Error,
    Success,
}
#[derive(Clone)]
pub enum Pill {
    Idle,
    Recording(Levels),
    Locked,
    Transcribing,
    Done,
    Message {
        text: String,
        kind: MessageKind,
        seconds: f64,
    },
}
impl Pill {
    pub fn message(text: impl Into<String>, kind: MessageKind, seconds: f64) -> Self {
        Self::Message {
            text: text.into(),
            kind,
            seconds,
        }
    }
}
struct ActiveRecording {
    recorder: Recorder,
    started: Instant,
}
/// Completion can overlap the next recording; only that recording owns the pill.
#[derive(Default)]
pub struct Controller {
    active: Option<ActiveRecording>,
    pub in_flight: usize,
    pub last_transcript: Option<String>,
}
pub enum Finish {
    Quiet,
    TooQuiet,
    Upload(Recorder),
}
/// How a finished transcript reached the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// Pasted into the window that was focused when dictation ended.
    Pasted,
    /// Focus moved while transcribing, so the text was only copied.
    FocusChanged,
    /// This desktop offers no verified way to paste, so the text was only copied.
    Copied,
    /// Neither pasted nor copied (no clipboard); the tray can retry the copy.
    Failed,
}
/// Finished text, its delivery outcome, and the recording's spoken duration.
pub struct Dictation {
    pub text: String,
    pub delivery: Delivery,
    pub spoken: Duration,
}
impl Controller {
    pub fn recording(&self) -> bool {
        self.active.is_some()
    }
    pub fn start(&mut self, settings: &Settings) -> Result<Option<Pill>, StartError> {
        if self.recording() {
            return Ok(None);
        }
        if auth::login().is_none() {
            return Err(StartError::NoLogin);
        }
        let recorder = Recorder::start(&settings.microphone)?;
        let levels = recorder.levels.clone();
        self.active = Some(ActiveRecording {
            recorder,
            started: Instant::now(),
        });
        Ok(Some(Pill::Recording(levels)))
    }
    pub fn cancel(&mut self) -> Option<Pill> {
        self.active.take()?;
        Some(if self.in_flight == 0 {
            Pill::Idle
        } else {
            Pill::Transcribing
        })
    }
    pub fn finish(&mut self) -> Option<Finish> {
        let active = self.active.take()?;
        if active.started.elapsed() < Duration::from_millis(300) {
            return Some(Finish::Quiet);
        }
        if active.recorder.levels.peak() < 0.12 {
            return Some(Finish::TooQuiet);
        }
        self.in_flight += 1;
        Some(Finish::Upload(active.recorder))
    }
    pub fn complete(&mut self, result: Result<Dictation, DictationError>) -> Option<Pill> {
        self.in_flight -= 1;
        let pill = match result {
            Ok(Dictation { text, delivery, .. }) if !text.trim().is_empty() => {
                self.last_transcript = Some(text);
                match delivery {
                    Delivery::Pasted => Pill::Done,
                    Delivery::FocusChanged => Pill::message(
                        "Copied. You switched windows, so press Ctrl + V",
                        MessageKind::Info,
                        3.5,
                    ),
                    Delivery::Copied => {
                        Pill::message("Copied. Press Ctrl + V to paste", MessageKind::Info, 3.5)
                    }
                    Delivery::Failed => Pill::message(
                        "Couldn't paste or copy. Try Copy last transcript",
                        MessageKind::Error,
                        4.0,
                    ),
                }
            }
            Ok(_) => Pill::message("Didn't catch that", MessageKind::Warn, 2.6),
            Err(DictationError::Transcribe(error)) => {
                Pill::message(error.to_string(), MessageKind::Error, 3.5)
            }
            Err(DictationError::Recording(_)) => {
                Pill::message("Transcription failed", MessageKind::Error, 2.6)
            }
        };
        (!self.recording()).then_some(pill)
    }
}
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("Sign in to the Codex app first")]
    NoLogin,
    #[error("No microphone found")]
    Microphone(#[from] RecordError),
}
#[derive(Debug, thiserror::Error)]
pub enum DictationError {
    #[error("{0}")]
    Transcribe(#[from] TranscribeError),
    #[error("Transcription failed")]
    Recording(#[from] RecordError),
}

#[cfg(test)]
mod tests {
    // Failure modes: a completion while recording steals the pill; an empty
    // transcript replaces the last one; copy-only delivery claims a paste; a
    // failed copy claims the text is on the clipboard or loses the transcript.
    use super::*;

    fn completed(delivery: Delivery) -> (Controller, Option<Pill>) {
        let mut controller = Controller {
            in_flight: 1,
            ..Controller::default()
        };
        let pill = controller.complete(Ok(Dictation {
            text: "hello".into(),
            delivery,
            spoken: Duration::from_secs(1),
        }));
        (controller, pill)
    }

    #[test]
    fn delivery_selects_the_pill_and_keeps_the_transcript() {
        let (controller, pill) = completed(Delivery::Pasted);
        assert!(matches!(pill, Some(Pill::Done)));
        assert_eq!(controller.last_transcript.as_deref(), Some("hello"));
        assert_eq!(controller.in_flight, 0);
        for (delivery, expected) in [
            (Delivery::FocusChanged, "You switched windows"),
            (Delivery::Copied, "Press Ctrl + V to paste"),
            (Delivery::Failed, "Couldn't paste or copy"),
        ] {
            let (controller, pill) = completed(delivery);
            assert_eq!(controller.last_transcript.as_deref(), Some("hello"));
            match pill {
                Some(Pill::Message { text, .. }) => {
                    assert!(text.contains(expected), "{text}");
                    assert_eq!(delivery == Delivery::Failed, !text.starts_with("Copied"));
                }
                _ => panic!("{delivery:?} must explain the copy"),
            }
        }
    }

    #[test]
    fn empty_transcripts_keep_the_previous_one() {
        let mut controller = Controller {
            in_flight: 1,
            last_transcript: Some("before".into()),
            ..Controller::default()
        };
        let pill = controller.complete(Ok(Dictation {
            text: "  ".into(),
            delivery: Delivery::Pasted,
            spoken: Duration::from_secs(1),
        }));
        assert!(matches!(pill, Some(Pill::Message { .. })));
        assert_eq!(controller.last_transcript.as_deref(), Some("before"));
    }
}
