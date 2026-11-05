//! WispX: hold a key, speak, and your words appear. See `README.md`.

#![deny(unsafe_code)]

mod audio;
mod cli;
mod config;
mod daemon;
mod dbus;
mod desktop;
mod doctor;
mod ipc;
mod paths;
mod portal;
mod process;
mod session;
mod shortcuts;
mod sounds;
mod sys;
#[cfg(test)]
mod testutil;
mod transcribe;

fn main() {
    std::process::exit(cli::main());
}
