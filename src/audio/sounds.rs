//! The original deterministic wood-block cues, synthesized without sound assets.
use super::wav;
use std::{f64::consts::PI, fs, io, path::Path};
#[derive(Clone, Copy)]
pub enum Cue {
    Start,
    Stop,
    Cancel,
}
impl Cue {
    pub fn wav(self) -> Vec<u8> {
        let (hz, volume) = match self {
            Self::Start => (220.0, 0.09),
            Self::Stop => (150.0, 0.085),
            Self::Cancel => (120.0, 0.06),
        };
        wood_block(hz, volume)
    }
}
pub fn export(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for (name, cue) in [
        ("start", Cue::Start),
        ("stop", Cue::Stop),
        ("cancel", Cue::Cancel),
    ] {
        fs::write(dir.join(format!("wispx-{name}.wav")), cue.wav())?;
    }
    Ok(())
}

// System.Random(int seed)'s subtractive generator; preserves the C# noise and waveform seeds.
pub(crate) struct DotNetRandom {
    values: [i32; 56],
    next: usize,
    next_p: usize,
}
impl DotNetRandom {
    pub(crate) fn new(seed: i32) -> Self {
        let mut values = [0; 56];
        let mut mj = 161803398 - seed.abs();
        values[55] = mj;
        let mut mk = 1;
        for i in 1..55 {
            let ii = 21 * i % 55;
            values[ii] = mk;
            mk = mj - mk;
            if mk < 0 {
                mk += i32::MAX;
            }
            mj = values[ii];
        }
        for _ in 1..5 {
            for i in 1..56 {
                values[i] -= values[1 + (i + 30) % 55];
                if values[i] < 0 {
                    values[i] += i32::MAX;
                }
            }
        }
        Self {
            values,
            next: 0,
            next_p: 21,
        }
    }
    pub(crate) fn next_double(&mut self) -> f64 {
        self.next = self.next % 55 + 1;
        self.next_p = self.next_p % 55 + 1;
        let mut value = self.values[self.next] - self.values[self.next_p];
        if value == i32::MAX {
            value -= 1;
        }
        if value < 0 {
            value += i32::MAX;
        }
        self.values[self.next] = value;
        f64::from(value) / f64::from(i32::MAX)
    }
}
fn wood_block(hz: f64, volume: f64) -> Vec<u8> {
    const RATE: u32 = 44100;
    let seconds = 0.08;
    let count = (seconds * f64::from(RATE)) as usize;
    let modes = [(1.0, 1.0, 70.0), (2.76, 0.30, 120.0), (5.40, 0.08, 220.0)];
    let mut phases = [0.0; 3];
    let mut rng = DotNetRandom::new(7);
    let (mut noise_lp, mut lp1, mut lp2): (f64, f64, f64) = (0.0, 0.0, 0.0);
    let a_noise = 1.0 - (-2.0 * PI * 1800.0 / f64::from(RATE)).exp();
    let a_out = 1.0 - (-2.0 * PI * 3200.0 / f64::from(RATE)).exp();
    let mut peak: f64 = 1e-9;
    let mut buffer = Vec::with_capacity(count);
    for i in 0..count {
        let t = i as f64 / f64::from(RATE);
        let settle = 1.0 + 0.05 * (-t / 0.004).exp();
        let mut s = 0.0;
        for (m, &(ratio, amp, decay)) in modes.iter().enumerate() {
            phases[m] += 2.0 * PI * hz * ratio * settle / f64::from(RATE);
            s += phases[m].sin() * amp * (-t * decay).exp();
        }
        noise_lp += (rng.next_double() * 2.0 - 1.0 - noise_lp) * a_noise;
        s += noise_lp * 0.9 * (-t / 0.0025).exp();
        let attack = if t < 0.0015 {
            0.5 - 0.5 * (PI * t / 0.0015).cos()
        } else {
            1.0
        };
        s *= attack * ((seconds - t) / 0.012).clamp(0.0, 1.0);
        lp1 += (s - lp1) * a_out;
        lp2 += (lp1 - lp2) * a_out;
        buffer.push(lp2);
        peak = peak.max(lp2.abs());
    }
    wav::encode(
        &buffer
            .iter()
            .map(|v| ((v / peak * volume).clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect::<Vec<_>>(),
        RATE,
    )
}
