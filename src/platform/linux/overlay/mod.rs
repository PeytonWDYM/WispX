//! The non-activating pill on its own thread. Plasma and other wlroots-style
//! compositors get a layer-shell surface; X11 (and XWayland as a fallback) gets an
//! override-redirect ARGB window. Neither takes focus or accepts input.
mod render;
pub mod scene;
mod wayland;
mod x11;

use super::session::{Protocol, Session};
use crate::app::Pill;
use render::Renderer;
use scene::Scene;
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, LoopHandle,
    channel::{self, Channel, Sender},
    timer::{TimeoutAction, Timer},
};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

const FRAME: Duration = Duration::from_millis(16);
const HOUSEKEEPING: Duration = Duration::from_millis(1500);

pub enum Failure {
    /// This display can't host the presenter; the commands go to the next one.
    Unsupported(Channel<Command>, String),
    Stopped(String),
}
pub enum Command {
    Show(Pill),
    IdleEnabled(bool),
    /// Writes the next painted frame as PNG, for repeatable verification.
    Snapshot(PathBuf, mpsc::SyncSender<std::io::Result<()>>),
}
pub struct Overlay {
    commands: Sender<Command>,
}
impl Overlay {
    pub fn new(idle_enabled: bool) -> Self {
        let (commands, channel) = channel::channel();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || {
                let session = Session::detect();
                let channel = match session.protocol {
                    Protocol::Wayland => match wayland::run(channel, idle_enabled) {
                        Ok(()) => return,
                        // GNOME has no layer shell; an XWayland window still shows the pill.
                        Err(Failure::Unsupported(channel, error)) => {
                            eprintln!("Overlay: {error}; trying XWayland");
                            channel
                        }
                        Err(Failure::Stopped(error)) => {
                            eprintln!("Overlay: {error}");
                            return;
                        }
                    },
                    _ => channel,
                };
                match x11::run(channel, idle_enabled) {
                    Ok(()) => {}
                    Err(Failure::Unsupported(_, error) | Failure::Stopped(error)) => {
                        eprintln!("Overlay unavailable: {error}");
                    }
                }
            })
            .expect("spawn overlay thread");
        Self { commands }
    }
    pub fn show(&self, pill: Pill) -> Result<(), String> {
        self.commands
            .send(Command::Show(pill))
            .map_err(|_| "the overlay stopped".to_owned())
    }
    pub fn set_idle_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::IdleEnabled(enabled));
    }
    pub fn snapshot(&self, path: PathBuf) -> std::io::Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Snapshot(path, tx))
            .map_err(|_| std::io::Error::other("the overlay stopped"))?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| std::io::Error::other("the overlay didn't paint"))?
    }
}

/// Protocol-neutral state every presenter drives.
pub struct Driver {
    pub scene: Scene,
    pub renderer: Renderer,
    ticking: bool,
    closed: bool,
    snapshots: Vec<(PathBuf, mpsc::SyncSender<std::io::Result<()>>)>,
}
impl Driver {
    pub fn new(idle_enabled: bool, scale: f32) -> Self {
        Self {
            scene: Scene::new(idle_enabled),
            renderer: Renderer::new(scale),
            ticking: false,
            closed: false,
            snapshots: Vec::new(),
        }
    }
    pub fn paint(&mut self, now: Instant) {
        self.renderer.draw(&self.scene, now);
        for (path, reply) in self.snapshots.drain(..) {
            let _ = reply.send(save_png(&self.renderer.pixmap, &path));
        }
    }
    /// Advances one frame; returns when the next one is due, if any.
    fn step(&mut self, now: Instant) -> Option<Duration> {
        let animating = self.scene.advance(now);
        if animating {
            Some(FRAME)
        } else {
            self.scene
                .deadline()
                .map(|d| d.saturating_duration_since(now).max(FRAME))
        }
    }
}
pub trait Presenter: Sized + 'static {
    fn driver(&mut self) -> &mut Driver;
    /// Pushes the painted pixmap to the screen.
    fn present(&mut self);
    /// Moves the pill to the monitor in use, before a recording or message.
    fn reposition(&mut self);
    /// Periodic checks: fullscreen apps, layer and stacking order.
    fn housekeeping(&mut self) {}
}

