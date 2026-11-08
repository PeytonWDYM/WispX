//! Shared Options UI; the host supplies global capture and Windows startup actions.
mod options;
pub mod tour;
slint::include_modules!();
pub use options::{Options, OptionsHost};
