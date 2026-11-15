//! The Codex Responses endpoint over server-sent events, shared by dictation cleanup
//! and Listen notes. Blocking: call it from worker threads only.
use super::{
    ChatGpt,
    auth::{self, AuthMode},
};
use serde_json::{Value, json};
use std::time::Instant;

pub const MODEL: &str = "gpt-6-luna";

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RespondError {
    #[error("Sign in to the Codex app first")]
    NoLogin,
    #[error("Needs Codex signed in with ChatGPT")]
    NotChatGpt,
    #[error("ChatGPT took too long")]
    Timeout,
    #[error("No connection to ChatGPT")]
    Connection,
    #[error("ChatGPT error {0}")]
    Status(u16),
    #[error("ChatGPT didn't finish its answer")]
    Incomplete,
}

/// Read complete SSE frames; done supersedes deltas, and a later failure wins.
fn parse_sse(stream: &str) -> Option<String> {
    let normalized = stream.replace("\r\n", "\n");
    let mut deltas = String::new();
    let mut done = None;
    let mut completed = false;
    for frame in normalized.split("\n\n") {
        let mut event = "";
        let mut data = Vec::new();
        for line in frame.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event = value.trim();
            }
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.trim_start());
            }
        }
        if matches!(event, "response.failed" | "error") {
            return None;
        }
        if data.is_empty() {
            continue;
        }
        let data = data.join("\n");
        if data == "[DONE]" {
            completed = true;
            continue;
        }
        let value: Value = serde_json::from_str(&data).ok()?;
        let kind = value["type"].as_str().unwrap_or(event);
        match kind {
            "response.failed" | "error" => return None,
            "response.output_text.done" => {
                done = Some(value["text"].as_str()?.to_owned());
            }
            "response.output_text.delta" => deltas.push_str(value["delta"].as_str()?),
            "response.completed" => completed = true,
            _ => {}
        }
    }
    let text = done.or_else(|| completed.then_some(deltas))?;
    (!text.trim().is_empty()).then_some(text)
}

impl ChatGpt {
    /// One streamed answer to `input`, refreshing the login once on 401.
    /// `deadline` bounds each request, so a hung connection can't stall the caller.
    pub fn respond(
        &self,
        instructions: &str,
        input: &str,
        effort: &str,
        deadline: Instant,
    ) -> Result<String, RespondError> {
        let login = auth::login().ok_or(RespondError::NoLogin)?;
        if login.mode != AuthMode::ChatGpt {
            return Err(RespondError::NotChatGpt);
        }
        let access = if auth::expired(&login.access) {
            auth::refresh(&self.agent, &login.access).unwrap_or(login.access)
        } else {
            login.access
        };
        let request = json!({"model": MODEL, "instructions": instructions,
            "input": [{"type":"message", "role":"user", "content":[{"type":"input_text", "text":input}]}],
            "reasoning":{"effort":effort}, "stream":true, "store":false});
        let (mut status, mut body) =
            self.respond_post(&request, &access, &login.account, deadline)?;
        if status == 401
            && let Some(fresh) = auth::refresh(&self.agent, &access)
        {
            (status, body) = self.respond_post(&request, &fresh, &login.account, deadline)?;
        }
        if status != 200 {
            return Err(RespondError::Status(status));
        }
        parse_sse(&body).ok_or(RespondError::Incomplete)
    }

    fn respond_post(
        &self,
        request: &Value,
        access: &str,
        account: &str,
        deadline: Instant,
    ) -> Result<(u16, String), RespondError> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(RespondError::Timeout)?;
        let failed = |error: ureq::Error| match error {
            ureq::Error::Timeout(_) => RespondError::Timeout,
            _ => RespondError::Connection,
        };
        let mut response = self
            .agent
            .post("https://chatgpt.com/backend-api/codex/responses")
            .header("Authorization", format!("Bearer {access}"))
            .header("chatgpt-account-id", account)
            .header("originator", "codex_desktop")
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .config()
            .timeout_global(Some(remaining))
            .build()
            .send_json(request)
            .map_err(failed)?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(failed)?;
        Ok((status, body))
    }
}

#[cfg(test)]
mod tests {
    // SSE: CRLF/multiline events, deltas duplicated by done, absent event names, malformed
    // JSON, failed/error events after valid text, empty output, or incomplete streams.
    use super::*;

    #[test]
    fn sse_prefers_done_and_accepts_delta_only_completion() {
        assert_eq!(parse_sse("event: response.output_text.delta\r\ndata: {\"delta\":\"wrong\"}\r\n\r\nevent: response.output_text.done\r\ndata: {\"text\":\"WispX.\"}\r\n\r\n").as_deref(), Some("WispX."));
        assert_eq!(parse_sse("data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"!\"}\n\nevent: response.completed\ndata: {}\n\n").as_deref(), Some("Hi!"));
        assert_eq!(
            parse_sse("event: response.output_text.done\ndata: {\n data: ignored\n\n"),
            None
        );
        assert_eq!(
            parse_sse("event: response.output_text.done\ndata: {\ndata: \"text\":\"yes\"}\n\n")
                .as_deref(),
            Some("yes")
        );
    }

    #[test]
    fn sse_rejects_failures_and_incomplete_or_empty_output() {
        for ending in [
            "event: response.failed\ndata: {}\n\n",
            "event: error\ndata: {}\n\n",
            "data: {\"type\":\"error\"}\n\n",
            "data: broken\n\n",
        ] {
            assert!(
                parse_sse(&format!(
                    "event: response.output_text.done\ndata: {{\"text\":\"ok\"}}\n\n{ending}"
                ))
                .is_none()
            );
        }
        for stream in [
            "",
            "event: response.output_text.delta\ndata: {\"delta\":\"partial\"}\n\n",
            "event: response.output_text.done\ndata: {\"text\":\"  \"}\n\n",
        ] {
            assert!(parse_sse(stream).is_none());
        }
    }
}
