//! Everything only Windows has. As on Linux the program links no GUI toolkit and has
//! one crate for a dependency, so the system's own libraries are declared by hand
//! in `ffi`, and this is the one other place that uses `unsafe`.
//!
//! The logic is shared with Linux. All this does is reach the system: the microphone,
//! the keyboard, the clipboard, named pipes, and windows.

#![allow(unsafe_code)]

pub mod clipboard;
mod ffi;
pub mod gui;
pub mod hook;
pub mod input;
pub mod install;
pub mod pipe;
pub mod security;
pub mod shell;
pub mod sound;
pub mod sys;
pub mod wave;
