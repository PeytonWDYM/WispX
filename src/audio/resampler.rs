//! Stateful 16 kHz resampling with an anti-alias FIR before interpolation/decimation.
use super::recorder::RATE;
use std::f64::consts::{PI, TAU};

pub(super) struct Resampler {
    coefficients: Vec<f64>,
    history: Vec<f32>,
    cursor: usize,
    input_rate: u32,
    position: u64,
    next_output: u64,
    previous: f64,
}
impl Resampler {
    pub fn new(input_rate: u32) -> Self {
        let coefficients = if input_rate > RATE {
            // A 4 ms Blackman-windowed sinc gives a narrow transition above 7.2 kHz
            // and >40 dB rejection by the output Nyquist frequency at common mic rates.
            // Odd length makes the linear-phase delay exactly half the history (2 ms).
            let taps = (f64::from(input_rate) * 0.004).ceil() as usize | 1;
            let cutoff = 7_200.0 / f64::from(input_rate);
            let middle = (taps - 1) as f64 / 2.0;
            let mut coefficients: Vec<_> = (0..taps)
                .map(|i| {
                    let x = i as f64 - middle;
                    let sinc = if x == 0.0 {
                        2.0 * cutoff
                    } else {
                        (TAU * cutoff * x).sin() / (PI * x)
                    };
                    let phase = TAU * i as f64 / (taps - 1) as f64;
                    sinc * (0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos())
                })
                .collect();
            let sum: f64 = coefficients.iter().sum();
            for coefficient in &mut coefficients {
                *coefficient /= sum;
            }
            coefficients
        } else {
            vec![1.0]
        };
        Self {
            history: vec![0.0; coefficients.len()],
            coefficients,
            cursor: 0,
            input_rate,
            position: 0,
            next_output: 0,
            previous: 0.0,
        }
    }

    /// History and integer timestamp phase persist across CPAL callbacks; no per-sample allocation.
    pub fn push(&mut self, sample: f32, pcm: &mut Vec<i16>) {
        self.history[self.cursor] = sample;
        self.cursor = (self.cursor + 1) % self.history.len();
        // Sinc taps are symmetric: oldest-to-newest history has the same response as reversed taps.
        let (before, after) = self.history.split_at(self.cursor);
        let filtered: f64 = after
            .iter()
            .chain(before)
            .zip(&self.coefficients)
            .map(|(&sample, &coefficient)| f64::from(sample) * coefficient)
            .sum();
        while self.next_output <= self.position {
            // Timestamps use 1/(input_rate * RATE) units, avoiding fractional-rate drift.
            let fraction = 1.0 - (self.position - self.next_output) as f64 / f64::from(RATE);
            let value = self.previous + (filtered - self.previous) * fraction;
            pcm.push((value * 32768.0).clamp(-32768.0, 32767.0) as i16);
            self.next_output += u64::from(self.input_rate);
        }
        self.previous = filtered;
        self.position += u64::from(RATE);
    }
}

#[cfg(test)]
mod tests {
    // DSP failure modes: out-of-band tones alias into speech; passband speech loses gain;
    // fractional rates drift or emit the wrong sample count; callback boundaries reset phase
    // or filter history and click; silence/DC develop offsets or unstable transients.
    use super::*;
    use std::f64::consts::TAU;

    fn tone(rate: u32, frequency: f64) -> Vec<f32> {
        (0..rate)
            .map(|i| (0.5 * (TAU * frequency * f64::from(i) / f64::from(rate)).sin()) as f32)
            .collect()
    }

    fn gain_db(rate: u32, frequency: f64) -> f64 {
        let mut resampler = Resampler::new(rate);
        let mut pcm = Vec::new();
        for sample in tone(rate, frequency) {
            resampler.push(sample, &mut pcm);
        }
        // Ignore startup FIR delay and measure steady-state total energy, including aliases.
        let steady = &pcm[1000..];
        let energy = steady
            .iter()
            .map(|&v| (f64::from(v) / 32768.0).powi(2))
            .sum::<f64>()
            / steady.len() as f64;
        10.0 * (energy / 0.125).log10()
    }

    #[test]
    fn rejects_out_of_band_tones() {
        for rate in [44_100, 48_000, 96_000] {
            let gain = gain_db(rate, 12_000.0);
            assert!(gain <= -40.0, "{rate} Hz: stopband gain {gain:.2} dB");
        }
    }

    #[test]
    fn preserves_speech_gain() {
        for rate in [8_000, 16_000, 44_100, 48_000, 96_000] {
            let gain = gain_db(rate, 1_000.0);
            assert!(gain.abs() <= 1.0, "{rate} Hz: passband gain {gain:.2} dB");
        }
    }

    #[test]
    fn output_count_tracks_fractional_rates() {
        for rate in [8_000, 16_000, 44_100, 48_000, 96_000] {
            for count in [1, 127, rate as usize, rate as usize * 3 + 13] {
                let mut resampler = Resampler::new(rate);
                let mut pcm = Vec::new();
                for _ in 0..count {
                    resampler.push(0.0, &mut pcm);
                }
                // Streaming output covers sample timestamps from zero through the last input.
                let expected =
                    ((count - 1) as u64 * u64::from(RATE) / u64::from(rate) + 1) as usize;
                assert_eq!(pcm.len(), expected, "rate {rate}, count {count}");
                assert!(pcm.iter().all(|&v| v == 0));
            }
        }
    }

    #[test]
    fn callback_boundaries_preserve_phase_and_history() {
        let input = tone(44_100, 1_000.0);
        let mut whole = Resampler::new(44_100);
        let mut whole_pcm = Vec::new();
        for &sample in &input {
            whole.push(sample, &mut whole_pcm);
        }
        for size in [1, 7, 128, 511] {
            let mut split = Resampler::new(44_100);
            let mut split_pcm = Vec::new();
            for callback in input.chunks(size) {
                for &sample in callback {
                    split.push(sample, &mut split_pcm);
                }
            }
            assert_eq!(whole_pcm, split_pcm, "callback size {size}");
        }
        let steady = &whole_pcm[1000..];
        assert!(
            steady
                .windows(2)
                .all(|w| (i32::from(w[1]) - i32::from(w[0])).abs() < 7000)
        );

        let mut dc = Resampler::new(48_000);
        let mut dc_pcm = Vec::new();
        for _ in 0..48_000 {
            dc.push(0.25, &mut dc_pcm);
        }
        assert!(
            dc_pcm[1000..]
                .iter()
                .all(|&v| (i32::from(v) - 8192).abs() <= 1)
        );
    }
}
