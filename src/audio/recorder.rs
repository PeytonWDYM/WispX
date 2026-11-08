//! CPAL capture downmixes and resamples to 16 kHz mono i16 PCM.
use cpal::{
    Sample, SampleFormat, SizedSample, Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
};

pub const RATE: u32 = 16_000;
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("No microphone found")]
    NoDevice,
    #[error("{0}")]
    Devices(#[from] cpal::DevicesError),
    #[error("{0}")]
    Name(#[from] cpal::DeviceNameError),
    #[error("{0}")]
    Config(#[from] cpal::DefaultStreamConfigError),
    #[error("{0}")]
    Build(#[from] cpal::BuildStreamError),
    #[error("{0}")]
    Play(#[from] cpal::PlayStreamError),
    #[error("Unsupported microphone sample format")]
    Format,
    #[error("Microphone stopped: {0}")]
    Stream(String),
}
/// Lock-free level reads shared by the recording pill and the mic-test meter.
#[derive(Clone, Default)]
pub struct Levels {
    level: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
}
impl Levels {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }
    fn update(&self, sum: f64, count: usize) {
        let rms = if count == 0 {
            0.0
        } else {
            (sum / count as f64).sqrt()
        };
        let level = ((20.0 * (rms + 1e-9).log10() + 52.0) / 40.0).clamp(0.0, 1.0) as f32;
        self.level.store(level.to_bits(), Ordering::Relaxed);
        self.peak.fetch_max(level.to_bits(), Ordering::Relaxed);
    }
}
pub struct Recording {
    pub pcm: Vec<i16>,
    pub peak: f32,
    pub duration_ms: u64,
}
/// Owns a capture thread, keeping CPAL's thread-affine stream off the UI thread.
pub struct Recorder {
    command: Option<mpsc::Sender<CaptureCommand>>,
    finished: mpsc::Receiver<Result<Recording, RecordError>>,
    pub levels: Levels,
}
enum CaptureCommand {
    Stop,
    Cancel,
}
struct CaptureStream {
    stream: Stream,
    data: Arc<Mutex<Capture>>,
    levels: Levels,
}
struct Capture {
    pcm: Vec<i16>,
    input_rate: f64,
    position: u64,
    next_output: f64,
    previous: f32,
    failure: Option<String>,
}
impl Capture {
    // Linear interpolation retains phase across callbacks and across channel frames.
    fn push(&mut self, sample: f32) {
        let position = self.position as f64;
        while self.next_output <= position {
            let fraction = (self.next_output - (position - 1.0)).clamp(0.0, 1.0) as f32;
            let value = self.previous + (sample - self.previous) * fraction;
            self.pcm
                .push((value.clamp(-1.0, 1.0) * 32768.0).clamp(-32768.0, 32767.0) as i16);
            self.next_output += self.input_rate / f64::from(RATE);
        }
        self.previous = sample;
        self.position += 1;
    }
}
pub fn input_devices() -> Result<Vec<String>, RecordError> {
    cpal::default_host()
        .input_devices()?
        .map(|d| d.name().map_err(RecordError::from))
        .collect()
}
impl Recorder {
    pub fn start(name: &str) -> Result<Self, RecordError> {
        let name = name.to_owned();
        let levels = Levels::default();
        let worker_levels = levels.clone();
        let (command, commands) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        thread::spawn(move || match CaptureStream::open(&name, worker_levels) {
            Ok(capture) => {
                let _ = ready.send(Ok(()));
                if matches!(commands.recv(), Ok(CaptureCommand::Stop)) {
                    let _ = done.send(capture.stop());
                }
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        });
        started
            .recv()
            .map_err(|_| RecordError::Stream("Capture thread stopped".into()))??;
        Ok(Self {
            command: Some(command),
            finished,
            levels,
        })
    }
    pub fn stop(mut self) -> Result<Recording, RecordError> {
        if let Some(command) = self.command.take() {
            command
                .send(CaptureCommand::Stop)
                .map_err(|_| RecordError::Stream("Capture thread stopped".into()))?;
        }
        self.finished
            .recv()
            .map_err(|_| RecordError::Stream("Capture thread stopped".into()))?
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(command) = self.command.take() {
            let _ = command.send(CaptureCommand::Cancel);
        }
    }
}
impl CaptureStream {
    fn open(name: &str, levels: Levels) -> Result<Self, RecordError> {
        let host = cpal::default_host();
        let devices: Vec<_> = host.input_devices()?.collect();
        let selected = if name.is_empty() {
            None
        } else {
            devices
                .iter()
                .find(|d| d.name().is_ok_and(|n| n == name))
                .or_else(|| {
                    devices
                        .iter()
                        .find(|d| d.name().is_ok_and(|n| n.starts_with(name)))
                })
                .cloned()
        };
        let device = selected
            .or_else(|| host.default_input_device())
            .ok_or(RecordError::NoDevice)?;
        let supported = device.default_input_config()?;
        let config = supported.config();
        let data = Arc::new(Mutex::new(Capture {
            pcm: Vec::new(),
            input_rate: f64::from(config.sample_rate.0),
            position: 0,
            next_output: 0.0,
            previous: 0.0,
            failure: None,
        }));
        let stream = match supported.sample_format() {
            SampleFormat::I8 => build::<i8>(&device, &config, &data, &levels),
            SampleFormat::I16 => build::<i16>(&device, &config, &data, &levels),
            SampleFormat::I32 => build::<i32>(&device, &config, &data, &levels),
            SampleFormat::I64 => build::<i64>(&device, &config, &data, &levels),
            SampleFormat::U8 => build::<u8>(&device, &config, &data, &levels),
            SampleFormat::U16 => build::<u16>(&device, &config, &data, &levels),
            SampleFormat::U32 => build::<u32>(&device, &config, &data, &levels),
            SampleFormat::U64 => build::<u64>(&device, &config, &data, &levels),
            SampleFormat::F32 => build::<f32>(&device, &config, &data, &levels),
            SampleFormat::F64 => build::<f64>(&device, &config, &data, &levels),
            _ => return Err(RecordError::Format),
        }?;
        stream.play()?;
        Ok(Self {
            stream,
            data,
            levels,
        })
    }
    fn stop(self) -> Result<Recording, RecordError> {
        drop(self.stream);
        let mut data = self.data.lock().expect("audio callback does not panic");
        if let Some(error) = data.failure.take() {
            return Err(RecordError::Stream(error));
        }
        let pcm = std::mem::take(&mut data.pcm);
        let duration_ms = pcm.len() as u64 * 1000 / u64::from(RATE);
        Ok(Recording {
            pcm,
            peak: self.levels.peak(),
            duration_ms,
        })
    }
}
fn build<T: SizedSample + Sample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    data: &Arc<Mutex<Capture>>,
    levels: &Levels,
) -> Result<Stream, cpal::BuildStreamError>
where
    f32: cpal::FromSample<T>,
{
    let capture = data.clone();
    let failure = data.clone();
    let levels = levels.clone();
    let channels = usize::from(config.channels);
    device.build_input_stream(
        config,
        move |buffer: &[T], _| {
            let mut data = capture.lock().expect("audio callback does not panic");
            let start = data.pcm.len();
            for frame in buffer.chunks_exact(channels) {
                let mono =
                    frame.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() / channels as f32;
                data.push(mono);
            }
            let samples = &data.pcm[start..];
            let sum = samples
                .iter()
                .map(|&s| (f64::from(s) / 32768.0).powi(2))
                .sum();
            levels.update(sum, samples.len());
        },
        move |error| {
            failure
                .lock()
                .expect("audio callback does not panic")
                .failure = Some(error.to_string());
        },
        None,
    )
}
