//! Listen's audio: what the computer plays (WASAPI loopback, or a PulseAudio monitor)
//! plus an optional microphone. Each source is converted to 16 kHz mono by its backend
//! and queued; a wall-clock mixer lines the queues up, so a source that stalls (loopback
//! sends nothing during silence) becomes silence instead of shifting the other.
//!
//! Memory stays bounded: each queue holds at most `MAX_BUFFERED`, and the mixer never
//! pads more than `MAX_GAP` of silence at once. Exceeding either (WispX fell behind, or
//! the computer slept) ends the capture with a reason after the audio already queued,
//! so the notes say the session is incomplete instead of losing words silently.
use super::recorder::{Levels, RATE};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Jitter allowance: the mix trails real time by this much so late callbacks still fit.
pub const LATENCY: Duration = Duration::from_millis(300);
/// More queued than this means a source clock runs fast: the mix runs ahead of the wall
/// clock to take it down to `LATENCY`, padding the other sources, so nothing is dropped.
const MAX_QUEUED: usize = RATE as usize;
const LATENCY_SAMPLES: usize = (RATE as u64 * LATENCY.as_millis() as u64 / 1000) as usize;
/// Hard bound on one source's queue: audio the mixer hasn't taken for this long means
/// it can't keep up.
pub const MAX_BUFFERED: usize = 20 * RATE as usize;
/// The most silence padded in one pull; more means time jumped (sleep, or a stall).
pub const MAX_GAP: Duration = Duration::from_secs(10);
const MAX_GAP_SAMPLES: u64 = MAX_GAP.as_secs() * RATE as u64;
const OVERRUN: &str = "audio arrived faster than WispX could process it";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    /// What settings store and the backend opens.
    pub id: String,
    /// What people see.
    pub name: String,
}
#[derive(Debug, Default)]
pub struct Devices {
    pub outputs: Vec<Device>,
    pub microphones: Vec<Device>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Output,
    Microphone,
}
/// A message written for people, naming what to do.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct CaptureError(pub String);

/// The real devices this platform can capture for Listen.
pub fn devices() -> Result<Devices, CaptureError> {
    #[cfg(windows)]
    {
        wasapi::devices()
    }
    #[cfg(target_os = "linux")]
    {
        super::pulse::devices()
    }
}

