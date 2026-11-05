//! The desktop app's logic: everything except drawing, which is the Qt bridge in
//! `src/bin/wispx-gui.rs` and `ui/`. Keeping it here means it can be tested without
//! a display, and that `wispx` itself never links Qt.

pub mod controller;
pub mod install;
pub mod instance;
pub mod launch;
pub mod link;
pub mod models;
pub mod overlay;
pub mod view;
