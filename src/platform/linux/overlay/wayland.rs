//! Layer-shell presenter: a surface above windows (and above fullscreen apps only
//! while active), anchored bottom-center, with an empty input region and no
//! keyboard interactivity so it can never take focus from the paste target.
use super::{Command, Driver, Failure, Presenter, drive};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{EventLoop, channel::Channel},
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_output, wl_shm, wl_surface},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::time::Instant;

pub fn run(commands: Channel<Command>, idle_enabled: bool) -> Result<(), Failure> {
    let fail = |commands, error: &dyn std::fmt::Display| {
        Err(Failure::Unsupported(commands, error.to_string()))
    };
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(error) => return fail(commands, &error),
    };
    let (globals, queue) = match registry_queue_init::<Wl>(&conn) {
        Ok(pair) => pair,
        Err(error) => return fail(commands, &error),
    };
    let qh = queue.handle();
    let (compositor, shell, shm) = match (
        CompositorState::bind(&globals, &qh),
        LayerShell::bind(&globals, &qh),
        Shm::bind(&globals, &qh),
    ) {
        (Ok(c), Ok(l), Ok(s)) => (c, l, s),
        (_, Err(error), _) => return fail(commands, &format!("no layer shell ({error})")),
        _ => return fail(commands, &"missing wl_compositor or wl_shm"),
    };
    let pool = match SlotPool::new(480 * 120 * 4 * 4, &shm) {
        Ok(pool) => pool,
        Err(error) => return fail(commands, &error),
    };
    let event_loop: EventLoop<'static, Wl> = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => return fail(commands, &error),
    };
    if let Err(error) = WaylandSource::new(conn, queue).insert(event_loop.handle()) {
        return fail(commands, &error);
    }
    let mut wl = Wl {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        shell,
        shm,
        pool,
        layer: None,
        layer_kind: Layer::Top,
        configured: false,
        qh,
        driver: Driver::new(idle_enabled, 1.0),
    };
    wl.create_layer();
    drive(event_loop, wl, commands).map_err(Failure::Stopped)
}

struct Wl {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    layer_kind: Layer,
    configured: bool,
    qh: QueueHandle<Self>,
    driver: Driver,
}
impl Wl {
    fn create_layer(&mut self) {
        let surface = self.compositor.create_surface(&self.qh);
        // No output: the compositor places it on the active one, as Windows follows focus.
        let layer = self.shell.create_layer_surface(
            &self.qh,
            surface,
            self.layer_kind,
            Some("wispx-pill"),
            None,
        );
        layer.set_anchor(Anchor::BOTTOM);
        layer.set_size(super::render::WIDTH as u32, super::render::HEIGHT as u32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(0);
        if let Ok(region) = Region::new(&self.compositor) {
            layer
                .wl_surface()
                .set_input_region(Some(region.wl_region()));
        }
        layer.commit();
        self.layer = Some(layer);
        self.configured = false;
    }
}
impl Presenter for Wl {
    fn driver(&mut self) -> &mut Driver {
        &mut self.driver
    }
    fn present(&mut self) {
        let Some(layer) = &self.layer else { return };
        if !self.configured {
            return;
        }
        // Idle sits in the top layer, beneath fullscreen apps; activity rises above them.
        let wanted = if self.driver.scene.active() {
            Layer::Overlay
        } else {
            Layer::Top
        };
        if wanted != self.layer_kind {
            layer.set_layer(wanted);
            self.layer_kind = wanted;
        }
        let (width, height) = self.driver.renderer.size();
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) else {
            return;
        };
        // tiny-skia is RGBA; ARGB8888 is BGRA in little-endian memory. Both premultiplied.
        let pixels = self.driver.renderer.pixmap.data().as_chunks::<4>().0;
        for (dst, src) in canvas.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
            *dst = [src[2], src[1], src[0], src[3]];
        }
        let surface = layer.wl_surface();
        surface.set_buffer_scale(self.driver.renderer.scale.round() as i32);
        if buffer.attach_to(surface).is_ok() {
            surface.damage_buffer(0, 0, width as i32, height as i32);
            layer.commit();
        }
    }
    fn reposition(&mut self) {
        if self.outputs.outputs().nth(1).is_some() {
            self.layer.take();
            self.create_layer();
        }
    }
}
impl CompositorHandler for Wl {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        self.driver.renderer.set_scale(factor.max(1) as f32);
        self.driver.paint(Instant::now());
        self.present();
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Wl {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl LayerShellHandler for Wl {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // The compositor removed it, e.g. with its output; make a new one.
        self.layer.take();
        self.create_layer();
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configured = true;
        self.driver.paint(Instant::now());
        self.present();
    }
}
impl ShmHandler for Wl {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for Wl {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
delegate_compositor!(Wl);
delegate_output!(Wl);
delegate_layer!(Wl);
delegate_shm!(Wl);
delegate_registry!(Wl);
