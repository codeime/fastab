//! Prefix and fuzzy matching used by lookup, generators, and ranking.
//!
//! The WebView implementation uses the small local copy of `fuzzysort` in
//! `packages/fuzzysort`. Keeping the scoring here (rather than just checking
//! for a subsequence) matters: two names can both match a query while the
//! tighter/word-boundary match must remain the first row in the overlay.

use std::cmp::Ordering;

/// The same broad buckets used by `filterSuggestions` in the old WebView.
/// Lower buckets sort first. Within the fuzzy bucket, `score` is larger for
/// a better match (fuzzysort's raw scores are zero or negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    ExactCase,
    ExactInsensitive,
    PrefixCase,
    PrefixInsensitive,
    Fuzzy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchScore {
    pub kind: MatchKind,
    pub score: i64,
}

impl MatchScore {
    pub const fn bucket(self) -> u8 {
        match self.kind {
            MatchKind::ExactCase => 0,
            MatchKind::ExactInsensitive => 1,
            MatchKind::PrefixCase => 2,
            MatchKind::PrefixInsensitive => 3,
            MatchKind::Fuzzy => 4,
        }
    }
}

/// Match `name` the way the old suggestion filter did and return a sortable
/// quality score. Exact and prefix matches intentionally get their own
/// buckets: in fuzzy mode a prefix should beat a merely scattered fuzzy hit,
/// even when the latter has a decent fuzzysort score.
pub fn match_score(name: &str, query: &str, fuzzy: bool) -> Option<MatchScore> {
    if query.is_empty() {
        return None;
    }

    if name == query {
        return Some(MatchScore {
            kind: MatchKind::ExactCase,
            score: 0,
        });
    }

    let name_lower = name.to_lowercase();
    let query_lower = query.to_lowercase();
    if name_lower == query_lower {
        return Some(MatchScore {
            kind: MatchKind::ExactInsensitive,
            // This mirrors getPrefixNameMatches: case-insensitive exact is
            // just behind a case-sensitive exact match.
            score: -1,
        });
    }

    if name.starts_with(query) {
        return Some(MatchScore {
            kind: MatchKind::PrefixCase,
            score: -2,
        });
    }
    if name_lower.starts_with(&query_lower) {
        return Some(MatchScore {
            kind: MatchKind::PrefixInsensitive,
            score: -3,
        });
    }

    if fuzzy {
        fuzzy_match(name, query).map(|fuzzy| MatchScore {
            kind: MatchKind::Fuzzy,
            score: fuzzy.score,
        })
    } else {
        None
    }
}

pub fn matches_query(name: &str, query: &str, fuzzy: bool) -> bool {
    if query.is_empty() {
        return true;
    }
    // History runs this over every line for a keystroke, and `match_score`
    // lowercases a copy of both strings each time. ASCII needs neither copy.
    if name.is_ascii() && query.is_ascii() {
        let prefix = name
            .as_bytes()
            .get(..query.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(query.as_bytes()));
        if prefix || !fuzzy {
            return prefix;
        }
    }
    match_score(name, query, fuzzy).is_some()
}

pub fn starts_with_ignore_case(name: &str, query: &str) -> bool {
    name.to_lowercase().starts_with(&query.to_lowercase())
}

