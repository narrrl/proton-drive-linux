//! Match highlighting: which characters of a result name the query matched,
//! as Pango markup with those characters in bold.
//!
//! Each query term is looked for as a substring of the name first, and as a
//! subsequence ("rpt" in "report") when that fails, which is roughly how the
//! daemon's ranking treats a term too. Case is folded one character at a
//! time, so every range stays on a character boundary of the original name,
//! whatever the script.

use gtk4::glib;

/// One character of a name: its byte range and its case-folded form.
struct Folded {
    start: usize,
    end: usize,
    folded: String,
}

fn fold(text: &str) -> Vec<Folded> {
    text.char_indices()
        .map(|(start, c)| Folded {
            start,
            end: start + c.len_utf8(),
            folded: c.to_lowercase().collect(),
        })
        .collect()
}

/// The byte ranges of `name` that `query` matched, sorted and merged.
pub(crate) fn ranges(name: &str, query: &str) -> Vec<(usize, usize)> {
    let chars = fold(name);
    let mut found: Vec<(usize, usize)> = Vec::new();
    for term in query.split_whitespace() {
        let term: Vec<String> = fold(term).into_iter().map(|c| c.folded).collect();
        if let Some(at) = substring(&chars, &term) {
            found.push((chars[at].start, chars[at + term.len() - 1].end));
        } else if let Some(hits) = subsequence(&chars, &term) {
            found.extend(hits.into_iter().map(|at| (chars[at].start, chars[at].end)));
        }
    }
    found.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(found.len());
    for (start, end) in found {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Where `term` first occurs in `chars`, as a character index.
fn substring(chars: &[Folded], term: &[String]) -> Option<usize> {
    if term.is_empty() || term.len() > chars.len() {
        return None;
    }
    (0..=chars.len() - term.len()).find(|&at| {
        chars[at..at + term.len()]
            .iter()
            .zip(term)
            .all(|(c, t)| c.folded == *t)
    })
}

/// The character indices of the leftmost match of `term` as a subsequence,
/// or `None` when not every character of it can be found in order.
fn subsequence(chars: &[Folded], term: &[String]) -> Option<Vec<usize>> {
    if term.is_empty() {
        return None;
    }
    let mut hits = Vec::with_capacity(term.len());
    let mut next = 0;
    for wanted in term {
        let at = (next..chars.len()).find(|&i| chars[i].folded == *wanted)?;
        hits.push(at);
        next = at + 1;
    }
    Some(hits)
}

/// `name` as Pango markup, escaped, with the matched characters in bold.
pub(crate) fn markup(name: &str, query: &str) -> String {
    let mut out = String::with_capacity(name.len() + 16);
    let mut done = 0;
    for (start, end) in ranges(name, query) {
        out.push_str(&glib::markup_escape_text(&name[done..start]));
        out.push_str("<b>");
        out.push_str(&glib::markup_escape_text(&name[start..end]));
        out.push_str("</b>");
        done = end;
    }
    out.push_str(&glib::markup_escape_text(&name[done..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_substring_match_is_one_range_in_any_case() {
        assert_eq!(ranges("Tax Report.pdf", "REP"), vec![(4, 7)]);
    }

    #[test]
    fn a_term_that_is_no_substring_falls_back_to_a_subsequence() {
        // r, p and t of "report", not the "rt" pair at the end of a substring.
        assert_eq!(ranges("report.txt", "rpt"), vec![(0, 1), (2, 3), (5, 6)]);
    }

    #[test]
    fn a_term_with_no_match_highlights_nothing() {
        assert!(ranges("report.txt", "xyz").is_empty());
        assert!(ranges("report.txt", "").is_empty());
    }

    #[test]
    fn several_terms_merge_into_sorted_ranges() {
        assert_eq!(
            ranges("tax return 2024", "2024 tax ret"),
            vec![(0, 3), (4, 7), (11, 15)]
        );
        assert_eq!(ranges("abc", "ab bc"), vec![(0, 3)]);
    }

    #[test]
    fn non_ascii_names_keep_ranges_on_character_boundaries() {
        assert_eq!(ranges("Über Größe.odt", "über"), vec![(0, 5)]);
        assert_eq!(ranges("Über Größe.odt", "GRÖ"), vec![(6, 10)]);
        assert_eq!(markup("Über Größe.odt", "größe"), "Über <b>Größe</b>.odt");
        assert_eq!(
            markup("日本語のメモ.txt", "メモ"),
            "日本語の<b>メモ</b>.txt"
        );
    }

    #[test]
    fn markup_escapes_the_name_inside_and_outside_matches() {
        assert_eq!(markup("a<b>&c", "b"), "a&lt;<b>b</b>&gt;&amp;c");
        assert_eq!(markup("R&D.pdf", "r&d"), "<b>R&amp;D</b>.pdf");
        assert_eq!(markup("<none>", "zzz"), "&lt;none&gt;");
    }
}
