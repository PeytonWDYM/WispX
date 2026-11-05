//! Playing the original's sounds, which are generated in `crate::sounds`.

use super::ffi::*;
use std::ptr;

/// Plays a whole WAV file held in memory, returning when it has finished.
pub fn play(wav: &[u8]) {
    // SAFETY: with `SND_MEMORY`, the first argument is the file's bytes rather than a
    // name, and being synchronous means they outlive the sound.
    unsafe {
        PlaySoundW(
            wav.as_ptr().cast(),
            ptr::null_mut(),
            SND_MEMORY | SND_SYNC | SND_NODEFAULT,
        );
    }
}
