//! Splitting each track into upload-sized parts on one shared clock, and the
//! per-part transcription result that decides completion.

use crate::speech::Segment;
use crate::wav;
use serde::{Deserialize, Serialize};

/// Longest part: 5 minutes of 16 kHz PCM16 is 9.6 MB, ~12.8 MB as base64,
/// comfortably under the provider's 25 MB limit.
pub const MAX_PART_MS: u64 = 5 * 60 * 1000;
/// Look this far back from the limit for the quietest place to cut.
pub const CUT_WINDOW_MS: u64 = 20 * 1000;
const FRAME_MS: u64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Track {
    /// Microphone.
    You,
    /// System audio output.
    Others,
}

impl Track {
    pub fn label(self) -> &'static str {
        match self {
            Track::You => "You",
            Track::Others => "Others",
        }
    }

    pub fn file_prefix(self) -> &'static str {
        match self {
            Track::You => "you",
            Track::Others => "others",
        }
    }
}

/// One staged part of one track.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    /// File stem under `parts/`, e.g. `you-000`.
    pub name: String,
    pub track: Track,
    pub index: usize,
    /// Start on the shared meeting clock.
    pub offset_ms: u64,
    pub duration_ms: u64,
    /// Contains speech energy.
    pub voiced: bool,
}

impl Part {
    pub fn end_ms(&self) -> u64 {
        self.offset_ms + self.duration_ms
    }
}

/// The durable result of transcribing one part.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartResult {
    /// Words (with segment timestamps relative to the part when returned), or
    /// empty for a part without speech.
    Resolved {
        text: String,
        segments: Vec<Segment>,
    },
    /// Failed, or returned no words although the part contains speech.
    Unresolved { reason: String },
}

impl PartResult {
    pub fn is_resolved(&self) -> bool {
        matches!(self, PartResult::Resolved { .. })
    }
}

/// Sample ranges `[start, end)` cutting `samples` into parts no longer than
/// [`MAX_PART_MS`], each cut at the quietest 30 ms frame within the last
/// [`CUT_WINDOW_MS`] before the limit.
pub fn split_points(samples: &[i16], rate: u32) -> Vec<(usize, usize)> {
    split_points_with(samples, rate, MAX_PART_MS, CUT_WINDOW_MS)
}

pub fn split_points_with(
    samples: &[i16],
    rate: u32,
    max_ms: u64,
    window_ms: u64,
) -> Vec<(usize, usize)> {
    let per_ms = rate as u64;
    let max = (max_ms * per_ms / 1000) as usize;
    let window = (window_ms * per_ms / 1000) as usize;
    let frame = ((FRAME_MS * per_ms / 1000) as usize).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while samples.len() - start > max {
        let limit = start + max;
        let lo = limit.saturating_sub(window).max(start + frame);
        let mut best = limit;
        let mut best_rms = f32::INFINITY;
        let mut pos = lo;
        while pos + frame <= limit {
            let r = wav::rms(&samples[pos..pos + frame]);
            if r < best_rms {
                best_rms = r;
                best = pos + frame / 2;
            }
            pos += frame;
        }
        out.push((start, best));
        start = best;
    }
    if start < samples.len() || out.is_empty() {
        out.push((start, samples.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_track_is_one_part() {
        let s = vec![0i16; 16_000];
        assert_eq!(split_points(&s, 16_000), vec![(0, 16_000)]);
        assert_eq!(split_points(&[], 16_000), vec![(0, 0)]);
    }

    #[test]
    fn long_track_cuts_at_the_quietest_frame_before_the_limit() {
        // 10 s max, 4 s window; loud everywhere except a quiet gap at 8.0 s.
        let rate = 1000;
        let mut s: Vec<i16> = (0..25_000)
            .map(|i| if i % 2 == 0 { 8000 } else { -8000 })
            .collect();
        for x in &mut s[8000..8030] {
            *x = 0;
        }
        let parts = split_points_with(&s, rate, 10_000, 4_000);
        assert!(parts.len() >= 3);
        assert!(parts[0].1 >= 8000 && parts[0].1 <= 8030, "{parts:?}");
        for w in parts.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        for (a, b) in &parts {
            assert!(b - a <= 10_000);
        }
        assert_eq!(parts.last().unwrap().1, s.len());
    }
}
