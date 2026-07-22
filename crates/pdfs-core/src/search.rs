//! Shared, deterministic search-result relevance scoring.
//!
//! The scorer deliberately operates on a small candidate set rather than
//! replacing an index. Callers should use SQLite (or another index) to find
//! candidates, then use this module to put local and remote results on the
//! same relevance scale.

/// Scores a candidate against a user query.
///
/// Every query term must match either `basename` or `parent_path`. Basename
/// matches are weighted twice as strongly as path matches. The returned score
/// is stable and only meaningful when compared with scores produced by this
/// function.
#[must_use]
pub fn relevance_score(query: &str, basename: &str, parent_path: &str) -> Option<i64> {
    let query = normalize(query);
    if query.is_empty() {
        return Some(0);
    }

    let name = normalize(basename);
    let path = normalize(parent_path);
    let query_terms: Vec<_> = query.split_whitespace().collect();
    let name_words: Vec<_> = name.split_whitespace().collect();
    let path_words: Vec<_> = path.split_whitespace().collect();

    let mut total = 0_i64;
    for term in query_terms {
        let name_score = field_term_score(term, &name, &name_words).map(|score| score * 2);
        let path_score = field_term_score(term, &path, &path_words);
        total += name_score.into_iter().chain(path_score).max()?;
    }

    // Whole-field bonuses make ordering intuitive without changing whether a
    // candidate matches. They dominate small differences between term scores.
    if name == query {
        total += 20_000;
    } else if name.starts_with(&query) {
        total += 10_000;
    } else if name.contains(&query) {
        total += 4_000;
    }

    Some(total)
}

fn normalize(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut last_was_space = true;

    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            normalized.push(character);
            last_was_space = false;
        } else if !last_was_space {
            normalized.push(' ');
            last_was_space = true;
        }
    }

    if last_was_space {
        normalized.pop();
    }
    normalized
}

fn field_term_score(term: &str, field: &str, words: &[&str]) -> Option<i64> {
    if field == term {
        return Some(10_000);
    }
    if field.starts_with(term) {
        return Some(8_500);
    }
    if words.contains(&term) {
        return Some(8_000);
    }
    if words.iter().any(|word| word.starts_with(term)) {
        return Some(7_000);
    }
    if field.contains(term) {
        return Some(6_000);
    }

    // Subsequence matching supports useful abbreviations such as "qtrrpt".
    let subsequence = std::iter::once(field)
        .chain(words.iter().copied())
        .filter_map(|word| subsequence_gap(term, word))
        .min();
    if let Some(gap) = subsequence {
        return Some(4_500 - i64::try_from(gap.min(500)).unwrap_or(500));
    }

    // Typo matching is deliberately restricted to substantial terms to avoid
    // noisy matches for one- and two-letter searches.
    if term.chars().count() >= 4
        && words
            .iter()
            .any(|word| bounded_damerau_levenshtein(term, word, typo_limit(term)))
    {
        return Some(3_500);
    }

    None
}

fn subsequence_gap(needle: &str, haystack: &str) -> Option<usize> {
    let needle_length = needle.chars().count();
    let mut needle = needle.chars();
    let mut wanted = needle.next()?;
    let mut first = None;

    for (index, character) in haystack.chars().enumerate() {
        if character == wanted {
            first.get_or_insert(index);
            match needle.next() {
                Some(next) => wanted = next,
                None => return Some(index + 1 - first.unwrap_or(index) - needle_length),
            }
        }
    }
    None
}

fn typo_limit(term: &str) -> usize {
    usize::from(term.chars().count() >= 5) + 1
}

/// A bounded optimal-string-alignment distance check. This treats a single
/// adjacent transposition (for example, `vidoe`) as one edit.
fn bounded_damerau_levenshtein(left: &str, right: &str, limit: usize) -> bool {
    let left: Vec<_> = left.chars().collect();
    let right: Vec<_> = right.chars().collect();
    if left.len().abs_diff(right.len()) > limit || left.len().max(right.len()) > 64 {
        return false;
    }

    let mut distances = vec![vec![0_usize; right.len() + 1]; left.len() + 1];
    for (index, row) in distances.iter_mut().enumerate() {
        row[0] = index;
    }
    for (index, distance) in distances[0].iter_mut().enumerate() {
        *distance = index;
    }

    for i in 1..=left.len() {
        for j in 1..=right.len() {
            let cost = usize::from(left[i - 1] != right[j - 1]);
            distances[i][j] = (distances[i - 1][j] + 1)
                .min(distances[i][j - 1] + 1)
                .min(distances[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                distances[i][j] = distances[i][j].min(distances[i - 2][j - 2] + 1);
            }
        }
    }

    distances[left.len()][right.len()] <= limit
}

#[cfg(test)]
mod tests {
    use super::relevance_score;

    #[test]
    fn exact_name_beats_prefix_and_substring() {
        let exact = relevance_score("report", "report", "work").unwrap();
        let prefix = relevance_score("report", "report final", "work").unwrap();
        let substring = relevance_score("report", "annual report", "work").unwrap();
        assert!(exact > prefix);
        assert!(prefix > substring);
    }

    #[test]
    fn basename_is_weighted_above_parent_path() {
        let name = relevance_score("budget", "budget notes", "archive").unwrap();
        let path = relevance_score("budget", "notes", "work/budget").unwrap();
        assert!(name > path);
    }

    #[test]
    fn matches_typos_and_adjacent_transpositions() {
        assert!(relevance_score("vedio", "video.mp4", "media").is_some());
        assert!(relevance_score("vidoe", "video.mp4", "media").is_some());
        assert!(relevance_score("presentaton", "presentation.pdf", "work").is_some());
    }

    #[test]
    fn separators_form_searchable_word_boundaries() {
        assert!(relevance_score("final draft", "Final_Draft-v2.pdf", "work").is_some());
        assert!(relevance_score("quarter report", "quarterly-report.pdf", "work").is_some());
    }

    #[test]
    fn unicode_case_folding_preserves_accented_matches() {
        assert!(relevance_score("CAFÉ", "Café Photos", "Trips").is_some());
        assert!(relevance_score("ångström", "ÅNGSTRÖM notes", "Research").is_some());
    }

    #[test]
    fn matches_terms_across_name_and_path() {
        assert!(relevance_score("tax invoice", "invoice-1042.pdf", "archive/tax/2025").is_some());
    }

    #[test]
    fn ordered_subsequence_supports_abbreviations() {
        assert!(relevance_score("qtrrpt", "quarter-report.pdf", "finance").is_some());
    }

    #[test]
    fn rejects_unrelated_candidates_and_short_typos() {
        assert_eq!(relevance_score("video", "holiday.jpg", "photos"), None);
        assert_eq!(relevance_score("xy", "xz.txt", "misc"), None);
    }
}
