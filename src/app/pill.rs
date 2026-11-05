//! The floating pill with no window around it: how it moves, and what each frame
//! looks like. This is `ui/Overlay.qml` over again for Windows, where there is no Qt
//! to draw it. It knows nothing of Windows, so all of it can be tested anywhere.
//!
//! Sizes are in logical pixels. Time is in milliseconds, from any starting point.

use super::overlay::{Kind, Mode};
use serde_json::Value;

/// The window the pill moves around in. It is wide enough for the longest message.
pub const HOST_WIDTH: f32 = 500.0;
pub const HOST_HEIGHT: f32 = 120.0;
const BOTTOM_MARGIN: f32 = 24.0;
/// How far the host window hangs below the work area, as the original's does.
pub const BELOW_WORK_AREA: f32 = 12.0;

const BAR_COUNT: usize = 15;
const BAR_WIDTH: f32 = 3.0;
const BAR_STEP: f32 = 6.0;
const BARS_WIDTH: f32 = 87.0;
const MIN_BAR: f32 = 3.0;
const MAX_BAR: f32 = 24.0;
const RADIUS: f32 = 20.0;
const MAX_MESSAGE_WIDTH: f32 = 470.0;
/// Room around a message inside the pill.
const MESSAGE_PADDING: f32 = 42.0;
const MESSAGE_MARGIN: f32 = 18.0;
/// How wide a message can be drawn. Any wider and the end of it is left off.
pub const MESSAGE_ROOM: f32 = MAX_MESSAGE_WIDTH - 2.0 * MESSAGE_MARGIN;
const SHADOW_LAYERS: u32 = 10;
const SURFACE: Color = Color::rgba(0x0E, 0x0E, 0x10, 0xF2);
const BORDER: Color = Color::rgba(0xFF, 0xFF, 0xFF, 0x33);
const LOCK: Color = Color::rgba(0xFF, 0xFF, 0xFF, 0xD9);
const REC_DOT: Color = Color::rgba(0xFF, 0x5A, 0x5F, 0xFF);
const WHITE: Color = Color::rgba(0xFF, 0xFF, 0xFF, 0xFF);
const SHADOW: Color = Color::rgba(0, 0, 0, 0xFF);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Curve {
    Linear,
    OutQuad,
    OutCubic,
    OutBack(f32),
    InOutSine,
}

impl Curve {
    fn at(self, t: f32) -> f32 {
        match self {
            Curve::Linear => t,
            Curve::OutQuad => 1.0 - (1.0 - t) * (1.0 - t),
            Curve::OutCubic => 1.0 - (1.0 - t).powi(3),
            Curve::OutBack(overshoot) => {
                let t = t - 1.0;
                t * t * ((overshoot + 1.0) * t + overshoot) + 1.0
            }
            Curve::InOutSine => 0.5 - 0.5 * (std::f32::consts::PI * t).cos(),
        }
    }
}

/// A value on its way from one number to another.
#[derive(Clone, Copy, Debug)]
struct Tween {
    from: f32,
    to: f32,
    start: f64,
    duration: f64,
    curve: Curve,
}

impl Tween {
    fn still(value: f32) -> Tween {
        Tween {
            from: value,
            to: value,
            start: 0.0,
            duration: 0.0,
            curve: Curve::Linear,
        }
    }

    fn at(&self, now: f64) -> f32 {
        if self.duration <= 0.0 || now >= self.start + self.duration {
            return self.to;
        }
        if now <= self.start {
            return self.from;
        }
        let t = ((now - self.start) / self.duration) as f32;
        self.from + (self.to - self.from) * self.curve.at(t)
    }

    fn go(&mut self, now: f64, to: f32, duration: f64, curve: Curve) {
        self.from = self.at(now);
        self.to = to;
        self.start = now;
        self.duration = duration;
        self.curve = curve;
    }

    fn running(&self, now: f64) -> bool {
        now < self.start + self.duration
    }
}

/// Up quickly, then back to rest with a little give.
#[derive(Clone, Copy, Debug)]
struct Pop {
    start: f64,
    from: f32,
    peak: f32,
    up: f64,
    down: f64,
}

impl Pop {
    fn at(&self, now: f64) -> f32 {
        let elapsed = now - self.start;
        if elapsed < self.up {
            let t = (elapsed / self.up).max(0.0) as f32;
            self.from + (self.peak - self.from) * Curve::OutQuad.at(t)
        } else if elapsed < self.up + self.down {
            let t = ((elapsed - self.up) / self.down) as f32;
            self.peak + (1.0 - self.peak) * Curve::OutBack(0.8).at(t)
        } else {
            1.0
        }
    }

    fn running(&self, now: f64) -> bool {
        now < self.start + self.up + self.down
    }
}

/// What the app wants on screen, from `OverlayModel::to_json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Wanted {
    pub mode: Mode,
    pub message: String,
    pub kind: Kind,
    pub idle_pill: bool,
}

