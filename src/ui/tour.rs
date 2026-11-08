//! Read-only screenshots in a process without installation, tray or global hooks.
use super::Options;
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
    fs::create_dir_all(&dir)?;
    let page = Cell::new(0);
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
                    ["account", "shortcut", "audio", "general"][i]
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
        if i == 3 {
            let _ = slint::quit_event_loop();
        } else {
            page.set(i + 1);
            ui.window.set_page(i as i32 + 1);
        }
    });
    Ok(timer)
}
