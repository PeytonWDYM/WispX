use crate::audio::sounds::DotNetRandom;

pub struct Waveform {
    pub heights: [f64; 15],
    pub alphas: [f64; 15],
    seed: [f64; 15],
    smooth: f64,
    pub time: f64,
}
impl Default for Waveform {
    fn default() -> Self {
        let mut rng = DotNetRandom::new(7);
        Self {
            heights: [3.0; 15],
            alphas: [1.0; 15],
            seed: std::array::from_fn(|_| rng.next_double() * std::f64::consts::TAU),
            smooth: 0.0,
            time: 0.0,
        }
    }
}
impl Waveform {
    pub fn frame(&mut self, dt: f64, level: f64, recording: bool, transcribing: bool) {
        self.time += dt;
        self.smooth +=
            (level - self.smooth) * (dt * if level > self.smooth { 22.0 } else { 9.0 }).min(1.0);
        for i in 0..15 {
            let center = 1.0 - (i as f64 - 7.0).abs() / 7.0;
            let (target, alpha) = if recording {
                let envelope = 0.28 + 0.72 * center.powf(0.9);
                let wobble =
                    0.55 + 0.45 * (self.time * (5.5 + (i % 4) as f64 * 1.3) + self.seed[i]).sin();
                let breathe = 1.2 * (0.5 + 0.5 * (self.time * 2.4 + i as f64 * 0.5).sin());
                (
                    3.0 + breathe + 21.0 * self.smooth * envelope * wobble,
                    0.55 + 0.45 * (self.smooth * 2.0 + center * 0.4).min(1.0),
                )
            } else if transcribing {
                let wave = 0.5 + 0.5 * (self.time * 7.5 - i as f64 * 0.62).sin();
                (3.0 + 7.0 * wave * wave, 0.35 + 0.65 * wave)
            } else {
                (3.0, 0.6)
            };
            self.heights[i] += (target - self.heights[i]) * (dt * 18.0).min(1.0);
            self.alphas[i] = alpha;
        }
    }
}
