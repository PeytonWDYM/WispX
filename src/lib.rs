//! WispX: hold a key, speak, and your words appear. See `README.md`.
//!
//! Everything is here apart from the Qt bridge, which only the `wispx-gui` program
//! links. That keeps `wispx` free of native libraries, and lets the desktop
//! interface's logic be tested without a display.

#![deny(unsafe_code)]

pub mod app;
pub mod audio;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod dbus;
pub mod desktop;
pub mod doctor;
pub mod ipc;
pub mod paths;
pub mod portal;
pub mod process;
pub mod session;
pub mod shortcuts;
pub mod sounds;
pub mod sys;
#[cfg(test)]
mod testutil;
pub mod transcribe;
