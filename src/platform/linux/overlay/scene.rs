//! The pill's state and animation, following the Windows overlay transition by
//! transition so both platforms move identically. Presenters only paint it.
use crate::{
    app::{MessageKind, Pill},
    audio::recorder::Levels,
    overlay::{
        motion::{Ease, Motion},
        waveform::Waveform,
    },
};
use std::time::{Duration, Instant};

pub enum Mode {
    Idle,
    Recording { levels: Levels, locked: bool },
    Transcribing,
    Message,
}
pub struct Scene {
    pub mode: Mode,
    pub motion: Motion,
    pub wave: Waveform,
    pub message: String,
    pub message_kind: MessageKind,
    pub idle_enabled: bool,
    /// The presenter hides the idle pill over fullscreen windows, as Windows does.
    pub fullscreen: bool,
    pub transcribing_at: Instant,
    pub locked_at: Instant,
    deadline: Option<Instant>,
    last_frame: Option<Instant>,
}
impl Scene {
    pub fn new(idle_enabled: bool) -> Self {
        let now = Instant::now();
        let mut scene = Self {
            mode: Mode::Idle,
            motion: Motion::default(),
            wave: Waveform::default(),
            message: String::new(),
            message_kind: MessageKind::Info,
            idle_enabled,
            fullscreen: false,
            transcribing_at: now,
            locked_at: now,
            deadline: None,
            last_frame: None,
        };
        scene.show(Pill::Idle, now, |_| 0.0);
        scene
    }
    pub fn idle_opacity(&self) -> f64 {
        if self.idle_enabled && !self.fullscreen {
            0.55
        } else {
            0.0
        }
    }
    pub fn active(&self) -> bool {
        !matches!(self.mode, Mode::Idle)
    }
    /// Returns true when the presenter should move to the focused monitor first.
    pub fn show(&mut self, pill: Pill, now: Instant, measure: impl Fn(&str) -> f64) -> bool {
        self.deadline = None;
        let mut reposition = false;
        match pill {
            Pill::Recording(levels) => {
                reposition = true;
                self.mode = Mode::Recording {
                    levels,
                    locked: false,
                };
                self.motion.morph(156.0, 38.0, 1.0, Ease::Back(0.45), now);
                self.motion.bars.set(1.0, 160, Ease::Quadratic, 0, now);
                self.motion.message.set(0.0, 100, Ease::Quadratic, 0, now);
                self.motion.lock.set(0.0, 120, Ease::Quadratic, 0, now);
                self.motion.dot.set(0.0, 120, Ease::Quadratic, 0, now);
                self.motion.glow.set(0.0, 150, Ease::Quadratic, 0, now);
                self.motion.shadow.set(0.5, 200, Ease::Quadratic, 0, now);
                self.motion.pop(1.06, 160, now);
            }
            Pill::Locked => {
                let Mode::Recording { locked, .. } = &mut self.mode else {
                    return false;
                };
                *locked = true;
                self.locked_at = now;
                self.motion.morph(176.0, 38.0, 1.0, Ease::Back(0.45), now);
                self.motion.lock.set(1.0, 180, Ease::Quadratic, 0, now);
                self.motion.dot.set(1.0, 180, Ease::Quadratic, 0, now);
                self.motion.pop(1.04, 160, now);
            }
            Pill::Transcribing => {
                self.mode = Mode::Transcribing;
                self.transcribing_at = now;
                self.motion.morph(120.0, 38.0, 1.0, Ease::Cubic, now);
                self.motion.lock.set(0.0, 120, Ease::Quadratic, 0, now);
                self.motion.dot.set(0.0, 120, Ease::Quadratic, 0, now);
                self.motion.message.set(0.0, 100, Ease::Quadratic, 0, now);
                self.motion.bars.set(1.0, 150, Ease::Quadratic, 0, now);
                self.motion.glow.set(1.0, 300, Ease::Quadratic, 0, now);
            }
            Pill::Done => {
                self.mode = Mode::Idle;
                self.motion.glow.set(0.0, 150, Ease::Quadratic, 0, now);
                self.motion.pop(1.1, 140, now);
                self.deadline = Some(now + Duration::from_secs_f64(0.17));
            }
            Pill::Message {
                text,
                kind,
                seconds,
            } => {
                reposition = matches!(self.mode, Mode::Idle);
                let width = (measure(&text) + 42.0).min(470.0);
                self.mode = Mode::Message;
                self.message = text;
                self.message_kind = kind;
                self.motion.morph(width, 40.0, 1.0, Ease::Back(0.3), now);
                self.motion.bars.set(0.0, 100, Ease::Quadratic, 0, now);
                self.motion.lock.set(0.0, 100, Ease::Quadratic, 0, now);
                self.motion.dot.set(0.0, 100, Ease::Quadratic, 0, now);
                self.motion.glow.set(0.0, 150, Ease::Quadratic, 0, now);
                self.motion.message.set(1.0, 220, Ease::Quadratic, 90, now);
                self.motion.shadow.set(0.5, 200, Ease::Quadratic, 0, now);
                self.deadline = Some(now + Duration::from_secs_f64(seconds));
            }
            Pill::Idle => {
                self.mode = Mode::Idle;
                for value in [
                    &mut self.motion.bars,
                    &mut self.motion.message,
                    &mut self.motion.lock,
                    &mut self.motion.dot,
                ] {
                    value.set(0.0, 120, Ease::Quadratic, 0, now);
                }
                self.motion.glow.set(0.0, 150, Ease::Quadratic, 0, now);
                self.motion.shadow.set(0.0, 250, Ease::Quadratic, 0, now);
                self.motion
                    .morph(44.0, 8.0, self.idle_opacity(), Ease::Cubic, now);
            }
        }
        self.last_frame = None;
        reposition
    }
    /// Re-targets the idle opacity after a setting or fullscreen change.
    pub fn refresh_idle(&mut self, now: Instant) {
        if matches!(self.mode, Mode::Idle) && self.deadline.is_none() {
            let opacity = self.idle_opacity();
            if (self.motion.opacity.get(now) - opacity).abs() > 0.01 {
                self.motion
                    .opacity
                    .set(opacity, 300, Ease::Quadratic, 0, now);
            }
        }
    }
    /// Expires messages and Done; returns true while another frame is needed.
    pub fn advance(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|d| now >= d) {
            self.show(Pill::Idle, now, |_| 0.0);
        }
        let dt = self.last_frame.map_or(1.0 / 60.0, |last| {
            now.duration_since(last).as_secs_f64().min(0.05)
        });
        self.last_frame = Some(now);
        let (level, recording, transcribing) = match &self.mode {
            Mode::Recording { levels, .. } => (f64::from(levels.level()), true, false),
            Mode::Transcribing => (0.0, false, true),
            _ => (0.0, false, false),
        };
        self.wave.frame(dt, level, recording, transcribing);
        recording || transcribing || self.motion.moving(now)
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: Locked without a recording changes shape; messages never
    // expire; Done leaves the pill busy; idle visibility ignores the setting.
    use super::*;

