//! The one flat Custom Vocabulary list: canonical spellings only, no aliases,
//! rules or regexes. It biases transcription (`keyterms`) and guides rewrite.

/// Misty Step's seed terms, in order.
pub const SEED: [&str; 14] = [
    "Scry",
    "Codex",
    "Persephone",
    "Serenity",
    "Supercortex",
    "Mistystack",
    "OpenRouter",
    "Cloudflare",
    "T3 Code",
    "Kaylee",
    "Phaedrus",
    "Mercadia",
    "Cantrip",
    "Overmind",
];

pub fn seed() -> Vec<String> {
    SEED.iter().map(|s| s.to_string()).collect()
}

/// Normalize one user-entered term: trim, collapse inner whitespace. Returns
/// `None` for an empty term.
pub fn normalize_term(raw: &str) -> Option<String> {
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

/// Add a term unless an identical spelling (case-sensitive canonical form) is
/// already present. Returns true when the list changed.
pub fn add_term(list: &mut Vec<String>, raw: &str) -> bool {
    match normalize_term(raw) {
        Some(t) if !list.iter().any(|x| x == &t) => {
            list.push(t);
            true
        }
        _ => false,
    }
}

/// Remove an exact term. Returns true when the list changed.
pub fn remove_term(list: &mut Vec<String>, term: &str) -> bool {
    let before = list.len();
    list.retain(|x| x != term);
    list.len() != before
}

/// The `<VOCABULARY BLOCK>` of DESIGN.md's prompt contract, verbatim, or an
/// empty string when the list is empty.
pub fn vocabulary_block(terms: &[String]) -> String {
    if terms.is_empty() {
        return String::new();
    }
    format!(
        "Custom Vocabulary (canonical spellings): {}.\n\
A speech recognizer sometimes writes these as similar-sounding words (for example Scrum for Scry, Codecs for Codex, Misty Stack for Mistystack, computing for confusing). Replace a word only when it is wrong in its sentence and the vocabulary entry fits: \"open Scrum and search\" becomes \"open Scry and search\", but \"the scrum ran long\" and \"audio codecs\" are ordinary uses and stay exactly as dictated. When unsure, keep the dictated word. Do not insert an entry the speaker did not say.",
        terms.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_is_exactly_the_fourteen_misty_step_terms() {
        assert_eq!(SEED.len(), 14);
        assert_eq!(seed()[8], "T3 Code");
        assert_eq!(seed().last().unwrap(), "Overmind");
    }

    #[test]
    fn add_and_remove_are_exact_and_deduplicated() {
        let mut v = seed();
        assert!(!add_term(&mut v, "  Scry "));
        assert!(!add_term(&mut v, "   "));
        assert!(add_term(&mut v, "Hex  Board"));
        assert_eq!(v.last().unwrap(), "Hex Board");
        assert!(remove_term(&mut v, "Hex Board"));
        assert!(!remove_term(&mut v, "hex board"));
        assert_eq!(v, seed());
    }

    #[test]
    fn vocabulary_block_lists_terms_comma_separated() {
        let b = vocabulary_block(&["Scry".into(), "T3 Code".into()]);
        assert!(b.starts_with("Custom Vocabulary (canonical spellings): Scry, T3 Code.\n"));
        assert!(b.contains("When unsure, keep the dictated word."));
        assert_eq!(vocabulary_block(&[]), "");
    }
}
