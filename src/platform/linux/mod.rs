//! Linux desktop host: X11 and Wayland sessions, with Plasma as the primary target.
pub mod clipboard;
pub mod desktop;
pub mod diagnose;
pub mod duck;
pub mod hotkey;
pub mod instance;
pub mod kwin;
pub mod notify;
pub mod overlay;
pub mod paste;
pub mod runtime;
pub mod session;
pub mod sound;
pub mod tray;

/// The application ID portals use to identify WispX by its desktop entry.
pub const APP_ID: &str = "io.github.PeytonWDYM.WispX";

/// Current portals refuse shortcuts and remote control to unregistered host apps.
/// Registration must precede other calls on ashpd's shared connection, so every
/// portal thread calls this first; the first does the work and the rest wait.
/// A running portal can take a moment to notice a desktop entry written just now.
pub fn register_app() {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        let Ok(id) = ashpd::AppID::try_from(APP_ID) else {
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match futures_lite::future::block_on(ashpd::register_host_app(id.clone())) {
                Ok(()) => return,
                Err(error) if std::time::Instant::now() >= deadline => {
                    eprintln!("Portal registration: {error}");
                    return;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(250)),
            }
        }
    });
}
