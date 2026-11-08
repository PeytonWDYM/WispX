//! Speech upload encoding: 16 kHz mono PCM to 32 kbps Opus in WebM.
use super::webm::Webm;
use opus_rs::{Application, OpusEncoder};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EncodeError(&'static str);

/// Encodes once, including the encoder lookahead and final partial frame.
pub fn encode(pcm: &[i16]) -> Result<Vec<u8>, EncodeError> {
    const FRAME: usize = 320; // 20 ms at the recorder's 16 kHz
    let mut encoder = OpusEncoder::new(16_000, 1, Application::Voip).map_err(EncodeError)?;
    encoder.bitrate_bps = 32_000;
    encoder.use_cbr = true;
    let lookahead = encoder.lookahead();
    let mut webm = Webm::new((lookahead * 3) as u16, pcm.len());
    let frames = (pcm.len() + lookahead).div_ceil(FRAME);
    let mut packet = [0_u8; 1276];
    for index in 0..frames {
        let mut frame = [0_i16; FRAME];
        let start = index * FRAME;
        if start < pcm.len() {
            let count = FRAME.min(pcm.len() - start);
            frame[..count].copy_from_slice(&pcm[start..start + count]);
        }
        let len = encoder
            .encode_i16(&frame, FRAME, &mut packet)
            .map_err(EncodeError)?;
        let padding = if index + 1 == frames {
            (frames * FRAME - pcm.len() - lookahead) as u64 * 1_000_000_000 / 16_000
        } else {
            0
        };
        webm.push_packet(&packet[..len], padding);
    }
    Ok(webm.finish())
}
