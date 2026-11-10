//! Shared Options UI; the host supplies global capture and sign-in startup actions.
mod chart;
mod options;
pub mod tour;
slint::include_modules!();
pub use options::{DesktopShortcut, Options, OptionsHost};
