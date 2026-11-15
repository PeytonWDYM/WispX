pub mod capture;
pub mod duck;
pub mod opus;
#[cfg(target_os = "linux")]
mod pulse;
pub mod recorder;
mod resampler;
pub mod sounds;
pub mod wav;
mod webm;
