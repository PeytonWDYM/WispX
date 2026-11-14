//! Shared Options UI; the host supplies global capture and sign-in startup actions.
mod blur;
mod chart;
mod options;
#[cfg(test)]
mod tests;
pub mod tour;
slint::include_modules!();
pub use options::{DesktopShortcut, ListenAction, Options, OptionsHost};
