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
    // Each step is the file name and page of one capture; the Account page is shot blurred,
    // then again after the email is revealed.
    const STEPS: [(&str, i32); 7] = [
        ("1-account", 0),
        ("1-account-revealed", 0),
        ("2-shortcut", 1),
        ("3-audio", 2),
        ("4-cleanup", 3),
        ("5-stats", 4),
        ("6-general", 5),
    ];
    let step = Cell::new(0);
    let original_size = Cell::new(ui.window.window().size());
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(600), move || {
        let i = step.get();
        let result = ui
            .window
            .window()
            .take_snapshot()
            .map_err(io::Error::other)
            .and_then(|pixels| {
                let path = dir.join(format!("{}.png", STEPS[i].0));
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
        let Some(&(name, page)) = STEPS.get(i + 1) else {
            let _ = slint::quit_event_loop();
            return;
        };
        step.set(i + 1);
        ui.window.set_email_revealed(name == "1-account-revealed");
        ui.window.set_page(page);
        // Capture the full Stats page, including its reset card below the chart.
        let window = ui.window.window();
        if name == "5-stats" {
            original_size.set(window.size());
            window.set_size(slint::LogicalSize::new(
                original_size.get().width as f32 / window.scale_factor(),
                760.0,
            ));
        } else if name == "6-general" {
            window.set_size(original_size.get());
        }
    });
    Ok(timer)
}
