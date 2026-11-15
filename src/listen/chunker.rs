//! Cuts the live 16 kHz mix into upload-sized chunks, preferring a pause so no word
//! is split. Without a pause the cut repeats a second of audio in the next chunk,
//! and assembly trims the words both transcripts share.
use crate::audio::recorder::RATE;

const FRAME: usize = RATE as usize / 10;
const MIN_FRAMES: usize = 200;
const MAX_FRAMES: usize = 400;
const PAUSE_FRAMES: usize = 3;
const OVERLAP: usize = RATE as usize;
/// Frame RMS (of i16 full scale) below which a chunk is treated as silence: about -50 dBFS.
const SILENT_RMS: f32 = 100.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Part {
    pub index: u64,
    /// Where this chunk's new audio starts and ends on the session timeline.
    pub start_ms: u64,
    pub end_ms: u64,
    /// The chunk begins with audio already sent at the end of the previous one.
    pub overlap: bool,
}
pub struct Chunk {
    pub part: Part,
    pub pcm: Vec<i16>,
    pub silent: bool,
}
#[derive(Default)]
pub struct Chunker {
    buffer: Vec<i16>,
    /// Timeline sample of `buffer[0]`.
    origin: u64,
    /// Leading samples of `buffer` that the previous chunk already carried.
    overlap: usize,
    /// RMS of each complete 100 ms frame after the overlap.
    frames: Vec<f32>,
    next: u64,
}
fn ms(samples: u64) -> u64 {
    samples * 1000 / u64::from(RATE)
}
fn rms(frame: &[i16]) -> f32 {
    let sum: f64 = frame.iter().map(|&s| f64::from(s).powi(2)).sum();
    (sum / frame.len().max(1) as f64).sqrt() as f32
}
impl Chunker {
    pub fn chunks(&self) -> u64 {
        self.next
    }
    /// Adds live audio and returns any chunks now complete, in order.
    pub fn push(&mut self, pcm: &[i16]) -> Vec<Chunk> {
        self.buffer.extend_from_slice(pcm);
        let mut chunks = Vec::new();
        loop {
            while (self.frames.len() + 1) * FRAME <= self.buffer.len() - self.overlap {
                let start = self.overlap + self.frames.len() * FRAME;
                self.frames.push(rms(&self.buffer[start..start + FRAME]));
            }
            let Some((cut, overlap)) = self.cut_point() else {
                return chunks;
            };
            chunks.push(self.cut(self.overlap + cut * FRAME, overlap));
        }
    }
    /// The rest of the audio, after Stop.
    pub fn finish(&mut self) -> Option<Chunk> {
        (self.buffer.len() > self.overlap).then(|| self.cut(self.buffer.len(), false))
    }
    /// The frame to cut before, and whether the cut falls in speech.
    fn cut_point(&self) -> Option<(usize, bool)> {
        if self.frames.len() < MIN_FRAMES {
            return None;
        }
        let mut sorted = self.frames.clone();
        sorted.sort_by(f32::total_cmp);
        let threshold = (sorted[sorted.len() / 2] * 0.15).clamp(SILENT_RMS, 600.0);
        let window = |start: usize| -> f32 {
            self.frames[start..start + PAUSE_FRAMES].iter().sum::<f32>() / PAUSE_FRAMES as f32
        };
        let candidates = MIN_FRAMES - 1..self.frames.len().min(MAX_FRAMES) - PAUSE_FRAMES + 1;
        if let Some(start) = candidates.clone().find(|&start| {
            self.frames[start..start + PAUSE_FRAMES]
                .iter()
                .all(|&f| f < threshold)
        }) {
            return Some((start + PAUSE_FRAMES / 2 + 1, false));
        }
        if self.frames.len() < MAX_FRAMES {
            return None;
        }
        let quietest = candidates
            .min_by(|&a, &b| window(a).total_cmp(&window(b)))
            .expect("non-empty range");
        Some((quietest + PAUSE_FRAMES / 2 + 1, true))
    }
    fn cut(&mut self, at: usize, overlap: bool) -> Chunk {
        let pcm = self.buffer[..at].to_vec();
        let silent = self.buffer[self.overlap..at]
            .chunks(FRAME)
            .all(|frame| rms(frame) < SILENT_RMS);
        let chunk = Chunk {
            part: Part {
                index: self.next,
                start_ms: ms(self.origin + self.overlap as u64),
                end_ms: ms(self.origin + at as u64),
                overlap: self.overlap > 0,
            },
            pcm,
            silent,
        };
        self.next += 1;
        let keep = if overlap { at - OVERLAP.min(at) } else { at };
        self.buffer.drain(..keep);
        self.origin += keep as u64;
        self.overlap = at - keep;
        self.frames.clear();
        chunk
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: chunks grow without bound in nonstop speech; a cut lands mid-word
    // without overlap; timestamps drift, skip or overlap; silence is uploaded; a pause
    // before the minimum makes tiny chunks; audio after the last cut is lost on Stop;
    // one large push yields only one of several due chunks.
    use super::*;

    fn speech(seconds: f32) -> Vec<i16> {
        (0..(seconds * RATE as f32) as usize)
            .map(|i| ((i as f32 * 0.07).sin() * 8000.0) as i16)
            .collect()
    }
    fn silence(seconds: f32) -> Vec<i16> {
        vec![0; (seconds * RATE as f32) as usize]
    }

    #[test]
    fn cuts_in_the_first_pause_after_the_minimum() {
        let mut c = Chunker::default();
        // An early pause is too soon to cut.
        assert!(
            c.push(&[speech(5.0), silence(1.0), speech(19.0)].concat())
                .is_empty()
        );
        let chunks = c.push(&[silence(0.5), speech(3.0)].concat());
        assert_eq!(chunks.len(), 1);
        let part = chunks[0].part;
        assert!(!part.overlap && !chunks[0].silent);
        assert!((25_000..25_500).contains(&part.end_ms), "{part:?}");
        let rest = c.finish().expect("tail");
        assert_eq!(rest.part.start_ms, part.end_ms);
        assert_eq!(rest.part.end_ms, 28_500);
        assert!(!rest.part.overlap);
        assert!(c.finish().is_none());
    }

    #[test]
    fn nonstop_speech_cuts_at_the_maximum_with_overlap() {
        let mut c = Chunker::default();
        let mut audio = speech(95.0);
        // Slightly quieter stretches are where the forced cuts should land.
        for second in [30, 60] {
            let start = second * RATE as usize;
            for s in &mut audio[start..start + 4800] {
                *s /= 4;
            }
        }
        let chunks = c.push(&audio);
        assert_eq!(chunks.len(), 2, "one push can complete several chunks");
        let (first, second) = (&chunks[0], &chunks[1]);
        assert!(
            (30_000..30_400).contains(&first.part.end_ms),
            "{:?}",
            first.part
        );
        assert!(!first.part.overlap);
        assert!(second.part.overlap);
        assert_eq!(second.part.start_ms, first.part.end_ms);
        assert!(
            (60_000..60_400).contains(&second.part.end_ms),
            "{:?}",
            second.part
        );
        // The repeated second is in the audio, not the timeline.
        assert_eq!(
            second.pcm.len() as u64,
            (second.part.end_ms - second.part.start_ms) * 16 + OVERLAP as u64
        );
        assert_eq!(
            &second.pcm[..OVERLAP],
            &first.pcm[first.pcm.len() - OVERLAP..]
        );
        for chunk in &chunks {
            assert!(chunk.pcm.len() <= (MAX_FRAMES * FRAME) + OVERLAP);
        }
        let tail = c.finish().expect("tail");
        assert_eq!(tail.part.start_ms, second.part.end_ms);
        assert_eq!(tail.part.end_ms, 95_000);
    }

    #[test]
    fn silence_is_marked_and_numbered_in_order() {
        let mut c = Chunker::default();
        let mut parts = Vec::new();
        for _ in 0..10 {
            for chunk in c.push(&silence(10.0)) {
                assert!(chunk.silent);
                parts.push(chunk.part);
            }
        }
        parts.extend(c.finish().map(|chunk| chunk.part));
        assert_eq!(c.chunks(), parts.len() as u64);
        for (i, pair) in parts.windows(2).enumerate() {
            assert_eq!(pair[0].index, i as u64);
            assert_eq!(pair[0].end_ms, pair[1].start_ms);
        }
        assert_eq!(parts.last().unwrap().end_ms, 100_000);
    }
}