impl Wanted {
    pub fn parse(json: &str) -> Option<Wanted> {
        let value: Value = serde_json::from_str(json).ok()?;
        Some(Wanted {
            mode: match value["mode"].as_str()? {
                "recording" => Mode::Recording,
                "locked" => Mode::Locked,
                "transcribing" => Mode::Transcribing,
                _ => Mode::Idle,
            },
            message: value["message"].as_str().unwrap_or_default().to_string(),
            kind: match value["kind"].as_str().unwrap_or_default() {
                "success" => Kind::Success,
                "warn" => Kind::Warn,
                "error" => Kind::Error,
                _ => Kind::Info,
            },
            idle_pill: value["idle_pill"].as_bool().unwrap_or(false),
        })
    }
}

/// A message drawn by the window system, as how much of each pixel it covers.
pub struct TextMask<'a> {
    pub width: usize,
    pub height: usize,
    pub coverage: &'a [u8],
}

pub struct Pill {
    wanted: Wanted,
    previous_mode: Mode,
    had_message: bool,
    /// Stays after the message has gone, for it to fade out.
    shown_message: String,
    shown_kind: Kind,
    width: Tween,
    height: Tween,
    opacity: Tween,
    pop: Option<Pop>,
    bars_opacity: Tween,
    lock_opacity: Tween,
    glow_opacity: Tween,
    text_opacity: Tween,
    sweep_start: f64,
    dot_start: Option<f64>,
    done_at: Option<f64>,
    settle_at: Option<f64>,
    unmap_at: Option<f64>,
    rendering: bool,
    mapped: bool,
    clock: f32,
    smooth: f32,
    level: f32,
    last_frame: Option<f64>,
    seeds: [f32; BAR_COUNT],
    heights: [f32; BAR_COUNT],
    alphas: [f32; BAR_COUNT],
}

impl Default for Pill {
    fn default() -> Self {
        Pill::new()
    }
}

impl Pill {
    pub fn new() -> Pill {
        let mut seeds = [0.0; BAR_COUNT];
        for (index, seed) in seeds.iter_mut().enumerate() {
            // The golden angle: looks random, and every run is the same.
            *seed = (index as f32 * 2.399_963).rem_euclid(std::f32::consts::TAU);
        }
        Pill {
            wanted: Wanted {
                mode: Mode::Idle,
                message: String::new(),
                kind: Kind::Info,
                idle_pill: false,
            },
            previous_mode: Mode::Idle,
            had_message: false,
            shown_message: String::new(),
            shown_kind: Kind::Info,
            width: Tween::still(44.0),
            height: Tween::still(8.0),
            opacity: Tween::still(0.0),
            pop: None,
            bars_opacity: Tween::still(0.0),
            lock_opacity: Tween::still(0.0),
            glow_opacity: Tween::still(0.0),
            text_opacity: Tween::still(0.0),
            sweep_start: 0.0,
            dot_start: None,
            done_at: None,
            settle_at: None,
            unmap_at: None,
            rendering: false,
            mapped: false,
            clock: 0.0,
            smooth: 0.0,
            level: 0.0,
            last_frame: None,
            seeds,
            heights: [MIN_BAR; BAR_COUNT],
            alphas: [0.6; BAR_COUNT],
        }
    }

    fn is_wanted(&self) -> bool {
        self.wanted.mode != Mode::Idle || !self.wanted.message.is_empty() || self.wanted.idle_pill
    }

    fn busy(&self) -> bool {
        self.wanted.mode != Mode::Idle || !self.wanted.message.is_empty()
    }

    /// Whether the window should be on screen at all.
    pub fn mapped(&self) -> bool {
        self.mapped
    }

    pub fn wanted(&self) -> &Wanted {
        &self.wanted
    }

    pub fn set_level(&mut self, level: f32) {
        self.level = level.clamp(0.0, 1.0);
    }

    /// Takes on a new state. `measure` says how wide a message is when drawn.
    pub fn set(&mut self, now: f64, wanted: Wanted, measure: impl Fn(&str) -> f32) {
        if wanted == self.wanted {
            return;
        }
        self.previous_mode = self.wanted.mode;
        self.had_message = !self.wanted.message.is_empty();
        self.wanted = wanted;
        self.apply(now, measure);
        if self.is_wanted() {
            self.unmap_at = None;
            self.mapped = true;
        } else {
            self.unmap_at = Some(now + 320.0);
        }
    }

