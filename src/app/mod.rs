//! The desktop app's logic: everything except drawing, which is the Qt bridge in
//! `src/gui_qt.rs` and `ui/` on Linux and plain Win32 in `src/win/gui` on Windows.
//! Keeping it here means it can be tested without a display, and that `wispx`
//! itself never links Qt.

pub mod controller;
#[cfg(unix)]
pub mod install;
pub mod instance;
pub mod launch;
pub mod link;
pub mod models;
pub mod overlay;
pub mod pill;
pub mod sha256;
pub mod view;

#[cfg(windows)]
pub use crate::win::install;
