//! Typed endpoint errors preserve server messages for unknown and legacy replies.

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("Sign in to the Codex app first")]
    NoLogin,
    #[error("Codex login expired. Open Codex once")]
    LoginExpired,
    #[error("Sign in to Codex again")]
    LoginRevoked,
    #[error("Unexpected reply from ChatGPT")]
    UnexpectedReply,
    #[error("Too short. Hold a bit longer")]
    TooShort,
    #[error("ChatGPT refused the request (403)")]
    Refused,
    #[error("Recording too long")]
    TooLong,
    #[error("ChatGPT couldn't read the recording")]
    UnsupportedAudio,
    #[error("ChatGPT rate limit. Try again soon")]
    RateLimit,
    #[error("ChatGPT is busy. Try again soon")]
    Overloaded,
    #[error("No connection to ChatGPT")]
    Connection,
    #[error("ChatGPT took too long")]
    Timeout,
    #[error("Couldn't encode recording: {0}")]
    Encoding(#[from] crate::audio::opus::EncodeError),
    #[error("{message}")]
    Server { message: String },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    AudioTooShort,
    FileAboveMaxSize,
    UnsupportedFileMimetype,
    RateLimitExceeded,
    TokenExpired,
    TokenRevoked,
    ModelOverloaded,
    #[serde(other)]
    Unknown,
}

#[derive(Default, Deserialize)]
pub(super) struct ServerError {
    code: Option<ErrorCode>,
    message: Option<String>,
}

impl ServerError {
    pub(super) fn parse(body: &str) -> Self {
        #[derive(Deserialize)]
        struct Reply {
            detail: Detail,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Detail {
            Structured(ServerError),
            Legacy(String),
        }
        match serde_json::from_str::<Reply>(body) {
            Ok(Reply {
                detail: Detail::Structured(error),
            }) => error,
            Ok(Reply {
                detail: Detail::Legacy(message),
            }) => Self {
                code: None,
                message: Some(message),
            },
            Err(_) => Self::default(),
        }
    }

    pub(super) fn needs_refresh(&self, status: u16) -> bool {
        self.code != Some(ErrorCode::TokenRevoked)
            && (status == 401 || self.code == Some(ErrorCode::TokenExpired))
    }

    pub(super) fn into_error(self, status: u16) -> TranscribeError {
        match (self.code, status) {
            (Some(ErrorCode::AudioTooShort), _) => TranscribeError::TooShort,
            (Some(ErrorCode::FileAboveMaxSize), _) => TranscribeError::TooLong,
            (Some(ErrorCode::UnsupportedFileMimetype), _) => TranscribeError::UnsupportedAudio,
            (Some(ErrorCode::RateLimitExceeded), _) => TranscribeError::RateLimit,
            (Some(ErrorCode::TokenExpired), _) => TranscribeError::LoginExpired,
            (Some(ErrorCode::TokenRevoked), _) => TranscribeError::LoginRevoked,
            (Some(ErrorCode::ModelOverloaded), _) => TranscribeError::Overloaded,
            (None, 401) => TranscribeError::LoginExpired,
            (None, 403) => TranscribeError::Refused,
            (None, 413) => TranscribeError::TooLong,
            (None, 429) => TranscribeError::RateLimit,
            _ => TranscribeError::Server {
                message: self
                    .message
                    .filter(|message| !message.trim().is_empty())
                    .unwrap_or_else(|| format!("ChatGPT error {status}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    // Failure cases: structured codes are confused with message text; legacy,
    // unknown, absent, or malformed details lose the useful fallback; expired
    // tokens fail to refresh, or revoked tokens keep trying to refresh.
    use super::*;

    #[test]
    fn known_codes_take_precedence_over_status_and_message() {
        for (code, expected) in [
            ("audio_too_short", "Too short. Hold a bit longer"),
            ("file_above_max_size", "Recording too long"),
            (
                "unsupported_file_mimetype",
                "ChatGPT couldn't read the recording",
            ),
            ("rate_limit_exceeded", "ChatGPT rate limit. Try again soon"),
            ("token_expired", "Codex login expired. Open Codex once"),
            ("token_revoked", "Sign in to Codex again"),
            ("model_overloaded", "ChatGPT is busy. Try again soon"),
        ] {
            let body =
                serde_json::json!({"detail": {"code": code, "message": "ignored"}}).to_string();
            assert_eq!(
                ServerError::parse(&body).into_error(400).to_string(),
                expected
            );
        }
    }

    #[test]
    fn unknown_and_legacy_errors_keep_messages_without_text_matching() {
        for (body, expected) in [
            (
                r#"{"detail":{"code":"new_code","message":"Try a different clip"}}"#,
                "Try a different clip",
            ),
            (r#"{"detail":"audio too short"}"#, "audio too short"),
            (r#"{"detail":{"code":"new_code"}}"#, "ChatGPT error 400"),
            (r#"{"detail":{"message":""}}"#, "ChatGPT error 400"),
            (r#"{"detail":null}"#, "ChatGPT error 400"),
            ("<html>Bad gateway</html>", "ChatGPT error 400"),
        ] {
            assert_eq!(
                ServerError::parse(body).into_error(400).to_string(),
                expected
            );
        }
        for (status, message) in [
            (401, "Codex login expired. Open Codex once"),
            (403, "ChatGPT refused the request (403)"),
            (413, "Recording too long"),
            (429, "ChatGPT rate limit. Try again soon"),
        ] {
            assert_eq!(
                ServerError::parse("").into_error(status).to_string(),
                message
            );
        }
    }

    #[test]
    fn refreshes_expired_tokens_and_unauthorized_but_not_revoked_tokens() {
        assert!(ServerError::parse("").needs_refresh(401));
        assert!(!ServerError::parse("").needs_refresh(403));
        assert!(ServerError::parse(r#"{"detail":{"code":"token_expired"}}"#).needs_refresh(400));
        assert!(!ServerError::parse(r#"{"detail":{"code":"token_revoked"}}"#).needs_refresh(401));
    }
}