#[derive(Default)]
struct Feed {
    samples: VecDeque<i16>,
    failure: Option<String>,
}
/// The producing half of a source, handed to its backend's thread.
#[derive(Clone)]
pub(super) struct Input {
    feed: Arc<Mutex<Feed>>,
    levels: Levels,
}
impl Input {
    /// Queues audio, or fails the source once its queue would pass `MAX_BUFFERED`.
    pub(super) fn push(&self, pcm: &[i16]) {
        let sum = pcm.iter().map(|&s| (f64::from(s) / 32768.0).powi(2)).sum();
        self.levels.update(sum, pcm.len());
        let mut feed = self.feed.lock().expect("capture lock");
        if feed.failure.is_some() {
            return;
        }
        if feed.samples.len() + pcm.len() > MAX_BUFFERED {
            feed.failure = Some(OVERRUN.into());
            return;
        }
        feed.samples.extend(pcm);
    }
    pub(super) fn fail(&self, message: impl Into<String>) {
        let mut feed = self.feed.lock().expect("capture lock");
        feed.failure.get_or_insert_with(|| message.into());
    }
    #[cfg(target_os = "linux")]
    pub(super) fn failure(&self) -> Option<String> {
        self.feed.lock().expect("capture lock").failure.clone()
    }
}
struct Source {
    kind: Kind,
    input: Input,
    /// Stops the backend when dropped.
    _backend: Box<dyn Send>,
}
pub struct Capture {
    sources: Vec<Source>,
    started: Instant,
    emitted: u64,
    levels: Levels,
    /// Why the capture ended; reported after the audio queued before it.
    failure: Option<CaptureError>,
}
impl Capture {
    /// Opens the output and, when asked, the microphone. Never substitutes a device:
    /// an unavailable choice is an error naming it.
    pub fn start(output: &str, microphone: Option<&str>) -> Result<Self, CaptureError> {
        let mut sources = vec![open(Kind::Output, output)?];
        if let Some(microphone) = microphone {
            sources.push(open(Kind::Microphone, microphone)?);
        }
        Ok(Self::from_sources(sources, Instant::now()))
    }
    fn from_sources(sources: Vec<Source>, started: Instant) -> Self {
        Self {
            sources,
            started,
            emitted: 0,
            levels: Levels::default(),
            failure: None,
        }
    }
    /// The mix's level, then each source's, in start order.
    pub fn levels(&self) -> (Levels, Vec<Levels>) {
        (
            self.levels.clone(),
            self.sources
                .iter()
                .map(|s| s.input.levels.clone())
                .collect(),
        )
    }
    /// Milliseconds of mixed audio emitted so far: the session timeline.
    pub fn position_ms(&self) -> u64 {
        self.emitted * 1000 / u64::from(RATE)
    }
    /// A failure deferred by the last pull while it delivered retained audio.
    pub fn failure(&self) -> Option<&CaptureError> {
        self.failure.as_ref()
    }
    /// Mixed samples due by `now`. `flush` also takes everything still queued, for Stop.
    /// A failure ends the capture: this pull still returns all the audio queued before
    /// it, and the next returns the reason.
    pub fn pull(&mut self, now: Instant, flush: bool) -> Result<Vec<i16>, CaptureError> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let elapsed = now.saturating_duration_since(self.started);
        let wall = (elapsed.saturating_sub(LATENCY).as_millis() as u64) * u64::from(RATE) / 1000;
        let mut feeds = Vec::with_capacity(self.sources.len());
        let mut queued = 0;
        let mut failure = None;
        for source in &self.sources {
            let feed = source.input.feed.lock().expect("capture lock");
            if let Some(reason) = &feed.failure
                && failure.is_none()
            {
                let what = match source.kind {
                    Kind::Output => "Computer audio",
                    Kind::Microphone => "Microphone",
                };
                failure = Some(CaptureError(format!("{what} stopped: {reason}")));
            }
            queued = queued.max(feed.samples.len() as u64);
            feeds.push(feed);
        }
        let padding = wall.saturating_sub(self.emitted).saturating_sub(queued);
        if failure.is_none() && padding > MAX_GAP_SAMPLES {
            failure = Some(CaptureError(format!(
                "No audio was captured for {} seconds (the computer may have been asleep).",
                padding / u64::from(RATE)
            )));
        }
        let count = if flush || failure.is_some() {
            // Everything queued, and no padding past it.
            queued
        } else if queued > MAX_QUEUED as u64 {
            // A fast source clock: take it down to the latency allowance.
            (wall.saturating_sub(self.emitted)).max(queued - LATENCY_SAMPLES as u64)
        } else {
            wall.saturating_sub(self.emitted)
        } as usize;
        let mut mixed = vec![0_i32; count];
        for feed in &mut feeds {
            let take = count.min(feed.samples.len());
            for (slot, sample) in mixed.iter_mut().zip(feed.samples.drain(..take)) {
                *slot += i32::from(sample);
            }
        }
        drop(feeds);
        self.emitted += count as u64;
        self.failure = failure;
        let pcm: Vec<i16> = mixed
            .into_iter()
            .map(|s| s.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16)
            .collect();
        let sum = pcm.iter().map(|&s| (f64::from(s) / 32768.0).powi(2)).sum();
        if !pcm.is_empty() {
            self.levels.update(sum, pcm.len());
        }
        Ok(pcm)
    }
}
/// Feeds a capture by hand, for tests of what consumes it.
#[cfg(test)]
pub struct Feeder(Input);
#[cfg(test)]
impl Feeder {
    pub fn push(&self, pcm: &[i16]) {
        self.0.push(pcm);
    }
    pub fn fail(&self, message: &str) {
        self.0.fail(message);
    }
    /// The producing half, to hand to a real backend in tests.
    #[cfg(target_os = "linux")]
    pub(super) fn input(&self) -> Input {
        self.0.clone()
    }
    /// Samples the mixer hasn't taken yet.
    pub fn queued(&self) -> usize {
        self.0.feed.lock().expect("capture lock").samples.len()
    }
}
#[cfg(test)]
impl Capture {
    pub fn fed() -> (Self, Feeder) {
        let input = Input {
            feed: Arc::default(),
            levels: Levels::default(),
        };
        let source = Source {
            kind: Kind::Output,
            input: input.clone(),
            _backend: Box::new(()),
        };
        (
            Self::from_sources(vec![source], Instant::now()),
            Feeder(input),
        )
    }
}
fn open(kind: Kind, id: &str) -> Result<Source, CaptureError> {
    let input = Input {
        feed: Arc::default(),
        levels: Levels::default(),
    };
    #[cfg(windows)]
    let backend = wasapi::open(kind, id, input.clone())?;
    #[cfg(target_os = "linux")]
    let backend = super::pulse::open(kind, id, input.clone())?;
    Ok(Source {
        kind,
        input,
        _backend: backend,
    })
}

