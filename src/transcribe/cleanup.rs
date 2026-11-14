//! Optional Codex cleanup. Every failure returns the original dictation unchanged.
use super::{
    ChatGpt,
    auth::{self, AuthMode},
};
use std::time::{Duration, Instant};

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
        text.push_str(&format!("Surrounding text ([cursor] marks where the transcript will be inserted; adjust capitalization to continue the text before it, and return only the transcript):\n{surrounding}\n"));
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

/// Add a word separator only when a single known insertion point requires one.
/// UIA already preserves surrounding whitespace; plain ValuePattern context cannot locate a caret.
pub fn join_context(text: String, surrounding: &str) -> String {
    let Some((before, after)) = surrounding.split_once("[cursor]") else {
        return text;
    };
    if after.contains("[cursor]") {
        return text;
    }
    let mut preceding = before.chars().rev();
    let (Some(previous), Some(first)) = (preceding.next(), text.chars().next()) else {
        return text;
    };
    // A straight quote opens at a boundary; after text it closes a quotation
    // or a possessive. Curly opening quotes are unambiguous.
    let opens_quote = matches!(previous, '"' | '\'')
        && preceding.next().is_none_or(|ch| {
            ch.is_whitespace()
                || matches!(ch, '(' | '[' | '{' | '"' | '\'' | '\u{201c}' | '\u{2018}')
        });
    if previous.is_whitespace()
        || opens_quote
        || matches!(previous, '(' | '[' | '{' | '\u{201c}' | '\u{2018}')
        || first.is_whitespace()
        || matches!(
            first,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{201d}' | '\u{2019}'
        )
    {
        return text;
    }
    format!(" {text}")
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
                let _ = tx.send(client.respond(INSTRUCTIONS, &prompt, "low", deadline).ok());
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
}

#[cfg(test)]
mod tests {
    // Joining failures: missing/ambiguous caret, empty text or prefix, duplicate whitespace,
    // a space after an opening bracket/quote, or a space before closing punctuation.
    // Quote boundary failures: treating a closing straight quote or possessive apostrophe
    // as an opening quote glues the next word; opening quotes after whitespace/brackets
    // must still suppress a separator. Cases added before correcting the quote rule.
    // Prompt failures: an unexplained insertion marker or instructions that ask for context
    // to be repeated rather than adjusting only the transcript's initial capitalization.
    #[test]
    fn joins_only_when_the_known_caret_needs_a_word_separator() {
        for (context, text, expected) in [
            ("testing the[cursor]", "system.", " system."),
            ("testing the [cursor]", "system.", "system."),
            ("testing the\n[cursor]", "system.", "system."),
            ("testing the\t[cursor]", "system.", "system."),
            ("[cursor]after", "System.", "System."),
            ("plain context", "System.", "System."),
            ("a[cursor]b[cursor]", "system", "system"),
            ("the[cursor]", " system", " system"),
            ("the[cursor]", "", ""),
            ("the[cursor]", ".", "."),
            ("the[cursor]", ", yes", ", yes"),
            ("the[cursor]", ")", ")"),
            ("\u{00e9}[cursor]", "word", " word"),
            (
                "We call it \"WispX\"[cursor]",
                "and it works.",
                " and it works.",
            ),
            ("The developers'[cursor]", "tools", " tools"),
            ("He said \"[cursor]", "hello", "hello"),
            ("He said '[cursor]", "hello", "hello"),
            ("He said (\"[cursor]", "hello", "hello"),
        ] {
            assert_eq!(join_context(text.into(), context), expected);
        }
        for opening in ['(', '[', '{', '"', '\'', '\u{201c}', '\u{2018}'] {
            assert_eq!(
                join_context("word".into(), &format!("{opening}[cursor]")),
                "word"
            );
        }
    }
    // Failure modes, written before implementation:
    // Prompt: wrong separators/order, an empty surrounding block, Unicode split at a byte
    // boundary, context over 2,000 chars, dictionary over 100, or transcript truncation.
    // Dictionary: whitespace/empty entries, duplicate canonical entries, altered casing,
    // or saving more than 200 entries.
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
            "Surrounding text ([cursor] marks where the transcript will be inserted; adjust capitalization to continue the text before it, and return only the transcript):\nbefore[cursor]after\nDictionary (canonical entries; use exact spelling, casing, and punctuation when they match):\nWispX\nTranscript:\n um hello "
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