    #[test]
    fn transitions_and_expiry() {
        let start = Instant::now();
        let mut scene = Scene::new(true);
        assert!(!scene.show(Pill::Locked, start, |_| 0.0));
        assert!(matches!(scene.mode, Mode::Idle));
        assert!(scene.show(Pill::Recording(Levels::default()), start, |_| 0.0));
        assert!(!scene.show(Pill::Locked, start, |_| 0.0));
        assert!(matches!(scene.mode, Mode::Recording { locked: true, .. }));
        assert!(scene.advance(start + Duration::from_millis(16)));
        assert!(!scene.show(
            Pill::message("Hi", MessageKind::Warn, 1.0),
            start,
            |t| t.len() as f64
        ));
        assert!(matches!(scene.mode, Mode::Message));
        scene.advance(start + Duration::from_millis(1001));
        assert!(matches!(scene.mode, Mode::Idle));
        scene.show(Pill::Done, start, |_| 0.0);
        assert!(scene.deadline().is_some());
        let later = start + Duration::from_secs(2);
        scene.advance(later);
        assert!(!scene.advance(later + Duration::from_secs(1)));
    }

    #[test]
    fn idle_opacity_follows_setting_and_fullscreen() {
        let mut scene = Scene::new(false);
        assert_eq!(scene.idle_opacity(), 0.0);
        scene.idle_enabled = true;
        assert_eq!(scene.idle_opacity(), 0.55);
        scene.fullscreen = true;
        assert_eq!(scene.idle_opacity(), 0.0);
    }
}
