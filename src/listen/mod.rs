//! Listen: computer audio (and optionally the microphone) transcribed in chunks while
//! it plays, then formatted and summarized into notes. `Listen` is the UI-thread state;
//! its worker threads report back through the sink, tagged with the session they
//! belong to, so a result from a discarded or replaced session is ignored.
pub mod assembly;
pub mod chunker;
pub mod notes;
pub mod session;

use crate::{
    app::{MessageKind, Pill},
    audio::{capture, recorder::Levels},
    settings::Settings,
    transcribe::ChatGpt,
};
use assembly::{Assembly, Body, Line, clock};
use notes::{Formatted, Summary};
use session::Session;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Long enough for a long meeting; bounds the transcript kept in memory.
pub const MAX_SESSION: Duration = Duration::from_secs(4 * 3600);
pub const UNSAVED: &str = "Save or close your last notes first (Options → Listen).";
pub const CHANGED_WHILE_SAVING: &str = "The notes changed while saving, so the file has an earlier version. Save again to include everything.";

/// Ends a session's notes work: once cancelled, no further request starts. A request
/// already in flight runs to its deadline, and its answer is dropped.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);
impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The cloud work around a session: transcribing chunks, then writing notes.
/// Replaced in tests, so they never send audio or text anywhere.
pub trait Writer: Send + Sync + 'static {
    fn transcriber(&self, language: &str) -> Arc<dyn session::Transcriber>;
    /// `None` when cancelled before every section was done.
    fn format(
        &self,
        lines: &[Line],
        dictionary: &[String],
        cancel: &Cancel,
        progress: &dyn Fn(usize, usize),
    ) -> Option<Formatted>;
    fn summarize(
        &self,
        transcript: &str,
        incomplete: Option<&str>,
        cancel: &Cancel,
    ) -> Result<Summary, String>;
}
impl Writer for ChatGpt {
    fn transcriber(&self, language: &str) -> Arc<dyn session::Transcriber> {
        Arc::new(session::Cloud {
            client: self.clone(),
            language: language.to_owned(),
        })
    }
    fn format(
        &self,
        lines: &[Line],
        dictionary: &[String],
        cancel: &Cancel,
        progress: &dyn Fn(usize, usize),
    ) -> Option<Formatted> {
        notes::format(self, lines, dictionary, &|| cancel.cancelled(), progress)
    }
    fn summarize(
        &self,
        transcript: &str,
        incomplete: Option<&str>,
        cancel: &Cancel,
    ) -> Result<Summary, String> {
        notes::summarize(self, transcript, incomplete, &|| cancel.cancelled())
    }
}

