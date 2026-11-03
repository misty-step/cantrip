use anyhow::{bail, Result};
use caseless::Caseless;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// A canonical spelling, optionally owning exact recognition aliases.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum VocabularyEntry {
    Term(String),
    Aliased {
        term: String,
        #[serde(default)]
        aliases: Vec<String>,
    },
}

pub(crate) fn deserialize_entries<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<VocabularyEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = Vec<VocabularyEntry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(
                "an array of strings or { term = string, aliases = [string, ...] } tables",
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut entries = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
            while let Some(entry) = sequence.next_element::<VocabularyEntry>().map_err(|_| {
                <A::Error as serde::de::Error>::custom(format!(
                    "vocabulary entry {}: expected a string or {{ term = string, aliases = [string, ...] }} (aliases optional)",
                    entries.len() + 1,
                ))
            })? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(Entries)
}

impl VocabularyEntry {
    pub fn term(&self) -> &str {
        match self {
            Self::Term(term) | Self::Aliased { term, .. } => term,
        }
    }

    pub fn aliases(&self) -> &[String] {
        match self {
            Self::Term(_) => &[],
            Self::Aliased { aliases, .. } => aliases,
        }
    }
}

impl From<&str> for VocabularyEntry {
    fn from(term: &str) -> Self {
        Self::Term(term.to_owned())
    }
}

