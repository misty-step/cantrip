//! Deterministic cross-track merge: turns labeled by track, chronological by
//! turn start on the shared clock, `You` first on a tie, never joined across
//! speakers or markers.

use super::parts::{Part, PartResult, Track};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnBody {
    /// Recognized words. `approximate` marks a part returned without usable
    /// timestamps, placed coarsely at its own span.
    Text { text: String, approximate: bool },
    /// An unresolved part (only in partial notes).
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub speaker: Track,
    pub start_ms: u64,
    pub end_ms: u64,
    pub body: TurnBody,
}

impl Turn {
    pub fn text(&self) -> Option<&str> {
        match &self.body {
            TurnBody::Text { text, .. } => Some(text),
            TurnBody::Failed => None,
        }
    }
}

/// Build the merged transcript. Unresolved parts become `Failed` marker turns
/// when `include_failed` (partial notes); otherwise they are skipped.
pub fn build_turns(
    parts: &[Part],
    results: &BTreeMap<String, PartResult>,
    include_failed: bool,
) -> Vec<Turn> {
    // (start, track, part index, segment index) is a total order.
    let mut keyed: Vec<((u64, Track, usize, usize), Turn)> = Vec::new();
    for part in parts {
        match results.get(&part.name) {
            Some(PartResult::Resolved { text, segments }) => {
                let usable: Vec<_> = segments
                    .iter()
                    .filter(|s| !s.text.trim().is_empty() && s.end_ms >= s.start_ms)
                    .collect();
                if !usable.is_empty() {
                    for (i, seg) in usable.into_iter().enumerate() {
                        let start = part.offset_ms + seg.start_ms.min(part.duration_ms);
                        let end = part.offset_ms
                            + seg
                                .end_ms
                                .min(part.duration_ms)
                                .max(seg.start_ms.min(part.duration_ms));
                        keyed.push((
                            (start, part.track, part.index, i),
                            Turn {
                                speaker: part.track,
                                start_ms: start,
                                end_ms: end,
                                body: TurnBody::Text {
                                    text: seg.text.trim().to_string(),
                                    approximate: false,
                                },
                            },
                        ));
                    }
                } else if !text.trim().is_empty() {
                    keyed.push((
                        (part.offset_ms, part.track, part.index, 0),
                        Turn {
                            speaker: part.track,
                            start_ms: part.offset_ms,
                            end_ms: part.end_ms(),
                            body: TurnBody::Text {
                                text: text.trim().to_string(),
                                approximate: true,
                            },
                        },
                    ));
                }
            }
            Some(PartResult::Unresolved { .. }) | None => {
                if include_failed {
                    keyed.push((
                        (part.offset_ms, part.track, part.index, 0),
                        Turn {
                            speaker: part.track,
                            start_ms: part.offset_ms,
                            end_ms: part.end_ms(),
                            body: TurnBody::Failed,
                        },
                    ));
                }
            }
        }
    }
    keyed.sort_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::Segment;

    fn part(name: &str, track: Track, index: usize, offset: u64, dur: u64) -> Part {
        Part {
            name: name.into(),
            track,
            index,
            offset_ms: offset,
            duration_ms: dur,
            voiced: true,
        }
    }

    fn seg(a: u64, b: u64, t: &str) -> Segment {
        Segment {
            start_ms: a,
            end_ms: b,
            text: t.into(),
        }
    }

    #[test]
    fn merges_chronologically_with_you_first_on_ties() {
        let parts = vec![
            part("you-000", Track::You, 0, 0, 60_000),
            part("others-000", Track::Others, 0, 250, 60_000),
        ];
        let mut r = BTreeMap::new();
        r.insert(
            "you-000".into(),
            PartResult::Resolved {
                text: String::new(),
                segments: vec![seg(1000, 2000, "a"), seg(5250, 6000, "c")],
            },
        );
        r.insert(
            "others-000".into(),
            PartResult::Resolved {
                text: String::new(),
                // 250 + 2750 = 3000; 250 + 5000 = 5250 ties with You's 5250.
                segments: vec![seg(2750, 3500, "b"), seg(5000, 5500, "d")],
            },
        );
        let turns = build_turns(&parts, &r, false);
        let order: Vec<_> = turns
            .iter()
            .map(|t| (t.speaker, t.start_ms, t.text().unwrap().to_string()))
            .collect();
        assert_eq!(
            order,
            vec![
                (Track::You, 1000, "a".into()),
                (Track::Others, 3000, "b".into()),
                (Track::You, 5250, "c".into()),
                (Track::Others, 5250, "d".into()),
            ]
        );
        // Deterministic regardless of map/parts order.
        let mut rev = parts.clone();
        rev.reverse();
        assert_eq!(build_turns(&rev, &r, false), turns);
    }

    #[test]
    fn no_timestamps_gives_one_approximate_turn_per_part_and_failed_markers() {
        let parts = vec![
            part("you-000", Track::You, 0, 0, 10_000),
            part("others-000", Track::Others, 0, 0, 10_000),
        ];
        let mut r = BTreeMap::new();
        r.insert(
            "you-000".into(),
            PartResult::Resolved {
                text: "no".into(),
                segments: vec![],
            },
        );
        r.insert(
            "others-000".into(),
            PartResult::Unresolved {
                reason: "http".into(),
            },
        );
        let turns = build_turns(&parts, &r, true);
        assert_eq!(turns.len(), 2);
        assert_eq!(
            turns[0].body,
            TurnBody::Text {
                text: "no".into(),
                approximate: true
            }
        );
        assert_eq!(turns[1].body, TurnBody::Failed);
        assert_eq!(build_turns(&parts, &r, false).len(), 1);
    }
}
