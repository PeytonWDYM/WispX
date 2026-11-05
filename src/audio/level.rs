//! Loudness metering, ported from the Windows recorder so "did we hear
//! anything?" keeps the same meaning.

/// 30 ms at 16 kHz, the buffer size the meter was tuned for.
const FRAME_SAMPLES: usize = 480;

#[derive(Debug, Default)]
pub struct LevelMeter {
    /// A sample split across two reads.
    odd_byte: Option<u8>,
    sum_squares: f64,
    samples: usize,
    level: f32,
    peak: f32,
}

impl LevelMeter {
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut bytes = bytes;
        if let (Some(low), Some((&high, rest))) = (self.odd_byte, bytes.split_first()) {
            self.add(i16::from_le_bytes([low, high]));
            self.odd_byte = None;
            bytes = rest;
        }
        let (samples, rest) = bytes.as_chunks::<2>();
        for sample in samples {
            self.add(i16::from_le_bytes(*sample));
        }
        if let [last] = rest {
            self.odd_byte = Some(*last);
        }
    }

    fn add(&mut self, sample: i16) {
        let value = f64::from(sample) / 32768.0;
        self.sum_squares += value * value;
        self.samples += 1;
        if self.samples == FRAME_SAMPLES {
            let rms = (self.sum_squares / FRAME_SAMPLES as f64).sqrt();
            self.level = normalize(rms);
            self.peak = self.peak.max(self.level);
            self.sum_squares = 0.0;
            self.samples = 0;
        }
    }

    /// The most recent frame, from 0 (below -52 dBFS) to 1 (-12 dBFS or above).
    pub fn level(&self) -> f32 {
        self.level
    }

    /// The loudest frame so far.
    pub fn peak(&self) -> f32 {
        self.peak
    }
}

fn normalize(rms: f64) -> f32 {
    let db = 20.0 * (rms + 1e-9).log10();
    ((db + 52.0) / 40.0).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(amplitude: f64, samples: usize) -> Vec<u8> {
        (0..samples)
            .flat_map(|n| {
                let value = amplitude * (TAU * 440.0 * n as f64 / 16_000.0).sin();
                ((value * 32767.0) as i16).to_le_bytes()
            })
            .collect()
    }

    #[test]
    fn digital_silence_never_rises_above_zero() {
        let mut meter = LevelMeter::default();
        meter.feed(&vec![0u8; 16_000]);
        assert_eq!(meter.peak(), 0.0);
        assert_eq!(meter.level(), 0.0);
    }

    #[test]
    fn speech_level_clears_the_default_threshold_and_room_noise_does_not() {
        let mut speech = LevelMeter::default();
        speech.feed(&tone(0.1, 8_000));
        assert!(speech.peak() > 0.12, "peak {}", speech.peak());

        // About -60 dBFS: a quiet room with the mic open.
        let mut room = LevelMeter::default();
        room.feed(&tone(0.001, 8_000));
        assert!(room.peak() < 0.12, "peak {}", room.peak());
    }

    #[test]
    fn peak_survives_later_quiet_while_level_follows_it() {
        let mut meter = LevelMeter::default();
        meter.feed(&tone(0.5, 960));
        let loud = meter.peak();
        meter.feed(&vec![0u8; 1920]);
        assert_eq!(meter.peak(), loud);
        assert_eq!(meter.level(), 0.0);
    }

    #[test]
    fn full_scale_saturates_at_one() {
        let mut meter = LevelMeter::default();
        meter.feed(&tone(1.0, 960));
        assert_eq!(meter.peak(), 1.0);
    }

    #[test]
    fn reads_split_mid_sample_give_the_same_result() {
        let bytes = tone(0.3, 4_800);
        let mut whole = LevelMeter::default();
        whole.feed(&bytes);
        let mut split = LevelMeter::default();
        for chunk in bytes.chunks(7) {
            split.feed(chunk);
        }
        assert_eq!(whole.peak(), split.peak());
        assert_eq!(whole.level(), split.level());
    }

    #[test]
    fn nothing_is_reported_until_a_full_frame_arrives() {
        let mut meter = LevelMeter::default();
        meter.feed(&tone(1.0, FRAME_SAMPLES - 1));
        assert_eq!(meter.peak(), 0.0);
    }
}
