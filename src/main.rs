//! The `wispx` command: the command line and the background daemon.

#![deny(unsafe_code)]

fn main() {
    std::process::exit(wispx::cli::main());
}
