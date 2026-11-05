//! WispX: hold a key, speak, and your words appear. See `README.md`.
//!
//! Everything is here apart from the Qt bridge, which only the Linux `wispx-gui`
//! program links. That keeps `wispx` free of native libraries, and lets the desktop
//! interface's logic be tested without a display.

#![deny(unsafe_code)]

pub mod app;
pub mod audio;
pub mod cli;
pub mod config;
pub mod daemon;
#[cfg(unix)]
pub mod dbus;
pub mod desktop;
pub mod doctor;
pub mod hotkey;
pub mod ipc;
pub mod paths;
#[cfg(unix)]
pub mod portal;
pub mod process;
pub mod session;
pub mod shortcuts;
pub mod sounds;
#[cfg(unix)]
pub mod sys;
#[cfg(all(test, unix))]
mod testutil;
pub mod transcribe;
#[cfg(windows)]
pub mod win;

#[cfg(windows)]
pub use win::sys;
