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
    pub fn complete(&mut self, result: Result<(String, bool), DictationError>) -> Option<Pill> {
        self.in_flight -= 1;
        let pill = match result {
            Ok((text, pasted)) if !text.trim().is_empty() => {
                self.last_transcript = Some(text);
                if pasted {
                    Pill::Done
                } else {
                    Pill::message(
                        "Copied. You switched windows, so press Ctrl + V",
                        MessageKind::Info,
                        3.5,
                    )
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
