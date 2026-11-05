//! The desktop app: Qt on Linux and plain Win32 on Windows. Both only draw, and
//! leave everything else to `wispx::app::controller`.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(unix)]
#[path = "../gui_qt.rs"]
mod qt;

fn main() {
    #[cfg(unix)]
    std::process::exit(qt::run());
    #[cfg(windows)]
    std::process::exit(wispx::win::gui::run());
}
