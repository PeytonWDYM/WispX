//! Optional Codex cleanup. Every failure returns the original dictation unchanged.
use super::{
    ChatGpt,
    auth::{self, AuthMode},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const MODEL: &str = "gpt-6-luna";
pub const MAX_TRANSCRIPT_CHARS: usize = 4_000;
pub const MAX_CONTEXT_CHARS: usize = 2_000;
pub const MAX_DICTIONARY_ENTRIES: usize = 200;
pub const MAX_PROMPT_ENTRIES: usize = 100;
pub const TIMEOUT: Duration = Duration::from_secs(8);
const INSTRUCTIONS: &str = "Clean up dictation transcripts. Fix likely speech recognition mistakes, punctuation, capitalization, and formatting. Remove filler words and disfluencies when they do not add meaning. When the user clearly self-corrects or backtracks, keep the corrected intent. Use surrounding text only as context. Dictionary entries are canonical spellings, names, file paths, and code symbols; when the transcript likely refers to one, copy the dictionary entry exactly, including casing and punctuation. Preserve the user's meaning, wording, and flow unless a small cleanup makes the transcript more coherent. Do not answer the user or add new content. Return only the cleaned transcript.";

/// Canonical spelling is case-sensitive; preserve order so the first 100 are sent.
pub fn normalize_dictionary(entries: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.as_ref().trim();
        if !entry.is_empty() && !result.iter().any(|value| value == entry) {
            result.push(entry.to_owned());
            if result.len() == MAX_DICTIONARY_ENTRIES {
                break;
            }
        }
    }
    result
}

