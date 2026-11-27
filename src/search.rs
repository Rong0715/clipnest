//! Search modes from Maccy: exact, fuzzy, regex and mixed (exact → regex → fuzzy).

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use regex::{Regex, RegexBuilder};

use crate::settings::SearchMode;

/// A match: the index into the searched slice and matched byte ranges in its title.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub index: usize,
    pub ranges: Vec<(usize, usize)>,
}

/// Searching very long titles fuzzily is slow and rarely useful.
const FUZZY_LIMIT: usize = 5_000;

pub fn search(query: &str, mode: SearchMode, titles: &[&str]) -> Vec<Match> {
    if query.is_empty() {
        return (0..titles.len()).map(|index| Match { index, ranges: Vec::new() }).collect();
    }
    match mode {
        SearchMode::Exact => exact(query, titles),
        SearchMode::Regexp => regexp(query, titles),
        SearchMode::Fuzzy => fuzzy(query, titles),
        SearchMode::Mixed => {
            let results = exact(query, titles);
            if !results.is_empty() {
                return results;
            }
            let results = regexp(query, titles);
            if !results.is_empty() {
                return results;
            }
            fuzzy(query, titles)
        }
    }
}

fn with_regex(re: &Regex, titles: &[&str]) -> Vec<Match> {
    titles
        .iter()
        .enumerate()
        .filter_map(|(index, title)| {
            re.find(title).filter(|m| m.end() > m.start()).map(|m| Match {
                index,
                ranges: vec![(m.start(), m.end())],
            })
        })
        .collect()
}

fn exact(query: &str, titles: &[&str]) -> Vec<Match> {
    match RegexBuilder::new(&regex::escape(query)).case_insensitive(true).build() {
        Ok(re) => with_regex(&re, titles),
        Err(_) => Vec::new(),
    }
}

fn regexp(query: &str, titles: &[&str]) -> Vec<Match> {
    match Regex::new(query) {
        Ok(re) => with_regex(&re, titles),
        Err(_) => Vec::new(),
    }
}

fn fuzzy(query: &str, titles: &[&str]) -> Vec<Match> {
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut scored: Vec<(i64, Match)> = titles
        .iter()
        .enumerate()
        .filter_map(|(index, title)| {
            let limited: &str = match title.char_indices().nth(FUZZY_LIMIT) {
                Some((end, _)) => &title[..end],
                None => title,
            };
            let (score, char_indices) = matcher.fuzzy_indices(limited, query)?;
            Some((score, Match { index, ranges: char_ranges_to_bytes(limited, &char_indices) }))
        })
        .collect();
    // Best score first; keep the original order among equal scores.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.index.cmp(&b.1.index)));
    scored.into_iter().map(|(_, m)| m).collect()
}

/// Converts matched char indices into merged byte ranges.
fn char_ranges_to_bytes(text: &str, char_indices: &[usize]) -> Vec<(usize, usize)> {
    let offsets: Vec<(usize, char)> = text.char_indices().collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &ci in char_indices {
        let Some(&(start, ch)) = offsets.get(ci) else { continue };
        let end = start + ch.len_utf8();
        match ranges.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    const TITLES: &[&str] = &["Hello World", "foo bar", "fizzbuzz", "Ünïcode Straße"];

    #[test]
    fn exact_is_case_insensitive() {
        let r = search("world", SearchMode::Exact, TITLES);
        assert_eq!(r, vec![Match { index: 0, ranges: vec![(6, 11)] }]);
        let r = search("STRASSE", SearchMode::Exact, TITLES);
        assert!(r.is_empty());
        let r = search("straße", SearchMode::Exact, TITLES);
        assert_eq!(r[0].index, 3);
    }

    #[test]
    fn regex_and_invalid_regex() {
        assert_eq!(search("f.z+", SearchMode::Regexp, TITLES)[0].index, 2);
        assert!(search("(", SearchMode::Regexp, TITLES).is_empty());
    }

    #[test]
    fn fuzzy_and_mixed() {
        let r = search("fzbz", SearchMode::Fuzzy, TITLES);
        assert_eq!(r[0].index, 2);
        assert!(!r[0].ranges.is_empty());
        // mixed falls back to fuzzy when nothing matches exactly
        assert_eq!(search("hlowrd", SearchMode::Mixed, TITLES)[0].index, 0);
        // and prefers exact matches
        assert_eq!(search("bar", SearchMode::Mixed, TITLES).len(), 1);
    }

    #[test]
    fn empty_query_returns_everything() {
        assert_eq!(search("", SearchMode::Fuzzy, TITLES).len(), TITLES.len());
    }
}
