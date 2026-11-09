//! Plays the synthesized cues through the default CPAL output (PipeWire, Pulse or ALSA).
use crate::audio::sounds::Cue;
use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const CUE_RATE: u32 = 44_100;

/// Cues are queued to one worker so a burst never opens overlapping streams.
pub fn play(cue: Cue) {
    static WORKER: OnceLock<Mutex<mpsc::Sender<Cue>>> = OnceLock::new();
    let sender = WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Cue>();
        thread::Builder::new()
            .name("cue-sounds".into())
            .spawn(move || {
                for cue in rx {
                    if let Err(error) = render(&samples(cue)) {
                        eprintln!("Sound: {error}");
                    }
                }
            })
            .expect("spawn sound worker");
        Mutex::new(tx)
    });
    let _ = sender.lock().expect("sound sender").send(cue);
}

/// Decodes the cue's mono 16-bit RIFF data back to floating point.
fn samples(cue: Cue) -> Vec<f32> {
    cue.wav()[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| f32::from(i16::from_le_bytes(b)) / 32768.0)
        .collect()
}

/// Linear interpolation is ample for an 80 ms percussive cue.
pub(crate) fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let count = (input.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    (0..count)
        .map(|i| {
            let position = i as f64 * f64::from(from) / f64::from(to);
            let index = position as usize;
            let fraction = (position - index as f64) as f32;
            let a = input[index.min(input.len() - 1)];
            let b = input[(index + 1).min(input.len() - 1)];
            a + (b - a) * fraction
        })
        .collect()
}

fn render(mono: &[f32]) -> Result<(), Box<dyn std::error::Error>> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no output device")?;
    let supported = device.default_output_config()?;
    let config = supported.config();
    let pcm = Arc::new(resample(mono, CUE_RATE, config.sample_rate.0));
    let finished = Arc::new(AtomicBool::new(false));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, pcm.clone(), finished.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, pcm.clone(), finished.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, pcm.clone(), finished.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, pcm.clone(), finished.clone()),
        cpal::SampleFormat::U8 => build::<u8>(&device, &config, pcm.clone(), finished.clone()),
        cpal::SampleFormat::F64 => build::<f64>(&device, &config, pcm.clone(), finished.clone()),
        other => return Err(format!("unsupported output format {other}").into()),
    }?;
    stream.play()?;
    let seconds = pcm.len() as f64 / f64::from(config.sample_rate.0);
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(seconds + 0.5);
    while !finished.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    // Let the device drain its last buffer before the stream closes.
    thread::sleep(Duration::from_millis(60));
    Ok(())
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    pcm: Arc<Vec<f32>>,
    finished: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let channels = usize::from(config.channels);
    let mut position = 0;
    device.build_output_stream(
        config,
        move |buffer: &mut [T], _| {
            for frame in buffer.chunks_mut(channels) {
                let value = pcm.get(position).copied().unwrap_or(0.0);
                position += 1;
                frame.fill(T::from_sample(value));
            }
            if position >= pcm.len() {
                finished.store(true, Ordering::Release);
            }
        },
        |error| eprintln!("Sound: {error}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    // Failure modes: cue length scales the wrong way; resampling reads past
    // the end; the RIFF header is played as audio.
    use super::*;

    #[test]
    fn resampling_keeps_duration_and_endpoints() {
        let input = [0.0, 1.0, 0.0, -1.0];
        let up = resample(&input, 44_100, 88_200);
        assert_eq!(up.len(), 8);
        assert_eq!(up[1], 0.5);
        assert_eq!(resample(&input, 48_000, 48_000), input);
        let cue = samples(Cue::Start);
        assert_eq!(cue.len(), (0.08 * f64::from(CUE_RATE)) as usize);
        assert!(cue.iter().all(|s| s.abs() <= 1.0));
    }
}
