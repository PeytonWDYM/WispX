//! Read-only screenshots in a process without installation, tray or global hooks.
use super::{Options, Palette};
use slint::{ComponentHandle, Timer, TimerMode};
use std::{
    cell::Cell,
    fs::{self, File},
    io,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

pub fn start(ui: Rc<Options>, dir: PathBuf) -> io::Result<Timer> {
    // Fixture-only palette selection leaves the desktop's own theme untouched.
    match std::env::var("WISPX_TOUR_THEME").as_deref() {
        Ok("light") => ui
            .window
            .global::<Palette>()
            .set_color_scheme(slint::language::ColorScheme::Light),
        Ok("dark") => ui
            .window
            .global::<Palette>()
            .set_color_scheme(slint::language::ColorScheme::Dark),
        _ => {}
    }
    fs::create_dir_all(&dir)?;
    let page = Cell::new(0);
    let original_size = Cell::new(ui.window.window().size());
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(600), move || {
        let i = page.get();
        let result = ui
            .window
            .window()
            .take_snapshot()
            .map_err(io::Error::other)
            .and_then(|pixels| {
                let path = dir.join(format!(
                    "{}-{}.png",
                    i + 1,
                    [
                        "account", "shortcut", "audio", "cleanup", "stats", "general"
                    ][i]
                ));
                let mut encoder =
                    png::Encoder::new(File::create(path)?, pixels.width(), pixels.height());
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()?
                    .write_image_data(pixels.as_bytes())?;
                Ok(())
            });
        if let Err(error) = result {
            eprintln!("Options tour: {error}");
            let _ = slint::quit_event_loop();
            return;
        }
        if i == 5 {
            let _ = slint::quit_event_loop();
        } else {
            page.set(i + 1);
            ui.window.set_page(i as i32 + 1);
            // Capture the full Stats page, including its reset card below the chart.
            if i == 3 {
                original_size.set(ui.window.window().size());
                ui.window.window().set_size(slint::LogicalSize::new(
                    original_size.get().width as f32 / ui.window.window().scale_factor(),
                    760.0,
                ));
            } else if i == 4 {
                ui.window.window().set_size(original_size.get());
            }
        }
    });
    Ok(timer)
}
