//! Non-activating layered pill; DWM frames are requested only for active animation.
mod frame_clock;
mod motion;
mod render;
mod waveform;
use crate::{
    app::{MessageKind, Pill},
    audio::recorder::Levels,
};
use motion::{Ease, Motion};
use render::Renderer;
use std::{
    cell::RefCell,
    time::{Duration, Instant},
};
use waveform::Waveform;
use windows::{
    Win32::{
        Foundation::{D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Shell::SHQueryUserNotificationState,
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

enum Mode {
    Idle,
    Recording { levels: Levels, locked: bool },
    Transcribing,
    Message,
}
struct State {
    hwnd: HWND,
    mode: Mode,
    motion: Motion,
    wave: Waveform,
    renderer: Renderer,
    message: String,
    message_kind: MessageKind,
    idle_enabled: bool,
    monitor: HMONITOR,
    clock: Option<frame_clock::FrameClock>,
    rendering: bool,
    draw_failed: bool,
    last_frame: Option<Instant>,
    transcribing_at: Instant,
    locked_at: Instant,
    deadline: Option<Instant>,
    preview_start: Option<Instant>,
}
pub struct Overlay {
    hwnd: HWND,
    state: Box<RefCell<State>>,
}
impl Overlay {
    pub fn new(idle_enabled: bool) -> windows::core::Result<Self> {
        let renderer = Renderer::new(480, 120, 1.0)?;
        let mut state = Box::new(RefCell::new(State {
            hwnd: HWND::default(),
            mode: Mode::Idle,
            motion: Motion::default(),
            wave: Waveform::default(),
            renderer,
            message: String::new(),
            message_kind: MessageKind::Info,
            idle_enabled,
            monitor: HMONITOR::default(),
            clock: None,
            rendering: false,
            draw_failed: false,
            last_frame: None,
            transcribing_at: Instant::now(),
            locked_at: Instant::now(),
            deadline: None,
            preview_start: None,
        }));
        // SAFETY: the boxed state has a stable address and outlives the owned window.
        let hwnd = unsafe {
            let module = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: module.into(),
                lpszClassName: w!("WispX.Overlay"),
                ..Default::default()
            });
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST,
                w!("WispX.Overlay"),
                w!("WispX overlay"),
                WS_POPUP,
                -32000,
                -32000,
                480,
                120,
                None,
                None,
                Some(module.into()),
                Some((&mut *state as *mut RefCell<State>).cast()),
            )?
        };
        state.borrow_mut().hwnd = hwnd;
        state.borrow_mut().clock = Some(frame_clock::FrameClock::new(hwnd));
        let overlay = Self { hwnd, state };
        {
            let mut state = overlay.state.borrow_mut();
            state.reposition(true)?;
            state.show(Pill::Idle)?;
        }
        // SAFETY: our timer is tied to our window; it does no idle repainting.
        unsafe {
            SetTimer(Some(hwnd), 2, 1500, None);
        }
        Ok(overlay)
    }
    pub fn show(&self, pill: Pill) -> windows::core::Result<()> {
        self.state.borrow_mut().show(pill)
    }
    pub fn snapshot(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.state.borrow().renderer.snapshot(path)
    }
    pub fn set_idle_enabled(&self, enabled: bool) {
        self.state.borrow_mut().idle_enabled = enabled;
    }
    pub fn preview_recording(&self) -> windows::core::Result<()> {
        let mut state = self.state.borrow_mut();
        state.show(Pill::Recording(Levels::default()))?;
        state.preview_start = Some(Instant::now());
        Ok(())
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        self.state.borrow_mut().clock.take();
        // SAFETY: destroying the window stops its timers before the boxed state drops.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
impl State {
    fn idle_opacity(&self) -> f64 {
        // SAFETY: this shell query returns a value and doesn't require any owned resources.
        let fullscreen =
            unsafe { SHQueryUserNotificationState() }.is_ok_and(|s| matches!(s.0, 2..=4));
        if self.idle_enabled && !fullscreen {
            0.55
        } else {
            0.0
        }
    }
    fn show(&mut self, pill: Pill) -> windows::core::Result<()> {
        let now = Instant::now();
        self.deadline = None;
        // SAFETY: timer 3 belongs to this window and is replaced for each message/done state.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), 3);
        }
        match pill {
            Pill::Recording(levels) => {
                self.reposition(true)?;
                self.mode = Mode::Recording {
                    levels,
                    locked: false,
                };
                self.preview_start = None;
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
                    return Ok(());
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
                self.auto_idle(0.17, now);
            }
            Pill::Message {
                text,
                kind,
                seconds,
            } => {
                if matches!(self.mode, Mode::Idle) {
                    self.reposition(true)?;
                }
                let width = (self.renderer.measure(&text)? + 42.0).min(470.0);
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
                self.auto_idle(seconds, now);
            }
            Pill::Idle => {
                self.mode = Mode::Idle;
                self.preview_start = None;
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
        self.start_frames();
        self.frame()
    }
    fn auto_idle(&mut self, seconds: f64, now: Instant) {
        self.deadline = Some(now + Duration::from_secs_f64(seconds));
        // SAFETY: message expiration is owned by this window and replaced on state change.
        unsafe {
            SetTimer(Some(self.hwnd), 3, (seconds * 1000.0).ceil() as u32, None);
        }
    }
    fn start_frames(&mut self) {
        if self.rendering || self.draw_failed {
            return;
        }
        self.rendering = true;
        self.last_frame = None;
        self.clock
            .as_ref()
            .expect("clock installed after window creation")
            .start();
    }
    fn frame(&mut self) -> windows::core::Result<()> {
        // A posted frame can remain queued after the clock stops on a draw failure.
        if !self.rendering {
            return Ok(());
        }
        let now = Instant::now();
        let dt = self.last_frame.map_or(1.0 / 60.0, |last| {
            now.duration_since(last).as_secs_f64().min(0.05)
        });
        self.last_frame = Some(now);
        let (level, recording, transcribing) = match &self.mode {
            Mode::Recording { levels, .. } => (
                self.preview_start.map_or_else(
                    || f64::from(levels.level()),
                    |start| 0.55 + 0.45 * (now.duration_since(start).as_secs_f64() * 3.0).sin(),
                ),
                true,
                false,
            ),
            Mode::Transcribing => (0.0, false, true),
            _ => (0.0, false, false),
        };
        self.wave.frame(dt, level, recording, transcribing);
        self.draw(now)?;
        if !recording && !transcribing && !self.motion.moving(now) {
            self.clock
                .as_ref()
                .expect("clock installed after window creation")
                .stop();
            self.rendering = false;
        }
        Ok(())
    }
    fn draw(&mut self, now: Instant) -> windows::core::Result<()> {
        if self.draw_failed {
            return Ok(());
        }
        let result = (|| {
            match self.renderer.draw(self.hwnd, self, now) {
                Err(error) if error.code() == D2DERR_RECREATE_TARGET => {
                    // Rebuild at the current size/DPI, including every target-dependent resource,
                    // and retry this frame once. A persistent device failure must not spin.
                    self.renderer = Renderer::new(
                        self.renderer.width,
                        self.renderer.height,
                        self.renderer.scale,
                    )?;
                    self.renderer.draw(self.hwnd, self, now)
                }
                result => result,
            }
        })();
        if result.is_err() {
            self.clock
                .as_ref()
                .expect("clock installed after window creation")
                .stop();
            self.rendering = false;
            // Propagate/log only the first failure; timers and queued frames cannot restart it.
            // A successful resize installs fresh resources and permits rendering again.
            self.draw_failed = true;
        }
        result
    }
    fn housekeeping(&mut self) -> windows::core::Result<()> {
        // SAFETY: only z-order flags are changed; the pill never activates.
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
        }
        if matches!(self.mode, Mode::Idle) {
            self.reposition(false)?;
            let opacity = self.idle_opacity();
            let now = Instant::now();
            if (self.motion.opacity.get(now) - opacity).abs() > 0.01 {
                self.motion
                    .opacity
                    .set(opacity, 300, Ease::Quadratic, 0, now);
                self.start_frames();
            }
        }
        Ok(())
    }
    fn reposition(&mut self, force: bool) -> windows::core::Result<()> {
        // SAFETY: monitor info includes its correct size; HWNDs are queried without dereferencing them.
        unsafe {
            let foreground = GetForegroundWindow();
            let mut monitor = if foreground != self.hwnd && !foreground.is_invalid() {
                MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)
            } else {
                HMONITOR::default()
            };
            if monitor.is_invalid() {
                let mut point = Default::default();
                GetCursorPos(&mut point)?;
                monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            }
            if !force && monitor == self.monitor {
                return Ok(());
            }
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            GetMonitorInfoW(monitor, &mut info.monitorInfo).ok()?;
            let (mut dx, mut dy) = (96, 96);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let scale = f64::from(dx) / 96.0;
            let width = (480.0 * scale).round() as i32;
            let height = (120.0 * scale).round() as i32;
            if self.renderer.width != width || self.renderer.height != height {
                self.renderer = Renderer::new(width, height, scale)?;
                self.draw_failed = false;
            }
            let work = info.monitorInfo.rcWork;
            let x = work.left + (work.right - work.left - width) / 2;
            let y = work.bottom - height + (12.0 * scale).round() as i32;
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
            self.monitor = monitor;
            self.draw(Instant::now())?;
        }
        Ok(())
    }
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: the window's user data is its live boxed RefCell<State>, installed at WM_NCCREATE.
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if msg == WM_MOUSEACTIVATE {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if msg == frame_clock::FRAME && !pointer.is_null() {
            let mut state = (*pointer).borrow_mut();
            if let Err(error) = state.frame() {
                eprintln!("Overlay: {error}");
            }
            state
                .clock
                .as_ref()
                .expect("clock installed after window creation")
                .presented();
            return LRESULT(0);
        }
        if msg == WM_TIMER && !pointer.is_null() {
            let mut state = (*pointer).borrow_mut();
            let result = match w.0 {
                2 => state.housekeeping(),
                3 => {
                    if state.deadline.is_some_and(|d| Instant::now() >= d) {
                        state.show(Pill::Idle)
                    } else {
                        Ok(())
                    }
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                eprintln!("Overlay: {error}");
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}