    fn apply(&mut self, now: f64, measure: impl Fn(&str) -> f32) {
        self.settle_at = None;
        self.done_at = None;
        let mode = self.wanted.mode;
        if self.busy() {
            self.rendering = true;
        }
        let locked = mode == Mode::Locked;
        self.lock_opacity
            .go(now, if locked { 1.0 } else { 0.0 }, 180.0, Curve::Linear);
        self.dot_start = match (locked, self.dot_start) {
            (true, Some(start)) => Some(start),
            (true, None) => Some(now),
            (false, _) => None,
        };
        let has_bars = mode != Mode::Idle;
        if !self.wanted.message.is_empty() && mode == Mode::Idle {
            self.bars_opacity.go(now, 0.0, 180.0, Curve::Linear);
            self.fade_glow(now, 0.0);
            let width = (measure(&self.wanted.message) + MESSAGE_PADDING).min(MAX_MESSAGE_WIDTH);
            self.morph(now, width, 40.0, 1.0, 0.8);
            // After the pill has begun to open, so the text is not seen squeezed.
            self.text_opacity.go(now + 90.0, 1.0, 220.0, Curve::Linear);
            self.shown_message.clone_from(&self.wanted.message);
            self.shown_kind = self.wanted.kind;
            return;
        }
        self.text_opacity.go(now, 0.0, 120.0, Curve::Linear);
        self.bars_opacity
            .go(now, if has_bars { 1.0 } else { 0.0 }, 180.0, Curve::Linear);
        match mode {
            Mode::Recording => {
                self.fade_glow(now, 0.0);
                self.morph(now, 156.0, 38.0, 1.0, 1.0);
                self.start_pop(now, 1.06, 160.0);
            }
            Mode::Locked => {
                self.fade_glow(now, 0.0);
                self.morph(now, 176.0, 38.0, 1.0, 1.0);
                self.start_pop(now, 1.04, 160.0);
            }
            Mode::Transcribing => {
                self.fade_glow(now, 1.0);
                self.morph(now, 120.0, 38.0, 1.0, 0.0);
            }
            Mode::Idle if self.previous_mode == Mode::Transcribing && !self.had_message => {
                // Done, and the text has arrived: a small pop says so.
                self.fade_glow(now, 0.0);
                self.start_pop(now, 1.1, 140.0);
                self.done_at = Some(now + 170.0);
            }
            Mode::Idle => self.rest(now),
        }
    }

    fn rest(&mut self, now: f64) {
        self.fade_glow(now, 0.0);
        let opacity = if self.wanted.idle_pill { 0.55 } else { 0.0 };
        self.morph(now, 44.0, 8.0, opacity, 0.0);
        self.settle_at = Some(now + 450.0);
    }

    fn fade_glow(&mut self, now: f64, to: f32) {
        if to > 0.0 && self.glow_opacity.at(now) <= 0.0 {
            self.sweep_start = now;
        }
        self.glow_opacity.go(now, to, 300.0, Curve::Linear);
    }

    fn morph(&mut self, now: f64, width: f32, height: f32, opacity: f32, overshoot: f32) {
        let (duration, curve) = if overshoot > 0.0 {
            (380.0, Curve::OutBack(overshoot))
        } else {
            (300.0, Curve::OutCubic)
        };
        self.width.go(now, width, duration, curve);
        self.height.go(now, height, duration, curve);
        self.opacity.go(now, opacity, 220.0, Curve::OutQuad);
    }

    fn start_pop(&mut self, now: f64, peak: f32, duration: f64) {
        let from = self.pop.as_ref().map_or(1.0, |pop| pop.at(now));
        self.pop = Some(Pop {
            start: now,
            from,
            peak,
            up: duration * 0.45,
            down: duration * 1.6,
        });
    }

    /// Moves everything on to `now`.
    pub fn advance(&mut self, now: f64) {
        if self.done_at.is_some_and(|at| now >= at) {
            self.done_at = None;
            self.rest(now);
        }
        if self.settle_at.is_some_and(|at| now >= at) {
            self.settle_at = None;
            self.rendering = false;
        }
        if self.unmap_at.is_some_and(|at| now >= at) {
            self.unmap_at = None;
            if !self.is_wanted() {
                self.mapped = false;
            }
        }
        let step = self.last_frame.map_or(0.0, |last| now - last);
        self.last_frame = Some(now);
        if self.rendering && self.mapped {
            self.frame((step / 1000.0) as f32);
        }
    }

    /// One step of the bars, exactly as the original's `OnFrame` has it.
    fn frame(&mut self, step: f32) {
        let dt = step.clamp(0.0, 0.05);
        self.clock += dt;
        let recording = matches!(self.wanted.mode, Mode::Recording | Mode::Locked);
        let level = if recording { self.level } else { 0.0 };
        let k = if level > self.smooth { 22.0 } else { 9.0 };
        self.smooth += (level - self.smooth) * (dt * k).min(1.0);

        let mid = (BAR_COUNT - 1) as f32 / 2.0;
        for i in 0..BAR_COUNT {
            let index = i as f32;
            let center = 1.0 - (index - mid).abs() / mid;
            let (target, alpha) = if recording {
                let envelope = 0.28 + 0.72 * center.powf(0.9);
                let speed = 5.5 + (i % 4) as f32 * 1.3;
                let wobble = 0.55 + 0.45 * (self.clock * speed + self.seeds[i]).sin();
                let breathe = 1.2 * (0.5 + 0.5 * (self.clock * 2.4 + index * 0.5).sin());
                (
                    MIN_BAR + breathe + (MAX_BAR - MIN_BAR) * self.smooth * envelope * wobble,
                    0.55 + 0.45 * (self.smooth * 2.0 + center * 0.4).min(1.0),
                )
            } else if self.wanted.mode == Mode::Transcribing {
                let wave = 0.5 + 0.5 * (self.clock * 7.5 - index * 0.62).sin();
                (MIN_BAR + 7.0 * wave * wave, 0.35 + 0.65 * wave)
            } else {
                (MIN_BAR, 0.6)
            };
            self.heights[i] += (target - self.heights[i]) * (dt * 18.0).min(1.0);
            self.alphas[i] = alpha;
        }
    }

