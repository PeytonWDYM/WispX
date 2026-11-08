//! Snapshot-and-replace scalar animations matching the WPF easing curves.
use std::time::Instant;
#[derive(Clone, Copy)]
pub enum Ease {
    Quadratic,
    Cubic,
    Back(f64),
}
impl Ease {
    fn sample(self, t: f64) -> f64 {
        match self {
            Self::Quadratic => 1.0 - (1.0 - t).powi(2),
            Self::Cubic => {
                if t < 0.5 {
                    4.0 * t.powi(3)
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Self::Back(amp) => {
                let u = 1.0 - t;
                1.0 - (u.powi(3) - u * amp.max(0.0) * (std::f64::consts::PI * u).sin())
            }
        }
    }
}
pub struct Value {
    from: f64,
    to: f64,
    start: Instant,
    duration: f64,
    delay: f64,
    ease: Ease,
}
impl Value {
    pub fn new(value: f64) -> Self {
        Self {
            from: value,
            to: value,
            start: Instant::now(),
            duration: 0.0,
            delay: 0.0,
            ease: Ease::Quadratic,
        }
    }
    pub fn get(&self, now: Instant) -> f64 {
        let elapsed = now.duration_since(self.start).as_secs_f64() - self.delay;
        if elapsed < 0.0 {
            return self.from;
        }
        if self.duration == 0.0 {
            return self.to;
        }
        self.from
            + (self.to - self.from) * self.ease.sample((elapsed / self.duration).clamp(0.0, 1.0))
    }
    pub fn set(&mut self, to: f64, ms: u32, ease: Ease, delay_ms: u32, now: Instant) {
        self.from = self.get(now);
        self.to = to;
        self.start = now;
        self.duration = f64::from(ms) / 1000.0;
        self.delay = f64::from(delay_ms) / 1000.0;
        self.ease = ease;
    }
    pub fn moving(&self, now: Instant) -> bool {
        self.from != self.to
            && now.duration_since(self.start).as_secs_f64() < self.duration + self.delay
    }
}
pub struct Motion {
    pub width: Value,
    pub height: Value,
    pub opacity: Value,
    pub bars: Value,
    pub message: Value,
    pub lock: Value,
    pub dot: Value,
    pub glow: Value,
    pub shadow: Value,
    pop: Option<(Instant, f64, f64)>,
}
impl Default for Motion {
    fn default() -> Self {
        Self {
            width: Value::new(44.0),
            height: Value::new(8.0),
            opacity: Value::new(0.0),
            bars: Value::new(0.0),
            message: Value::new(0.0),
            lock: Value::new(0.0),
            dot: Value::new(0.0),
            glow: Value::new(0.0),
            shadow: Value::new(0.0),
            pop: None,
        }
    }
}
impl Motion {
    pub fn morph(&mut self, width: f64, height: f64, opacity: f64, ease: Ease, now: Instant) {
        let ms = if matches!(ease, Ease::Back(_)) {
            380
        } else {
            300
        };
        self.width.set(width, ms, ease, 0, now);
        self.height.set(height, ms, ease, 0, now);
        self.opacity.set(opacity, 220, Ease::Quadratic, 0, now);
    }
    pub fn pop(&mut self, peak: f64, ms: u32, now: Instant) {
        self.pop = Some((now, peak, f64::from(ms) / 1000.0));
    }
    pub fn scale(&self, now: Instant) -> f64 {
        let Some((start, peak, duration)) = self.pop else {
            return 1.0;
        };
        let t = now.duration_since(start).as_secs_f64();
        if t < duration * 0.45 {
            1.0 + (peak - 1.0) * Ease::Quadratic.sample(t / (duration * 0.45))
        } else {
            peak + (1.0 - peak)
                * Ease::Back(0.6)
                    .sample(((t - duration * 0.45) / (duration * 1.15)).clamp(0.0, 1.0))
        }
    }
    pub fn moving(&self, now: Instant) -> bool {
        [
            &self.width,
            &self.height,
            &self.opacity,
            &self.bars,
            &self.message,
            &self.lock,
            &self.dot,
            &self.glow,
            &self.shadow,
        ]
        .iter()
        .any(|v| v.moving(now))
            || self.pop.is_some_and(|(start, _, duration)| {
                now.duration_since(start).as_secs_f64() < duration * 1.6
            })
    }
}
