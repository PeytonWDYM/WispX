//! Microphone capture, plus what is shared by whichever way each platform does it:
//! a native tool streaming PCM on Linux, and the waveIn API on Windows.

pub mod level;
pub mod wav;

#[cfg(unix)]
mod tools;

#[cfg(unix)]
pub use tools::*;

#[cfg(windows)]
pub use crate::win::wave::*;
