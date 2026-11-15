//! Puts chunk transcripts back in order, whatever order they finish in, and keeps every
//! missing stretch visible as a gap instead of letting the text close over it.
use super::chunker::Part;
use std::collections::BTreeMap;

/// What one chunk turned into.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Text(String),
    /// Too quiet to upload.
    Silent,
    /// Transcription failed after its retries.
    Failed(String),
    /// Dropped before upload because too many chunks were already waiting.
    Skipped,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Body {
    Text(String),
    /// A stretch with no transcript, and why.
    Gap(String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Line {
    pub start_ms: u64,
    pub end_ms: u64,
    pub body: Body,
}
/// Words two neighbouring transcripts can share when their audio overlaps by a second.
const MAX_REPEATED_WORDS: usize = 8;

#[derive(Default)]
pub struct Assembly {
    waiting: BTreeMap<u64, (Part, Outcome)>,
    next: u64,
    lines: Vec<Line>,
    /// The chunk just placed produced text, so an overlapping successor may repeat it.
    previous_text: bool,
}

/// `m:ss`, or `h:mm:ss` from an hour on.
pub fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}
fn key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
/// Drops the longest run of words that ends `previous` and starts `next`.
pub fn trim_repeat(previous: &str, next: &str) -> String {
    let before: Vec<String> = previous.split_whitespace().map(key).collect();
    let after: Vec<&str> = next.split_whitespace().collect();
    let limit = MAX_REPEATED_WORDS.min(before.len()).min(after.len());
    for count in (1..=limit).rev() {
        let tail = &before[before.len() - count..];
        let head = after[..count].iter().map(|w| key(w));
        if tail.iter().all(|w| !w.is_empty()) && tail.iter().cloned().eq(head) {
            return after[count..].join(" ");
        }
    }
    next.to_owned()
}
impl Assembly {
    /// Chunk results received, placed or waiting for an earlier one.
    pub fn received(&self) -> u64 {
        self.next + self.waiting.len() as u64
    }
    /// Results held back until an earlier chunk finishes.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }
    pub fn has_text(&self) -> bool {
        self.lines.iter().any(|l| matches!(l.body, Body::Text(_)))
    }
    pub fn gaps(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter().filter(|l| matches!(l.body, Body::Gap(_)))
    }
    /// Records a result; a duplicate index is ignored.
    pub fn add(&mut self, part: Part, outcome: Outcome) {
        if part.index < self.next || self.waiting.contains_key(&part.index) {
            return;
        }
        self.waiting.insert(part.index, (part, outcome));
        while let Some((part, outcome)) = self.waiting.remove(&self.next) {
            self.place(part, outcome);
            self.next += 1;
        }
    }
    fn place(&mut self, part: Part, outcome: Outcome) {
        let reason = match outcome {
            Outcome::Text(text) => {
                let text = if part.overlap && self.previous_text {
                    match self.lines.last() {
                        Some(Line {
                            body: Body::Text(previous),
                            ..
                        }) => trim_repeat(previous, text.trim()),
                        _ => text.trim().to_owned(),
                    }
                } else {
                    text.trim().to_owned()
                };
                self.previous_text = true;
                if !text.is_empty() {
                    self.lines.push(Line {
                        start_ms: part.start_ms,
                        end_ms: part.end_ms,
                        body: Body::Text(text),
                    });
                }
                return;
            }
            Outcome::Silent => {
                self.previous_text = false;
                return;
            }
            Outcome::Failed(reason) => reason,
            Outcome::Skipped => "too many parts were waiting to upload".to_owned(),
        };
        self.previous_text = false;
        if let Some(Line {
            end_ms,
            body: Body::Gap(previous),
            ..
        }) = self.lines.last_mut()
            && *previous == reason
            && *end_ms == part.start_ms
        {
            *end_ms = part.end_ms;
            return;
        }
        self.lines.push(Line {
            start_ms: part.start_ms,
            end_ms: part.end_ms,
            body: Body::Gap(reason),
        });
    }
    /// The draft as plain text: each part with its start time, each gap with its span.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(render)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
pub fn render(line: &Line) -> String {
    match &line.body {
        Body::Text(text) => format!("[{}] {text}", clock(line.start_ms)),
        Body::Gap(reason) => format!(
            "[{}–{}] (not transcribed: {reason})",
            clock(line.start_ms),
            clock(line.end_ms)
        ),
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: results finishing out of order are shown out of order or
    // overwrite each other; a duplicate result is appended twice; overlapping audio
    // repeats words at the join, or a legitimately repeated word is removed where
    // there was no overlap; failures disappear from the text; silence becomes a gap.
    use super::*;

    fn part(index: u64, overlap: bool) -> Part {
        Part {
            index,
            start_ms: index * 30_000,
            end_ms: (index + 1) * 30_000,
            overlap,
        }
    }
    fn text(value: &str) -> Outcome {
        Outcome::Text(value.into())
    }

    #[test]
    fn places_results_in_chunk_order() {
        let mut a = Assembly::default();
        a.add(part(2, false), text("third"));
        a.add(part(1, false), text("second"));
        assert!(a.lines().is_empty());
        assert_eq!(a.waiting(), 2);
        a.add(part(0, false), text("first"));
        a.add(part(1, false), text("duplicate"));
        assert_eq!(a.received(), 3);
        assert_eq!(a.text(), "[00:00] first\n\n[00:30] second\n\n[01:00] third");
    }

    #[test]
    fn trims_words_repeated_by_overlapping_audio_only() {
        let mut a = Assembly::default();
        a.add(part(0, false), text("we ship on Thursday, after"));
        a.add(part(1, true), text("Thursday after the review."));
        a.add(part(2, false), text("the review. Again"));
        assert_eq!(
            a.text(),
            "[00:00] we ship on Thursday, after\n\n[00:30] the review.\n\n[01:00] the review. Again"
        );
        assert_eq!(trim_repeat("a b c", "x y"), "x y");
        assert_eq!(trim_repeat("it is", "is it is good"), "it is good");
        assert_eq!(trim_repeat("", "hello"), "hello");
        // Punctuation-only words never anchor a match.
        assert_eq!(trim_repeat("— ok", "— ok fine"), "— ok fine");
    }

    #[test]
    fn failures_and_skips_stay_visible_and_silence_does_not() {
        let mut a = Assembly::default();
        a.add(part(0, false), text("hello"));
        a.add(
            part(1, true),
            Outcome::Failed("No connection to ChatGPT".into()),
        );
        a.add(
            part(2, false),
            Outcome::Failed("No connection to ChatGPT".into()),
        );
        a.add(part(3, false), Outcome::Silent);
        a.add(part(4, false), Outcome::Skipped);
        a.add(part(5, true), text("hello again"));
        assert_eq!(a.gaps().count(), 2);
        assert_eq!(
            a.text(),
            "[00:00] hello\n\n[00:30–01:30] (not transcribed: No connection to ChatGPT)\n\n[02:00–02:30] (not transcribed: too many parts were waiting to upload)\n\n[02:30] hello again"
        );
        assert!(a.has_text());
        assert_eq!(clock(3_725_000), "1:02:05");
    }
}