fn frame<P: Presenter>(presenter: &mut P) -> Option<Duration> {
    let now = Instant::now();
    let next = presenter.driver().step(now);
    presenter.driver().paint(now);
    presenter.present();
    next
}
fn start_ticking<P: Presenter>(handle: &LoopHandle<'static, P>, presenter: &mut P) {
    if presenter.driver().ticking {
        return;
    }
    presenter.driver().ticking = true;
    let inserted =
        handle.insert_source(Timer::immediate(), |_, _, presenter: &mut P| {
            match frame(presenter) {
                Some(next) => TimeoutAction::ToDuration(next),
                None => {
                    presenter.driver().ticking = false;
                    TimeoutAction::Drop
                }
            }
        });
    if inserted.is_err() {
        presenter.driver().ticking = false;
    }
}
/// Runs the presenter's calloop until the command channel closes.
pub fn drive<P: Presenter>(
    mut event_loop: EventLoop<'static, P>,
    mut presenter: P,
    commands: Channel<Command>,
) -> Result<(), String> {
    let handle = event_loop.handle();
    let command_handle = handle.clone();
    handle
        .insert_source(commands, move |event, _, presenter: &mut P| {
            let channel::Event::Msg(command) = event else {
                // Every sender dropped: the app is exiting.
                presenter.driver().closed = true;
                return;
            };
            let now = Instant::now();
            match command {
                Command::Show(pill) => {
                    let driver = presenter.driver();
                    let renderer = &driver.renderer;
                    let reposition = driver.scene.show(pill, now, |t| renderer.measure(t));
                    if reposition {
                        presenter.reposition();
                    }
                }
                Command::IdleEnabled(enabled) => {
                    presenter.driver().scene.idle_enabled = enabled;
                    presenter.driver().scene.refresh_idle(now);
                }
                Command::Snapshot(path, reply) => presenter.driver().snapshots.push((path, reply)),
            }
            start_ticking(&command_handle, presenter);
        })
        .map_err(|e| e.to_string())?;
    let housekeeping_handle = handle.clone();
    handle
        .insert_source(
            Timer::from_duration(HOUSEKEEPING),
            move |_, _, presenter: &mut P| {
                presenter.housekeeping();
                presenter.driver().scene.refresh_idle(Instant::now());
                start_ticking(&housekeeping_handle, presenter);
                TimeoutAction::ToDuration(HOUSEKEEPING)
            },
        )
        .map_err(|e| e.to_string())?;
    start_ticking(&handle, &mut presenter);
    while !presenter.driver().closed {
        event_loop
            .dispatch(None, &mut presenter)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Paints each pill state offscreen at 1x and 2x, for checking the painter without a display.
pub fn tour(dir: &std::path::Path) -> std::io::Result<()> {
    use crate::{app::MessageKind, audio::recorder::Levels};
    std::fs::create_dir_all(dir)?;
    for scale in [1.0, 2.0] {
        let mut driver = Driver::new(true, scale);
        let mut now = Instant::now();
        let states = [
            ("idle", Pill::Idle),
            ("recording", Pill::Recording(Levels::default())),
            ("locked", Pill::Locked),
            ("transcribing", Pill::Transcribing),
            (
                "message",
                Pill::message("Hold Ctrl + Win to dictate", MessageKind::Info, 9.0),
            ),
            (
                "warning",
                Pill::message(
                    "Didn't hear anything. Check your mic?",
                    MessageKind::Warn,
                    9.0,
                ),
            ),
            (
                "error",
                Pill::message(
                    "A deliberately long error message that must be trimmed with an ellipsis",
                    MessageKind::Error,
                    9.0,
                ),
            ),
        ];
        for (name, pill) in states {
            let renderer = &driver.renderer;
            driver.scene.show(pill, now, |t| renderer.measure(t));
            // Let every transition settle on a simulated 60 Hz clock.
            for _ in 0..60 {
                now += FRAME;
                driver.scene.advance(now);
            }
            driver.renderer.draw(&driver.scene, now);
            save_png(
                &driver.renderer.pixmap,
                &dir.join(format!("pill-{name}@{scale}x.png")),
            )?;
        }
    }
    Ok(())
}

/// Straight-alpha PNG of a premultiplied pixmap.
fn save_png(pixmap: &tiny_skia::Pixmap, path: &std::path::Path) -> std::io::Result<()> {
    let rgba: Vec<u8> = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    let mut encoder = png::Encoder::new(
        std::fs::File::create(path)?,
        pixmap.width(),
        pixmap.height(),
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(())
}
