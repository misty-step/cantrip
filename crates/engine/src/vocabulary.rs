use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// A canonical spelling, optionally owning exact recognition aliases.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum VocabularyEntry {
    Term(String),
    Aliased {
        term: String,
        #[serde(default)]
        aliases: Vec<String>,
    },
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
        .map(|entry| entry.term().to_lowercase())
        .collect();
    let mut owners = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if matches!(entry, VocabularyEntry::Aliased { .. }) && entry.term().trim().is_empty() {
            bail!("vocabulary entry {} has an empty canonical term", index + 1);
        }
        for alias in entry.aliases() {
            if alias.trim().is_empty() {
                bail!("vocabulary entry {} has an empty alias", index + 1);
            }
            let folded = alias.to_lowercase();
            if canonical
                .iter()
                .enumerate()
                .any(|(other, term)| other != index && *term == folded)
            {
                bail!(
                    "vocabulary entry {} has an alias equal to another canonical term",
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
pub(crate) fn apply<'a>(text: &'a str, entries: &[VocabularyEntry]) -> (Cow<'a, str>, usize) {
    let mut matches = Vec::new();
    for entry in entries {
        for alias in entry
            .aliases()
            .iter()
            .map(|alias| alias.trim())
            .filter(|alias| !alias.is_empty())
        {
            let mut before = None;
            for (start, character) in text.char_indices() {
                if !before.is_some_and(word_character) {
                    if let Some(end) = phrase_end(&text[start..], alias) {
                        matches.push(Match {
                            start,
                            end: start + end,
                            term: entry.term(),
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
    if selected == 0 {
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
    (Cow::Owned(output), selected)
}

fn word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn phrase_end(text: &str, alias: &str) -> Option<usize> {
    let mut expected = alias.chars().flat_map(char::to_lowercase).peekable();
    for (offset, character) in text.char_indices() {
        for folded in character.to_lowercase() {
            if expected.next() != Some(folded) {
                return None;
            }
        }
        if expected.peek().is_none() {
            let end = offset + character.len_utf8();
            return (!text[end..].chars().next().is_some_and(word_character)).then_some(end);
        }
    }
    None
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
}
