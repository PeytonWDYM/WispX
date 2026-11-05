//! RIFF/WAVE for the one format every part of WispX uses: 16-bit PCM.

pub const SAMPLE_RATE: u32 = 16_000;
pub const BYTES_PER_SECOND: u64 = SAMPLE_RATE as u64 * 2;

const FORMAT_PCM: u16 = 1;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Wraps little-endian mono s16 samples in a canonical 44-byte header.
pub fn encode_mono(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    // Cannot be reached with bounded recordings; never emit a corrupt header.
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX - 36);
    let pcm = &pcm[..data_len as usize];
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

pub fn duration_ms(data_bytes: u64) -> u64 {
    data_bytes * 1000 / BYTES_PER_SECOND
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub data_bytes: u64,
}

impl WavInfo {
    pub fn is_dictation_format(&self) -> bool {
        self.sample_rate == SAMPLE_RATE && self.channels == 1 && self.bits_per_sample == 16
    }

    pub fn duration_ms(&self) -> u64 {
        duration_ms(self.data_bytes)
    }
}

/// Reads the structure of a WAV file, walking chunks instead of assuming the
/// 44-byte layout because tools commonly insert `LIST` before `data`.
pub fn parse(bytes: &[u8]) -> Result<WavInfo, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }
    let mut format = None;
    let mut position = 12usize;
    while let Some(header) = bytes.get(position..position + 8) {
        let id = &header[0..4];
        let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let body_start = position + 8;
        match id {
            b"fmt " => {
                let body = bytes
                    .get(body_start..body_start + size)
                    .filter(|body| body.len() >= 16)
                    .ok_or("truncated `fmt ` chunk")?;
                format = Some(parse_format(body)?);
            }
            b"data" => {
                let (sample_rate, channels, bits_per_sample) =
                    format.ok_or("`data` chunk appears before `fmt `")?;
                // Streaming writers leave a placeholder size, so trust the file.
                let available = bytes.len() - body_start;
                return Ok(WavInfo {
                    sample_rate,
                    channels,
                    bits_per_sample,
                    data_bytes: size.min(available) as u64,
                });
            }
            _ => {}
        }
        // Chunks are padded to even sizes.
        position = body_start
            .checked_add(size)
            .and_then(|end| end.checked_add(size & 1))
            .ok_or("chunk size overflows")?;
    }
    Err("no `data` chunk found".to_string())
}

fn parse_format(body: &[u8]) -> Result<(u32, u16, u16), String> {
    let word = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
    let mut tag = word(0);
    if tag == FORMAT_EXTENSIBLE && body.len() >= 26 {
        // The real format is the first two bytes of the sub-format GUID.
        tag = word(24);
    }
    if tag != FORMAT_PCM {
        return Err(format!("unsupported encoding (format tag {tag})"));
    }
    let channels = word(2);
    let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    Ok((rate, channels, word(14)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_chunks(format: &[u8], extra: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(format.len() as u32).to_le_bytes());
        out.extend_from_slice(format);
        out.extend_from_slice(extra);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn pcm_format(rate: u32, channels: u16, bits: u16) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * channels as u32 * bits as u32 / 8).to_le_bytes());
        f.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f
    }

    #[test]
    fn encoded_header_matches_the_canonical_layout() {
        let wav = encode_mono(&[1, 0, 2, 0], SAMPLE_RATE);
        assert_eq!(wav.len(), 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 40);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
        assert_eq!(&wav[44..], &[1, 0, 2, 0]);
    }

    #[test]
    fn encoded_output_parses_as_dictation_format() {
        let pcm = vec![0u8; 32_000];
        let info = parse(&encode_mono(&pcm, SAMPLE_RATE)).unwrap();
        assert!(info.is_dictation_format());
        assert_eq!(info.data_bytes, 32_000);
        assert_eq!(info.duration_ms(), 1_000);
    }

    #[test]
    fn walks_past_extra_chunks_including_odd_padding() {
        let mut extra = Vec::new();
        extra.extend_from_slice(b"LIST");
        extra.extend_from_slice(&3u32.to_le_bytes());
        extra.extend_from_slice(b"abc\0");
        let wav = with_chunks(&pcm_format(16_000, 1, 16), &extra, &[0; 8]);
        assert_eq!(parse(&wav).unwrap().data_bytes, 8);
    }

    #[test]
    fn reports_other_formats_so_callers_can_explain_conversion() {
        let wav = with_chunks(&pcm_format(44_100, 2, 16), &[], &[0; 8]);
        let info = parse(&wav).unwrap();
        assert!(!info.is_dictation_format());
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn a_placeholder_data_size_is_clamped_to_the_file() {
        let mut wav = encode_mono(&[0; 10], SAMPLE_RATE);
        wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&wav).unwrap().data_bytes, 10);
    }

    #[test]
    fn extensible_pcm_is_accepted_and_float_is_not() {
        let extensible = |sub_format: u16| {
            let mut f = pcm_format(16_000, 1, 16);
            f[0..2].copy_from_slice(&FORMAT_EXTENSIBLE.to_le_bytes());
            f.extend_from_slice(&[22, 0, 16, 0, 0, 0, 0, 0]);
            f.extend_from_slice(&sub_format.to_le_bytes());
            f.extend_from_slice(&[0; 14]);
            f
        };
        assert!(parse(&with_chunks(&extensible(1), &[], &[0; 2])).is_ok());
        assert!(parse(&with_chunks(&extensible(3), &[], &[0; 2])).is_err());
    }

    #[test]
    fn malformed_files_are_errors_not_panics() {
        assert!(parse(b"").is_err());
        assert!(parse(b"RIFFxxxxWAVE").is_err());
        assert!(parse(b"RIFFxxxxWAVEfmt ").is_err());
        assert!(parse(b"RIFFxxxxWAVEfmt \xff\xff\xff\xff").is_err());
        assert!(parse(b"RIFFxxxxWAVEdata\x04\0\0\0abcd").is_err());
        assert!(parse(b"RIFFxxxxWAVELIST\xff\xff\xff\xff").is_err());
        assert!(parse(b"OggSxxxxWAVE").is_err());
    }
}
