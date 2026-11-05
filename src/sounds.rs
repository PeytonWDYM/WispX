//! The "wood block" cues from the Windows app, synthesized in memory so there
//! are no sound files to ship. Real wood rings at a few inharmonic overtones
//! that die out fast, highest first, and the mallet adds a tiny muffled tap.

use crate::audio::wav;
use std::f64::consts::PI;

const RATE: u32 = 44_100;
const SECONDS: f64 = 0.08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Start,
    Stop,
    Cancel,
}

impl Cue {
    pub const ALL: [Cue; 3] = [Cue::Start, Cue::Stop, Cue::Cancel];

    pub fn file_name(self) -> &'static str {
        match self {
            Cue::Start => "cue-start.wav",
            Cue::Stop => "cue-stop.wav",
            Cue::Cancel => "cue-cancel.wav",
        }
    }

    pub fn wav(self) -> Vec<u8> {
        match self {
            Cue::Start => wood_block(220.0, 0.09), // A little higher: "on".
            Cue::Stop => wood_block(150.0, 0.085), // Deeper: "done".
            Cue::Cancel => wood_block(120.0, 0.06), // Deepest and quietest.
        }
    }
}

fn wood_block(hz: f64, volume: f64) -> Vec<u8> {
    let rate = f64::from(RATE);
    let total = (SECONDS * rate) as usize;
    // (frequency ratio, loudness, decay per second). Short and dry: no tail.
    let modes = [(1.00, 1.00, 70.0), (2.76, 0.30, 120.0), (5.40, 0.08, 220.0)];
    let mut phases = [0.0f64; 3];
    let mut noise = Noise(7);
    let (mut noise_lp, mut lp1, mut lp2) = (0.0f64, 0.0f64, 0.0f64);
    let a_noise = 1.0 - (-2.0 * PI * 1800.0 / rate).exp();
    let a_out = 1.0 - (-2.0 * PI * 3200.0 / rate).exp(); // Soft, not clacky.
    const ATTACK: f64 = 0.0015;

    let mut samples = Vec::with_capacity(total);
    let mut peak = 1e-9f64;
    for i in 0..total {
        let t = i as f64 / rate;
        let settle = 1.0 + 0.05 * (-t / 0.004).exp(); // The wood gives when struck.
        let mut s = 0.0;
        for (phase, (ratio, amplitude, decay)) in phases.iter_mut().zip(modes) {
            *phase += 2.0 * PI * hz * ratio * settle / rate;
            s += phase.sin() * amplitude * (-t * decay).exp();
        }
        noise_lp += (noise.next() - noise_lp) * a_noise;
        s += noise_lp * 0.9 * (-t / 0.0025).exp(); // Mallet tap.

        let attack = if t < ATTACK {
            0.5 - 0.5 * (PI * t / ATTACK).cos()
        } else {
            1.0
        };
        let tail = ((SECONDS - t) / 0.012).clamp(0.0, 1.0);
        s *= attack * tail;

        lp1 += (s - lp1) * a_out;
        lp2 += (lp1 - lp2) * a_out;
        samples.push(lp2);
        peak = peak.max(lp2.abs());
    }

    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|value| {
            let scaled = (value / peak * volume).clamp(-1.0, 1.0) * 32767.0;
            (scaled as i16).to_le_bytes()
        })
        .collect();
    wav::encode_mono(&pcm, RATE)
}

/// A small deterministic generator: the mallet tap only needs to be noise-like
/// and every daemon start should sound the same.
struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f64 {
        // xorshift32
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        f64::from(self.0) / f64::from(u32::MAX) * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(wav: &[u8]) -> Vec<i16> {
        let (samples, _) = wav[44..].as_chunks::<2>();
        samples.iter().map(|b| i16::from_le_bytes(*b)).collect()
    }

    #[test]
    fn cues_are_short_valid_mono_wavs() {
        for cue in Cue::ALL {
            let bytes = cue.wav();
            let info = wav::parse(&bytes).unwrap();
            assert_eq!(info.sample_rate, RATE);
            assert_eq!(info.channels, 1);
            assert_eq!(info.bits_per_sample, 16);
            assert_eq!(info.data_bytes as usize, (SECONDS * 44_100.0) as usize * 2);
        }
    }

    #[test]
    fn loudness_is_normalized_and_quiet() {
        let peak = |cue: Cue| {
            samples(&cue.wav())
                .iter()
                .map(|s| i32::from(*s).abs())
                .max()
                .unwrap()
        };
        let close = |actual: i32, volume: f64| (f64::from(actual) / 32767.0 - volume).abs() < 0.001;
        assert!(close(peak(Cue::Start), 0.09));
        assert!(close(peak(Cue::Stop), 0.085));
        assert!(close(peak(Cue::Cancel), 0.06));
    }

    #[test]
    fn cues_fade_to_silence_without_a_click() {
        for cue in Cue::ALL {
            let all = samples(&cue.wav());
            let last = all[all.len() - 1];
            assert!(i32::from(last).abs() < 100, "{cue:?} ends at {last}");
        }
    }

    #[test]
    fn synthesis_is_deterministic_and_cues_differ() {
        assert_eq!(Cue::Start.wav(), Cue::Start.wav());
        assert_ne!(Cue::Start.wav(), Cue::Stop.wav());
    }
}