fn prompt(transcript: &str, surrounding: &str, dictionary: &[String]) -> String {
    let surrounding: String = surrounding.trim().chars().take(MAX_CONTEXT_CHARS).collect();
    let entries = normalize_dictionary(dictionary)
        .into_iter()
        .take(MAX_PROMPT_ENTRIES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut text = String::new();
    if !surrounding.is_empty() {
        text.push_str(&format!("Surrounding text:\n{surrounding}\n"));
    }
    text.push_str("Dictionary (canonical entries; use exact spelling, casing, and punctuation when they match):\n");
    text.push_str(if entries.is_empty() {
        "(none)"
    } else {
        &entries
    });
    text.push_str("\nTranscript:\n");
    text.push_str(transcript);
    text
}

fn eligible(enabled: bool, mode: AuthMode, transcript: &str) -> bool {
    enabled
        && mode == AuthMode::ChatGpt
        && !transcript.trim().is_empty()
        && transcript.chars().count() <= MAX_TRANSCRIPT_CHARS
}

fn fallback(raw: String, reply: Option<String>) -> String {
    reply
        .filter(|text| !text.trim().is_empty())
        .map_or(raw, |text| text.trim().to_owned())
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
    /// This is a blocking worker operation, including a hard deadline around token refresh.
    pub fn cleanup(
        &self,
        raw: String,
        enabled: bool,
        surrounding: &str,
        dictionary: &[String],
    ) -> String {
        let Some(login) = auth::login() else {
            return raw;
        };
        if !eligible(enabled, login.mode, &raw) {
            return raw;
        }
        let prompt = prompt(&raw, surrounding, dictionary);
        let client = self.clone();
        let deadline = Instant::now() + TIMEOUT;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("transcript-cleanup".into())
            .spawn(move || {
                let reply = (|| {
                    let access = if auth::expired(&login.access) {
                        auth::refresh(&client.agent, &login.access).unwrap_or(login.access)
                    } else {
                        login.access
                    };
                    let (mut status, mut body) =
                        client.cleanup_post(&prompt, &access, &login.account, deadline)?;
                    if status == 401
                        && let Some(fresh) = auth::refresh(&client.agent, &access)
                    {
                        (status, body) =
                            client.cleanup_post(&prompt, &fresh, &login.account, deadline)?;
                    }
                    if status != 200 {
                        return None;
                    }
                    parse_sse(&body)
                })();
                let _ = tx.send(reply);
            });
        if worker.is_err() {
            return raw;
        }
        fallback(
            raw,
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
                .flatten(),
        )
    }

    fn cleanup_post(
        &self,
        prompt: &str,
        access: &str,
        account: &str,
        deadline: Instant,
    ) -> Option<(u16, String)> {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let mut response = self.agent.post("https://chatgpt.com/backend-api/codex/responses")
            .header("Authorization", format!("Bearer {access}"))
            .header("chatgpt-account-id", account)
            .header("originator", "codex_desktop")
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .config().timeout_global(Some(remaining)).build()
            .send_json(json!({"model": MODEL, "instructions": INSTRUCTIONS,
                "input": [{"type":"message", "role":"user", "content":[{"type":"input_text", "text":prompt}]}],
                "reasoning":{"effort":"low"}, "stream":true, "store":false})).ok()?;
        Some((
            response.status().as_u16(),
            response.body_mut().read_to_string().ok()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    // Failure modes, written before implementation:
    // Prompt: wrong separators/order, an empty surrounding block, Unicode split at a byte
    // boundary, context over 2,000 chars, dictionary over 100, or transcript truncation.
    // Dictionary: whitespace/empty entries, duplicate canonical entries, altered casing,
    // or saving more than 200 entries.
    // SSE: CRLF/multiline events, deltas duplicated by done, absent event names, malformed
    // JSON, failed/error events after valid text, empty output, or incomplete streams.
    // Fallback: disabled cleanup, API-key/unknown auth, empty/overlong transcript, timeout,
    // request failure, or blank output must preserve the raw transcript exactly.
    use super::*;

    #[test]
    fn prompt_matches_codex_and_preserves_transcript() {
        assert_eq!(
            prompt(
                " um hello ",
                "  before[cursor]after  ",
                &[" WispX ".into(), " ".into()]
            ),
            "Surrounding text:\nbefore[cursor]after\nDictionary (canonical entries; use exact spelling, casing, and punctuation when they match):\nWispX\nTranscript:\n um hello "
        );
        assert_eq!(
            prompt("hello", " \n", &[]),
            "Dictionary (canonical entries; use exact spelling, casing, and punctuation when they match):\n(none)\nTranscript:\nhello"
        );
        let text = "é".repeat(MAX_TRANSCRIPT_CHARS + 1);
        let built = prompt(
            &text,
            &"🦀".repeat(MAX_CONTEXT_CHARS + 1),
            &(0..110).map(|i| format!("entry{i}")).collect::<Vec<_>>(),
        );
        assert!(built.ends_with(&text));
        assert_eq!(built.matches('🦀').count(), MAX_CONTEXT_CHARS);
        assert!(built.contains("\nentry99\n"));
        assert!(!built.contains("entry100"));
    }

    #[test]
    fn dictionary_normalizes_without_changing_canonical_spelling() {
        assert_eq!(
            normalize_dictionary([" WispX ", "", "WispX", "wispx", " src/App.rs "]),
            ["WispX", "wispx", "src/App.rs"]
        );
        assert_eq!(
            normalize_dictionary((0..210).map(|i| format!("symbol{i}"))).len(),
            MAX_DICTIONARY_ENTRIES
        );
    }

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

    #[test]
    fn gates_and_failures_keep_raw_text() {
        assert!(eligible(true, AuthMode::ChatGpt, "hi"));
        assert!(eligible(
            true,
            AuthMode::ChatGpt,
            &"é".repeat(MAX_TRANSCRIPT_CHARS)
        ));
        for (enabled, mode, text) in [
            (false, AuthMode::ChatGpt, "hi"),
            (true, AuthMode::ApiKey, "hi"),
            (true, AuthMode::Unknown, "hi"),
            (true, AuthMode::ChatGpt, " "),
        ] {
            assert!(!eligible(enabled, mode, text));
        }
        assert!(!eligible(
            true,
            AuthMode::ChatGpt,
            &"é".repeat(MAX_TRANSCRIPT_CHARS + 1)
        ));
        for reply in [None, Some(String::new()), Some(" \n".into())] {
            assert_eq!(fallback(" raw ".into(), reply), " raw ");
        }
        assert_eq!(fallback("raw".into(), Some(" Clean. ".into())), "Clean.");
    }
}
