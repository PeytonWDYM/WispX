//! X11 presenter: an override-redirect, input-transparent ARGB window above the
//! work area of the monitor in use. Translucency needs a compositing manager,
//! which Plasma, GNOME and most current X11 desktops run.
use super::{Command, Driver, Failure, Presenter, drive};
use smithay_client_toolkit::reexports::calloop::{EventLoop, channel::Channel};
use x11rb::{
    connection::Connection,
    protocol::{
        randr::ConnectionExt as _,
        shape,
        xfixes::ConnectionExt as _,
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

pub fn run(commands: Channel<Command>, idle_enabled: bool) -> Result<(), Failure> {
    match X::open(idle_enabled) {
        Ok(x) => {
            let event_loop = EventLoop::try_new().map_err(|e| Failure::Stopped(e.to_string()))?;
            drive(event_loop, x, commands).map_err(Failure::Stopped)
        }
        Err(error) => Err(Failure::Unsupported(commands, error.to_string())),
    }
}

struct Atoms {
    active: xproto::Atom,
    state: xproto::Atom,
    fullscreen: xproto::Atom,
    workarea: xproto::Atom,
}
struct X {
    conn: RustConnection,
    root: xproto::Window,
    window: xproto::Window,
    gc: xproto::Gcontext,
    atoms: Atoms,
    driver: Driver,
    monitor: Option<(i16, i16)>,
}
fn atom(conn: &RustConnection, name: &str) -> Result<xproto::Atom, Box<dyn std::error::Error>> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}
impl X {
    fn open(idle_enabled: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let screen = &conn.setup().roots[screen];
        let root = screen.root;
        let visual = screen
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| &d.visuals)
            .find(|v| v.class == xproto::VisualClass::TRUE_COLOR)
            .ok_or("no 32-bit visual for a translucent window")?
            .visual_id;
        let dpi = x11rb::resource_manager::new_from_default(&conn)
            .ok()
            .and_then(|db| db.get_value::<f32>("Xft.dpi", "").ok().flatten())
            .unwrap_or(96.0);
        let driver = Driver::new(idle_enabled, (dpi / 96.0).clamp(1.0, 4.0));
        let (width, height) = driver.renderer.size();
        let colormap = conn.generate_id()?;
        conn.create_colormap(xproto::ColormapAlloc::NONE, colormap, root, visual)?;
        let window = conn.generate_id()?;
        conn.create_window(
            32,
            window,
            root,
            0,
            0,
            width as u16,
            height as u16,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual,
            &xproto::CreateWindowAux::new()
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .override_redirect(1),
        )?;
        let notification = atom(&conn, "_NET_WM_WINDOW_TYPE_NOTIFICATION")?;
        conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atom(&conn, "_NET_WM_WINDOW_TYPE")?,
            xproto::AtomEnum::ATOM,
            &[notification],
        )?;
        conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_CLASS,
            xproto::AtomEnum::STRING,
            b"wispx-pill\0WispX\0",
        )?;
        // An empty input shape lets every click fall through to the window below.
        conn.xfixes_query_version(5, 0)?.reply()?;
        let region = conn.generate_id()?;
        conn.xfixes_create_region(region, &[])?;
        conn.xfixes_set_window_shape_region(window, shape::SK::INPUT, 0, 0, region)?;
        conn.xfixes_destroy_region(region)?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, window, &xproto::CreateGCAux::new())?;
        let atoms = Atoms {
            active: atom(&conn, "_NET_ACTIVE_WINDOW")?,
            state: atom(&conn, "_NET_WM_STATE")?,
            fullscreen: atom(&conn, "_NET_WM_STATE_FULLSCREEN")?,
            workarea: atom(&conn, "_NET_WORKAREA")?,
        };
        conn.map_window(window)?;
        conn.flush()?;
        let mut x = Self {
            conn,
            root,
            window,
            gc,
            atoms,
            driver,
            monitor: None,
        };
        x.place(true);
        Ok(x)
    }
    fn active_window(&self) -> Option<xproto::Window> {
        self.conn
            .get_property(
                false,
                self.root,
                self.atoms.active,
                xproto::AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
            .filter(|&w| w != 0 && w != self.window)
    }
    /// The focused window's center, else the pointer.
    fn focus_point(&self) -> Option<(i16, i16)> {
        let from_window = self.active_window().and_then(|window| {
            let geometry = self.conn.get_geometry(window).ok()?.reply().ok()?;
            let origin = self
                .conn
                .translate_coordinates(window, self.root, 0, 0)
                .ok()?
                .reply()
                .ok()?;
            Some((
                origin.dst_x + (geometry.width / 2) as i16,
                origin.dst_y + (geometry.height / 2) as i16,
            ))
        });
        from_window.or_else(|| {
            let pointer = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
            Some((pointer.root_x, pointer.root_y))
        })
    }
    /// Bottom-center of the monitor in use, 12 px into the work area's bottom margin.
    fn place(&mut self, force: bool) {
        let point = self.focus_point().unwrap_or((0, 0));
        let Some(monitors) = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.monitors)
        else {
            return;
        };
        let contains = |m: &&x11rb::protocol::randr::MonitorInfo| {
            (m.x..m.x + m.width as i16).contains(&point.0)
                && (m.y..m.y + m.height as i16).contains(&point.1)
        };
        let Some(monitor) = monitors
            .iter()
            .find(contains)
            .or_else(|| monitors.iter().find(|m| m.primary))
            .or(monitors.first())
        else {
            return;
        };
        if !force && self.monitor == Some((monitor.x, monitor.y)) {
            return;
        }
        self.monitor = Some((monitor.x, monitor.y));
        let mut bottom = i32::from(monitor.y) + i32::from(monitor.height);
        // _NET_WORKAREA spans all monitors; it only tells us where a bottom panel ends.
        if let Some(area) = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.workarea,
                xproto::AtomEnum::CARDINAL,
                0,
                4,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect::<Vec<_>>()))
            .filter(|v| v.len() == 4)
        {
            let area_bottom = area[1] as i32 + area[3] as i32;
            if area_bottom > i32::from(monitor.y) && area_bottom < bottom {
                bottom = area_bottom;
            }
        }
        let (width, height) = self.driver.renderer.size();
        let scale = self.driver.renderer.scale;
        let x = i32::from(monitor.x) + (i32::from(monitor.width) - width as i32) / 2;
        let y = bottom - height as i32 + (12.0 * scale).round() as i32;
        let _ = self.conn.configure_window(
            self.window,
            &xproto::ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width)
                .height(height)
                .stack_mode(xproto::StackMode::ABOVE),
        );
        let _ = self.conn.flush();
    }
    fn fullscreen_active(&self) -> bool {
        self.active_window()
            .and_then(|window| {
                self.conn
                    .get_property(
                        false,
                        window,
                        self.atoms.state,
                        xproto::AtomEnum::ATOM,
                        0,
                        32,
                    )
                    .ok()?
                    .reply()
                    .ok()
            })
            .and_then(|r| {
                r.value32()
                    .map(|mut v| v.any(|a| a == self.atoms.fullscreen))
            })
            .unwrap_or(false)
    }
}
impl Presenter for X {
    fn driver(&mut self) -> &mut Driver {
        &mut self.driver
    }
    fn present(&mut self) {
        let (width, height) = self.driver.renderer.size();
        let bgra: Vec<u8> = self
            .driver
            .renderer
            .pixmap
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let _ = self.conn.put_image(
            xproto::ImageFormat::Z_PIXMAP,
            self.window,
            self.gc,
            width as u16,
            height as u16,
            0,
            0,
            0,
            32,
            &bgra,
        );
        let _ = self.conn.flush();
        // Nothing is selected, but drain errors so the queue can't grow.
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
    }
    fn reposition(&mut self) {
        self.place(true);
    }
    fn housekeeping(&mut self) {
        let _ = self.conn.configure_window(
            self.window,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        );
        self.driver.scene.fullscreen = self.fullscreen_active();
        if !self.driver.scene.active() {
            self.place(false);
        }
    }
}
