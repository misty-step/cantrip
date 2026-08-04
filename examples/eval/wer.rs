//! Word- and character-level error-rate scoring for the evaluation gauntlet.

/// Normalize a string into lowercased word tokens. Keeps ASCII alphanumerics
/// and internal apostrophe/hyphen; every other character becomes a space.
pub fn words(s: &str) -> Vec<String> {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '\'' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().map(str::to_owned).collect()
}

/// Normalize a string into a lowercase character stream for CER. Spaces are
/// dropped; alphanumerics and apostrophe/hyphen are kept.
pub fn chars(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '\'' || *c == '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Levenshtein edit distance between two sequences (one vector row DP).
pub fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let (a, b) = if a.len() < b.len() { (a, b) } else { (b, a) };
    let mut prev: Vec<usize> = (0..=a.len()).collect();
    let mut curr = vec![0usize; a.len() + 1];
    for (j, bj) in b.iter().enumerate() {
        curr[0] = j + 1;
        for (i, ai) in a.iter().enumerate() {
            let cost = if ai == bj { 0 } else { 1 };
            curr[i + 1] = (prev[i + 1] + 1).min(curr[i] + 1).min(prev[i] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[a.len()]
}

/// Word error rate: (substitutions + deletions + insertions) / reference words.
pub fn wer(reference: &str, hypothesis: &str) -> f64 {
    let ref_words = words(reference);
    let hyp_words = words(hypothesis);
    if ref_words.is_empty() {
        return if hyp_words.is_empty() { 0.0 } else { 1.0 };
    }
    levenshtein(&ref_words, &hyp_words) as f64 / ref_words.len() as f64
}

/// Character error rate: edit distance over normalized characters.
pub fn cer(reference: &str, hypothesis: &str) -> f64 {
    let ref_chars = chars(reference);
    let hyp_chars = chars(hypothesis);
    if ref_chars.is_empty() {
        return if hyp_chars.is_empty() { 0.0 } else { 1.0 };
    }
    levenshtein(&ref_chars, &hyp_chars) as f64 / ref_chars.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_perfect_is_zero() {
        assert_eq!(wer("alpha beta gamma.", "alpha beta gamma"), 0.0);
    }

    #[test]
    fn wer_single_substitution() {
        assert!((wer("hello world", "hello there") - 1.0 / 2.0).abs() < 1e-9);
    }

    #[test]
    fn wer_counts_insertions_and_deletions() {
        // one deletion, one insertion -> 2 edits / 3 ref words
        let w = wer("the cat sat", "cat the sat!");
        assert!((w - 2.0 / 3.0).abs() < 1e-9, "got {w}");
    }

    #[test]
    fn wer_empty_reference() {
        assert_eq!(wer("", ""), 0.0);
        assert_eq!(wer("", "anything"), 1.0);
    }

    #[test]
    fn cer_counts_character_edits() {
        // "cat" vs "cart": one character
        assert!((cer("cat", "cart") - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn wer_deletion_only() {
        assert!((wer("a b c", "a c") - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn wer_insertion_only() {
        assert!((wer("a c", "a b c") - 1.0 / 2.0).abs() < 1e-9);
    }

    #[test]
    fn wer_empty_hypothesis_is_full_error() {
        assert_eq!(wer("some words here", ""), 1.0);
    }

    #[test]
    fn levenshtein_direct_operations() {
        assert_eq!(levenshtein(&[1, 2, 3], &[1, 2, 3]), 0);
        assert_eq!(levenshtein(&[1, 2, 3], &[1, 3]), 1); // deletion
        assert_eq!(levenshtein(&[1, 3], &[1, 2, 3]), 1); // insertion
        assert_eq!(levenshtein(&[1, 2], &[3, 4]), 2); // two substitutions
        assert_eq!(levenshtein(&[], &[9]), 1);
        assert_eq!(levenshtein(&["x"], &[]), 1);
    }

    #[test]
    fn cer_empty_edge_cases() {
        assert_eq!(cer("", ""), 0.0);
        assert_eq!(cer("", "x"), 1.0);
        assert_eq!(cer("x", ""), 1.0);
    }

    #[test]
    fn punctuation_and_case_ignored() {
        assert_eq!(
            wer("Hello, world!", "hello  world"),
            0.0,
            "punctuation and whitespace should not count"
        );
        assert_eq!(cer("Hello!", "hello"), 0.0);
    }
}
