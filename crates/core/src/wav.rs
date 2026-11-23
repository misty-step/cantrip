//! Minimal 16 kHz mono PCM16 WAV helpers: read samples (tolerating a header
//! whose sizes were never finalized after a crash), encode, and measure speech
//! energy.

use std::io;
use std::path::Path;

pub const SAMPLE_RATE: u32 = 16_000;

/// Frame length for energy analysis.
const FRAME_MS: u32 = 30;
/// RMS above this (≈ -36 dBFS) counts as voiced.
const VOICE_RMS: f32 = 0.016;
/// Consecutive voiced time needed to count as speech.
const MIN_VOICED_MS: u32 = 90;

/// Read PCM16 samples (first channel) and the file's sample rate. Accepts a
/// header with zero/garbage data length by using the file length.
pub fn read_pcm16(path: &Path) -> io::Result<(Vec<i16>, u32)> {
    decode_pcm16(&std::fs::read(path)?)
}

/// Decode a WAV byte buffer; see [`read_pcm16`].
pub fn decode_pcm16(bytes: &[u8]) -> io::Result<(Vec<i16>, u32)> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if body + 16 > bytes.len() {
                return Err(bad("truncated fmt chunk"));
            }
            let f = &bytes[body..body + 16];
            fmt = Some((
                u16::from_le_bytes([f[0], f[1]]),
                u16::from_le_bytes([f[2], f[3]]),
                u32::from_le_bytes([f[4], f[5], f[6], f[7]]),
                u16::from_le_bytes([f[14], f[15]]),
            ));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt.ok_or_else(|| bad("data before fmt"))?;
            if !(format == 1 || format == 0xFFFE) || bits != 16 || channels == 0 {
                return Err(bad("only PCM16 WAV is supported"));
            }
            // Unfinalized headers carry 0 or 0xFFFFFFFF; trust the file length.
            let end = if len == 0 || body + len > bytes.len() {
                bytes.len()
            } else {
                body + len
            };
            let frame = 2 * channels as usize;
            let samples = bytes[body..end]
                .chunks_exact(frame)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            return Ok((samples, rate));
        }
        pos = body + len + (len & 1);
    }
    Err(bad("no data chunk"))
}

/// Encode PCM16 mono samples as a complete WAV byte buffer.
pub fn encode_pcm16(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Root-mean-square level of `samples`, 0.0..=1.0.
pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| {
            let x = s as f64 / 32768.0;
            x * x
        })
        .sum();
    ((sum / samples.len() as f64).sqrt() as f32).min(1.0)
}

/// True when `samples` contain speech energy: voiced 30 ms frames sustained
/// for ≥ 90 ms (so a 180 ms "no" counts).
pub fn has_speech(samples: &[i16], rate: u32) -> bool {
    let frame = (rate * FRAME_MS / 1000).max(1) as usize;
    let needed = MIN_VOICED_MS.div_ceil(FRAME_MS) as usize;
    let mut run = 0;
    for chunk in samples.chunks(frame) {
        if rms(chunk) >= VOICE_RMS {
            run += 1;
            if run >= needed {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: u32, amp: f32) -> Vec<i16> {
        let n = SAMPLE_RATE * ms / 1000;
        (0..n)
            .map(|i| ((i as f32 * 0.07).sin() * amp * 32767.0) as i16)
            .collect()
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let s = tone(100, 0.5);
        let (back, rate) = decode_pcm16(&encode_pcm16(&s, SAMPLE_RATE)).unwrap();
        assert_eq!(rate, SAMPLE_RATE);
        assert_eq!(back, s);
    }

    #[test]
    fn unfinalized_header_uses_file_length() {
        let s = tone(50, 0.3);
        let mut bytes = encode_pcm16(&s, SAMPLE_RATE);
        bytes[40..44].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_pcm16(&bytes).unwrap().0, s);
        bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_pcm16(&bytes).unwrap().0, s);
    }

    #[test]
    fn silence_is_not_speech_but_a_180ms_word_is() {
        assert!(!has_speech(&vec![0; 16_000], SAMPLE_RATE));
        assert!(!has_speech(&tone(1000, 0.005), SAMPLE_RATE));
        let mut s = vec![0i16; 8000];
        s.extend(tone(180, 0.2));
        s.extend(vec![0i16; 8000]);
        assert!(has_speech(&s, SAMPLE_RATE));
    }

    #[test]
    fn rejects_non_wav() {
        assert!(decode_pcm16(b"hello world!").is_err());
    }
}