pub enum Update {
    Session(session::Event),
    Formatting { done: usize, total: usize },
    Formatted(Formatted),
    Summary(Result<Summary, String>),
}
/// Delivers a worker update for session `generation` to the UI thread.
pub type Sink = Arc<dyn Fn(u64, Update) + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Setup,
    Listening,
    /// Capture is over; the last chunks are still being transcribed.
    Finishing,
    Formatting,
    Summarizing,
    Notes,
}
pub struct Request {
    pub output: String,
    pub output_name: String,
    /// The microphone's ID and name, when it's included.
    pub microphone: Option<(String, String)>,
    pub language: String,
    pub dictionary: Vec<String>,
}
impl Request {
    /// What the saved settings ask for, with each device's current name for the notes.
    pub fn from_settings(settings: &Settings) -> Self {
        let devices = capture::devices().unwrap_or_default();
        let name = |list: &[capture::Device], id: &str, default: &str| {
            if id.is_empty() {
                return default.to_owned();
            }
            list.iter()
                .find(|d| d.id == id)
                .map_or_else(|| id.to_owned(), |d| d.name.clone())
        };
        Self {
            output: settings.listen_output.clone(),
            output_name: name(&devices.outputs, &settings.listen_output, "Default output"),
            microphone: settings.listen_include_microphone.then(|| {
                (
                    settings.listen_microphone.clone(),
                    name(
                        &devices.microphones,
                        &settings.listen_microphone,
                        "Default microphone",
                    ),
                )
            }),
            language: settings.language.clone(),
            dictionary: settings.dictionary.clone(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SummaryState {
    Pending,
    Ready(Summary),
    Failed(String),
    /// Nothing was said, so there is nothing to summarize.
    NoSpeech,
    /// Capture or transcription failed without recovering any speech.
    NoTranscript(String),
}
pub struct Notes {
    pub details: String,
    /// The transcript exactly as assembled from the chunks.
    pub raw: String,
    pub formatted: Option<Formatted>,
    pub summary: SummaryState,
    /// What is missing, when the session is incomplete.
    pub warning: Option<String>,
    pub saved: Option<PathBuf>,
    /// Counts changes to what `markdown` writes, so a save of older content can't
    /// mark newer notes saved.
    pub revision: u64,
}
/// What Save writes, tagged with the session and notes revision it came from.
pub struct SaveRequest {
    pub generation: u64,
    pub revision: u64,
    pub name: String,
    pub text: String,
}
impl Notes {
    /// The saved file no longer holds these notes.
    fn changed(&mut self) {
        self.revision += 1;
        self.saved = None;
    }
    pub fn title(&self) -> String {
        match &self.summary {
            SummaryState::Ready(summary) if !summary.title.is_empty() => summary.title.clone(),
            _ => "Listen notes".into(),
        }
    }
    /// The formatted transcript, or the raw one when formatting didn't run.
    pub fn transcript(&self) -> &str {
        self.formatted.as_ref().map_or(&self.raw, |f| &f.text)
    }
    pub fn transcript_note(&self) -> Option<String> {
        let formatted = self.formatted.as_ref()?;
        if formatted.raw_sections == 0 {
            return None;
        }
        let reason = formatted.error.as_deref().unwrap_or("unknown error");
        Some(if formatted.raw_sections == formatted.sections {
            format!("Couldn't format the transcript ({reason}), so this is the original text.")
        } else {
            format!(
                "{} of {} sections couldn't be formatted ({reason}) and are shown as originally transcribed.",
                formatted.raw_sections, formatted.sections
            )
        })
    }
    pub fn summary_text(&self) -> String {
        let SummaryState::Ready(summary) = &self.summary else {
            return String::new();
        };
        let mut text = notes::markdown(&notes::Document {
            title: &self.title(),
            details: &self.details,
            warning: self.warning.as_deref(),
            summary: Some(summary),
            transcript: "",
            transcript_note: None,
        });
        if let Some(end) = text.rfind("\n## Transcript") {
            text.truncate(end);
        }
        text
    }
    pub fn markdown(&self) -> String {
        let note = self.transcript_note();
        notes::markdown(&notes::Document {
            title: &self.title(),
            details: &self.details,
            warning: self.warning.as_deref(),
            summary: match &self.summary {
                SummaryState::Ready(summary) => Some(summary),
                _ => None,
            },
            transcript: self.transcript(),
            transcript_note: note.as_deref(),
        })
    }
    /// A file name from the title and date, safe on Windows and Linux.
    pub fn file_name(&self) -> String {
        let name: String = format!(
            "{} {}",
            self.details.split(" · ").next().unwrap_or(""),
            self.title()
        )
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
        format!(
            "{}.md",
            name.split_whitespace().collect::<Vec<_>>().join(" ")
        )
    }
}

pub struct Listen {
    sink: Sink,
    writer: Arc<dyn Writer>,
    generation: u64,
    stage: Stage,
    session: Option<Session>,
    started: Option<Instant>,
    started_label: String,
    sources: Vec<String>,
    dictionary: Vec<String>,
    assembly: Assembly,
    total: Option<u64>,
    duration_ms: u64,
    failure: Option<String>,
    backlog: usize,
    progress: (usize, usize),
    notes: Option<Notes>,
    /// Cancels this generation's notes work when the session is replaced or dropped.
    cancel: Cancel,
}
impl Drop for Listen {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Listen {
    pub fn new(writer: Arc<dyn Writer>, sink: Sink) -> Self {
        Self {
            sink,
            writer,
            generation: 0,
            stage: Stage::Setup,
            session: None,
            started: None,
            started_label: String::new(),
            sources: Vec::new(),
            dictionary: Vec::new(),
            assembly: Assembly::default(),
            total: None,
            duration_ms: 0,
            failure: None,
            backlog: 0,
            progress: (0, 0),
            notes: None,
            cancel: Cancel::default(),
        }
    }
    pub fn stage(&self) -> Stage {
        self.stage
    }
    pub fn capturing(&self) -> bool {
        self.stage == Stage::Listening
    }
    /// Notes nobody saved yet, which a new session would replace.
    pub fn unsaved(&self) -> bool {
        self.notes.as_ref().is_some_and(|n| {
            n.saved.is_none()
                && !matches!(
                    &n.summary,
                    SummaryState::NoSpeech | SummaryState::NoTranscript(_)
                )
        })
    }
    /// Quit must leave active work or recovered, unsaved text available.
    pub fn blocks_quit(&self) -> bool {
        !matches!(self.stage, Stage::Setup | Stage::Notes) || self.unsaved()
    }
    pub fn started(&self) -> Option<Instant> {
        self.started
    }
    /// Each source's name and live level, while capturing.
    pub fn sources(&self) -> Vec<(String, Levels)> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        self.sources
            .iter()
            .cloned()
            .zip(session.levels.1.iter().cloned())
            .collect()
    }
    /// The mix's level, for the pill.
    pub fn level(&self) -> Option<Levels> {
        self.session.as_ref().map(|s| s.levels.0.clone())
    }
    pub fn lines(&self) -> &[Line] {
        self.assembly.lines()
    }
    pub fn notes(&self) -> Option<&Notes> {
        self.notes.as_ref()
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Progress of the current step, 0 to 1, and its label.
    pub fn step(&self) -> (f32, String) {
        match self.stage {
            Stage::Finishing => {
                let received = self.assembly.received() as f32;
                let total = self.total.map_or(received + 1.0, |t| t as f32).max(1.0);
                (received / total, "Finishing transcript…".into())
            }
            Stage::Formatting => {
                let (done, total) = self.progress;
                (
                    done as f32 / total.max(1) as f32,
                    "Formatting transcript…".into(),
                )
            }
            Stage::Summarizing => (0.5, "Writing summary…".into()),
            _ => (0.0, String::new()),
        }
    }
    /// What the live view should warn about: a backlog, or parts already missing.
    pub fn status(&self) -> String {
        let gaps = self.assembly.gaps().count();
        if self.backlog > 3 {
            format!(
                "Transcription is behind: {} parts waiting. If the queue fills, the gap is marked in your notes.",
                self.backlog
            )
        } else if gaps > 0 {
            format!(
                "{gaps} {} couldn't be transcribed. Your notes will show what's missing.",
                if gaps == 1 { "part" } else { "parts" }
            )
        } else {
            String::new()
        }
    }
    pub fn start(&mut self, request: Request) -> Result<(), String> {
        if self.stage != Stage::Setup && self.stage != Stage::Notes {
            return Err("Listen is already running.".into());
        }
        if self.unsaved() {
            return Err(UNSAVED.into());
        }
        let generation = self.generation + 1;
        let sink = self.sink.clone();
        let transcriber = self.writer.transcriber(&request.language);
        let session = Session::start(
            &request.output,
            request.microphone.as_ref().map(|(id, _)| id.as_str()),
            transcriber,
            Arc::new(move |event| sink(generation, Update::Session(event))),
        )
        .map_err(|e| e.0)?;
        // A fresh state; dropping the old one cancels its notes work.
        let fresh = Self::new(self.writer.clone(), self.sink.clone());
        drop(std::mem::replace(self, fresh));
        self.generation = generation;
        self.stage = Stage::Listening;
        self.session = Some(session);
        self.started = Some(Instant::now());
        self.started_label = local_time();
        self.sources = [
            Some(request.output_name),
            request.microphone.map(|(_, name)| name),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.dictionary = request.dictionary;
        Ok(())
    }
    /// Ends capture and goes on to notes. Returns false when nothing was capturing.
    pub fn stop(&mut self) -> bool {
        if self.stage != Stage::Listening {
            return false;
        }
        if let Some(session) = &mut self.session {
            session.stop();
        }
        self.stage = Stage::Finishing;
        true
    }
    /// Moves on from the current session: its late results are ignored, and its notes
    /// work starts no further requests.
    fn advance(&mut self) {
        self.generation += 1;
        self.cancel.cancel();
        self.cancel = Cancel::default();
        self.session = None;
        self.stage = Stage::Setup;
        self.assembly = Assembly::default();
        self.notes = None;
    }
    /// Throws away a session that hasn't produced notes yet.
    pub fn discard(&mut self) -> bool {
        if matches!(self.stage, Stage::Setup | Stage::Notes) {
            return false;
        }
        self.advance();
        true
    }
    /// Closes finished notes for a new session; the UI confirms unsaved ones first.
    pub fn close_notes(&mut self) {
        if self.stage == Stage::Notes {
            self.advance();
        }
    }
    /// On quit: stops capture and every request still to come, keeping nothing.
    pub fn shutdown(&mut self) {
        self.advance();
    }
    /// The notes as they stand, for Save.
    pub fn save_request(&self) -> Option<SaveRequest> {
        let notes = self.notes.as_ref()?;
        Some(SaveRequest {
            generation: self.generation,
            revision: notes.revision,
            name: notes.file_name(),
            text: notes.markdown(),
        })
    }
    /// Marks the notes saved when the file holds them as they are now. False when the
    /// notes changed or were replaced while saving.
    pub fn mark_saved(&mut self, generation: u64, revision: u64, path: PathBuf) -> bool {
        match &mut self.notes {
            Some(notes) if generation == self.generation && revision == notes.revision => {
                notes.saved = Some(path);
                true
            }
            _ => false,
        }
    }
    /// Applies a worker update; returns a pill to show, if the moment calls for one.
    pub fn update(&mut self, generation: u64, update: Update) -> Option<Pill> {
        if generation != self.generation {
            return None;
        }
        match update {
            Update::Session(session::Event::Chunk(part, outcome)) => {
                self.assembly.add(part, outcome);
                self.drained()
            }
            Update::Session(session::Event::Backlog(waiting)) => {
                self.backlog = waiting;
                None
            }
            Update::Session(session::Event::Ended {
                chunks,
                duration_ms,
                failure,
            }) => {
                self.total = Some(chunks);
                self.duration_ms = duration_ms;
                let stopped = failure.as_ref().map(|reason| {
                    Pill::message(format!("Listen stopped. {reason}"), MessageKind::Warn, 4.0)
                });
                self.failure = failure;
                if self.stage == Stage::Listening {
                    self.stage = Stage::Finishing;
                }
                self.drained().or(stopped)
            }
            Update::Formatting { done, total } => {
                if self.stage == Stage::Formatting {
                    self.progress = (done, total);
                }
                None
            }
            Update::Formatted(formatted) => {
                if let Some(notes) = &mut self.notes {
                    notes.formatted = Some(formatted);
                    notes.changed();
                    self.stage = Stage::Summarizing;
                }
                None
            }
            Update::Summary(result) => {
                let notes = self.notes.as_mut()?;
                if notes.summary != SummaryState::Pending {
                    return None;
                }
                self.stage = Stage::Notes;
                Some(match result {
                    Ok(summary) => {
                        notes.summary = SummaryState::Ready(summary);
                        notes.changed();
                        Pill::message("Notes ready in WispX", MessageKind::Success, 3.0)
                    }
                    Err(error) => {
                        notes.summary = SummaryState::Failed(error);
                        Pill::message(
                            "Transcript ready; the summary failed",
                            MessageKind::Warn,
                            3.5,
                        )
                    }
                })
            }
        }
    }
    /// Once every chunk is back, builds the notes and starts formatting them.
    fn drained(&mut self) -> Option<Pill> {
        let total = self.total?;
        if self.stage != Stage::Finishing || self.assembly.received() < total {
            return None;
        }
        self.session = None;
        let lines = self.assembly.lines().to_vec();
        let gaps: Vec<String> = self
            .assembly
            .gaps()
            .map(|g| format!("{}–{}", clock(g.start_ms), clock(g.end_ms)))
            .collect();
        let mut warning = Vec::new();
        if let Some(failure) = &self.failure {
            warning.push(format!("Listening stopped early: {failure}"));
        }
        if !gaps.is_empty() {
            warning.push(format!(
                "Not transcribed: {}. The transcript and summary cover the rest.",
                gaps.join(", ")
            ));
        }
        let incomplete =
            (!gaps.is_empty()).then(|| format!("{} gaps: {}", gaps.len(), gaps.join(", ")));
        let minutes = (self.duration_ms + 30_000) / 60_000;
        self.notes = Some(Notes {
            details: format!(
                "{} · {} · {}",
                self.started_label,
                if minutes == 0 {
                    "under 1 min".into()
                } else {
                    format!("{minutes} min")
                },
                self.sources.join(" + ")
            ),
            raw: self.assembly.text(),
            formatted: None,
            summary: SummaryState::Pending,
            warning: (!warning.is_empty()).then(|| warning.join(" ")),
            saved: None,
            revision: 0,
        });
        if !notes::has_speech(&lines) {
            let (summary, message) = if let Some(reason) = &self.failure {
                (
                    SummaryState::NoTranscript(reason.clone()),
                    format!("Listen stopped. {reason}"),
                )
            } else if !gaps.is_empty() {
                (
                    SummaryState::NoTranscript("No audio parts could be transcribed.".into()),
                    "No audio could be transcribed. See Listen notes.".into(),
                )
            } else {
                (
                    SummaryState::NoSpeech,
                    "Listen didn't hear any speech".into(),
                )
            };
            let notes = self.notes.as_mut().expect("just set");
            notes.summary = summary;
            self.stage = Stage::Notes;
            return Some(Pill::message(message, MessageKind::Warn, 3.0));
        }
        self.stage = Stage::Formatting;
        let dictionary = self.dictionary.clone();
        self.spawn_notes("listen-notes", move |writer, cancel, post| {
            let progress = |done, total| post(Update::Formatting { done, total });
            let Some(formatted) = writer.format(&lines, &dictionary, cancel, &progress) else {
                return;
            };
            let text = formatted.text.clone();
            post(Update::Formatted(formatted));
            if !cancel.cancelled() {
                post(Update::Summary(writer.summarize(
                    &text,
                    incomplete.as_deref(),
                    cancel,
                )));
            }
        });
        None
    }
    /// Runs notes work for this session on its own thread. It posts nothing once the
    /// session is cancelled, and the writer starts no request after that.
    fn spawn_notes(
        &self,
        name: &str,
        work: impl FnOnce(&dyn Writer, &Cancel, &dyn Fn(Update)) + Send + 'static,
    ) {
        let (writer, sink, generation, cancel) = (
            self.writer.clone(),
            self.sink.clone(),
            self.generation,
            self.cancel.clone(),
        );
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let post = |update| {
                    if !cancel.cancelled() {
                        sink(generation, update);
                    }
                };
                work(&*writer, &cancel, &post);
            })
            .expect("spawn Listen notes");
    }
    /// Tries the summary again after it failed, keeping the transcript.
    pub fn retry_summary(&mut self) {
        let Some(notes) = &mut self.notes else {
            return;
        };
        if !matches!(notes.summary, SummaryState::Failed(_)) {
            return;
        }
        // The notes stay open, and closable, while the summary is written again. The
        // saved file is still accurate: a failed summary isn't part of it.
        notes.summary = SummaryState::Pending;
        let text = notes.transcript().to_owned();
        let incomplete = notes.warning.clone();
        self.spawn_notes("listen-summary", move |writer, cancel, post| {
            post(Update::Summary(writer.summarize(
                &text,
                incomplete.as_deref(),
                cancel,
            )));
        });
    }
    /// The tray's Listen item: its label and whether it can be used now.
    pub fn tray_item(&self) -> (&'static str, bool) {
        match self.stage {
            Stage::Setup | Stage::Notes => ("Start listening", true),
            Stage::Listening => ("Stop listening and write notes", true),
            _ => ("Writing notes…", false),
        }
    }
    /// The pill while capturing, distinct from dictation's.
    pub fn pill(&self) -> Option<Pill> {
        self.capturing()
            .then(|| Pill::Listening(self.level().unwrap_or_default()))
    }
    /// Draft lines as (time, text, is a gap) for the live view.
    pub fn draft(&self) -> Vec<(String, String, bool)> {
        self.lines()
            .iter()
            .map(|line| match &line.body {
                Body::Text(text) => (clock(line.start_ms), text.clone(), false),
                Body::Gap(reason) => (
                    clock(line.start_ms),
                    format!(
                        "Not transcribed ({}–{}): {reason}",
                        clock(line.start_ms),
                        clock(line.end_ms)
                    ),
                    true,
                ),
            })
            .collect()
    }
}
fn local_time() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    )
}

#[cfg(test)]
mod tests {
    // Failure modes: a result from a discarded or replaced session changes the new one;
    // notes are built before the last chunk returns; a failed source loses the partial
    // transcript; silence goes to the formatter; a failed summary is reported as done or
    // loses the transcript; a new session silently replaces unsaved notes.
    use super::*;
    use assembly::Outcome;
    use chunker::Part;
    use std::sync::{Mutex, mpsc};

    /// Formats by capitalizing and never reaches the network.
    struct Offline;
    impl Writer for Offline {
        fn transcriber(&self, _: &str) -> Arc<dyn session::Transcriber> {
            unreachable!("tests never start capture")
        }
        fn format(
            &self,
            lines: &[Line],
            _: &[String],
            _: &Cancel,
            progress: &dyn Fn(usize, usize),
        ) -> Option<Formatted> {
            progress(1, 1);
            Some(Formatted {
                text: lines
                    .iter()
                    .map(assembly::render)
                    .collect::<Vec<_>>()
                    .join("\n\n")
                    .to_uppercase(),
                sections: 1,
                ..Formatted::default()
            })
        }
        fn summarize(&self, _: &str, _: Option<&str>, _: &Cancel) -> Result<Summary, String> {
            Err("offline".into())
        }
    }
    /// Reports each request as it starts, holds it until the test lets it finish, then
    /// reports whether the session was cancelled meanwhile, as a request in flight
    /// during Discard would.
    struct Gated {
        entered: Mutex<mpsc::Sender<&'static str>>,
        gate: Mutex<mpsc::Receiver<()>>,
        seen: Mutex<mpsc::Sender<(&'static str, bool)>>,
    }
    impl Gated {
        fn wait(&self, call: &'static str, cancel: &Cancel) {
            let _ = self.entered.lock().unwrap().send(call);
            self.gate.lock().unwrap().recv().unwrap();
            let _ = self.seen.lock().unwrap().send((call, cancel.cancelled()));
        }
    }
    impl Writer for Gated {
        fn transcriber(&self, _: &str) -> Arc<dyn session::Transcriber> {
            unreachable!("tests never start capture")
        }
        fn format(
            &self,
            lines: &[Line],
            _: &[String],
            cancel: &Cancel,
            _: &dyn Fn(usize, usize),
        ) -> Option<Formatted> {
            self.wait("format", cancel);
            Some(Formatted {
                text: lines.iter().map(assembly::render).collect(),
                sections: 1,
                ..Formatted::default()
            })
        }
        fn summarize(&self, _: &str, _: Option<&str>, cancel: &Cancel) -> Result<Summary, String> {
            self.wait("summarize", cancel);
            Err("offline".into())
        }
    }
    type Posted = mpsc::Receiver<(u64, Update)>;
    /// Updates the workers posted, for the test to apply as the UI thread would.
    fn listen_with(writer: Arc<dyn Writer>) -> (Listen, Posted) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |generation, update| {
            let _ = tx.lock().unwrap().send((generation, update));
        });
        (Listen::new(writer, sink), rx)
    }
    fn listen() -> (Listen, Posted) {
        listen_with(Arc::new(Offline))
    }
    /// A Listen whose requests wait on `gate`.
    struct Gates {
        /// Each request as it starts waiting.
        entered: mpsc::Receiver<&'static str>,
        gate: mpsc::Sender<()>,
        /// Each request as it finishes, and whether it saw the cancel.
        seen: mpsc::Receiver<(&'static str, bool)>,
    }
    impl Gates {
        /// Waits until `call` is in flight, so a cancel that follows lands mid-request.
        fn entered(&self, call: &'static str) {
            assert_eq!(self.entered.recv_timeout(WAIT).unwrap(), call);
        }
    }
    fn gated() -> (Listen, Posted, Gates) {
        let (entered_tx, entered) = mpsc::channel();
        let (gate, gate_rx) = mpsc::channel();
        let (seen_tx, seen) = mpsc::channel();
        let (l, posted) = listen_with(Arc::new(Gated {
            entered: Mutex::new(entered_tx),
            gate: Mutex::new(gate_rx),
            seen: Mutex::new(seen_tx),
        }));
        (
            l,
            posted,
            Gates {
                entered,
                gate,
                seen,
            },
        )
    }
    /// Bounds waits for the notes threads only to fail a hung test.
    const WAIT: Duration = Duration::from_secs(60);
    const QUIET: Duration = Duration::from_millis(300);
    /// Records a session up to notes being written, for one spoken chunk.
    fn finished(l: &mut Listen) {
        listening(l);
        let g = l.generation;
        chunk(l, g, 0, Outcome::Text("hello there".into()));
        l.stop();
        ended(l, 1, None);
        assert_eq!(l.stage(), Stage::Formatting);
    }
    /// A session as if capture had started, without devices.
    fn listening(listen: &mut Listen) {
        listen.generation += 1;
        listen.stage = Stage::Listening;
        listen.sources = vec!["Speakers".into()];
    }
    fn part(index: u64) -> Part {
        Part {
            index,
            start_ms: index * 30_000,
            end_ms: index * 30_000 + 30_000,
            overlap: false,
        }
    }
    fn chunk(listen: &mut Listen, generation: u64, index: u64, outcome: Outcome) -> Option<Pill> {
        listen.update(
            generation,
            Update::Session(session::Event::Chunk(part(index), outcome)),
        )
    }
    fn ended(listen: &mut Listen, chunks: u64, failure: Option<&str>) -> Option<Pill> {
        let generation = listen.generation;
        listen.update(
            generation,
            Update::Session(session::Event::Ended {
                chunks,
                duration_ms: chunks * 30_000,
                failure: failure.map(str::to_owned),
            }),
        )
    }

    #[test]
    fn notes_wait_for_every_chunk_and_ignore_stale_sessions() {
        let (mut l, _) = listen();
        listening(&mut l);
        let g = l.generation;
        chunk(&mut l, g, 1, Outcome::Text("second".into()));
        assert!(l.stop());
        assert_eq!(l.stage(), Stage::Finishing);
        ended(&mut l, 2, None);
        assert_eq!(l.stage(), Stage::Finishing, "chunk 0 is still out");
        // A late result from an older session changes nothing.
        chunk(&mut l, g - 1, 0, Outcome::Text("stale".into()));
        assert_eq!(l.stage(), Stage::Finishing);
        chunk(&mut l, g, 0, Outcome::Failed("Timeout".into()));
        assert_eq!(l.stage(), Stage::Formatting);
        let notes = l.notes().unwrap();
        assert_eq!(
            notes.raw,
            "[00:00–00:30] (not transcribed: Timeout)\n\n[00:30] second"
        );
        assert!(notes.warning.as_ref().unwrap().contains("00:00–00:30"));
        assert!(notes.details.contains("Speakers") && notes.details.contains("1 min"));
    }

    #[test]
    fn a_failed_source_keeps_the_partial_transcript() {
        let (mut l, _) = listen();
        listening(&mut l);
        let g = l.generation;
        chunk(&mut l, g, 0, Outcome::Text("before it broke".into()));
        let pill = ended(&mut l, 1, Some("Computer audio stopped: device removed"));
        assert!(matches!(pill, Some(Pill::Message { .. })));
        assert_eq!(l.stage(), Stage::Formatting);
        let notes = l.notes().unwrap();
        assert!(notes.raw.contains("before it broke"));
        assert!(notes.warning.as_ref().unwrap().contains("device removed"));
    }

    #[test]
    fn failed_audio_is_not_classified_as_silence() {
        let (mut l, posted) = listen();
        listening(&mut l);
        let generation = l.generation();
        chunk(
            &mut l,
            generation,
            0,
            Outcome::Failed("No connection".into()),
        );
        let pill = ended(&mut l, 1, None);
        assert!(
            matches!(pill, Some(Pill::Message { text, .. }) if text.contains("could be transcribed"))
        );
        assert!(matches!(
            l.notes().unwrap().summary,
            SummaryState::NoTranscript(_)
        ));
        assert!(l.notes().unwrap().raw.contains("No connection"));
        assert!(
            !l.unsaved(),
            "diagnostics alone don't block another session"
        );
        l.retry_summary();
        assert!(
            posted.try_recv().is_err(),
            "no summary without recovered speech"
        );

        l.close_notes();
        listening(&mut l);
        let pill = ended(&mut l, 0, Some("device removed"));
        assert!(
            matches!(pill, Some(Pill::Message { text, .. }) if text.contains("device removed"))
        );
        assert!(matches!(
            l.notes().unwrap().summary,
            SummaryState::NoTranscript(_)
        ));
        assert!(!l.blocks_quit());
    }

    #[test]
    fn quit_keeps_active_work_and_unsaved_text_open() {
        let (mut l, _) = listen();
        assert!(!l.blocks_quit());
        for stage in [
            Stage::Listening,
            Stage::Finishing,
            Stage::Formatting,
            Stage::Summarizing,
        ] {
            l.stage = stage;
            assert!(l.blocks_quit(), "{stage:?}");
        }
        l.stage = Stage::Finishing;
        ended(&mut l, 0, None);
        assert!(!l.blocks_quit(), "silence has no recovered text to save");
        let notes = l.notes.as_mut().unwrap();
        notes.raw = "[00:00] recovered words".into();
        notes.summary = SummaryState::Failed("summary unavailable".into());
        assert!(l.blocks_quit());
        let save = l.save_request().unwrap();
        assert!(l.mark_saved(save.generation, save.revision, "notes.md".into()));
        assert!(!l.blocks_quit());
    }

    #[test]
    fn silence_skips_formatting_and_summary_failures_keep_the_transcript() {
        let (mut l, posted) = listen();
        listening(&mut l);
        let g = l.generation;
        chunk(&mut l, g, 0, Outcome::Silent);
        l.stop();
        assert!(ended(&mut l, 1, None).is_some());
        assert_eq!(l.stage(), Stage::Notes);
        assert_eq!(l.notes().unwrap().summary, SummaryState::NoSpeech);
        assert!(!l.unsaved());
        assert!(posted.try_recv().is_err(), "nothing was sent to format");

        let (mut l, _) = listen();
        listening(&mut l);
        let g = l.generation;
        l.stage = Stage::Finishing;
        l.notes = Some(Notes {
            details: String::new(),
            raw: "[00:00] raw".into(),
            formatted: None,
            summary: SummaryState::Pending,
            warning: None,
            saved: None,
            revision: 0,
        });
        l.stage = Stage::Formatting;
        l.update(
            g,
            Update::Formatted(Formatted {
                text: "[00:00] Raw.".into(),
                sections: 1,
                ..Formatted::default()
            }),
        );
        assert_eq!(l.stage(), Stage::Summarizing);
        let pill = l.update(g, Update::Summary(Err("ChatGPT took too long".into())));
        assert!(matches!(
            pill,
            Some(Pill::Message {
                kind: MessageKind::Warn,
                ..
            })
        ));
        let notes = l.notes().unwrap();
        assert_eq!(
            notes.summary,
            SummaryState::Failed("ChatGPT took too long".into())
        );
        assert_eq!(notes.transcript(), "[00:00] Raw.");
        assert_eq!(notes.raw, "[00:00] raw");
        assert!(notes.summary_text().is_empty());
        assert!(notes.markdown().contains("[00:00] Raw."));
        // Unsaved notes block a new session until saved or closed.
        assert_eq!(
            l.start(Request {
                output: String::new(),
                output_name: String::new(),
                microphone: None,
                language: String::new(),
                dictionary: Vec::new(),
            })
            .unwrap_err(),
            UNSAVED
        );
        let revision = l.save_request().unwrap().revision;
        assert!(l.mark_saved(g, revision, "notes.md".into()));
        assert!(!l.unsaved());
        assert!(l.mark_saved(g, revision, "other.md".into()));
        l.close_notes();
        assert_eq!(l.stage(), Stage::Setup);
        assert!(l.notes().is_none());
    }

    #[test]
    fn discard_drops_the_session_and_its_late_results() {
        let (mut l, _) = listen();
        listening(&mut l);
        let g = l.generation;
        chunk(&mut l, g, 0, Outcome::Text("secret".into()));
        assert!(l.discard());
        assert_eq!(l.stage(), Stage::Setup);
        assert!(l.lines().is_empty());
        assert!(chunk(&mut l, g, 1, Outcome::Text("late".into())).is_none());
        assert!(ended(&mut l, 2, None).is_none() && l.lines().is_empty());
        assert!(!l.discard(), "nothing left to discard");
    }

    #[test]
    fn file_names_are_safe() {
        let notes = Notes {
            details: "2026-11-14 09:30 · 3 min · Speakers".into(),
            raw: String::new(),
            formatted: None,
            summary: SummaryState::Ready(Summary {
                title: "Q3: plan/review?".into(),
                tldr: "x".into(),
                ..Summary::default()
            }),
            warning: None,
            saved: None,
            revision: 0,
        };
        assert_eq!(notes.file_name(), "2026-11-14 09 30 Q3 plan review.md");
    }

    #[test]
    fn notes_flow_through_formatting_and_summary_for_the_current_session_only() {
        let (mut l, posted) = listen();
        listening(&mut l);
        let g = l.generation;
        chunk(&mut l, g, 0, Outcome::Text("hello there".into()));
        l.stop();
        ended(&mut l, 1, None);
        assert_eq!(l.stage(), Stage::Formatting);
        let mut stages = Vec::new();
        while let Ok((generation, update)) = posted.recv_timeout(WAIT) {
            let last = matches!(update, Update::Summary(_));
            l.update(generation, update);
            stages.push(l.stage());
            if last {
                break;
            }
        }
        assert_eq!(stages.last(), Some(&Stage::Notes));
        assert!(stages.contains(&Stage::Summarizing));
        let notes = l.notes().unwrap();
        assert_eq!(notes.transcript(), "[00:00] HELLO THERE");
        assert_eq!(notes.raw, "[00:00] hello there");
        assert_eq!(notes.summary, SummaryState::Failed("offline".into()));
        // Retrying runs the summary again with the notes still open; notes closed
        // meanwhile ignore its answer.
        l.retry_summary();
        assert_eq!(l.stage(), Stage::Notes);
        assert_eq!(l.notes().unwrap().summary, SummaryState::Pending);
        let (generation, update) = posted.recv_timeout(WAIT).unwrap();
        l.update(generation, update);
        assert_eq!(l.stage(), Stage::Notes);
        l.retry_summary();
        l.close_notes();
        // Either the retry was cancelled before it posted, or its answer is ignored.
        if let Ok((generation, update)) = posted.recv_timeout(QUIET) {
            assert!(l.update(generation, update).is_none());
        }
        assert_eq!(l.stage(), Stage::Setup);
    }

    #[test]
    fn discard_while_formatting_sends_no_summary_and_posts_nothing() {
        let (mut l, posted, gates) = gated();
        finished(&mut l);
        gates.entered("format");
        assert!(l.discard());
        gates.gate.send(()).unwrap();
        // The request in flight finishes, sees the cancel, and nothing follows it.
        assert_eq!(gates.seen.recv_timeout(WAIT).unwrap(), ("format", true));
        assert!(
            gates.entered.recv_timeout(QUIET).is_err(),
            "no summary request"
        );
        assert!(posted.recv_timeout(QUIET).is_err(), "nothing published");
    }

    #[test]
    fn discard_while_summarizing_posts_nothing() {
        let (mut l, posted, gates) = gated();
        finished(&mut l);
        gates.entered("format");
        gates.gate.send(()).unwrap();
        assert_eq!(gates.seen.recv_timeout(WAIT).unwrap(), ("format", false));
        let (generation, update) = posted.recv_timeout(WAIT).unwrap();
        l.update(generation, update);
        assert_eq!(l.stage(), Stage::Summarizing);
        gates.entered("summarize");
        assert!(l.discard());
        gates.gate.send(()).unwrap();
        assert_eq!(gates.seen.recv_timeout(WAIT).unwrap(), ("summarize", true));
        assert!(posted.recv_timeout(QUIET).is_err());
        assert_eq!(l.stage(), Stage::Setup);
    }

    #[test]
    fn closing_or_quitting_cancels_a_retried_summary() {
        for quit in [false, true] {
            let (mut l, posted, gates) = gated();
            finished(&mut l);
            for call in ["format", "summarize"] {
                gates.entered(call);
                gates.gate.send(()).unwrap();
                gates.seen.recv_timeout(WAIT).unwrap();
                let (generation, update) = posted.recv_timeout(WAIT).unwrap();
                l.update(generation, update);
            }
            assert_eq!(
                l.notes().unwrap().summary,
                SummaryState::Failed("offline".into())
            );
            l.retry_summary();
            gates.entered("summarize");
            if quit {
                l.shutdown();
            } else {
                l.close_notes();
            }
            gates.gate.send(()).unwrap();
            assert_eq!(gates.seen.recv_timeout(WAIT).unwrap(), ("summarize", true));
            assert!(posted.recv_timeout(QUIET).is_err());
            assert!(l.notes().is_none());
        }
        // Replacing the whole state, as a new session does, cancels too.
        let (mut l, posted, gates) = gated();
        finished(&mut l);
        gates.entered("format");
        drop(std::mem::replace(&mut l, listen().0));
        gates.gate.send(()).unwrap();
        assert_eq!(gates.seen.recv_timeout(WAIT).unwrap(), ("format", true));
        assert!(posted.recv_timeout(QUIET).is_err());
    }

    #[test]
    fn a_save_counts_only_for_the_notes_it_wrote() {
        let (mut l, _) = listen();
        listening(&mut l);
        let g = l.generation;
        l.stage = Stage::Notes;
        l.notes = Some(Notes {
            details: String::new(),
            raw: "[00:00] raw".into(),
            formatted: None,
            summary: SummaryState::Failed("offline".into()),
            warning: None,
            saved: None,
            revision: 3,
        });
        // Save starts with the transcript-only notes; the retried summary lands first.
        let old = l.save_request().unwrap();
        assert!(!old.text.contains("TL;DR"));
        l.retry_summary();
        let ready = Summary {
            tldr: "Ship it.".into(),
            ..Summary::default()
        };
        l.update(g, Update::Summary(Ok(ready.clone())));
        assert!(!l.mark_saved(old.generation, old.revision, "old.md".into()));
        assert!(l.unsaved(), "the file lacks the summary");
        // Saving the current notes counts.
        let new = l.save_request().unwrap();
        assert!(new.text.contains("Ship it."));
        assert!(l.mark_saved(new.generation, new.revision, "new.md".into()));
        assert!(!l.unsaved());
        // A summary result that isn't awaited changes nothing, saved notes included.
        l.update(g, Update::Summary(Err("late".into())));
        assert_eq!(l.notes().unwrap().summary, SummaryState::Ready(ready));
        assert!(!l.unsaved());
        // An older session's save never marks these notes.
        assert!(!l.mark_saved(g - 1, new.revision, "stale.md".into()));
        assert_eq!(l.notes().unwrap().saved, Some("new.md".into()));
    }

    #[test]
    fn a_summary_after_saving_makes_the_notes_unsaved_again() {
        let (mut l, _) = listen();
        listening(&mut l);
        let g = l.generation;
        l.stage = Stage::Notes;
        l.notes = Some(Notes {
            details: String::new(),
            raw: "[00:00] raw".into(),
            formatted: None,
            summary: SummaryState::Failed("offline".into()),
            warning: None,
            saved: None,
            revision: 0,
        });
        let saved = l.save_request().unwrap();
        assert!(l.mark_saved(g, saved.revision, "notes.md".into()));
        // Retrying leaves the saved file accurate until the summary arrives.
        l.retry_summary();
        assert!(!l.unsaved());
        l.update(
            g,
            Update::Summary(Ok(Summary {
                tldr: "New.".into(),
                ..Summary::default()
            })),
        );
        assert!(l.unsaved());
        assert!(!l.mark_saved(g, saved.revision, "notes.md".into()));
    }

    /// The real cloud round trip with synthetic, nonprivate content: transcription of
    /// `WISPX_LIVE_PCM` (raw 16 kHz mono s16le speech, if given), then formatting and the
    /// summary through the Codex login on this machine:
    ///
    ///     WISPX_LIVE_PCM=speech.raw cargo test --lib listen::tests::live -- --ignored --nocapture
    #[test]
    #[ignore = "sends synthetic audio and text to ChatGPT with this machine's Codex login"]
    fn live_cloud_round_trip() {
        let client = ChatGpt::default();
        let cancel = Cancel::default();
        let mut lines = Vec::new();
        if let Some(path) = std::env::var_os("WISPX_LIVE_PCM") {
            let bytes = std::fs::read(path).expect("speech");
            let pcm: Vec<i16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| i16::from_le_bytes(pair))
                .collect();
            let duration_ms = pcm.len() as u64 * 1000 / 16_000;
            let webm = crate::audio::opus::encode(&pcm).expect("encode");
            let text = Writer::transcriber(&client, "")
                .transcribe(&webm, duration_ms)
                .expect("transcription");
            println!("transcribed: {text}");
            assert!(!text.trim().is_empty());
            lines.push(Line {
                start_ms: 0,
                end_ms: duration_ms,
                body: Body::Text(text),
            });
        }
        let start = lines.last().map_or(0, |l: &Line| l.end_ms);
        for (i, text) in [
            "okay so lets start the main thing today is the launch date for the wisp x onboarding flow",
            "i think thursday still works if the profiling on the sign up form comes back clean",
            "maya said she can run the profile tomorrow morning and share the numbers before lunch",
            "so we keep thursday and sam will email support about the change",
        ]
        .into_iter()
        .enumerate()
        {
            lines.push(Line {
                start_ms: start + i as u64 * 30_000,
                end_ms: start + i as u64 * 30_000 + 30_000,
                body: Body::Text(text.into()),
            });
        }
        let formatted = Writer::format(
            &client,
            &lines,
            &["WispX".into()],
            &cancel,
            &|done, total| println!("formatting {done}/{total}"),
        )
        .expect("not cancelled");
        println!(
            "formatted ({} sections): {}",
            formatted.sections, formatted.text
        );
        assert_eq!(formatted.error, None);
        assert_eq!(formatted.raw_sections, 0);
        let summary = Writer::summarize(&client, &formatted.text, None, &cancel).expect("summary");
        println!("summary: {summary:?}");
        assert!(!summary.tldr.is_empty());
        // Cancelled first: no request is sent at all.
        cancel.cancel();
        assert!(Writer::format(&client, &lines, &[], &cancel, &|_, _| ()).is_none());
        assert!(Writer::summarize(&client, &formatted.text, None, &cancel).is_err());
    }
}
