//! Turning a WAV file into text. There are exactly two backends and no fallback
//! between them: a failing local run never sends audio to the network.

pub mod chatgpt;
pub mod whisper;

use crate::config::{Backend, Config};
use crate::process::Toolbox;
use std::fmt;
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[derive(Debug, PartialEq, Eq)]
pub enum TranscribeError {
    Cancelled,
    Failed(String),
}

impl fmt::Display for TranscribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranscribeError::Cancelled => write!(f, "Cancelled"),
            TranscribeError::Failed(message) => write!(f, "{message}"),
        }
    }
}

/// Transcribes a 16 kHz mono s16 WAV file with the configured backend.
pub fn transcribe(
    config: &Config,
    tools: &Toolbox,
    wav: &Path,
    duration_ms: u64,
    cancel: &AtomicBool,
) -> Result<String, TranscribeError> {
    match config.backend {
        Backend::Local => {
            whisper::transcribe(&config.whisper, &config.language, tools, wav, cancel)
        }
        Backend::ChatGpt => chatgpt::transcribe(
            &config.chatgpt,
            &chatgpt::Endpoint::production(),
            &config.language,
            tools,
            wav,
            duration_ms,
            cancel,
        ),
    }
}

/// A fast check run before the microphone opens, so nobody dictates a whole
/// paragraph into a setup problem. Only inspects the selected backend: local
/// use never reads the Codex login.
pub fn precheck(config: &Config, tools: &Toolbox) -> Result<(), String> {
    match config.backend {
        Backend::Local => whisper::precheck(&config.whisper, tools),
        Backend::ChatGpt => chatgpt::precheck(&config.chatgpt, tools),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    #[test]
    fn local_precheck_and_run_never_touch_auth_or_curl() {
        let temp = TempDir::new();
        let marker = temp.path().join("curl-ran");
        temp.script("curl", &format!("touch {}", marker.display()));
        temp.script("whisper-cli", "echo ' local words'");
        let model = temp.path().join("model.bin");
        fs::write(&model, b"m").unwrap();
        let wav = temp.path().join("in.wav");
        fs::write(&wav, b"w").unwrap();

        // An auth file that would be an error if anything tried to read it.
        let auth = temp.path().join("auth-dir");
        fs::create_dir(&auth).unwrap();

        let mut config = Config::parse("{}").unwrap();
        config.backend = Backend::Local;
        config.whisper.model = model;
        config.chatgpt.auth_file = auth;
        let tools = Toolbox::with_path(temp.path());

        precheck(&config, &tools).unwrap();
        let cancel = AtomicBool::new(false);
        let text = transcribe(&config, &tools, &wav, 1000, &cancel).unwrap();
        assert_eq!(text, "local words");
        assert!(!marker.exists(), "local backend used the network tool");
    }

    #[test]
    fn a_local_failure_does_not_fall_back_to_chatgpt() {
        let temp = TempDir::new();
        let marker = temp.path().join("curl-ran");
        temp.script("curl", &format!("touch {}", marker.display()));
        temp.script("whisper-cli", "echo 'model load failed' >&2\nexit 1");
        let model = temp.path().join("model.bin");
        fs::write(&model, b"m").unwrap();
        let wav = temp.path().join("in.wav");
        fs::write(&wav, b"w").unwrap();
        let auth = temp.path().join("auth.json");
        fs::write(&auth, r#"{"tokens":{"access_token":"tok"}}"#).unwrap();

        let mut config = Config::parse("{}").unwrap();
        config.whisper.model = model;
        config.chatgpt.auth_file = auth;
        let tools = Toolbox::with_path(temp.path());
        let cancel = AtomicBool::new(false);

        let error = transcribe(&config, &tools, &wav, 1000, &cancel).unwrap_err();
        assert!(error.to_string().contains("model load failed"), "{error}");
        assert!(!marker.exists(), "fell back to the network");
    }
}
