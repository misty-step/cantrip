//! Minimal 16 kHz mono PCM16 WAV helpers: read samples (tolerating a header
//! whose sizes were never finalized after a crash), write, slice, and measure
//! speech energy.
//!
//! OWNER: core-meeting fork (dictation may use read/write helpers).

use std::io;
use std::path::Path;

pub const SAMPLE_RATE: u32 = 16_000;

/// Read PCM16 mono samples and the file's sample rate. Accepts a header with
/// zero/garbage data length by using the file length.
pub fn read_pcm16(path: &Path) -> io::Result<(Vec<i16>, u32)> {
    let _ = path;
    todo!()
}

/// Encode PCM16 mono samples as a complete WAV byte buffer.
pub fn encode_pcm16(samples: &[i16], rate: u32) -> Vec<u8> {
    let _ = (samples, rate);
    todo!()
}

/// Root-mean-square level of `samples`, 0.0..=1.0.
pub fn rms(samples: &[i16]) -> f32 {
    let _ = samples;
    todo!()
}

/// True when `samples` contain speech energy (any 30 ms frame above the voice
/// threshold for a sustained ~100 ms).
pub fn has_speech(samples: &[i16], rate: u32) -> bool {
    let _ = (samples, rate);
    todo!()
}
