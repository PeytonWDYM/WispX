#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("WispX supports Windows and Linux.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
const HELP: &str = "\
WispX: hold a shortcut, talk, let go, and the text is typed where your cursor is.

Usage: WispX [OPTION]

  (none)               start, or open Options in the running WispX
  --background         start without the welcome pill (used at sign-in)
  --quit               ask the running WispX to quit
  --uninstall          quit, then remove the launcher entry, icon and sign-in entry
  --diagnose           report what this desktop supports, without starting WispX
  --export-sounds DIR  write the cue sounds as WAV files
  --options-tour DIR   save screenshots of each Options page
  --pill-tour DIR      paint the pill's states to PNG files, without a display
  --help, --version
";

#[cfg(target_os = "linux")]
fn main() {
    match linux() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("WispX: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "linux")]
fn linux() -> Result<i32, Box<dyn std::error::Error>> {
    use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};
    use wispx::{
        audio::sounds,
        platform::linux::{
            desktop, diagnose,
            instance::{self, Instance, InstanceEvent},
            overlay,
            runtime::{self, Event, Runtime},
        },
        settings::SettingsStore,
        ui::{Options, tour},
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| -> Result<Option<PathBuf>, String> {
        match args.iter().position(|a| a == flag) {
            None => Ok(None),
            Some(i) => args
                .get(i + 1)
                .map(|v| Some(PathBuf::from(v)))
                .ok_or_else(|| format!("{flag} needs a directory")),
        }
    };
    const FLAGS: [&str; 6] = [
        "--background",
        "--quit",
        "--uninstall",
        "--diagnose",
        "--help",
        "--version",
    ];
    const VALUES: [&str; 3] = ["--export-sounds", "--options-tour", "--pill-tour"];
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if VALUES.contains(&arg.as_str()) {
            rest.next();
        } else if !FLAGS.contains(&arg.as_str()) && arg != "-h" && arg != "-V" {
            eprint!("WispX: unknown option {arg}\n\n{HELP}");
            return Ok(2);
        }
    }
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("--help") || has("-h") {
        print!("{HELP}");
        return Ok(0);
    }
    if has("--version") || has("-V") {
        println!("WispX {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if has("--diagnose") {
        return Ok(diagnose::run());
    }
    if let Some(dir) = value("--export-sounds")? {
        sounds::export(&dir)?;
        return Ok(0);
    }
    if let Some(dir) = value("--pill-tour")? {
        overlay::tour(&dir)?;
        return Ok(0);
    }
    if has("--quit") || has("--uninstall") {
        let running = instance::quit_running(&instance::runtime_dir()?)?;
        if has("--uninstall") {
            for path in desktop::remove()? {
                println!("removed {}", path.display());
            }
            println!("Settings in ~/.config/WispX and your Codex login are untouched.");
        } else if !running {
            println!("WispX isn't running.");
        }
        return Ok(0);
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    if let Some(dir) = value("--options-tour")? {
        let ui = Options::new(Rc::new(RefCell::new(SettingsStore::load()?)), None, true)?;
        ui.show()?;
        let _timer = tour::start(ui, dir)?;
        slint::run_event_loop_until_quit()?;
        return Ok(0);
    }
    let Some(_instance) = Instance::acquire(
        &instance::runtime_dir()?,
        InstanceEvent::Wake,
        Arc::new(|event| runtime::post(Event::Instance(event))),
    )?
    else {
        return Ok(0);
    };
    // Portals identify WispX by this entry, so Wayland shortcuts depend on it.
    if let Err(error) = desktop::integrate() {
        eprintln!("Desktop entry: {error}");
    }
    let app = Runtime::start(SettingsStore::load()?, has("--background"))?;
    let result = slint::run_event_loop_until_quit();
    Runtime::shutdown();
    drop(app);
    result?;
    Ok(0)
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
    if let Some(request) = value("--context-probe") {
        return wispx::platform::windows::context_probe::run(&request);
    }
    if let Some(out) = value("--cleanup-probe") {
        return wispx::platform::windows::context_probe::cleanup(&out);
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
