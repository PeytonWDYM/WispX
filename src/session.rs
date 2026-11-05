//! The dictation state machine, free of I/O so every transition can be tested.
//!
//! The gestures mirror the Windows app, using whole shortcuts because Wayland
//! lets no program watch keys it does not own:
//!
//! ```text
//! hold the shortcut              -> record; let go to transcribe
//! hands-free shortcut while idle -> record until it is pressed again
//! hands-free shortcut while held -> keep recording after letting go
//! cancel shortcut                -> throw the recording away
//! ```

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Ends when the shortcut is released.
    Hold,
    /// Hands-free: ends on the next press, `toggle` or `stop`.
    Locked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The hold shortcut went down.
    Press,
    /// The hold shortcut came up.
    Release,
    /// The hands-free shortcut, or `wispx toggle`.
    Toggle,
    /// Explicit `wispx start`: hands-free, since no key is being held.
    Start,
    Stop,
    Cancel,
    /// The key source went away, so a hold in progress can never be released.
    CancelSilent,
    MaxDuration,
    /// The capture tool went away by itself.
    RecorderClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Begin(Mode),
    Locked,
    Finish { held: Duration },
    Cancel { sound: bool },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Session {
    recording: Option<(Mode, Instant)>,
}

impl Session {
    pub fn apply(&mut self, input: Input, now: Instant) -> Action {
        use Input::*;
        match (self.recording, input) {
            (None, Press) => self.begin(Mode::Hold, now),
            (None, Start | Toggle) => self.begin(Mode::Locked, now),

            (Some((Mode::Hold, since)), Toggle) => {
                self.recording = Some((Mode::Locked, since));
                Action::Locked
            }

            (Some((Mode::Hold, since)), Release)
            | (Some((Mode::Locked, since)), Press | Toggle)
            | (Some((_, since)), Stop | MaxDuration | RecorderClosed) => {
                self.recording = None;
                Action::Finish {
                    held: now.saturating_duration_since(since),
                }
            }

            (Some(_), Cancel) => {
                self.recording = None;
                Action::Cancel { sound: true }
            }
            (Some((Mode::Hold, _)), CancelSilent) => {
                self.recording = None;
                Action::Cancel { sound: false }
            }

            _ => Action::None,
        }
    }

    /// Undoes a `Begin` whose recorder could not be started.
    pub fn abort(&mut self) {
        self.recording = None;
    }

    pub fn mode(&self) -> Option<Mode> {
        self.recording.map(|(mode, _)| mode)
    }

    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        self.recording
            .map(|(_, since)| now.saturating_duration_since(since))
    }

    fn begin(&mut self, mode: Mode, now: Instant) -> Action {
        self.recording = Some((mode, now));
        Action::Begin(mode)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Almost always an accident, so back off quietly.
    TooShort,
    Silent,
    Speech,
}

pub fn classify(held: Duration, peak: f32, min_hold: Duration, threshold: f32) -> Verdict {
    if held < min_hold {
        Verdict::TooShort
    } else if peak < threshold {
        Verdict::Silent
    } else {
        Verdict::Speech
    }
}

#[cfg(test)]
mod tests {
    use super::Input::*;
    use super::*;

    fn drive(inputs: &[Input]) -> (Session, Vec<Action>, Instant) {
        let start = Instant::now();
        let mut session = Session::default();
        let actions = inputs
            .iter()
            .enumerate()
            .map(|(step, input)| session.apply(*input, start + Duration::from_secs(step as u64)))
            .collect();
        (session, actions, start)
    }

