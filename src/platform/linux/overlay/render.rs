//! Paints the pill into a premultiplied pixmap with the Windows renderer's geometry:
//! a 480 x 120 logical canvas, multiplied by the output scale.
use super::scene::Scene;
use crate::app::MessageKind;
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use std::time::Instant;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, Rect,
    SpreadMode, Stroke, Transform,
};

pub const WIDTH: f32 = 480.0;
pub const HEIGHT: f32 = 120.0;
const FONT_SIZE: f32 = 15.0;

/// The system UI font at semibold weight, found through fontconfig like Slint does.
fn load_font() -> Option<FontVec> {
    use fontique::{
        Attributes, Collection, CollectionOptions, FontStyle, FontWeight, FontWidth, GenericFamily,
        QueryFamily, QueryStatus, SourceCache,
    };
    let mut collection = Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    });
    let mut cache = SourceCache::default();
    let mut query = collection.query(&mut cache);
    query.set_families([
        QueryFamily::Generic(GenericFamily::SystemUi),
        QueryFamily::Generic(GenericFamily::SansSerif),
    ]);
    query.set_attributes(Attributes::new(
        FontWidth::NORMAL,
        FontStyle::Normal,
        FontWeight::SEMI_BOLD,
    ));
    let mut found = None;
    query.matches_with(|font| {
        match FontVec::try_from_vec_and_index(font.blob.data().to_vec(), font.index) {
            Ok(f) => {
                found = Some(f);
                QueryStatus::Stop
            }
            Err(_) => QueryStatus::Continue,
        }
    });
    found
}

