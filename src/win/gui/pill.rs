//! The window around `crate::app::pill`: clear everywhere but the pill, always on top,
//! clicks going straight through it, and never taking focus or showing in Alt+Tab.

use super::register_class;
use crate::app::overlay::Mode;
use crate::app::pill::{self, HOST_HEIGHT, HOST_WIDTH, Pill, TextMask, Wanted};
use crate::win::ffi::*;
use std::cell::RefCell;
use std::ptr;
use std::slice;
use std::time::Instant;

const CLASS: &str = "WispXPill";
const FRAME_TIMER: usize = 1;
const HOUSEKEEPING_TIMER: usize = 2;
const FRAME_MS: u32 = 15;
/// Other programs can take "always on top" for themselves, the taskbar in particular.
const HOUSEKEEPING_MS: u32 = 1500;
const MESSAGE_FONT_SIZE: f32 = 15.0;

/// Pixels something can be drawn into and then handed to the window.
struct Surface {
    dc: Hdc,
    bitmap: Hgdi,
    previous: Hgdi,
    bits: *mut u8,
    width: usize,
    height: usize,
}

impl Surface {
    fn new(width: i32, height: i32) -> Option<Surface> {
        let info = bitmap_info(width, height);
        let mut bits = ptr::null_mut();
        // SAFETY: the structure is complete. The objects are checked, and belong to this
        // value from here on.
        unsafe {
            let dc = CreateCompatibleDC(ptr::null_mut());
            if dc.is_null() {
                return None;
            }
            let bitmap = CreateDIBSection(
                dc,
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                ptr::null_mut(),
                0,
            );
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                return None;
            }
            let previous = SelectObject(dc, bitmap);
            Some(Surface {
                dc,
                bitmap,
                previous,
                bits: bits.cast(),
                width: width as usize,
                height: height as usize,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: the system allocated this many bytes for a bitmap held until `drop`.
        unsafe { slice::from_raw_parts_mut(self.bits, self.width * self.height * 4) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: these were made in `new`, and nothing can use them after this.
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

/// Top-down, 32 bits per pixel.
fn bitmap_info(width: i32, height: i32) -> BitmapInfo {
    BitmapInfo {
        size: 40,
        width,
        height: -height,
        planes: 1,
        bit_count: 32,
        compression: 0,
        image_size: 0,
        x_per_meter: 0,
        y_per_meter: 0,
        colors_used: 0,
        colors_important: 0,
        colors: [0],
    }
}

/// A message, drawn by the system and kept as how much of each pixel it covers.
struct Mask {
    text: String,
    scale: f32,
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

fn draw_text(text: &str, scale: f32) -> Option<Mask> {
    let room = (pill::MESSAGE_ROOM * scale).floor() as i32;
    let face = wide("Segoe UI");
    let units = wide(text);
    // SAFETY: the strings are NUL-terminated and the rectangles are local. The font and
    // surface are checked, and the font is freed before leaving.
    unsafe {
        let font = CreateFontW(
            -((MESSAGE_FONT_SIZE * scale).round() as i32),
            0,
            0,
            0,
            FW_SEMIBOLD,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            0,
            0,
            ANTIALIASED_QUALITY,
            0,
            face.as_ptr(),
        );
        if font.is_null() {
            return None;
        }
        // Measured first, on anything that will do.
        let probe = Surface::new(1, 1)?;
        let previous = SelectObject(probe.dc, font);
        let mut size = Rect::default();
        DrawTextW(
            probe.dc,
            units.as_ptr(),
            -1,
            &raw mut size,
            DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT,
        );
        SelectObject(probe.dc, previous);

        let width = size.right.clamp(1, room);
        let height = size.bottom.max(1);
        let Some(mut surface) = Surface::new(width, height) else {
            DeleteObject(font);
            return None;
        };
        let previous = SelectObject(surface.dc, font);
        SetBkMode(surface.dc, TRANSPARENT);
        SetTextColor(surface.dc, 0x00FF_FFFF);
        let mut area = Rect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        DrawTextW(
            surface.dc,
            units.as_ptr(),
            -1,
            &raw mut area,
            DT_SINGLELINE | DT_NOPREFIX | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
        );
        SelectObject(surface.dc, previous);
        DeleteObject(font);
        // White on black, so any color channel says how much of a pixel the text covers.
        let coverage = surface
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[1])
            .collect();
        Some(Mask {
            text: text.to_string(),
            scale,
            width: width as usize,
            height: height as usize,
            coverage,
        })
    }
}

struct Inner {
    pill: Pill,
    surface: Option<Surface>,
    scale: f32,
    position: Point,
    size: Size,
    mask: Option<Mask>,
    level: f32,
    frame_timer: bool,
    housekeeping_timer: bool,
    shown: bool,
    /// Holds the idle pill back from a game or a presentation.
    stood_aside: bool,
}

pub struct PillWindow {
    hwnd: Hwnd,
    start: Instant,
    inner: RefCell<Inner>,
}

impl PillWindow {
    pub fn create() -> Result<PillWindow, String> {
        register_class(CLASS, window_proc, ptr::null_mut(), ptr::null_mut(), false);
        let class = wide(CLASS);
        let title = wide("WispX");
        // SAFETY: the strings are NUL-terminated and the class was registered above.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                super::instance_handle(),
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(format!(
                "cannot create the pill: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(PillWindow {
            hwnd,
            start: Instant::now(),
            inner: RefCell::new(Inner {
                pill: Pill::new(),
                surface: None,
                scale: 1.0,
                position: Point::default(),
                size: Size::default(),
                mask: None,
                level: 0.0,
                frame_timer: false,
                housekeeping_timer: false,
                shown: false,
                stood_aside: false,
            }),
        })
    }

    pub fn hwnd(&self) -> Hwnd {
        self.hwnd
    }

    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    pub fn set_overlay(&self, json: &str, level: f32) {
        let Some(wanted) = Wanted::parse(json) else {
            return;
        };
        let now = self.now();
        let mut inner = self.inner.borrow_mut();
        inner.level = level;
        // On whichever screen the user is on as things start.
        if !inner.pill.mapped() || wanted.mode != Mode::Idle {
            inner.place(self.hwnd);
        }
        if wanted.mode != Mode::Idle || !wanted.message.is_empty() {
            inner.stood_aside = false;
        }
        let scale = inner.scale;
        let width = if wanted.message.is_empty() {
            0.0
        } else {
            inner.ensure_mask(&wanted.message);
            inner
                .mask
                .as_ref()
                .map_or(0.0, |mask| mask.width as f32 / scale)
        };
        inner.pill.set(now, wanted, |_| width);
        inner.refresh(self.hwnd, now);
    }

    fn on_frame(&self, level: f32) {
        let now = self.now();
        let mut inner = self.inner.borrow_mut();
        inner.level = level;
        inner.refresh(self.hwnd, now);
    }

    fn on_housekeeping(&self) {
        let now = self.now();
        let mut inner = self.inner.borrow_mut();
        if !inner.pill.mapped() {
            return;
        }
        let idle = inner.pill.wanted().mode == Mode::Idle && inner.pill.wanted().message.is_empty();
        if idle {
            inner.stood_aside = something_is_fullscreen();
            inner.place(self.hwnd);
        }
        inner.refresh(self.hwnd, now);
        if inner.shown {
            // SAFETY: only reorders this thread's own window.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
}

fn something_is_fullscreen() -> bool {
    let mut state = 0;
    // SAFETY: writes to a local.
    let result = unsafe { SHQueryUserNotificationState(&raw mut state) };
    result == 0
        && matches!(
            state,
            QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE
        )
}

impl Inner {
    /// Sits the window at the bottom middle of the screen holding the foreground window,
    /// or else the pointer, at the size that screen calls for. As in the original, the
    /// window hangs a little below the work area and the pill sits above that.
    fn place(&mut self, hwnd: Hwnd) {
        // SAFETY: the structures are local, and a monitor handle needs no closing.
        let (work, dpi) = unsafe {
            let foreground = GetForegroundWindow();
            let monitor = if !foreground.is_null() && foreground != hwnd {
                MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)
            } else {
                let mut point = Point::default();
                GetCursorPos(&raw mut point);
                MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
            };
            let mut info = MonitorInfo {
                size: size_of::<MonitorInfo>() as u32,
                ..MonitorInfo::default()
            };
            if GetMonitorInfoW(monitor, &raw mut info) == 0 {
                return;
            }
            let (mut x, mut y) = (96, 96);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut x, &raw mut y);
            (info.work, x)
        };
        let scale = dpi.max(96) as f32 / 96.0;
        let size = Size {
            cx: (HOST_WIDTH * scale).ceil() as i32,
            cy: (HOST_HEIGHT * scale).ceil() as i32,
        };
        self.position = Point {
            x: work.left + (work.right - work.left - size.cx) / 2,
            y: work.bottom - size.cy + (pill::BELOW_WORK_AREA * scale).round() as i32,
        };
        if size != self.size || self.surface.is_none() {
            self.size = size;
            self.surface = Surface::new(size.cx, size.cy);
        }
        self.scale = scale;
    }

    fn ensure_mask(&mut self, text: &str) {
        let current = self
            .mask
            .as_ref()
            .is_some_and(|mask| mask.text == text && mask.scale == self.scale);
        if !current {
            self.mask = draw_text(text, self.scale);
        }
    }

    /// Brings the screen up to date, and sets up whatever is needed to keep it so.
    fn refresh(&mut self, hwnd: Hwnd, now: f64) {
        self.pill.set_level(self.level);
        self.pill.advance(now);
        let mapped = self.pill.mapped();
        let visible = mapped && !self.stood_aside;
        if visible {
            if let Some(text) = self.pill.message().map(|(text, _)| text.to_string()) {
                self.ensure_mask(&text);
            }
            self.draw(hwnd, now);
            if !self.shown {
                self.shown = true;
                // SAFETY: only shows and reorders this thread's own window, which has
                // what it should look like by now.
                unsafe {
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        } else if self.shown {
            self.shown = false;
            // SAFETY: only hides this thread's own window.
            unsafe {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
        let animating = visible && self.pill.animating(now);
        set_timer(
            hwnd,
            FRAME_TIMER,
            FRAME_MS,
            &mut self.frame_timer,
            animating,
        );
        set_timer(
            hwnd,
            HOUSEKEEPING_TIMER,
            HOUSEKEEPING_MS,
            &mut self.housekeeping_timer,
            mapped,
        );
    }

    fn draw(&mut self, hwnd: Hwnd, now: f64) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (width, height) = (surface.width, surface.height);
        let mask = self.mask.as_ref().map(|mask| TextMask {
            width: mask.width,
            height: mask.height,
            coverage: &mask.coverage,
        });
        self.pill.paint(
            now,
            surface.pixels(),
            width,
            height,
            self.scale,
            mask.as_ref(),
        );
        let blend = BlendFunction {
            op: 0,
            flags: 0,
            constant_alpha: 255,
            alpha_format: AC_SRC_ALPHA,
        };
        let source = Point::default();
        // SAFETY: all of it is local or owned here, and the window is this thread's.
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                ptr::null_mut(),
                &raw const self.position,
                &raw const self.size,
                surface.dc,
                &raw const source,
                0,
                &raw const blend,
                ULW_ALPHA,
            );
        }
    }
}

fn set_timer(hwnd: Hwnd, id: usize, milliseconds: u32, running: &mut bool, wanted: bool) {
    if wanted == *running {
        return;
    }
    *running = wanted;
    // SAFETY: the timer belongs to this thread's own window, and no callback is given.
    unsafe {
        if wanted {
            SetTimer(hwnd, id, milliseconds, ptr::null());
        } else {
            KillTimer(hwnd, id);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE,
        WM_NCHITTEST => return HTTRANSPARENT,
        // Sizes come from `place`, which looks at the screen the pill is going to.
        WM_DPICHANGED => return 0,
        WM_TIMER => {
            if let Some(app) = super::app() {
                match wparam {
                    FRAME_TIMER => app.pill.on_frame(app.ui.level()),
                    HOUSEKEEPING_TIMER => app.pill.on_housekeeping(),
                    _ => {}
                }
            }
            return 0;
        }
        _ => {}
    }
    // SAFETY: passes on what it was given, for a window of this thread.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
