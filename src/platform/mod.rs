//! Desktop integration. Cargo's target selects exactly one host implementation.
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod windows;
