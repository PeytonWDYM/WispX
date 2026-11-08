//! Direct2D/DirectWrite draw into a top-down premultiplied BGRA DIB.
use super::State;
use crate::app::MessageKind;
use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT, SIZE},
        Graphics::{
            Direct2D::{Common::*, *},
            DirectWrite::*,
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::*,
        },
        UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow},
    },
    core::{PCWSTR, Result, w},
};
use windows_numerics::{Matrix3x2, Vector2};

pub struct Renderer {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    target: ID2D1DCRenderTarget,
    write: IDWriteFactory,
    font: IDWriteTextFormat,
    icon: IDWriteTextFormat,
    pub width: i32,
    pub height: i32,
}
impl Renderer {
    pub fn new(width: i32, height: i32, scale: f64) -> Result<Self> {
        // SAFETY: initialized DIB parameters describe width*height BGRA pixels; all GDI resources are owned here.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let previous = SelectObject(dc, bitmap.into());
            let result = (|| {
                let factory: ID2D1Factory =
                    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let properties = D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: (96.0 * scale) as f32,
                    dpiY: (96.0 * scale) as f32,
                    ..Default::default()
                };
                let target = factory.CreateDCRenderTarget(&properties)?;
                target.BindDC(
                    dc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: width,
                        bottom: height,
                    },
                )?;
                target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
                let name = super::super::files::wide(super::super::fonts::family(
                    &write,
                    "Segoe UI Variable Display",
                    "Segoe UI",
                )?);
                let font = write.CreateTextFormat(
                    PCWSTR(name.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_SEMI_BOLD,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    15.0,
                    w!("en-us"),
                )?;
                font.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                font.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                font.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let name = super::super::files::wide(super::super::fonts::family(
                    &write,
                    "Segoe Fluent Icons",
                    "Segoe MDL2 Assets",
                )?);
                let icon = write.CreateTextFormat(
                    PCWSTR(name.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    11.0,
                    w!("en-us"),
                )?;
                icon.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                let ellipsis = write.CreateEllipsisTrimmingSign(&font)?;
                font.SetTrimming(
                    &DWRITE_TRIMMING {
                        granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                        ..Default::default()
                    },
                    &ellipsis,
                )?;
                Ok(Self {
                    dc,
                    bitmap,
                    previous,
                    bits: bits.cast(),
                    target,
                    write,
                    font,
                    icon,
                    width,
                    height,
                })
            })();
            if result.is_err() {
                SelectObject(dc, previous);
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
            }
            result
        }
    }
    /// Saves actual layered-window pixels for repeatable overlay verification.
    pub fn snapshot(&self, path: &std::path::Path) -> std::io::Result<()> {
        // SAFETY: this DIB owns exactly width*height*4 bytes until Renderer drops, on this thread.
        let pixels = unsafe {
            std::slice::from_raw_parts(self.bits, self.width as usize * self.height as usize * 4)
        };
        let rgba: Vec<_> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|pixel| {
                let a = u32::from(pixel[3]);
                let channel =
                    |v: u8| (u32::from(v) * 255).checked_div(a).unwrap_or(0).min(255) as u8;
                [
                    channel(pixel[2]),
                    channel(pixel[1]),
                    channel(pixel[0]),
                    pixel[3],
                ]
            })
            .collect();
        let mut encoder = png::Encoder::new(
            std::fs::File::create(path)?,
            self.width as u32,
            self.height as u32,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgba)?;
        Ok(())
    }
    pub fn measure(&self, text: &str) -> Result<f64> {
        let text: Vec<_> = text.encode_utf16().collect();
        // SAFETY: the layout reads our initialized text slice and returns metrics into a valid struct.
        unsafe {
            let layout = self
                .write
                .CreateTextLayout(&text, &self.font, 4096.0, 40.0)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            Ok(f64::from(metrics.width))
        }
    }
    pub fn draw(&self, hwnd: HWND, state: &State, now: std::time::Instant) -> Result<()> {
        let motion = &state.motion;
        let width = motion.width.get(now) as f32;
        let height = motion.height.get(now) as f32;
        let opacity = motion.opacity.get(now) as f32;
        let left = (480.0 - width) / 2.0;
        let top = 120.0 - 24.0 - height;
        let rect = D2D_RECT_F {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };
        // SAFETY: target/DC/resources belong to this UI thread; all geometry pointers live through their calls.
        unsafe {
            self.target.BeginDraw();
            self.target.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
            let scale = motion.scale(now) as f32;
            let center_x = 240.0;
            let center_y = top + height / 2.0;
            self.target.SetTransform(&Matrix3x2 {
                M11: scale,
                M12: 0.0,
                M21: 0.0,
                M22: scale,
                M31: center_x * (1.0 - scale),
                M32: center_y * (1.0 - scale),
            });
            let shadow = motion.shadow.get(now) as f32 * opacity;
            // Concentric Gaussian coverage approximates WPF's 22px soft shadow at a 5px offset.
            for spread in (1..=22).rev() {
                let d = spread as f32;
                let alpha = shadow * 0.012 * (-d * d / 100.0).exp();
                let brush = self
                    .target
                    .CreateSolidColorBrush(&color(0.0, 0.0, 0.0, alpha), None)?;
                self.target.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: left - d,
                            top: top + 5.0 - d,
                            right: rect.right + d,
                            bottom: rect.bottom + 5.0 + d,
                        },
                        radiusX: 20.0 + d,
                        radiusY: 20.0 + d,
                    },
                    &brush,
                );
            }
            let fill = self.target.CreateSolidColorBrush(
                &color(
                    14.0 / 255.0,
                    14.0 / 255.0,
                    16.0 / 255.0,
                    242.0 / 255.0 * opacity,
                ),
                None,
            )?;
            let rounded = D2D1_ROUNDED_RECT {
                rect,
                radiusX: 20.0_f32.min(height / 2.0),
                radiusY: 20.0_f32.min(height / 2.0),
            };
            self.target.FillRoundedRectangle(&rounded, &fill);
            let border = self
                .target
                .CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.2 * opacity), None)?;
            self.target
                .DrawRoundedRectangle(&rounded, &border, 1.0, None);
            let glow = motion.glow.get(now) as f32;
            if glow > 0.001 {
                let progress =
                    (now.duration_since(state.transcribing_at).as_secs_f64() % 1.1) / 1.1;
                let shift = -(-std::f64::consts::PI * progress).cos();
                let stops = [
                    D2D1_GRADIENT_STOP {
                        position: 0.0,
                        color: color(1.0, 1.0, 1.0, 0.0),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 0.5,
                        color: color(1.0, 1.0, 1.0, 34.0 / 255.0 * glow * opacity),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 1.0,
                        color: color(1.0, 1.0, 1.0, 0.0),
                    },
                ];
                let collection = self.target.CreateGradientStopCollection(
                    &stops,
                    D2D1_GAMMA_2_2,
                    D2D1_EXTEND_MODE_CLAMP,
                )?;
                let gradient = self.target.CreateLinearGradientBrush(
                    &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: Vector2 {
                            X: left + width * shift as f32,
                            Y: top,
                        },
                        endPoint: Vector2 {
                            X: left + width * (1.0 + shift as f32),
                            Y: top,
                        },
                    },
                    None,
                    &collection,
                )?;
                self.target.FillRoundedRectangle(&rounded, &gradient);
            }
            let bars_opacity = motion.bars.get(now) as f32 * opacity;
            for (i, &bar_height) in state.wave.heights.iter().enumerate() {
                let x = 240.0 - 93.0 / 2.0 + i as f32 * 6.0;
                let h = bar_height as f32;
                let y = top + height / 2.0 - h / 2.0;
                let brush = self.target.CreateSolidColorBrush(
                    &color(1.0, 1.0, 1.0, bars_opacity * state.wave.alphas[i] as f32),
                    None,
                )?;
                self.target.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: x,
                            top: y,
                            right: x + 3.0,
                            bottom: y + h,
                        },
                        radiusX: 1.5,
                        radiusY: 1.5,
                    },
                    &brush,
                );
            }
            let lock_opacity = motion.lock.get(now) as f32 * opacity;
            if lock_opacity > 0.001 {
                let brush = self.target.CreateSolidColorBrush(
                    &color(1.0, 1.0, 1.0, 217.0 / 255.0 * lock_opacity),
                    None,
                )?;
                self.target.DrawText(
                    &[0xe72e],
                    &self.icon,
                    &D2D_RECT_F {
                        left: left + 14.0,
                        top,
                        right: left + 28.0,
                        bottom: rect.bottom,
                    },
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            let elapsed = now.duration_since(state.locked_at).as_secs_f64() - 0.2;
            let pulse = if elapsed < 0.0 {
                1.0
            } else {
                0.625 + 0.375 * (std::f64::consts::PI * elapsed / 0.7).cos()
            };
            let dot = self.target.CreateSolidColorBrush(
                &color(
                    1.0,
                    90.0 / 255.0,
                    95.0 / 255.0,
                    motion.dot.get(now) as f32 * opacity * pulse as f32,
                ),
                None,
            )?;
            self.target.FillEllipse(
                &D2D1_ELLIPSE {
                    point: Vector2 {
                        X: rect.right - 18.0,
                        Y: top + height / 2.0,
                    },
                    radiusX: 3.0,
                    radiusY: 3.0,
                },
                &dot,
            );
            let message_opacity = motion.message.get(now) as f32 * opacity;
            if message_opacity > 0.001 {
                let (r, g, b) = match state.message_kind {
                    MessageKind::Error => (255, 168, 168),
                    MessageKind::Warn => (255, 222, 158),
                    MessageKind::Success => (168, 240, 192),
                    MessageKind::Info => (255, 255, 255),
                };
                let brush = self.target.CreateSolidColorBrush(
                    &color(
                        r as f32 / 255.0,
                        g as f32 / 255.0,
                        b as f32 / 255.0,
                        message_opacity,
                    ),
                    None,
                )?;
                let text: Vec<_> = state.message.encode_utf16().collect();
                self.target.DrawText(
                    &text,
                    &self.font,
                    &D2D_RECT_F {
                        left: left + 18.0,
                        top: top - 1.0,
                        right: rect.right - 18.0,
                        bottom: rect.bottom - 1.0,
                    },
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            self.target.EndDraw(None, None)?;
            let mut bounds = RECT::default();
            windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut bounds)?;
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&POINT {
                    x: bounds.left,
                    y: bounds.top,
                }),
                Some(&SIZE {
                    cx: self.width,
                    cy: self.height,
                }),
                Some(self.dc),
                Some(&POINT::default()),
                Default::default(),
                Some(&BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            )?;
            Ok(())
        }
    }
}
fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: restore the previous object before deleting the owned DIB/DC.
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