pub(crate) fn validate(entries: &[VocabularyEntry]) -> Result<()> {
    let canonical: Vec<_> = entries
        .iter()
        .map(|entry| {
            normalize(entry.term())
                .map(|(_, character)| character)
                .collect::<String>()
        })
        .collect();
    let mut owners = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if matches!(entry, VocabularyEntry::Aliased { .. }) && entry.term().trim().is_empty() {
            bail!("vocabulary entry {} has an empty canonical term", index + 1);
        }
        for alias in entry.aliases() {
            let folded: String = normalize(alias).map(|(_, character)| character).collect();
            if folded.is_empty() {
                bail!("vocabulary entry {} has an empty alias", index + 1);
            }
            if canonical
                .iter()
                .enumerate()
                .any(|(other, term)| other != index && term == &folded)
            {
                bail!(
                    "vocabulary entry {} has an alias equal to a canonical term",
                    index + 1
                );
            }
            if owners
                .insert(folded, index)
                .is_some_and(|other| other != index)
            {
                bail!(
                    "vocabulary entry {} duplicates an alias owned by another term",
                    index + 1
                );
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Match<'a> {
    start: usize,
    end: usize,
    term: &'a str,
}

/// Select globally longest non-overlapping phrases; replacements never cascade.
pub fn apply<'a>(text: &'a str, entries: &[VocabularyEntry]) -> (Cow<'a, str>, usize) {
    let mut matches = Vec::new();
    for entry in entries {
        for alias in entry.aliases() {
            let folded: Vec<_> = normalize(alias).map(|(_, character)| character).collect();
            if folded.is_empty() {
                continue;
            }
            let mut before = None;
            for (start, character) in text.char_indices() {
                if !character.is_whitespace() && !before.is_some_and(word_character) {
                    if let Some(end) = phrase_end(&text[start..], &folded) {
                        matches.push(Match {
                            start,
                            end: start + end,
                            term: entry.term().trim(),
                        });
                    }
                }
                before = Some(character);
            }
        }
    }
    matches.sort_by(|left, right| {
        (right.end - right.start)
            .cmp(&(left.end - left.start))
            .then_with(|| left.start.cmp(&right.start))
    });
    let mut selected = 0;
    for index in 0..matches.len() {
        let candidate = matches[index];
        if matches[..selected]
            .iter()
            .any(|existing| candidate.start < existing.end && existing.start < candidate.end)
        {
            continue;
        }
        matches.swap(selected, index);
        selected += 1;
    }
    let substitutions = matches[..selected]
        .iter()
        .filter(|matched| &text[matched.start..matched.end] != matched.term)
        .count();
    if substitutions == 0 {
        return (Cow::Borrowed(text), 0);
    }
    matches.truncate(selected);
    matches.sort_by_key(|matched| matched.start);
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for matched in matches {
        output.push_str(&text[cursor..matched.start]);
        output.push_str(matched.term);
        cursor = matched.end;
    }
    output.push_str(&text[cursor..]);
    (Cow::Owned(output), substitutions)
}

fn word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Trim and full Unicode case fold, retaining original UTF-8 character ends.
/// Validation and matching must use this same projection.
fn normalize(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let leading = text.len() - text.trim_start().len();
    text.trim()
        .char_indices()
        .flat_map(move |(offset, character)| {
            let end = leading + offset + character.len_utf8();
            std::iter::once(character)
                .default_case_fold()
                .map(move |folded| (end, folded))
        })
}

fn phrase_end(text: &str, alias: &[char]) -> Option<usize> {
    let mut source = normalize(text).peekable();
    let mut end = 0;
    for expected in alias {
        let (offset, character) = source.next()?;
        if character != *expected {
            return None;
        }
        end = offset;
    }
    // A match cannot consume only part of a multi-character fold such as ß → ss.
    if source.peek().is_some_and(|(offset, _)| *offset == end) {
        return None;
    }
    (!text[end..].chars().next().is_some_and(word_character)).then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(term: &str, aliases: &[&str]) -> VocabularyEntry {
        VocabularyEntry::Aliased {
            term: term.into(),
            aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
        }
    }

    #[test]
    fn longest_phrases_win_even_when_they_start_later() {
        let entries = [
            aliases("short", &["alpha beta"]),
            aliases("Long", &["beta gamma delta"]),
            aliases("Cantrip", &["Gantrip"]),
        ];
        assert_eq!(
            apply("alpha beta gamma delta; GANTRIP", &entries),
            (Cow::Owned("alpha Long; Cantrip".into()), 2)
        );
    }

    #[test]
    fn exact_phrases_respect_word_boundaries_case_and_unicode() {
        let entries = [
            aliases("Cantrip", &["Gantrip"]),
            aliases("PipeWire", &["pipe wire"]),
            aliases("Éclair", &["éclaire"]),
        ];
        assert_eq!(
            apply(
                "GANTRIP, gantrip's; xGantrip Gantrips Gantrip_ can trip. PIPE WIRE; ÉCLAIRE!",
                &entries
            )
            .0,
            "Cantrip, Cantrip's; xGantrip Gantrips Gantrip_ can trip. PipeWire; Éclair!"
        );
        assert_eq!(apply("can trip", &entries).1, 0);
        assert_eq!(
            apply("can trip", &[aliases("Cantrip", &["can trip"])]).0,
            "Cantrip"
        );
    }

    #[test]
    fn validation_and_matching_share_trim_and_full_unicode_case_folding() {
        for (alias, equivalent) in [
            (" pipewire ", "PipeWire"),
            (" ΟΔΟΣ ", "οδος"),
            (" οδοσ ", "ΟΔΟΣ"),
            (" Straße ", "STRASSE"),
            (" ﬃ ", "FFI"),
        ] {
            let folded = |text: &str| {
                normalize(text)
                    .map(|(_, character)| character)
                    .collect::<String>()
            };
            assert_eq!(folded(alias), folded(equivalent));
            let entries = [aliases("Canonical", &[alias])];
            validate(&entries).unwrap();
            assert_eq!(
                apply(equivalent, &entries),
                (Cow::Owned("Canonical".into()), 1)
            );

            let canonical_conflict = [VocabularyEntry::from(equivalent), entries[0].clone()];
            assert!(validate(&canonical_conflict).is_err(), "{alias}");
            let duplicate = [entries[0].clone(), aliases("Other", &[equivalent])];
            assert!(validate(&duplicate).is_err(), "{alias}");
            validate(&[aliases(equivalent, &[alias])]).unwrap();
        }
        assert_eq!(apply("ß", &[aliases("Canonical", &["s"])]).1, 0);
    }

    #[test]
    fn case_only_aliases_correct_spelling_without_counting_canonical_text() {
        let entries = [aliases("Daybook", &["daybook"])];
        validate(&entries).unwrap();
        assert_eq!(
            apply("daybook", &entries),
            (Cow::Owned("Daybook".into()), 1)
        );
        assert_eq!(apply("Daybook", &entries), (Cow::Borrowed("Daybook"), 0));
        assert_eq!(
            apply("Daybook daybook DAYBOOK", &entries),
            (Cow::Owned("Daybook Daybook Daybook".into()), 2)
        );
    }

    #[test]
    fn canonical_phrases_block_shorter_overlapping_aliases_without_substitutions() {
        let entries = [
            aliases("Kube Proxy", &["kube proxy"]),
            aliases("Kubernetes", &["kube"]),
        ];
        validate(&entries).unwrap();
        assert_eq!(
            apply("Kube Proxy", &entries),
            (Cow::Borrowed("Kube Proxy"), 0)
        );
    }

    #[test]
    fn canonical_terms_are_trimmed_before_emission_and_no_op_comparison() {
        let entries = [aliases(" Tern ", &["fern", "tern"])];
        validate(&entries).unwrap();
        assert_eq!(apply("fern", &entries), (Cow::Owned("Tern".into()), 1));
        assert_eq!(apply("Tern", &entries), (Cow::Borrowed("Tern"), 0));
    }
}
