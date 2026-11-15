//! A live Listen session's threads: one captures and chunks the mix, two upload chunks
//! from a bounded queue. Nothing here touches the UI; every result is posted through
//! the sink, tagged with its chunk so the UI can order and count them.
use super::{
    assembly::Outcome,
    chunker::{Chunker, Part},
};
use crate::{
    audio::{
        capture::{Capture, CaptureError},
        opus,
        recorder::Levels,
    },
    transcribe::{ChatGpt, TranscribeError},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Encoded chunks allowed to wait for upload: several minutes of audio, a few MB.
pub const QUEUE_LIMIT: usize = 12;
const WORKERS: usize = 2;
const TICK: Duration = Duration::from_millis(100);
const ATTEMPTS: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Chunk(Part, Outcome),
    /// Chunks waiting to upload changed.
    Backlog(usize),
    /// Capture is over: Stop, or a source failed. `chunks` results will arrive in total.
    Ended {
        chunks: u64,
        duration_ms: u64,
        failure: Option<String>,
    },
}
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Job {
    pub part: Part,
    /// `None` for a silent chunk, which needs no upload.
    pub audio: Option<Vec<u8>>,
    pub duration_ms: u64,
}
#[derive(Default)]
struct Queue {
    state: Mutex<(VecDeque<Job>, bool)>,
    ready: Condvar,
}
impl Queue {
    /// Hands back the job when the queue is full.
    fn push(&self, job: Job) -> Result<usize, Job> {
        let mut state = self.state.lock().expect("queue lock");
        if state.0.len() >= QUEUE_LIMIT {
            return Err(job);
        }
        state.0.push_back(job);
        self.ready.notify_one();
        Ok(state.0.len())
    }
    fn close(&self) {
        self.state.lock().expect("queue lock").1 = true;
        self.ready.notify_all();
    }
    /// Waits for work; `None` once closed and drained, or cancelled.
    fn pop(&self, cancelled: &AtomicBool) -> Option<(Job, usize)> {
        let mut state = self.state.lock().expect("queue lock");
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if let Some(job) = state.0.pop_front() {
                let left = state.0.len();
                return Some((job, left));
            }
            if state.1 {
                return None;
            }
            state = self.ready.wait(state).expect("queue lock");
        }
    }
}

/// Transcribes one chunk: retries brief outages, treats too-short audio as no speech.
pub trait Transcriber: Send + Sync + 'static {
    fn transcribe(&self, audio: &[u8], duration_ms: u64) -> Result<String, TranscribeError>;
}
pub struct Cloud {
    pub client: ChatGpt,
    pub language: String,
}
impl Transcriber for Cloud {
    fn transcribe(&self, audio: &[u8], duration_ms: u64) -> Result<String, TranscribeError> {
        self.client.transcribe(audio, duration_ms, &self.language)
    }
}
fn retryable(error: &TranscribeError) -> bool {
    matches!(
        error,
        TranscribeError::Connection
            | TranscribeError::Timeout
            | TranscribeError::Overloaded
            | TranscribeError::RateLimit
            | TranscribeError::UnexpectedReply
            | TranscribeError::Server { .. }
    )
}
fn upload(
    job: &Job,
    transcriber: &dyn Transcriber,
    cancelled: &AtomicBool,
    pause: Duration,
) -> Outcome {
    let Some(audio) = &job.audio else {
        return Outcome::Silent;
    };
    let mut attempt = 0;
    loop {
        match transcriber.transcribe(audio, job.duration_ms) {
            Ok(text) => return Outcome::Text(text),
            Err(TranscribeError::TooShort) => return Outcome::Text(String::new()),
            Err(error) if retryable(&error) && attempt + 1 < ATTEMPTS => {
                attempt += 1;
                let until = Instant::now() + pause * attempt;
                while Instant::now() < until {
                    if cancelled.load(Ordering::Acquire) {
                        return Outcome::Failed(error.to_string());
                    }
                    thread::sleep(TICK.min(pause));
                }
            }
            Err(error) => return Outcome::Failed(error.to_string()),
        }
    }
}
fn work(
    queue: &Queue,
    transcriber: &dyn Transcriber,
    cancelled: &AtomicBool,
    sink: &Sink,
    pause: Duration,
) {
    while let Some((job, left)) = queue.pop(cancelled) {
        sink(Event::Backlog(left));
        let outcome = upload(&job, transcriber, cancelled, pause);
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        sink(Event::Chunk(job.part, outcome));
    }
}

