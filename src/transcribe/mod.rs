pub mod auth;
pub mod chatgpt;
mod errors;
mod retry;
pub use chatgpt::{ChatGpt, TranscribeError};
