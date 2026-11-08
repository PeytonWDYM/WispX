//! Blocking HTTP work belongs on worker threads, never the UI or keyboard hook.
use super::auth;
use serde_json::Value;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use ureq::{
    Agent,
    tls::{RootCerts, TlsConfig},
};

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("Sign in to the Codex app first")]
    NoLogin,
    #[error("Codex login expired. Open Codex once")]
    LoginExpired,
    #[error("Unexpected reply from ChatGPT")]
    UnexpectedReply,
    #[error("Too short. Hold a bit longer")]
    TooShort,
    #[error("ChatGPT refused the request (403)")]
    Refused,
    #[error("Recording too long")]
    TooLong,
    #[error("ChatGPT rate limit. Try again soon")]
    RateLimit,
    #[error("No connection to ChatGPT")]
    Connection,
    #[error("ChatGPT took too long")]
    Timeout,
    #[error("ChatGPT error {status}{detail}")]
    Server { status: u16, detail: String },
}
#[derive(Clone)]
pub struct ChatGpt {
    pub(crate) agent: Agent,
}
impl Default for ChatGpt {
    fn default() -> Self {
        Self {
            agent: Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(180)))
                .max_idle_age(Duration::from_secs(600))
                .http_status_as_error(false)
                .user_agent(concat!("WispX/", env!("CARGO_PKG_VERSION"), " (Windows)"))
                .tls_config(
                    TlsConfig::builder()
                        .root_certs(RootCerts::PlatformVerifier)
                        .build(),
                )
                .build()
                .into(),
        }
    }
}
impl ChatGpt {
    pub fn warmup(&self) {
        let client = self.clone();
        std::thread::spawn(move || {
            let _ = client.agent.head("https://chatgpt.com/").call();
        });
    }
    pub fn transcribe(
        &self,
        wav: &[u8],
        duration_ms: u64,
        language: &str,
    ) -> Result<String, TranscribeError> {
        let login = auth::login().ok_or(TranscribeError::NoLogin)?;
        let access = if auth::expired(&login.access) {
            auth::refresh(&self.agent, &login.access).unwrap_or(login.access)
        } else {
            login.access
        };
        let (mut status, mut body) =
            self.post(wav, duration_ms, language, &access, &login.account)?;
        if status == 401
            && let Some(fresh) = auth::refresh(&self.agent, &access)
        {
            (status, body) = self.post(wav, duration_ms, language, &fresh, &login.account)?;
        }
        match status {
            200 => serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| match v.get("text") {
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(Value::Null) => Some(String::new()),
                    _ => None,
                })
                .ok_or(TranscribeError::UnexpectedReply),
            401 => Err(TranscribeError::LoginExpired),
            400 if body.replace('_', " ").to_lowercase().contains("too short") => {
                Err(TranscribeError::TooShort)
            }
            403 => Err(TranscribeError::Refused),
            413 => Err(TranscribeError::TooLong),
            429 => Err(TranscribeError::RateLimit),
            _ => {
                let detail = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["detail"].as_str().map(|d| format!(": {d}")))
                    .unwrap_or_default();
                Err(TranscribeError::Server { status, detail })
            }
        }
    }
    fn post(
        &self,
        wav: &[u8],
        duration_ms: u64,
        language: &str,
        access: &str,
        account: &str,
    ) -> Result<(u16, String), TranscribeError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let boundary = format!(
            "----wispx-{:x}{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"wispx.wav\"\r\nContent-Type: audio/wav\r\n\r\n").into_bytes();
        body.extend_from_slice(wav);
        body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"duration_ms\"\r\n\r\n{duration_ms}\r\n").as_bytes());
        if !language.trim().is_empty() {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n{}\r\n", language.trim()).as_bytes());
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let mut request = self
            .agent
            .post("https://chatgpt.com/backend-api/transcribe")
            .header("Authorization", format!("Bearer {access}"))
            .header("originator", "codex_desktop")
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            );
        if !account.is_empty() {
            request = request.header("chatgpt-account-id", account);
        }
        let mut response = request.send(body).map_err(network_error)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(network_error)?;
        Ok((status, text))
    }
}
fn network_error(error: ureq::Error) -> TranscribeError {
    match error {
        ureq::Error::Timeout(_) => TranscribeError::Timeout,
        _ => TranscribeError::Connection,
    }
}
