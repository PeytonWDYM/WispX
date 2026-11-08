use crate::audio::sounds::Cue;
use std::sync::OnceLock;
use windows::{
    Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
    core::PCWSTR,
};
/// Static buffers outlive asynchronous WinMM playback.
pub fn play(cue: Cue) {
    static START: OnceLock<Vec<u8>> = OnceLock::new();
    static STOP: OnceLock<Vec<u8>> = OnceLock::new();
    static CANCEL: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = match cue {
        Cue::Start => START.get_or_init(|| cue.wav()),
        Cue::Stop => STOP.get_or_init(|| cue.wav()),
        Cue::Cancel => CANCEL.get_or_init(|| cue.wav()),
    };
    // SAFETY: SND_MEMORY reads the RIFF data, whose static allocation remains alive.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(bytes.as_ptr().cast()),
            None,
            SND_ASYNC | SND_MEMORY | SND_NODEFAULT,
        );
    }
}
