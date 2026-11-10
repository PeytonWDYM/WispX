//! Cached, transparent glyph masks for the email's Gaussian blur animation.
use ab_glyph::{Font, FontVec, PxScale, ScaleFont, VariableFont, point};
use fontique::{
    Attributes, Collection, CollectionOptions, FontStyle, FontWeight, FontWidth, GenericFamily,
    QueryFamily, QueryStatus, SourceCache,
};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

const LEVELS: usize = 5;
const MAX_SIGMA: f32 = 3.0;

pub struct Frames {
    pub images: Vec<Image>,
    pub pad: f32,
}

/// Uses the window's family before the platform's UI and sans-serif fallbacks.
fn load_font(family: &str) -> Option<FontVec> {
    let mut collection = Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    });
    let mut cache = SourceCache::default();
    let mut query = collection.query(&mut cache);
    query.set_families([
        QueryFamily::Named(family),
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
            Ok(mut font) => {
                // Selecting a variable font's semibold face does not set its axis in ab_glyph.
                font.set_variation(b"wght", 600.0);
                found = Some(font);
                QueryStatus::Stop
            }
            Err(_) => QueryStatus::Continue,
        }
    });
    found
}

/// Rasterizes once at device resolution; every level shares the same origin and padding.
pub fn text(text: &str, family: &str, font_size: f32, scale: f32) -> Option<Frames> {
    if text.is_empty() {
        return None;
    }
    let font = load_font(family)?;
    // Slint sizes an em; ab_glyph sizes ascent minus descent instead.
    let pixels = font_size * scale;
    let em_scale = font.height_unscaled() / font.units_per_em()?;
    // Slint shapes at fractional sizes but its software glyph cache uses integer pixel sizes.
    let glyph_scale = PxScale::from(pixels.floor() * em_scale);
    let font = font.as_scaled(PxScale::from(pixels * em_scale));
    let pad = (3.0 * MAX_SIGMA * scale).ceil();
    // The software renderer snaps each glyph run's baseline to a device pixel.
    let baseline = pad + font.ascent().round();
    let mut caret = pad;
    let mut previous = None;
    let glyphs = text
        .chars()
        .map(|ch| {
            let id = font.glyph_id(ch);
            if let Some(previous) = previous {
                caret += font.kern(previous, id);
            }
            let glyph = id.with_scale_and_position(glyph_scale, point(caret, baseline));
            caret += font.h_advance(id);
            previous = Some(id);
            glyph
        })
        .collect::<Vec<_>>();
    let width = (caret + pad).ceil() as usize;
    let height = (font.height() + pad * 2.0).ceil() as usize;
    let mut alpha = vec![0.0; width * height];
    for glyph in glyphs {
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|x, y, coverage| {
            let x = bounds.min.x as i32 + x as i32;
            let y = bounds.min.y as i32 + y as i32;
            if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                let pixel = &mut alpha[y as usize * width + x as usize];
                *pixel += coverage * (1.0 - *pixel);
            }
        });
    }
    let images = (0..=LEVELS)
        .map(|level| {
            let mut mask = alpha.clone();
            if level > 0 {
                gaussian(
                    &mut mask,
                    width,
                    MAX_SIGMA * scale * level as f32 / LEVELS as f32,
                );
            }
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width as u32, height as u32);
            for (pixel, alpha) in buffer.make_mut_slice().iter_mut().zip(mask) {
                *pixel = Rgba8Pixel::new(255, 255, 255, (alpha * 255.0).round() as u8);
            }
            Image::from_rgba8(buffer)
        })
        .collect();
    Some(Frames {
        images,
        pad: pad / scale,
    })
}

/// Three separable box filters approximate a Gaussian with the requested variance.
fn gaussian(alpha: &mut [f32], width: usize, sigma: f32) {
    let ideal = (4.0 * sigma * sigma + 1.0).sqrt();
    let lower = (ideal.floor() as usize - 1) / 2 * 2 + 1;
    let lower_f = lower as f32;
    let count = ((12.0 * sigma * sigma - 3.0 * lower_f * lower_f - 12.0 * lower_f - 9.0)
        / (-4.0 * lower_f - 4.0))
        .round() as usize;
    let mut scratch = vec![0.0; alpha.len()];
    for pass in 0..3 {
        let radius = if pass < count {
            lower / 2
        } else {
            lower / 2 + 1
        };
        box_pass(alpha, &mut scratch, width, radius, false);
        box_pass(&scratch, alpha, width, radius, true);
    }
}

/// Sliding sums keep each pass linear; samples outside the mask are transparent.
fn box_pass(input: &[f32], output: &mut [f32], width: usize, radius: usize, vertical: bool) {
    let height = input.len() / width;
    let (lines, length, stride, line_stride) = if vertical {
        (width, height, width, 1)
    } else {
        (height, width, 1, width)
    };
    let divisor = (radius * 2 + 1) as f32;
    for line in 0..lines {
        let start = line * line_stride;
        let mut sum: f32 = (0..=radius.min(length - 1))
            .map(|i| input[start + i * stride])
            .sum();
        for i in 0..length {
            output[start + i * stride] = sum / divisor;
            if i >= radius {
                sum -= input[start + (i - radius) * stride];
            }
            if i + radius + 1 < length {
                sum += input[start + (i + radius + 1) * stride];
            }
        }
    }
}
