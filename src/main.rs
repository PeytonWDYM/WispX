#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    println!("WispX supports Windows only for now.");
}

#[cfg(windows)]
fn main() {
    if let Err(error) = run() {
        eprintln!("WispX: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};
    use wispx::{
        audio::sounds,
        platform::windows::{
            installer,
            instance::Instance,
            runtime::{self, Event, Runtime},
            selftest,
        },
        settings::SettingsStore,
        ui::{Options, tour},
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let value = |flag: &str| -> Option<PathBuf> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    };
    if let Some(dir) = value("--export-sounds") {
        sounds::export(&dir)?;
        return Ok(());
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    if let Some(dir) = value("--options-tour") {
        let ui = Options::new(Rc::new(RefCell::new(SettingsStore::load()?)), None, true)?;
        ui.show()?;
        let _timer = tour::start(ui, dir)?;
        slint::run_event_loop_until_quit()?;
        return Ok(());
    }
    if let Some(wav) = value("--selftest") {
        let index = args
            .iter()
            .position(|a| a == "--selftest")
            .expect("flag found");
        let out = args
            .get(index + 2)
            .ok_or("--selftest requires a WAV and log path")?;
        return selftest::run(
            wav,
            PathBuf::from(out),
            SettingsStore::load()?.value.language,
        );
    }
    let portable = args.iter().any(|a| a == "--portable");
    if installer::ensure_installed(portable)? {
        return Ok(());
    }
    let Some(_instance) =
        Instance::acquire(Arc::new(|event| runtime::post(Event::Instance(event))))?
    else {
        return Ok(());
    };
    let app = Runtime::start(
        SettingsStore::load()?,
        args.iter()
            .any(|a| a.to_string_lossy().eq_ignore_ascii_case("--background")),
    )?;
    let result = slint::run_event_loop_until_quit();
    Runtime::shutdown();
    drop(app);
    result?;
    Ok(())
}