/// Owns the threads. Dropping it cancels: capture stops at once, queued uploads are
/// abandoned, and in-flight replies are discarded by the receiver's session check.
pub struct Session {
    stop: Option<mpsc::Sender<()>>,
    cancelled: Arc<AtomicBool>,
    queue: Arc<Queue>,
    /// The mix, then each source.
    pub levels: (Levels, Vec<Levels>),
}
impl Session {
    pub fn start(
        output: &str,
        microphone: Option<&str>,
        transcriber: Arc<dyn Transcriber>,
        sink: Sink,
    ) -> Result<Self, CaptureError> {
        let capture = Capture::start(output, microphone)?;
        Ok(Self::run(
            capture,
            transcriber,
            sink,
            Duration::from_secs(4),
        ))
    }
    fn run(
        capture: Capture,
        transcriber: Arc<dyn Transcriber>,
        sink: Sink,
        pause: Duration,
    ) -> Self {
        let levels = capture.levels();
        let cancelled = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(Queue::default());
        let (stop, stopped) = mpsc::channel();
        {
            let (queue, cancelled, sink) = (queue.clone(), cancelled.clone(), sink.clone());
            thread::Builder::new()
                .name("listen-chunks".into())
                .spawn(move || record(capture, &stopped, &queue, &cancelled, &sink))
                .expect("spawn Listen capture");
        }
        for _ in 0..WORKERS {
            let (queue, cancelled, sink, transcriber) = (
                queue.clone(),
                cancelled.clone(),
                sink.clone(),
                transcriber.clone(),
            );
            thread::Builder::new()
                .name("listen-upload".into())
                .spawn(move || work(&queue, &*transcriber, &cancelled, &sink, pause))
                .expect("spawn Listen upload");
        }
        Self {
            stop: Some(stop),
            cancelled,
            queue,
            levels,
        }
    }
    /// Ends capture; the rest of the audio and queued chunks still finish.
    pub fn stop(&mut self) {
        self.stop.take();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.stop.take();
        self.queue.close();
    }
}
fn record(
    mut capture: Capture,
    stopped: &mpsc::Receiver<()>,
    queue: &Queue,
    cancelled: &AtomicBool,
    sink: &Sink,
) {
    let mut chunker = Chunker::default();
    let enqueue = |chunker_chunk: super::chunker::Chunk| {
        let part = chunker_chunk.part;
        let duration_ms = chunker_chunk.pcm.len() as u64 * 1000 / 16_000;
        let audio = if chunker_chunk.silent {
            None
        } else {
            match opus::encode(&chunker_chunk.pcm) {
                Ok(webm) => Some(webm),
                Err(error) => {
                    sink(Event::Chunk(
                        part,
                        Outcome::Failed(format!("couldn't encode audio ({error})")),
                    ));
                    return;
                }
            }
        };
        match queue.push(Job {
            part,
            audio,
            duration_ms,
        }) {
            Ok(waiting) => sink(Event::Backlog(waiting)),
            Err(job) => sink(Event::Chunk(job.part, Outcome::Skipped)),
        }
    };
    let failure = loop {
        let stopping = matches!(
            stopped.recv_timeout(TICK),
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected)
        );
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        match capture.pull(Instant::now(), stopping) {
            Ok(pcm) => {
                for chunk in chunker.push(&pcm) {
                    enqueue(chunk);
                }
                if stopping {
                    break capture.failure().map(|error| error.0.clone());
                }
            }
            Err(error) => break Some(error.0),
        }
    };
    if let Some(chunk) = chunker.finish() {
        enqueue(chunk);
    }
    let duration_ms = capture.position_ms();
    drop(capture);
    queue.close();
    sink(Event::Ended {
        chunks: chunker.chunks(),
        duration_ms,
        failure,
    });
}

