//! `WispX --diagnose`: what this desktop offers WispX, without starting it,
//! touching the clipboard, or printing tokens, transcripts or settings values.
use super::{
    APP_ID, clipboard, desktop,
    paste::Paster,
    session::{Protocol, Session},
};
use crate::{audio::recorder, settings::SettingsStore, transcribe::auth};
use cpal::traits::{DeviceTrait, HostTrait};
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};

struct Registry;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Registry {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
fn wayland_globals() -> Result<Vec<String>, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, _) = registry_queue_init::<Registry>(&conn).map_err(|e| e.to_string())?;
    Ok(globals
        .contents()
        .with_list(|list| list.iter().map(|g| g.interface.clone()).collect()))
}
fn has_owner(bus: &zbus::blocking::Connection, name: &str) -> bool {
    zbus::blocking::fdo::DBusProxy::new(bus)
        .ok()
        .and_then(|proxy| proxy.name_has_owner(name.try_into().ok()?).ok())
        .unwrap_or(false)
}
fn portal_version(bus: &zbus::blocking::Connection, interface: &str) -> Option<u32> {
    let proxy = zbus::blocking::fdo::PropertiesProxy::builder(bus)
        .destination("org.freedesktop.portal.Desktop")
        .ok()?
        .path("/org/freedesktop/portal/desktop")
        .ok()?
        .build()
        .ok()?;
    let value = proxy.get(interface.try_into().ok()?, "version").ok()?;
    u32::try_from(&*value).ok()
}
fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub fn run() -> i32 {
    let session = Session::detect();
    println!(
        "WispX {} for {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    );
    println!(
        "session: {:?}, desktop: {}",
        session.protocol,
        if session.desktop.is_empty() {
            "unknown"
        } else {
            &session.desktop
        }
    );
    match SettingsStore::load() {
        Ok(_) => println!("settings: readable (defaults apply to anything missing)"),
        Err(error) => println!("settings: unavailable ({error})"),
    }
    let login = auth::login();
    println!(
        "codex login: {}",
        match login.as_ref().map(|l| l.mode) {
            Some(auth::AuthMode::ChatGpt) => "ChatGPT (transcription and cleanup)",
            Some(auth::AuthMode::ApiKey) => "API key (transcription only; cleanup needs ChatGPT)",
            Some(auth::AuthMode::Unknown) => "present, unknown mode",
            None => "missing: sign in to Codex (~/.codex/auth.json)",
        }
    );
    match recorder::input_devices() {
        Ok(devices) => println!("microphones: {}", devices.len()),
        Err(error) => println!("microphones: unavailable ({error})"),
    }
    let output = cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok());
    println!("sound output: {}", output.as_deref().unwrap_or("none"));
    if session.protocol == Protocol::Wayland {
        match wayland_globals() {
            Ok(globals) => {
                let has = |name: &str| globals.iter().any(|g| g == name);
                println!(
                    "pill: {}",
                    if has("zwlr_layer_shell_v1") {
                        "layer shell"
                    } else {
                        "XWayland window (no layer shell)"
                    }
                );
                println!(
                    "clipboard data control: {}",
                    yes(has("ext_data_control_manager_v1") || has("zwlr_data_control_manager_v1"))
                );
            }
            Err(error) => println!("wayland: {error}"),
        }
    }
    if session.protocol != Protocol::None {
        println!("clipboard: {:?}", clipboard::backend());
    }
    match zbus::blocking::Connection::session() {
        Ok(bus) => {
            let portal = |interface: &str| {
                portal_version(&bus, interface).map_or("missing".to_owned(), |v| format!("v{v}"))
            };
            println!(
                "portals: GlobalShortcuts {}, RemoteDesktop {}",
                portal("org.freedesktop.portal.GlobalShortcuts"),
                portal("org.freedesktop.portal.RemoteDesktop")
            );
            println!(
                "tray host: {}",
                yes(has_owner(&bus, "org.kde.StatusNotifierWatcher"))
            );
            println!("KWin scripting: {}", yes(has_owner(&bus, "org.kde.KWin")));
            println!(
                "notifications: {}",
                yes(has_owner(&bus, "org.freedesktop.Notifications"))
            );
        }
        Err(error) => println!("session bus: unavailable ({error})"),
    }
    if session.protocol == Protocol::X11 {
        println!(
            "shortcuts: XInput2 raw keys with grabs; paste: {}",
            Paster::start(&session).describe()
        );
    } else if session.protocol == Protocol::Wayland {
        println!(
            "shortcuts: GlobalShortcuts portal; paste: {}",
            if session.kde() {
                "RemoteDesktop portal with KWin focus checks"
            } else {
                "copy only (focus can't be verified on this compositor)"
            }
        );
    }
    let exists = |path: std::io::Result<std::path::PathBuf>| path.is_ok_and(|p| p.exists());
    println!(
        "desktop entry ({APP_ID}): {}, start at sign-in: {}",
        yes(exists(desktop::entry_path())),
        yes(desktop::startup_enabled())
    );
    0
}