/// Windows: an output device opened for input is a WASAPI loopback stream (CPAL sets
/// AUDCLNT_STREAMFLAGS_LOOPBACK for render endpoints), so it hears every app.
#[cfg(windows)]
mod wasapi {
    use super::{CaptureError, Device, Devices, Input, Kind};
    use crate::audio::resampler::Resampler;
    use cpal::{
        Sample, SampleFormat, SizedSample,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };
    use std::{sync::mpsc, thread};

    pub(super) fn devices() -> Result<Devices, CaptureError> {
        let host = cpal::default_host();
        let list = |devices: Box<dyn Iterator<Item = cpal::Device>>| {
            devices
                .filter_map(|d| d.name().ok())
                .map(|name| Device {
                    id: name.clone(),
                    name,
                })
                .collect()
        };
        let failed = |e: cpal::DevicesError| CaptureError(e.to_string());
        Ok(Devices {
            outputs: list(Box::new(host.output_devices().map_err(failed)?)),
            microphones: list(Box::new(host.input_devices().map_err(failed)?)),
        })
    }
    /// Keeps CPAL's thread-affine stream on its own thread until dropped.
    struct Handle(#[allow(dead_code)] mpsc::Sender<()>);

    pub(super) fn open(kind: Kind, id: &str, input: Input) -> Result<Box<dyn Send>, CaptureError> {
        let id = id.to_owned();
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("listen-capture".into())
            .spawn(move || match stream(kind, &id, input) {
                Ok(stream) => {
                    let _ = ready.send(Ok(()));
                    // Returns once the handle drops its sender.
                    let _ = stopped.recv();
                    drop(stream);
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|e| CaptureError(e.to_string()))?;
        started
            .recv()
            .map_err(|_| CaptureError("Audio capture stopped".into()))??;
        Ok(Box::new(Handle(stop)))
    }
    fn stream(kind: Kind, id: &str, input: Input) -> Result<cpal::Stream, CaptureError> {
        let host = cpal::default_host();
        let failed = |e: cpal::DevicesError| CaptureError(e.to_string());
        let (devices, default, what): (Box<dyn Iterator<Item = cpal::Device>>, _, _) = match kind {
            Kind::Output => (
                Box::new(host.output_devices().map_err(failed)?),
                host.default_output_device(),
                "output",
            ),
            Kind::Microphone => (
                Box::new(host.input_devices().map_err(failed)?),
                host.default_input_device(),
                "microphone",
            ),
        };
        let device = if id.is_empty() {
            default.ok_or_else(|| CaptureError(format!("No {what} found")))?
        } else {
            devices
                .into_iter()
                .find(|d| d.name().is_ok_and(|n| n == id))
                .ok_or_else(|| {
                    CaptureError(format!("“{id}” isn't connected. Choose another {what}."))
                })?
        };
        let supported = match kind {
            Kind::Output => device.default_output_config(),
            Kind::Microphone => device.default_input_config(),
        }
        .map_err(|e| CaptureError(e.to_string()))?;
        let config = supported.config();
        let built = match supported.sample_format() {
            SampleFormat::I16 => build::<i16>(&device, &config, input),
            SampleFormat::I32 => build::<i32>(&device, &config, input),
            SampleFormat::U16 => build::<u16>(&device, &config, input),
            SampleFormat::F32 => build::<f32>(&device, &config, input),
            SampleFormat::F64 => build::<f64>(&device, &config, input),
            _ => return Err(CaptureError("Unsupported audio format".into())),
        };
        let stream = built.map_err(|e| CaptureError(e.to_string()))?;
        stream.play().map_err(|e| CaptureError(e.to_string()))?;
        Ok(stream)
    }
    fn build<T: SizedSample + Sample>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        input: Input,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        f32: cpal::FromSample<T>,
    {
        let channels = usize::from(config.channels);
        let mut resampler = Resampler::new(config.sample_rate.0);
        let mut pcm = Vec::new();
        let failure = input.clone();
        device.build_input_stream(
            config,
            move |buffer: &[T], _| {
                pcm.clear();
                for frame in buffer.chunks_exact(channels) {
                    let mono =
                        frame.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() / channels as f32;
                    resampler.push(mono, &mut pcm);
                }
                input.push(&pcm);
            },
            move |error| failure.fail(error.to_string()),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: a stalled source (silent loopback) delays the other instead of
    // reading as silence; sources are zipped by arrival rather than time; a fast clock
    // grows a queue without bound; Stop drops queued audio; a failed source keeps
    // capturing silently; mixing overflows instead of clipping.
    use super::*;

    fn source(kind: Kind) -> (Source, Input) {
        let input = Input {
            feed: Arc::default(),
            levels: Levels::default(),
        };
        (
            Source {
                kind,
                input: input.clone(),
                _backend: Box::new(()),
            },
            input,
        )
    }
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn mixes_on_the_wall_clock_and_pads_a_stalled_source() {
        let start = Instant::now();
        let (output, out) = source(Kind::Output);
        let (mic, mine) = source(Kind::Microphone);
        let mut capture = Capture::from_sources(vec![output, mic], start);
        // Inside the jitter allowance nothing is due yet.
        assert!(capture.pull(start + ms(200), false).unwrap().is_empty());
        mine.push(&[100; 3200]);
        // The output sent nothing (silent loopback); the mic still comes through on time.
        let first = capture.pull(start + LATENCY + ms(200), false).unwrap();
        assert_eq!(first.len(), 3200);
        assert!(first.iter().all(|&s| s == 100));
        out.push(&[50; 1600]);
        mine.push(&[100; 1600]);
        let second = capture.pull(start + LATENCY + ms(300), false).unwrap();
        assert_eq!(second, vec![150; 1600]);
        assert_eq!(capture.position_ms(), 300);
    }

    #[test]
    fn clips_catches_up_with_fast_clocks_and_flushes_on_stop() {
        let start = Instant::now();
        let (output, out) = source(Kind::Output);
        let (mic, mine) = source(Kind::Microphone);
        let mut capture = Capture::from_sources(vec![output, mic], start);
        out.push(&[30_000; 1600]);
        mine.push(&[30_000; 1600]);
        let loud = capture.pull(start + LATENCY + ms(100), false).unwrap();
        assert!(loud.iter().all(|&s| s == i16::MAX));
        // A queue beyond a second is mixed ahead of the clock down to the latency
        // allowance; none of it is dropped.
        out.push(&vec![1; MAX_QUEUED + 5000]);
        let ahead = capture.pull(start + LATENCY + ms(100), false).unwrap();
        assert_eq!(ahead.len(), MAX_QUEUED + 5000 - LATENCY_SAMPLES);
        assert!(ahead.iter().all(|&s| s == 1));
        // Stop takes what is still queued, padding the shorter source.
        mine.push(&[2; 100]);
        let rest = capture.pull(start + LATENCY + ms(100), true).unwrap();
        assert_eq!(rest.len(), LATENCY_SAMPLES);
        assert_eq!(rest[0], 3);
        assert_eq!(rest[LATENCY_SAMPLES - 1], 1);
        assert_eq!(
            capture.position_ms(),
            (1600 + MAX_QUEUED as u64 + 5000) * 1000 / u64::from(RATE)
        );
    }

    #[test]
    fn a_failed_source_ends_the_capture_after_its_queued_audio() {
        let start = Instant::now();
        let (output, out) = source(Kind::Output);
        let mut capture = Capture::from_sources(vec![output], start);
        out.push(&[7; 800]);
        out.fail("device removed");
        out.fail("later errors don't replace the first");
        out.push(&[9; 800]);
        assert_eq!(capture.pull(start + ms(900), false).unwrap(), vec![7; 800]);
        let error = capture.pull(start + ms(1000), false).unwrap_err();
        assert_eq!(error.0, "Computer audio stopped: device removed");
        assert!(capture.pull(start + ms(1100), true).is_err());
    }

    #[test]
    fn an_overrun_source_fails_instead_of_growing_or_dropping_words() {
        let start = Instant::now();
        let (output, out) = source(Kind::Output);
        let (mic, mine) = source(Kind::Microphone);
        let mut capture = Capture::from_sources(vec![output, mic], start);
        // The mixer stalls while the sources keep sending.
        for _ in 0..MAX_BUFFERED / 1600 {
            out.push(&[1; 1600]);
        }
        out.push(&[1; 1600]);
        mine.push(&[2; 1600]);
        let output_len = out.feed.lock().unwrap().samples.len();
        assert_eq!(output_len, MAX_BUFFERED, "the queue stops at its bound");
        // Everything kept is delivered, mixed on time with the other source; then the
        // reason ends the capture.
        let kept = capture.pull(start + ms(400), false).unwrap();
        assert_eq!(kept.len(), MAX_BUFFERED);
        assert!(kept[..1600].iter().all(|&s| s == 3));
        assert!(kept[1600..].iter().all(|&s| s == 1));
        let error = capture.pull(start + ms(500), false).unwrap_err();
        assert!(
            error.0.contains("faster than WispX could process"),
            "{error}"
        );
    }

    #[test]
    fn a_clock_jump_is_reported_without_padding_hours_of_silence() {
        let start = Instant::now();
        let (output, out) = source(Kind::Output);
        let mut capture = Capture::from_sources(vec![output], start);
        out.push(&[5; 1600]);
        assert_eq!(
            capture
                .pull(start + LATENCY + ms(100), false)
                .unwrap()
                .len(),
            1600
        );
        out.push(&[6; 3200]);
        // Three hours later: only what was queued comes back, then the reason.
        let after = capture
            .pull(start + Duration::from_secs(3 * 3600), false)
            .unwrap();
        assert_eq!(after, vec![6; 3200]);
        let error = capture
            .pull(start + Duration::from_secs(3 * 3600), false)
            .unwrap_err();
        assert!(error.0.contains("No audio was captured for"), "{error}");
        // A gap within the allowance is still padded as silence, in time.
        let (output, _out) = source(Kind::Output);
        let mut capture = Capture::from_sources(vec![output], start);
        let silence = capture.pull(start + LATENCY + MAX_GAP, false).unwrap();
        assert_eq!(silence.len() as u64, MAX_GAP_SAMPLES);
        assert!(silence.iter().all(|&s| s == 0));
    }
}