    /// Whether anything will change without being told, so more frames are needed.
    pub fn animating(&self, now: f64) -> bool {
        self.rendering
            || self.done_at.is_some()
            || self.settle_at.is_some()
            || self.unmap_at.is_some()
            || self.pop.as_ref().is_some_and(|pop| pop.running(now))
            || [
                &self.width,
                &self.height,
                &self.opacity,
                &self.bars_opacity,
                &self.lock_opacity,
                &self.glow_opacity,
                &self.text_opacity,
            ]
            .iter()
            .any(|tween| tween.running(now))
    }

    /// The message that is on screen or fading from it, with its color.
    pub fn message(&self) -> Option<(&str, Color)> {
        if self.shown_message.is_empty() {
            return None;
        }
        let color = match self.shown_kind {
            Kind::Error => Color::rgba(0xFF, 0xA8, 0xA8, 0xFF),
            Kind::Warn => Color::rgba(0xFF, 0xDE, 0x9E, 0xFF),
            Kind::Success => Color::rgba(0xA8, 0xF0, 0xC0, 0xFF),
            Kind::Info => WHITE,
        };
        Some((&self.shown_message, color))
    }

    /// The height of a bar, for tests and `inspect`.
    pub fn bar_height(&self, index: usize) -> f32 {
        self.heights[index]
    }

    /// The pill's outline right now, for `inspect`: width, height and opacity.
    pub fn outline(&self, now: f64) -> (f32, f32, f32) {
        (
            self.width.at(now),
            self.height.at(now),
            self.opacity.at(now),
        )
    }