    fn finished_after(seconds: u64) -> Action {
        Action::Finish {
            held: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn hold_then_release_transcribes() {
        let (session, actions, _) = drive(&[Press, Release]);
        assert_eq!(actions, [Action::Begin(Mode::Hold), finished_after(1)]);
        assert_eq!(session.mode(), None);
    }

    #[test]
    fn hands_free_while_holding_keeps_recording_after_letting_go() {
        let (session, actions, _) = drive(&[Press, Toggle, Release, Toggle]);
        assert_eq!(
            actions,
            [
                Action::Begin(Mode::Hold),
                Action::Locked,
                Action::None,
                finished_after(3),
            ]
        );
        assert_eq!(session.mode(), None);
    }

    #[test]
    fn the_hold_shortcut_also_finishes_a_hands_free_recording() {
        let (session, actions, _) = drive(&[Toggle, Press, Release]);
        assert_eq!(
            actions,
            [
                Action::Begin(Mode::Locked),
                finished_after(1),
                Action::None, // Letting go of the finishing press does nothing.
            ]
        );
        assert_eq!(session.mode(), None);
    }

    #[test]
    fn cancel_works_in_either_mode_with_a_sound() {
        let cancelled = Action::Cancel { sound: true };
        assert_eq!(drive(&[Press, Cancel]).1[1], cancelled);
        assert_eq!(drive(&[Press, Toggle, Cancel]).1[2], cancelled);
        assert_eq!(drive(&[Start, Cancel]).1[1], cancelled);
    }

    #[test]
    fn losing_the_key_source_only_cancels_a_hold() {
        let (session, actions, _) = drive(&[Press, CancelSilent, Release]);
        assert_eq!(actions[1], Action::Cancel { sound: false });
        assert_eq!(actions[2], Action::None);
        assert_eq!(session.mode(), None);

        // Hands-free needs no key held, so it carries on.
        let (session, actions, _) = drive(&[Toggle, CancelSilent]);
        assert_eq!(actions[1], Action::None);
        assert_eq!(session.mode(), Some(Mode::Locked));
    }

    #[test]
    fn start_and_stop_are_explicit() {
        assert_eq!(drive(&[Start]).1[0], Action::Begin(Mode::Locked));
        assert_eq!(drive(&[Start, Stop]).1[1], finished_after(1));
        assert_eq!(drive(&[Press, Stop]).1[1], finished_after(1));
        // A second start must not end the recording.
        assert_eq!(drive(&[Start, Start]).1[1], Action::None);
    }

    #[test]
    fn repeated_presses_while_holding_are_harmless() {
        // Compositor bindings without a no-repeat option re-run `wispx press`.
        let (session, actions, _) = drive(&[Press, Press, Press, Release]);
        assert_eq!(actions[1], Action::None);
        assert_eq!(actions[2], Action::None);
        assert_eq!(actions[3], finished_after(3));
        assert_eq!(session.mode(), None);
    }

    #[test]
    fn max_duration_and_recorder_loss_finish_any_recording() {
        assert_eq!(drive(&[Press, MaxDuration]).1[1], finished_after(1));
        assert_eq!(drive(&[Start, MaxDuration]).1[1], finished_after(1));
        assert_eq!(drive(&[Start, RecorderClosed]).1[1], finished_after(1));
    }

    #[test]
    fn events_while_idle_do_nothing() {
        for input in [
            Release,
            Stop,
            Cancel,
            CancelSilent,
            MaxDuration,
            RecorderClosed,
        ] {
            assert_eq!(drive(&[input]).1[0], Action::None, "{input:?}");
        }
    }

    #[test]
    fn a_new_recording_can_follow_a_finished_one_at_once() {
        let (session, actions, _) = drive(&[Press, Release, Press]);
        assert_eq!(actions[2], Action::Begin(Mode::Hold));
        assert_eq!(session.mode(), Some(Mode::Hold));
    }

    #[test]
    fn abort_undoes_a_failed_start() {
        let (mut session, _, start) = drive(&[Press]);
        session.abort();
        assert_eq!(session.mode(), None);
        assert_eq!(session.elapsed(start), None);
    }

    #[test]
    fn elapsed_tracks_the_clock_across_locking() {
        let (session, _, start) = drive(&[Press, Toggle]);
        assert_eq!(
            session.elapsed(start + Duration::from_secs(9)),
            Some(Duration::from_secs(9))
        );
    }

    #[test]
    fn classify_applies_tap_before_silence() {
        let min = Duration::from_millis(300);
        let at = |ms| Duration::from_millis(ms);
        assert_eq!(classify(at(299), 1.0, min, 0.12), Verdict::TooShort);
        assert_eq!(classify(at(299), 0.0, min, 0.12), Verdict::TooShort);
        assert_eq!(classify(at(300), 0.11, min, 0.12), Verdict::Silent);
        assert_eq!(classify(at(300), 0.12, min, 0.12), Verdict::Speech);
    }
}