pub fn cmp_ignore_ascii_case(left: &str, right: &str) -> Ordering {
    let mut left_chars = left.chars();
    let mut right_chars = right.chars();
    loop {
        match (left_chars.next(), right_chars.next()) {
            (Some(a), Some(b)) => {
                let ord = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
            },
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FuzzyMatch {
    score: i64,
    indexes: Vec<usize>,
}

/// A close Rust equivalent of the local fuzzysort algorithm.
///
/// fuzzysort first takes a greedy subsequence. It then tries to move each
/// character to a word beginning (or keep it consecutive), backtracking when
/// necessary. Strict matches are scored by the gaps between characters;
/// fallback subsequences are deliberately penalized by 1000.
fn fuzzy_match(name: &str, query: &str) -> Option<FuzzyMatch> {
    // JavaScript indexes strings by UTF-16 code unit, not Unicode scalar
    // value.  It also sizes the prepared lowercase array from the original
    // string.  Keeping both details avoids index drift for emoji and for case
    // folds such as `İ` -> `i` + COMBINING DOT ABOVE.
    let target = prepare_lower_codes(name);
    let search = prepare_lower_codes(query);
    if search.is_empty() {
        return None;
    }

    let simple = greedy_indexes(&target, &search)?;
    let beginning_indexes = beginning_indexes(name);
    let next_beginning_indexes = next_beginning_indexes(target.len(), &beginning_indexes);

    // The first simple match can start at the beginning of the target or at
    // the next word beginning before it. This is the initial window used by
    // fuzzysort's strict pass.
    let first_possible = if simple[0] == 0 {
        0
    } else {
        next_beginning_indexes[simple[0] - 1]
    };

    let strict = strict_indexes(&target, &search, &next_beginning_indexes, first_possible);
    let (indexes, strict_success) = match strict {
        Some(indexes) => (indexes, true),
        None => (simple, false),
    };

    let mut score = 0i64;
    let mut last_target = None;
    for &index in &indexes {
        if last_target != index.checked_sub(1) {
            score -= index as i64;
        }
        last_target = Some(index);
    }
    if !strict_success {
        score *= 1000;
    }
    score -= target.len().saturating_sub(search.len()) as i64;

    Some(FuzzyMatch { score, indexes })
}

fn prepare_lower_codes(value: &str) -> Vec<u32> {
    let original_len = value.encode_utf16().count();
    let lowered: Vec<u16> = value.to_lowercase().encode_utf16().collect();
    (0..original_len)
        // `charCodeAt` yields NaN beyond the lowered string.  A sentinel that
        // cannot equal a valid UTF-16 code unit has the same match behavior.
        .map(|index| lowered.get(index).copied().map_or(u32::MAX, u32::from))
        .collect()
}

fn greedy_indexes(target: &[u32], search: &[u32]) -> Option<Vec<usize>> {
    let mut indexes = Vec::with_capacity(search.len());
    let mut search_index = 0;
    for (target_index, target_char) in target.iter().enumerate() {
        if *target_char == search[search_index] {
            indexes.push(target_index);
            search_index += 1;
            if search_index == search.len() {
                return Some(indexes);
            }
        }
    }
    None
}

fn beginning_indexes(target: &str) -> Vec<usize> {
    let mut beginnings = Vec::new();
    let mut was_upper = false;
    let mut was_alphanumeric = false;
    for (index, code) in target.encode_utf16().enumerate() {
        let is_upper = (u16::from(b'A')..=u16::from(b'Z')).contains(&code);
        let is_alphanumeric = is_upper
            || (u16::from(b'a')..=u16::from(b'z')).contains(&code)
            || (u16::from(b'0')..=u16::from(b'9')).contains(&code);
        let is_beginning = (is_upper && !was_upper) || !was_alphanumeric || !is_alphanumeric;
        if is_beginning {
            beginnings.push(index);
        }
        was_upper = is_upper;
        was_alphanumeric = is_alphanumeric;
    }
    beginnings
}

fn next_beginning_indexes(target_len: usize, beginnings: &[usize]) -> Vec<usize> {
    if target_len == 0 {
        return Vec::new();
    }
    let mut next = vec![target_len; target_len];
    let mut beginning_index = 0;
    let mut last_beginning = beginnings.first().copied().unwrap_or(target_len);
    for (index, next_index) in next.iter_mut().enumerate() {
        if last_beginning > index {
            *next_index = last_beginning;
        } else {
            beginning_index += 1;
            last_beginning = beginnings.get(beginning_index).copied().unwrap_or(target_len);
            *next_index = last_beginning;
        }
    }
    next
}

fn strict_indexes(
    target: &[u32],
    search: &[u32],
    next_beginning: &[usize],
    mut target_index: usize,
) -> Option<Vec<usize>> {
    if target_index == target.len() {
        return None;
    }
    let mut search_index = 0usize;
    let mut indexes = Vec::with_capacity(search.len());

    loop {
        if target_index >= target.len() {
            if search_index == 0 {
                break;
            }
            search_index -= 1;
            let last_match = indexes.pop()?;
            target_index = next_beginning[last_match];
        } else if search[search_index] == target[target_index] {
            indexes.push(target_index);
            search_index += 1;
            if search_index == search.len() {
                return Some(indexes);
            }
            target_index += 1;
        } else {
            target_index = next_beginning[target_index];
        }
    }
    None
}

/// Character indexes in `name` that fuzzysort would use for highlighting.
/// This is kept under `cfg(test)` because the GPUI renderer has its own copy
/// and the engine only needs the score in production.
#[cfg(test)]
pub fn fuzzy_indexes(name: &str, query: &str) -> Option<Vec<usize>> {
    fuzzy_match(name, query).map(|matched| matched.indexes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_query_agrees_with_match_score() {
        let names = [
            "git",
            "GIT status",
            "gi",
            "",
            "checkout",
            "ΟΣ",
            "ΟΣ x",
            "\u{212a}elvin",
            "straße",
        ];
        let queries = ["git", "GI", "git status", "c", "x", "ος", "k", "STRASSE", "gts"];
        for name in names {
            for query in queries {
                for fuzzy in [false, true] {
                    assert_eq!(
                        matches_query(name, query, fuzzy),
                        match_score(name, query, fuzzy).is_some(),
                        "{name:?} {query:?} fuzzy={fuzzy}"
                    );
                }
            }
            assert!(matches_query(name, "", false));
        }
    }

    #[test]
    fn prefix_and_fuzzy_match() {
        assert!(matches_query("checkout", "ch", false));
        assert!(!matches_query("checkout", "ckt", false));
        assert!(matches_query("checkout", "ckt", true));
        assert!(!matches_query("status", "ch", true));
        assert!(matches_query("Checkout", "ch", false));
        assert!(matches_query("文件.txt", "文", true));
    }

    #[test]
    fn fuzzy_indexes_are_character_positions() {
        assert_eq!(fuzzy_indexes("checkout", "ckt").as_deref(), Some(&[0, 4, 7][..]));
        assert_eq!(fuzzy_indexes("checkout", "git"), None);
        assert_eq!(fuzzy_indexes("文件.txt", "文").as_deref(), Some(&[0][..]));
    }

    #[test]
    fn fuzzy_indexes_follow_javascript_utf16_units() {
        assert_eq!(fuzzy_indexes("x😀y", "😀").as_deref(), Some(&[1, 2][..]));
        assert_eq!(fuzzy_indexes("😀", "😀").as_deref(), Some(&[0, 1][..]));
        assert_eq!(fuzzy_match("x😀y", "😀").unwrap().score, -3);

        // JavaScript prepares one code unit for the one-unit query `İ`, even
        // though lowercasing expands it to `i` plus a combining dot.
        assert_eq!(fuzzy_indexes("İ", "i").as_deref(), Some(&[0][..]));
        assert_eq!(fuzzy_indexes("i", "İ").as_deref(), Some(&[0][..]));
        assert_eq!(match_score("İ", "i", true).unwrap().kind, MatchKind::PrefixInsensitive);
    }

    #[test]
    fn fuzzysort_prefers_a_word_boundary_for_repeated_letters() {
        let boundary = fuzzy_match("git-checkout", "ch").expect("boundary match");
        let scattered = fuzzy_match("git config", "gt").expect("scattered match");
        assert!(boundary.score > scattered.score);

        // A repeated character query should not always stick to the first
        // greedy occurrence when a later strict run is available.
        let indexes = fuzzy_indexes("fooBar", "fb").expect("fuzzy match");
        assert_eq!(indexes, vec![0, 3]);
    }

    #[test]
    fn match_buckets_follow_webview_exact_prefix_order() {
        assert_eq!(match_score("git", "git", true).unwrap().kind, MatchKind::ExactCase);
        assert_eq!(
            match_score("Git", "git", true).unwrap().kind,
            MatchKind::ExactInsensitive
        );
        assert_eq!(
            match_score("git-status", "git", true).unwrap().kind,
            MatchKind::PrefixCase
        );
        assert_eq!(
            match_score("Git-status", "git", true).unwrap().kind,
            MatchKind::PrefixInsensitive
        );
    }

    #[test]
    fn cmp_ignore_ascii_case_orders_like_lowercase() {
        assert_eq!(cmp_ignore_ascii_case("Git", "git"), Ordering::Equal);
        assert_eq!(cmp_ignore_ascii_case("gi", "git"), Ordering::Less);
        assert_eq!(cmp_ignore_ascii_case("gzip", "git"), Ordering::Greater);
    }
}