#[cfg(test)]
mod tests {
    // Failure modes: a full queue blocks capture or loses a chunk without a gap;
    // transient errors fail a chunk at once while permanent ones retry; too-short
    // audio counts as a failure; Stop loses queued work; cancel keeps uploading or
    // posts results; the end count disagrees with the results posted.
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Script {
        calls: AtomicUsize,
        replies: Mutex<VecDeque<Result<String, TranscribeError>>>,
    }
    impl Transcriber for Script {
        fn transcribe(&self, _: &[u8], _: u64) -> Result<String, TranscribeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("ok".into()))
        }
    }
    fn script(replies: Vec<Result<String, TranscribeError>>) -> Script {
        Script {
            calls: AtomicUsize::new(0),
            replies: Mutex::new(replies.into()),
        }
    }
    fn job(index: u64) -> Job {
        Job {
            part: Part {
                index,
                start_ms: index * 1000,
                end_ms: index * 1000 + 1000,
                overlap: false,
            },
            audio: Some(vec![1]),
            duration_ms: 1000,
        }
    }

    #[test]
    fn retries_brief_outages_only() {
        let never = AtomicBool::new(false);
        let s = script(vec![
            Err(TranscribeError::Connection),
            Err(TranscribeError::Overloaded),
            Ok("hi".into()),
        ]);
        assert_eq!(
            upload(&job(0), &s, &never, Duration::ZERO),
            Outcome::Text("hi".into())
        );
        assert_eq!(s.calls.load(Ordering::SeqCst), 3);
        let s = script((0..3).map(|_| Err(TranscribeError::Timeout)).collect());
        assert_eq!(
            upload(&job(0), &s, &never, Duration::ZERO),
            Outcome::Failed("ChatGPT took too long".into())
        );
        let s = script(vec![Err(TranscribeError::LoginRevoked)]);
        assert!(matches!(
            upload(&job(0), &s, &never, Duration::ZERO),
            Outcome::Failed(_)
        ));
        assert_eq!(s.calls.load(Ordering::SeqCst), 1);
        let s = script(vec![Err(TranscribeError::TooShort)]);
        assert_eq!(
            upload(&job(0), &s, &never, Duration::ZERO),
            Outcome::Text(String::new())
        );
        let silent = Job {
            audio: None,
            ..job(0)
        };
        assert_eq!(upload(&silent, &s, &never, Duration::ZERO), Outcome::Silent);
    }

    #[test]
    fn a_full_queue_refuses_instead_of_blocking_and_drains_after_close() {
        let queue = Queue::default();
        for i in 0..QUEUE_LIMIT as u64 {
            assert!(queue.push(job(i)).is_ok());
        }
        assert_eq!(queue.push(job(99)).err().map(|j| j.part.index), Some(99));
        queue.close();
        let never = AtomicBool::new(false);
        let drained: Vec<u64> =
            std::iter::from_fn(|| queue.pop(&never).map(|(j, _)| j.part.index)).collect();
        assert_eq!(drained, (0..QUEUE_LIMIT as u64).collect::<Vec<_>>());
        let cancelled = AtomicBool::new(true);
        let queue = Queue::default();
        assert!(queue.push(job(0)).is_ok());
        assert!(queue.pop(&cancelled).is_none());
    }

    #[test]
    fn workers_post_every_result_and_nothing_after_cancel() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink: Sink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let queue = Queue::default();
        for i in 0..5 {
            queue.push(job(i)).ok().unwrap();
        }
        queue.close();
        let never = AtomicBool::new(false);
        work(&queue, &script(vec![]), &never, &sink, Duration::ZERO);
        let chunks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::Chunk(..)))
            .count();
        assert_eq!(chunks, 5);

        events.lock().unwrap().clear();
        let queue = Queue::default();
        queue.push(job(0)).ok().unwrap();
        struct CancelDuringUpload(Arc<AtomicBool>);
        impl Transcriber for CancelDuringUpload {
            fn transcribe(&self, _: &[u8], _: u64) -> Result<String, TranscribeError> {
                self.0.store(true, Ordering::SeqCst);
                Ok("late".into())
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        work(
            &queue,
            &CancelDuringUpload(cancelled.clone()),
            &cancelled,
            &sink,
            Duration::ZERO,
        );
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Chunk(..)))
        );
    }

    fn collect() -> (Sink, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        (
            Arc::new(move |event| drop(tx.lock().unwrap().send(event))),
            rx,
        )
    }
    /// Bounds a wait on the session's threads only to fail a hung test.
    const HANG: Duration = Duration::from_secs(60);
    fn speech(seconds: usize) -> Vec<i16> {
        (0..seconds * 16_000)
            .map(|i| ((i as f32 * 0.07).sin() * 8000.0) as i16)
            .collect()
    }

    #[test]
    fn stop_drains_every_chunk_and_reports_the_total() {
        let (capture, feed) = Capture::fed();
        let (sink, events) = collect();
        let mut session = Session::run(
            capture,
            Arc::new(script(vec![Err(TranscribeError::Refused)])),
            sink,
            Duration::ZERO,
        );
        // Faster than real time, in bursts the capture's bounded queue accepts. Encoding
        // is slow in debug builds on a busy machine, so each burst waits for the last to
        // be taken, bounded only to fail a hung test.
        for burst in [speech(30), vec![0; 8000], speech(12)]
            .concat()
            .chunks(160_000)
        {
            let deadline = Instant::now() + HANG;
            while feed.queued() > 16_000 {
                assert!(Instant::now() < deadline, "capture stopped taking audio");
                thread::sleep(Duration::from_millis(20));
            }
            feed.push(burst);
        }
        session.stop();
        let mut chunks = Vec::new();
        let mut ended = None;
        while ended.is_none_or(|total| chunks.len() < total as usize) {
            // Every thread holds the sink, so this also fails once they have all exited.
            match events
                .recv_timeout(HANG)
                .expect("session posted every result")
            {
                Event::Chunk(part, outcome) => chunks.push((part.index, outcome)),
                Event::Ended {
                    chunks: total,
                    duration_ms,
                    failure,
                } => {
                    assert_eq!(failure, None);
                    assert_eq!(duration_ms, 42_500);
                    ended = Some(total);
                }
                Event::Backlog(_) => {}
            }
        }
        assert_eq!(ended, Some(2));
        chunks.sort_by_key(|c| c.0);
        assert_eq!(chunks.len(), 2);
        // One chunk failed permanently, one transcribed; both are accounted for.
        assert_eq!(
            chunks
                .iter()
                .filter(|c| matches!(c.1, Outcome::Failed(_)))
                .count(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|c| c.1 == Outcome::Text("ok".into()))
                .count(),
            1
        );
    }

    #[test]
    fn stop_preserves_a_failure_detected_by_the_final_pull() {
        let (capture, feed) = Capture::fed();
        feed.push(&speech(2));
        feed.fail("device removed");
        let (sink, events) = collect();
        let (stop, stopped) = mpsc::channel();
        drop(stop);
        let queue = Queue::default();
        let cancelled = AtomicBool::new(false);
        record(capture, &stopped, &queue, &cancelled, &sink);
        let ended = events
            .try_iter()
            .find(|event| matches!(event, Event::Ended { .. }));
        assert!(matches!(ended, Some(Event::Ended {
            chunks: 1,
            duration_ms: 2000,
            failure: Some(reason),
        }) if reason.contains("device removed")));
        let (job, _) = queue.pop(&cancelled).expect("retained audio still uploads");
        assert_eq!(job.duration_ms, 2000);
        assert!(job.audio.is_some());
    }

    #[test]
    fn a_failed_source_ends_capture_with_its_reason_and_cancel_posts_nothing() {
        let (capture, feed) = Capture::fed();
        feed.push(&speech(2));
        feed.fail("device removed");
        let (sink, events) = collect();
        let _session = Session::run(capture, Arc::new(script(vec![])), sink, Duration::ZERO);
        let ended = std::iter::from_fn(|| events.recv_timeout(HANG).ok())
            .find(|e| matches!(e, Event::Ended { .. }));
        assert!(
            matches!(ended, Some(Event::Ended { failure: Some(f), .. }) if f.contains("device removed"))
        );

        let (capture, feed) = Capture::fed();
        feed.push(&speech(45));
        let (sink, events) = collect();
        drop(Session::run(
            capture,
            Arc::new(script(vec![])),
            sink,
            Duration::ZERO,
        ));
        thread::sleep(Duration::from_millis(300));
        assert!(
            events
                .try_iter()
                .all(|e| !matches!(e, Event::Chunk(..) | Event::Ended { .. }))
        );
    }
}
