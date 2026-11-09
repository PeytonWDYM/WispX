//! Blocking HTTP work belongs on worker threads, never the UI or keyboard hook.
pub use super::errors::TranscribeError;
use super::{auth, errors::ServerError, retry};
use serde_json::Value;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use ureq::{
    Agent,
    tls::{RootCerts, TlsConfig},
};

#[cfg(windows)]
const USER_AGENT: &str = concat!("WispX/", env!("CARGO_PKG_VERSION"), " (Windows)");
#[cfg(not(windows))]
const USER_AGENT: &str = concat!("WispX/", env!("CARGO_PKG_VERSION"), " (Linux)");

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
                .user_agent(USER_AGENT)
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
        webm: &[u8],
        duration_ms: u64,
        language: &str,
    ) -> Result<String, TranscribeError> {
        let login = auth::login().ok_or(TranscribeError::NoLogin)?;
        let access = if auth::expired(&login.access) {
            auth::refresh(&self.agent, &login.access).unwrap_or(login.access)
        } else {
            login.access
        };
        let upload = Upload::new(webm, duration_ms, language);
        let mut response = self.post(&upload, &access, &login.account)?;
        if ServerError::parse(&response.body).needs_refresh(response.status)
            && let Some(fresh) = auth::refresh(&self.agent, &access)
        {
            response = self.post(&upload, &fresh, &login.account)?;
        }
        match response.status {
            200 => serde_json::from_str::<Value>(&response.body)
                .ok()
                .and_then(|v| match v.get("text") {
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(Value::Null) => Some(String::new()),
                    _ => None,
                })
                .ok_or(TranscribeError::UnexpectedReply),
            status => Err(ServerError::parse(&response.body).into_error(status)),
        }
    }
    fn post(&self, upload: &Upload, access: &str, account: &str) -> Result<Reply, TranscribeError> {
        let mut retry_index = 0;
        loop {
            let result = self.post_once(upload, access, account);
            let wait = match &result {
                Ok(reply) => retry::delay(
                    retry_index,
                    Some(reply.status),
                    reply.retry_after.as_deref(),
                    SystemTime::now(),
                ),
                // A timeout already spent the 3-minute budget; only quick connection failures retry.
                Err(TranscribeError::Connection) => {
                    retry::delay(retry_index, None, None, SystemTime::now())
                }
                Err(_) => None,
            };
            let Some(wait) = wait else {
                return result;
            };
            std::thread::sleep(wait);
            retry_index += 1;
        }
    }

    fn post_once(
        &self,
        upload: &Upload,
        access: &str,
        account: &str,
    ) -> Result<Reply, TranscribeError> {
        let mut request = self
            .agent
            .post("https://chatgpt.com/backend-api/transcribe")
            .header("Authorization", format!("Bearer {access}"))
            .header("originator", "codex_desktop")
            .header("Content-Type", &upload.content_type);
        if !account.is_empty() {
            request = request.header("chatgpt-account-id", account);
        }
        let mut response = request
            .send(upload.body.as_slice())
            .map_err(network_error)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(network_error)?;
        Ok(Reply {
            status,
            body: text,
            retry_after,
        })
    }
}

struct Reply {
    status: u16,
    body: String,
    retry_after: Option<String>,
}

/// Retain one multipart body across both transport retries and token refresh.
struct Upload {
    content_type: String,
    body: Vec<u8>,
}
impl Upload {
    fn new(webm: &[u8], duration_ms: u64, language: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let boundary = format!(
            "----wispx-{:x}{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"wispx.webm\"\r\nContent-Type: audio/webm\r\n\r\n").into_bytes();
        body.extend_from_slice(webm);
        body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"duration_ms\"\r\n\r\n{duration_ms}\r\n").as_bytes());
        if !language.trim().is_empty() {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n{}\r\n", language.trim()).as_bytes());
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Self {
            content_type: format!("multipart/form-data; boundary={boundary}"),
            body,
        }
    }
}
fn network_error(error: ureq::Error) -> TranscribeError {
    match error {
        ureq::Error::Timeout(_) => TranscribeError::Timeout,
        _ => TranscribeError::Connection,
    }
}