    /// Draws a whole frame into premultiplied BGRA pixels, top row first.
    pub fn paint(
        &self,
        now: f64,
        pixels: &mut [u8],
        width: usize,
        height: usize,
        scale: f32,
        text: Option<&TextMask<'_>>,
    ) {
        pixels.fill(0);
        let opacity = self.opacity.at(now);
        if opacity <= 0.0 {
            return;
        }
        let mut canvas = Canvas {
            pixels,
            width,
            height,
        };
        let pop = self.pop.as_ref().map_or(1.0, |pop| pop.at(now));
        let (w, h) = (self.width.at(now).max(1.0), self.height.at(now).max(1.0));
        let unit = scale * pop;
        let host_w = width as f32 / scale;
        let host_h = height as f32 / scale;
        let center_x = host_w / 2.0 * scale;
        let center_y = (host_h - BOTTOM_MARGIN - h / 2.0) * scale;
        let (half_w, half_h) = (w / 2.0 * unit, h / 2.0 * unit);
        let radius = RADIUS.min(w / 2.0).min(h / 2.0) * unit;

        // Ten dim copies, each a little larger: a soft shadow with no blur to run.
        let layer = if self.busy() { 0.035 } else { 0.012 } * opacity;
        let spread = SHADOW_LAYERS as f32 * unit;
        canvas.each_pixel(
            center_x,
            center_y + 2.0 * unit,
            half_w + spread + 1.0,
            half_h + spread + 1.0,
            |x, y, pixel| {
                let d = rounded(
                    x,
                    y,
                    center_x,
                    center_y + 2.0 * unit,
                    half_w,
                    half_h,
                    radius,
                );
                let mut left = 1.0f32;
                for step in 1..=SHADOW_LAYERS {
                    left *= 1.0 - layer * coverage(d - step as f32 * unit);
                }
                over(pixel, SHADOW, 1.0 - left);
            },
        );

        // The surface, with its border inside its edge as Qt draws one.
        let border = 1.0 * unit;
        canvas.each_pixel(
            center_x,
            center_y,
            half_w + 1.0,
            half_h + 1.0,
            |x, y, pixel| {
                let d = rounded(x, y, center_x, center_y, half_w, half_h, radius);
                let outer = coverage(d);
                if outer <= 0.0 {
                    return;
                }
                let inner = coverage(d + border);
                over(pixel, SURFACE, inner * opacity);
                over(pixel, BORDER, (outer - inner).max(0.0) * opacity);
            },
        );

        let glow = self.glow_opacity.at(now);
        if glow > 0.0 {
            self.paint_glow(
                now,
                &mut canvas,
                center_x,
                center_y,
                half_w,
                half_h,
                radius,
                unit,
                glow * opacity,
            );
        }

        let bars = self.bars_opacity.at(now);
        if bars > 0.0 {
            let left = center_x - BARS_WIDTH / 2.0 * unit;
            for i in 0..BAR_COUNT {
                let bar_h = self.heights[i] * unit;
                let bar_x = left + (i as f32 * BAR_STEP + BAR_WIDTH / 2.0) * unit;
                let alpha = self.alphas[i] * bars * opacity;
                canvas.rect(
                    bar_x,
                    center_y,
                    BAR_WIDTH / 2.0 * unit,
                    bar_h / 2.0,
                    1.5 * unit,
                    WHITE,
                    alpha,
                );
            }
        }

        let lock = self.lock_opacity.at(now);
        if lock > 0.0 {
            let x = center_x - half_w + 14.0 * unit;
            let y = center_y - 6.5 * unit;
            let alpha = lock * opacity;
            // The shackle is only an outline, and the body goes over its lower part.
            let (cx, cy) = (x + 5.5 * unit, y + 4.0 * unit);
            let (hw, hh) = (3.5 * unit, 4.0 * unit);
            let stroke = 1.5 * unit;
            canvas.each_pixel(cx, cy, hw + 1.0, hh + 1.0, |px, py, pixel| {
                let d = rounded(px, py, cx, cy, hw, hh, hw);
                let ring = (coverage(d) - coverage(d + stroke)).max(0.0);
                over(pixel, LOCK, ring * alpha);
            });
            canvas.rect(
                x + 5.5 * unit,
                y + 9.0 * unit,
                5.5 * unit,
                4.0 * unit,
                2.0 * unit,
                LOCK,
                alpha,
            );
        }

        if let Some(start) = self.dot_start {
            let alpha = dot_pulse(now - start) * opacity;
            let x = center_x + half_w - 18.0 * unit;
            canvas.rect(
                x,
                center_y,
                3.0 * unit,
                3.0 * unit,
                3.0 * unit,
                REC_DOT,
                alpha,
            );
        }

        if let (Some((_, color)), Some(mask)) = (self.message(), text) {
            let alpha = self.text_opacity.at(now) * opacity;
            if alpha > 0.0 {
                canvas.text(
                    mask,
                    center_x,
                    center_y,
                    half_w - MESSAGE_MARGIN * unit,
                    color,
                    alpha,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_glow(
        &self,
        now: f64,
        canvas: &mut Canvas<'_>,
        cx: f32,
        cy: f32,
        half_w: f32,
        half_h: f32,
        radius: f32,
        unit: f32,
        alpha: f32,
    ) {
        // A band of light crossing the pill from one side to the other, and back
        // to where it started off the far edge.
        let cycle = ((now - self.sweep_start).rem_euclid(1100.0) / 1100.0) as f32;
        let sweep = -1.0 + 2.0 * Curve::InOutSine.at(cycle);
        let band = sweep + 0.5;
        let shine = |at: f32| (1.0 - (at - band).abs() / 0.5).max(0.0) * 0x22 as f32 / 255.0;
        let stop = band.clamp(0.001, 0.999);
        let (first, middle, last) = (shine(0.0), shine(stop), shine(1.0));
        let inset = unit;
        let (w, h) = ((half_w - inset).max(0.0), (half_h - inset).max(0.0));
        let r = (radius - inset).max(0.0);
        let left = cx - w;
        let span = (2.0 * w).max(1.0);
        canvas.each_pixel(cx, cy, w + 1.0, h + 1.0, |x, y, pixel| {
            let shape = coverage(rounded(x, y, cx, cy, w, h, r));
            if shape <= 0.0 {
                return;
            }
            let at = (x - left) / span;
            let light = if at < stop {
                first + (middle - first) * (at / stop).clamp(0.0, 1.0)
            } else {
                middle + (last - middle) * ((at - stop) / (1.0 - stop)).clamp(0.0, 1.0)
            };
            over(pixel, WHITE, light * shape * alpha);
        });
    }
}

/// The dot rests lit, fades out, and comes back.
fn dot_pulse(elapsed: f64) -> f32 {
    let t = elapsed.rem_euclid(1600.0);
    if t < 200.0 {
        1.0
    } else if t < 900.0 {
        1.0 - 0.75 * Curve::InOutSine.at(((t - 200.0) / 700.0) as f32)
    } else {
        0.25 + 0.75 * Curve::InOutSine.at(((t - 900.0) / 700.0) as f32)
    }
}

/// Distance from a point to a rounded rectangle's edge, negative inside.
fn rounded(x: f32, y: f32, cx: f32, cy: f32, half_w: f32, half_h: f32, radius: f32) -> f32 {
    let qx = (x - cx).abs() - half_w + radius;
    let qy = (y - cy).abs() - half_h + radius;
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    outside + qx.max(qy).min(0.0) - radius
}

/// How much of a pixel a shape covers, from the distance to its edge.
fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

/// Puts a color over premultiplied BGRA.
fn over(pixel: &mut [u8], color: Color, amount: f32) {
    let alpha = color.a * amount;
    if alpha <= 0.0 {
        return;
    }
    let keep = 1.0 - alpha;
    let mix = |below: u8, value: f32| {
        (f32::from(below) * keep + value * alpha * 255.0 + 0.5).clamp(0.0, 255.0) as u8
    };
    pixel[0] = mix(pixel[0], color.b);
    pixel[1] = mix(pixel[1], color.g);
    pixel[2] = mix(pixel[2], color.r);
    pixel[3] = mix(pixel[3], 1.0);
}

struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: usize,
    height: usize,
}

impl Canvas<'_> {
    /// Runs `paint` for each pixel of a box, with the coordinates of its center.
    fn each_pixel(
        &mut self,
        cx: f32,
        cy: f32,
        half_w: f32,
        half_h: f32,
        mut paint: impl FnMut(f32, f32, &mut [u8]),
    ) {
        let left = (cx - half_w).floor().max(0.0) as usize;
        let top = (cy - half_h).floor().max(0.0) as usize;
        let right = ((cx + half_w).ceil().max(0.0) as usize).min(self.width);
        let bottom = ((cy + half_h).ceil().max(0.0) as usize).min(self.height);
        for y in top..bottom {
            let row = y * self.width * 4;
            for x in left..right {
                let at = row + x * 4;
                paint(x as f32 + 0.5, y as f32 + 0.5, &mut self.pixels[at..at + 4]);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rect(
        &mut self,
        cx: f32,
        cy: f32,
        half_w: f32,
        half_h: f32,
        radius: f32,
        color: Color,
        alpha: f32,
    ) {
        let radius = radius.min(half_w).min(half_h);
        self.each_pixel(cx, cy, half_w + 1.0, half_h + 1.0, |x, y, pixel| {
            over(
                pixel,
                color,
                coverage(rounded(x, y, cx, cy, half_w, half_h, radius)) * alpha,
            );
        });
    }

    /// Lays a message down centered, cut off at the edges of the room it has.
    fn text(
        &mut self,
        mask: &TextMask<'_>,
        cx: f32,
        cy: f32,
        half_room: f32,
        color: Color,
        alpha: f32,
    ) {
        let left = (cx - mask.width as f32 / 2.0).round() as isize;
        let top = (cy - mask.height as f32 / 2.0).round() as isize;
        let (room_left, room_right) = ((cx - half_room).max(0.0), cx + half_room);
        for row in 0..mask.height {
            let y = top + row as isize;
            if y < 0 || y as usize >= self.height {
                continue;
            }
            for column in 0..mask.width {
                let x = left + column as isize;
                if x < 0 || x as usize >= self.width {
                    continue;
                }
                let middle = x as f32 + 0.5;
                if middle < room_left || middle > room_right {
                    continue;
                }
                let amount = f32::from(mask.coverage[row * mask.width + column]) / 255.0;
                if amount > 0.0 {
                    let at = (y as usize * self.width + x as usize) * 4;
                    over(&mut self.pixels[at..at + 4], color, amount * alpha);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = HOST_WIDTH as usize;
    const H: usize = HOST_HEIGHT as usize;

    fn wanted(mode: Mode) -> Wanted {
        Wanted {
            mode,
            message: String::new(),
            kind: Kind::Info,
            idle_pill: false,
        }
    }

    fn set(pill: &mut Pill, now: f64, wanted: Wanted) {
        pill.set(now, wanted, |text| text.len() as f32 * 8.0);
    }

    /// Runs frames at 60 Hz from `from` until `to`.
    fn run(pill: &mut Pill, from: f64, to: f64) {
        let mut now = from;
        while now < to {
            now += 16.0;
            pill.advance(now);
        }
    }

    fn pixel(pixels: &[u8], x: usize, y: usize) -> [u8; 4] {
        let at = (y * W + x) * 4;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    fn paint(pill: &Pill, now: f64) -> Vec<u8> {
        let mut pixels = vec![0xAB; W * H * 4];
        pill.paint(now, &mut pixels, W, H, 1.0, None);
        pixels
    }

    #[test]
    fn each_state_settles_at_the_size_of_the_original() {
        for (mode, width, height) in [
            (Mode::Recording, 156.0, 38.0),
            (Mode::Locked, 176.0, 38.0),
            (Mode::Transcribing, 120.0, 38.0),
        ] {
            let mut pill = Pill::new();
            set(&mut pill, 0.0, wanted(mode));
            assert!(pill.mapped());
            assert_eq!(pill.outline(5000.0), (width, height, 1.0), "{mode:?}");
        }
    }

    #[test]
    fn a_new_recording_overshoots_on_its_way_open() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Recording));
        let widest = (0..380)
            .map(|now| pill.outline(f64::from(now)).0)
            .fold(0.0, f32::max);
        assert!(widest > 156.0, "{widest}");
    }

    #[test]
    fn a_message_fits_its_text_up_to_a_limit() {
        let mut pill = Pill::new();
        let mut note = wanted(Mode::Idle);
        note.message = "x".repeat(10);
        set(&mut pill, 0.0, note.clone());
        assert_eq!(pill.outline(5000.0), (122.0, 40.0, 1.0));

        note.message = "x".repeat(200);
        set(&mut pill, 6000.0, note);
        assert_eq!(pill.outline(9000.0).0, MAX_MESSAGE_WIDTH);
    }

    #[test]
    fn with_no_idle_pill_it_goes_away_and_stops_drawing() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Recording));
        run(&mut pill, 0.0, 1000.0);
        assert!(pill.animating(1000.0));
        set(&mut pill, 1000.0, wanted(Mode::Idle));
        assert!(pill.mapped(), "stays up for the fade");
        run(&mut pill, 1000.0, 2000.0);
        assert!(!pill.mapped());
        assert!(!pill.animating(2000.0));
        assert_eq!(pill.outline(2000.0), (44.0, 8.0, 0.0));
    }

    #[test]
    fn the_idle_pill_stays_faintly_on_screen_without_using_any_frames() {
        let mut pill = Pill::new();
        let mut idle = wanted(Mode::Idle);
        idle.idle_pill = true;
        set(&mut pill, 0.0, idle);
        run(&mut pill, 0.0, 1000.0);
        assert!(pill.mapped());
        assert!(!pill.animating(1000.0));
        assert_eq!(pill.outline(1000.0), (44.0, 8.0, 0.55));
    }

    #[test]
    fn finishing_pops_before_coming_to_rest() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Transcribing));
        run(&mut pill, 0.0, 1000.0);
        set(&mut pill, 1000.0, wanted(Mode::Idle));
        // Still its full size during the pop, then it shrinks.
        assert_eq!(pill.outline(1100.0).0, 120.0);
        run(&mut pill, 1000.0, 1200.0);
        assert!(pill.outline(1400.0).0 < 120.0);
        run(&mut pill, 1200.0, 2500.0);
        assert!(!pill.mapped());
    }

    #[test]
    fn a_message_can_still_be_seen_as_it_fades() {
        let mut pill = Pill::new();
        let mut note = wanted(Mode::Idle);
        note.message = "Copied".to_string();
        set(&mut pill, 0.0, note);
        run(&mut pill, 0.0, 1000.0);
        set(&mut pill, 1000.0, wanted(Mode::Idle));
        assert_eq!(pill.message().map(|(text, _)| text), Some("Copied"));
        assert!(pill.text_opacity.at(1060.0) > 0.0);
        assert_eq!(pill.text_opacity.at(1200.0), 0.0);
    }

    #[test]
    fn a_message_while_finishing_skips_the_pop() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Transcribing));
        run(&mut pill, 0.0, 1000.0);
        let mut note = wanted(Mode::Idle);
        note.message = "Copied".to_string();
        set(&mut pill, 1000.0, note);
        assert!(pill.message().is_some());
        assert!(pill.done_at.is_none());
    }

    #[test]
    fn the_bars_follow_the_microphone_and_are_tallest_in_the_middle() {
        let mut quiet = Pill::new();
        set(&mut quiet, 0.0, wanted(Mode::Recording));
        run(&mut quiet, 0.0, 1000.0);

        let mut loud = Pill::new();
        set(&mut loud, 0.0, wanted(Mode::Recording));
        loud.set_level(1.0);
        run(&mut loud, 0.0, 1000.0);

        let total = |pill: &Pill| (0..BAR_COUNT).map(|i| pill.bar_height(i)).sum::<f32>();
        assert!(total(&loud) > total(&quiet) * 2.0);
        for i in 0..BAR_COUNT {
            assert!(quiet.bar_height(i) <= MIN_BAR + 1.3, "{i}");
            assert!(loud.bar_height(i) <= MAX_BAR + 1.3, "{i}");
        }

        // Going by the envelope alone, since each bar also wobbles on its own.
        let edges = loud.bar_height(0) + loud.bar_height(14);
        let middle = loud.bar_height(6) + loud.bar_height(7) + loud.bar_height(8);
        assert!(middle / 3.0 > edges / 2.0, "{middle} {edges}");
    }

    #[test]
    fn nothing_says_there_is_sound_when_there_is_none() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Transcribing));
        pill.set_level(1.0);
        run(&mut pill, 0.0, 2000.0);
        assert_eq!(pill.smooth, 0.0);
    }

    #[test]
    fn an_empty_frame_is_entirely_transparent() {
        let pill = Pill::new();
        assert!(paint(&pill, 0.0).iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_frame_is_validly_premultiplied_and_has_the_pill_at_the_bottom_middle() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Transcribing));
        run(&mut pill, 0.0, 1000.0);
        let pixels = paint(&pill, 1000.0);

        for chunk in pixels.as_chunks::<4>().0 {
            assert!(chunk[0] <= chunk[3] && chunk[1] <= chunk[3] && chunk[2] <= chunk[3]);
        }
        // The pill spans 120 wide and 38 tall, ending 24 above the bottom.
        let (cx, cy) = (W / 2, H - 24 - 19);
        assert!(pixel(&pixels, cx - 50, cy)[3] > 200);
        assert!(pixel(&pixels, cx + 50, cy)[3] > 200);
        assert_eq!(pixel(&pixels, 2, 2), [0; 4]);
        // Past the shadow on either side, nothing has been touched.
        assert_eq!(pixel(&pixels, cx - 75, cy), [0; 4]);
        assert_eq!(pixel(&pixels, cx + 75, cy), [0; 4]);
    }

    #[test]
    fn the_surface_is_the_originals_dark_color_with_white_bars_on_it() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Recording));
        pill.set_level(1.0);
        run(&mut pill, 0.0, 1000.0);
        let pixels = paint(&pill, 1000.0);
        let (cx, cy) = (W / 2, H - 24 - 19);
        // Between the left edge and the first bar.
        let surface = pixel(&pixels, cx - 60, cy);
        // Nearly opaque, and a touch more than the surface alone with the shadow below.
        assert!((0xF2..=0xF8).contains(&surface[3]), "{surface:?}");
        assert!(surface[0] < 0x14, "{surface:?}");
        // The center bar is 3 wide, around the middle.
        assert!(pixel(&pixels, cx, cy)[1] > 0x90);
    }

    #[test]
    fn hands_free_has_its_lock_and_dot_while_holding_has_neither() {
        let (cx, cy) = (W / 2, H - 24 - 19);

        let mut held = Pill::new();
        set(&mut held, 0.0, wanted(Mode::Recording));
        run(&mut held, 0.0, 1000.0);
        let pixels = paint(&held, 1000.0);
        let dot_at = cx + 78 - 18;
        assert!(pixel(&pixels, dot_at, cy)[2] < 0x40);

        let mut locked = Pill::new();
        set(&mut locked, 0.0, wanted(Mode::Locked));
        run(&mut locked, 0.0, 1000.0);
        let pixels = paint(&locked, locked_bright_time());
        let dot = pixel(&pixels, cx + 88 - 18, cy);
        assert!(dot[2] > 0xC0 && dot[1] < 0x70, "{dot:?}");
        // The padlock's body, 14 from the left and a little below the middle.
        let body = pixel(&pixels, cx - 88 + 19, cy + 3);
        assert!(
            body[0] > 0xA0 && body[1] > 0xA0 && body[2] > 0xA0,
            "{body:?}"
        );
    }

    /// A moment in the dot's cycle when it is fully lit.
    fn locked_bright_time() -> f64 {
        1600.0
    }

    #[test]
    fn everything_scales_with_the_display() {
        let mut pill = Pill::new();
        set(&mut pill, 0.0, wanted(Mode::Transcribing));
        run(&mut pill, 0.0, 1000.0);
        let (w, h) = (W * 2, H * 2);
        let mut pixels = vec![0; w * h * 4];
        pill.paint(1000.0, &mut pixels, w, h, 2.0, None);
        let at = |x: usize, y: usize| pixels[(y * w + x) * 4 + 3];
        let (cx, cy) = (w / 2, h - 48 - 38);
        assert!(at(cx - 100, cy) > 200);
        assert!(at(cx + 100, cy) > 200);
        assert_eq!(at(cx - 150, cy), 0);
    }

    #[test]
    fn a_message_is_drawn_in_its_color_and_stays_within_the_pill() {
        let mut pill = Pill::new();
        let mut note = wanted(Mode::Idle);
        note.message = "x".repeat(10);
        note.kind = Kind::Error;
        set(&mut pill, 0.0, note);
        run(&mut pill, 0.0, 1000.0);

        // Solid, and wider than the room there is.
        let (mw, mh) = (200, 10);
        let coverage = vec![255u8; mw * mh];
        let mask = TextMask {
            width: mw,
            height: mh,
            coverage: &coverage,
        };
        let mut pixels = vec![0; W * H * 4];
        pill.paint(1000.0, &mut pixels, W, H, 1.0, Some(&mask));
        let (cx, cy) = (W / 2, H - 24 - 20);
        let middle = pixel(&pixels, cx, cy);
        assert_eq!(middle[2], 0xFF, "{middle:?}");
        assert!(middle[0] < 0xB0, "{middle:?}");
        // 122 wide with 18 of margin, so 43 either side.
        let past = pixel(&pixels, cx + 50, cy);
        assert!(past[2] < 0x20, "{past:?}");
    }

    #[test]
    fn the_overlay_json_is_understood() {
        let parsed =
            Wanted::parse(r#"{"mode":"locked","message":"hi","kind":"warn","idle_pill":true}"#)
                .unwrap();
        assert_eq!(parsed.mode, Mode::Locked);
        assert_eq!(parsed.message, "hi");
        assert_eq!(parsed.kind, Kind::Warn);
        assert!(parsed.idle_pill);
        assert!(Wanted::parse("not json").is_none());
    }

    #[test]
    fn the_curves_start_and_end_where_they_should() {
        for curve in [
            Curve::Linear,
            Curve::OutQuad,
            Curve::OutCubic,
            Curve::OutBack(1.0),
            Curve::InOutSine,
        ] {
            assert!(curve.at(0.0).abs() < 1e-5, "{curve:?}");
            assert!((curve.at(1.0) - 1.0).abs() < 1e-5, "{curve:?}");
        }
    }
}