pub struct Renderer {
    pub pixmap: Pixmap,
    pub scale: f32,
    font: Option<FontVec>,
}
impl Renderer {
    pub fn new(scale: f32) -> Self {
        Self {
            pixmap: canvas(scale),
            scale,
            font: load_font(),
        }
    }
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale;
            self.pixmap = canvas(scale);
        }
    }
    pub fn size(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }
    /// Logical width of `text` at the message size, for sizing the pill.
    pub fn measure(&self, text: &str) -> f64 {
        let Some(font) = &self.font else {
            return text.chars().count() as f64 * 8.0;
        };
        let font = font.as_scaled(PxScale::from(FONT_SIZE));
        let mut width = 0.0;
        let mut previous = None;
        for ch in text.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = previous {
                width += font.kern(p, id);
            }
            width += font.h_advance(id);
            previous = Some(id);
        }
        f64::from(width)
    }
    pub fn draw(&mut self, scene: &Scene, now: Instant) {
        self.pixmap.fill(Color::TRANSPARENT);
        let motion = &scene.motion;
        let width = motion.width.get(now) as f32;
        let height = motion.height.get(now) as f32;
        let opacity = motion.opacity.get(now) as f32;
        if opacity <= 0.001 {
            return;
        }
        let left = (WIDTH - width) / 2.0;
        let top = HEIGHT - 24.0 - height;
        let (right, bottom) = (left + width, top + height);
        let pop = motion.scale(now) as f32;
        let center_y = top + height / 2.0;
        let transform =
            Transform::from_scale(self.scale, self.scale).pre_concat(Transform::from_row(
                pop,
                0.0,
                0.0,
                pop,
                WIDTH / 2.0 * (1.0 - pop),
                center_y * (1.0 - pop),
            ));
        let shadow = motion.shadow.get(now) as f32 * opacity;
        // Concentric Gaussian coverage approximates WPF's 22px soft shadow at a 5px offset.
        for spread in (1..=22).rev() {
            let d = spread as f32;
            let alpha = shadow * 0.012 * (-d * d / 100.0).exp();
            if let Some(path) = rounded(
                left - d,
                top + 5.0 - d,
                right + d,
                bottom + 5.0 + d,
                20.0 + d,
            ) {
                fill(
                    &mut self.pixmap,
                    &path,
                    rgba(0.0, 0.0, 0.0, alpha),
                    transform,
                );
            }
        }
        let radius = 20.0_f32.min(height / 2.0);
        let Some(pill) = rounded(left, top, right, bottom, radius) else {
            return;
        };
        fill(
            &mut self.pixmap,
            &pill,
            rgba(
                14.0 / 255.0,
                14.0 / 255.0,
                16.0 / 255.0,
                242.0 / 255.0 * opacity,
            ),
            transform,
        );
        let mut paint = Paint::default();
        paint.set_color(rgba(1.0, 1.0, 1.0, 0.2 * opacity));
        paint.anti_alias = true;
        self.pixmap.stroke_path(
            &pill,
            &paint,
            &Stroke {
                width: 1.0,
                ..Stroke::default()
            },
            transform,
            None,
        );
        let glow = motion.glow.get(now) as f32;
        if glow > 0.001 {
            let progress = (now.duration_since(scene.transcribing_at).as_secs_f64() % 1.1) / 1.1;
            let shift = -(-std::f64::consts::PI * progress).cos() as f32;
            if let Some(shader) = LinearGradient::new(
                Point::from_xy(left + width * shift, top),
                Point::from_xy(left + width * (1.0 + shift), top),
                vec![
                    GradientStop::new(0.0, rgba(1.0, 1.0, 1.0, 0.0)),
                    GradientStop::new(0.5, rgba(1.0, 1.0, 1.0, 34.0 / 255.0 * glow * opacity)),
                    GradientStop::new(1.0, rgba(1.0, 1.0, 1.0, 0.0)),
                ],
                SpreadMode::Pad,
                Transform::identity(),
            ) {
                let paint = Paint {
                    shader,
                    anti_alias: true,
                    ..Paint::default()
                };
                self.pixmap
                    .fill_path(&pill, &paint, FillRule::Winding, transform, None);
            }
        }
        let bars = motion.bars.get(now) as f32 * opacity;
        if bars > 0.001 {
            for (i, &bar) in scene.wave.heights.iter().enumerate() {
                let x = WIDTH / 2.0 - 93.0 / 2.0 + i as f32 * 6.0;
                let h = bar as f32;
                let y = center_y - h / 2.0;
                if let Some(path) = rounded(x, y, x + 3.0, y + h, 1.5) {
                    let alpha = bars * scene.wave.alphas[i] as f32;
                    fill(
                        &mut self.pixmap,
                        &path,
                        rgba(1.0, 1.0, 1.0, alpha),
                        transform,
                    );
                }
            }
        }
        let lock = motion.lock.get(now) as f32 * opacity;
        if lock > 0.001 {
            self.padlock(
                left + 21.0,
                center_y,
                rgba(1.0, 1.0, 1.0, 217.0 / 255.0 * lock),
                transform,
            );
        }
        let elapsed = now.duration_since(scene.locked_at).as_secs_f64() - 0.2;
        let pulse = if elapsed < 0.0 {
            1.0
        } else {
            0.625 + 0.375 * (std::f64::consts::PI * elapsed / 0.7).cos()
        };
        let dot = motion.dot.get(now) as f32 * opacity * pulse as f32;
        if dot > 0.001
            && let Some(path) = PathBuilder::from_circle(right - 18.0, center_y, 3.0)
        {
            fill(
                &mut self.pixmap,
                &path,
                rgba(1.0, 90.0 / 255.0, 95.0 / 255.0, dot),
                transform,
            );
        }
        let message = motion.message.get(now) as f32 * opacity;
        if message > 0.001 {
            let (r, g, b) = match scene.message_kind {
                MessageKind::Error => (255, 168, 168),
                MessageKind::Warn => (255, 222, 158),
                MessageKind::Success => (168, 240, 192),
                MessageKind::Info => (255, 255, 255),
            };
            self.text(
                &scene.message,
                (left + 18.0, right - 18.0),
                center_y - 1.0,
                [r, g, b],
                message,
                transform,
            );
        }
    }
    /// The Segoe lock glyph's shape: a body with a shackle, in logical pixels.
    fn padlock(&mut self, cx: f32, cy: f32, color: Color, transform: Transform) {
        if let Some(body) = rounded(cx - 4.0, cy - 1.0, cx + 4.0, cy + 5.0, 1.2) {
            fill(&mut self.pixmap, &body, color, transform);
        }
        let mut shackle = PathBuilder::new();
        shackle.move_to(cx - 2.6, cy - 1.0);
        shackle.line_to(cx - 2.6, cy - 3.0);
        shackle.cubic_to(cx - 2.6, cy - 6.4, cx + 2.6, cy - 6.4, cx + 2.6, cy - 3.0);
        shackle.line_to(cx + 2.6, cy - 1.0);
        if let Some(path) = shackle.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            self.pixmap.stroke_path(
                &path,
                &paint,
                &Stroke {
                    width: 1.3,
                    ..Stroke::default()
                },
                transform,
                None,
            );
        }
    }
    /// Centered, single line, trimmed with an ellipsis like DirectWrite's character trimming.
    fn text(
        &mut self,
        text: &str,
        (x0, x1): (f32, f32),
        center_y: f32,
        rgb: [u8; 3],
        alpha: f32,
        transform: Transform,
    ) {
        let Some(font) = &self.font else { return };
        let available = x1 - x0;
        let mut shown: String = text.to_owned();
        if self.measure(&shown) as f32 > available {
            while !shown.is_empty() && self.measure(&format!("{shown}…")) as f32 > available {
                shown.pop();
            }
            shown.push('…');
        }
        let width = self.measure(&shown) as f32;
        // Glyphs rasterize at device size; positions go through the pill's transform.
        let device_scale = transform.sx;
        let font = font.as_scaled(PxScale::from(FONT_SIZE * device_scale));
        let baseline = center_y + (font.ascent() + font.descent()) / device_scale / 2.0;
        let mut origin = Point::from_xy(x0 + (available - width) / 2.0, baseline);
        transform.map_point(&mut origin);
        let (pw, ph) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let pixels = self.pixmap.pixels_mut();
        let mut caret = origin.x;
        let mut previous = None;
        for ch in shown.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = previous {
                caret += font.kern(p, id);
            }
            let glyph = id.with_scale_and_position(font.scale(), ab_glyph::point(caret, origin.y));
            caret += font.h_advance(id);
            previous = Some(id);
            let Some(outline) = font.outline_glyph(glyph) else {
                continue;
            };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let x = bounds.min.x as i32 + gx as i32;
                let y = bounds.min.y as i32 + gy as i32;
                if x < 0 || y < 0 || x >= pw || y >= ph {
                    return;
                }
                let a = (coverage * alpha).clamp(0.0, 1.0);
                let pixel = &mut pixels[(y * pw + x) as usize];
                // Source-over in premultiplied space: dst = src * a + dst * (1 - a).
                let over = |src: u8, dst: u8| {
                    (f32::from(src) * a + f32::from(dst) * (1.0 - a)).round() as u8
                };
                let alpha = over(255, pixel.alpha());
                if let Some(p) = tiny_skia::PremultipliedColorU8::from_rgba(
                    over(rgb[0], pixel.red()).min(alpha),
                    over(rgb[1], pixel.green()).min(alpha),
                    over(rgb[2], pixel.blue()).min(alpha),
                    alpha,
                ) {
                    *pixel = p;
                }
            });
        }
    }
}
fn canvas(scale: f32) -> Pixmap {
    Pixmap::new(
        (WIDTH * scale).round().max(1.0) as u32,
        (HEIGHT * scale).round().max(1.0) as u32,
    )
    .expect("non-empty overlay canvas")
}
fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    Color::from_rgba(r, g, b, a.clamp(0.0, 1.0)).unwrap_or(Color::TRANSPARENT)
}
fn fill(pixmap: &mut Pixmap, path: &Path, color: Color, transform: Transform) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, transform, None);
}
/// A rounded rectangle from circular-arc approximations.
fn rounded(l: f32, t: f32, r: f32, b: f32, radius: f32) -> Option<Path> {
    let radius = radius.min((r - l) / 2.0).min((b - t) / 2.0).max(0.0);
    if radius == 0.0 {
        return Some(PathBuilder::from_rect(Rect::from_ltrb(l, t, r, b)?));
    }
    const K: f32 = 0.552_284_8;
    let k = radius * K;
    let mut p = PathBuilder::new();
    p.move_to(l + radius, t);
    p.line_to(r - radius, t);
    p.cubic_to(r - radius + k, t, r, t + radius - k, r, t + radius);
    p.line_to(r, b - radius);
    p.cubic_to(r, b - radius + k, r - radius + k, b, r - radius, b);
    p.line_to(l + radius, b);
    p.cubic_to(l + radius - k, b, l, b - radius + k, l, b - radius);
    p.line_to(l, t + radius);
    p.cubic_to(l, t + radius - k, l + radius - k, t, l + radius, t);
    p.close();
    p.finish()
}

#[cfg(test)]
mod tests {
    // Failure modes: the idle pill paints nothing or the wrong size at 2x;
    // messages draw outside the pill.
    use super::*;
    use crate::app::Pill;

    #[test]
    fn paints_the_pill_at_the_output_scale() {
        let mut renderer = Renderer::new(2.0);
        assert_eq!(renderer.size(), (960, 240));
        let mut scene = Scene::new(true);
        let now = Instant::now();
        scene.show(
            Pill::message("Hold Ctrl + Win to dictate", MessageKind::Info, 2.0),
            now,
            |t| renderer.measure(t),
        );
        let later = now + std::time::Duration::from_millis(600);
        scene.advance(later);
        renderer.draw(&scene, later);
        let pixel = |x: u32, y: u32| renderer.pixmap.pixel(x, y).expect("in bounds");
        // Pill center is opaque dark; the far corner stays transparent.
        let center = pixel(480, (2.0 * (HEIGHT - 24.0 - 20.0)) as u32);
        assert!(center.alpha() > 200, "{center:?}");
        assert_eq!(pixel(2, 2).alpha(), 0);
    }
}
