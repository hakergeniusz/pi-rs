//! Fuzzy matching for autocomplete and selectors.
//!
//! Ground truth: `packages/tui/src/fuzzy.ts`. Matches if all query characters
//! appear in order (not necessarily consecutive); lower score = better match.
//!
//! Positions index `char`s rather than JS's UTF-16 code units — identical for
//! the ASCII inputs that dominate here.

/// `FuzzyMatch` (`fuzzy.ts:7-10`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

/// Word-boundary characters that reward a match (`fuzzy.ts:31`).
fn is_word_boundary(previous: char) -> bool {
    previous.is_whitespace() || matches!(previous, '-' | '_' | '.' | '/' | ':')
}

/// `scorePositions` (`fuzzy.ts:12-42`).
fn score_positions(text_lower: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match_index: isize = -1;
    let mut consecutive_matches = 0usize;

    for &i in positions {
        // Reward consecutive matches.
        if last_match_index == i as isize - 1 {
            consecutive_matches += 1;
            score -= (consecutive_matches * 5) as f64;
        } else {
            consecutive_matches = 0;
            // Penalize gaps.
            if last_match_index >= 0 {
                score += ((i as isize - last_match_index - 1) * 2) as f64;
            }
        }

        // Reward word boundary matches.
        if i == 0 || is_word_boundary(text_lower[i - 1]) {
            score -= 10.0;
        }

        // Slight penalty for later matches.
        score += i as f64 * 0.1;

        last_match_index = i as isize;
    }

    score
}

fn char_positions(haystack: &[char], needle: char, from: usize) -> Option<usize> {
    haystack[from..]
        .iter()
        .position(|c| *c == needle)
        .map(|i| from + i)
}

/// Substring starts of `needle` in `haystack` (char positions).
fn substring_starts(haystack: &[char], needle: &[char]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    for start in 0..=haystack.len() - needle.len() {
        if haystack[start..start + needle.len()] == *needle {
            starts.push(start);
        }
    }
    starts
}

/// `matchQuery` (`fuzzy.ts:44-86`).
fn match_query(normalized_query: &[char], text_lower: &[char]) -> FuzzyMatch {
    if normalized_query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }

    if normalized_query.len() > text_lower.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    // Greedy alignment: each query character at its first occurrence after the
    // previous one.
    let mut positions: Vec<usize> = Vec::with_capacity(normalized_query.len());
    let mut last_match_index: isize = -1;
    for &query_char in normalized_query {
        let Some(i) = char_positions(text_lower, query_char, (last_match_index + 1) as usize)
        else {
            return FuzzyMatch {
                matches: false,
                score: 0.0,
            };
        };
        positions.push(i);
        last_match_index = i as isize;
    }

    let mut score = score_positions(text_lower, &positions);

    // The greedy alignment can scatter the query over an earlier, unrelated
    // part of the text, e.g. "opus" in "openrouter openrouter/anthropic/
    // claude-opus" takes o, p, u from "openrouter". Also score every
    // contiguous occurrence and keep the best alignment.
    for start in substring_starts(text_lower, normalized_query) {
        for (k, position) in positions.iter_mut().enumerate() {
            *position = start + k;
        }
        score = score.min(score_positions(text_lower, &positions));
    }

    if normalized_query == text_lower {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// `fuzzyMatch` (`fuzzy.ts:88-115`): primary match, then a digits/letters
/// swapped retry ("sonnet45" ↔ "sonnet54") with a +5 penalty.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary_match = match_query(&query_lower, &text_lower);
    if primary_match.matches {
        return primary_match;
    }

    let swapped_query = swapped_alphanumeric(&query_lower);
    let Some(swapped_query) = swapped_query else {
        return primary_match;
    };

    let swapped_match = match_query(&swapped_query, &text_lower);
    if !swapped_match.matches {
        return primary_match;
    }

    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// The two swap regexes (`fuzzy.ts:97-103`): `letters+digits` ↔ `digits+letters`.
/// The whole query must be exactly one letters run and one digits run.
fn swapped_alphanumeric(query_lower: &[char]) -> Option<Vec<char>> {
    if query_lower.is_empty() || !query_lower.iter().all(char::is_ascii_alphanumeric) {
        return None;
    }
    let transitions = query_lower
        .windows(2)
        .filter(|w| w[0].is_ascii_alphabetic() != w[1].is_ascii_alphabetic())
        .count();
    if transitions != 1 {
        // Single-class queries match neither regex; interleavings match neither.
        return None;
    }
    let split = query_lower
        .iter()
        .position(|c| c.is_ascii_alphabetic() != query_lower[0].is_ascii_alphabetic())
        .expect("one transition");
    let swapped: String = query_lower[split..]
        .iter()
        .chain(query_lower[..split].iter())
        .collect();
    Some(swapped.chars().collect())
}

/// `fuzzyFilter` (`fuzzy.ts:121-159`): filter and sort items by fuzzy match
/// quality (best matches first). Whitespace- and slash-separated tokens must
/// all match. Ties keep the input order (stable sort).
pub fn fuzzy_filter<T: Clone>(items: &[T], query: &str, get_text: impl Fn(&T) -> String) -> Vec<T> {
    if query.trim().is_empty() {
        return items.to_vec();
    }

    let tokens: Vec<String> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();

    if tokens.is_empty() {
        return items.to_vec();
    }

    let mut results: Vec<(T, f64)> = Vec::new();

    for item in items {
        let text = get_text(item);
        let mut total_score = 0.0;
        let mut all_match = true;

        for token in &tokens {
            let matched = fuzzy_match(token, &text);
            if matched.matches {
                total_score += matched.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((item.clone(), total_score));
        }
    }

    results.sort_by(|a, b| a.1.total_cmp(&b.1));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(query: &str, text: &str) -> f64 {
        let m = fuzzy_match(query, text);
        assert!(m.matches, "{query} should match {text}");
        m.score
    }

    #[test]
    fn in_order_subsequence_matches() {
        matched("opus", "claude-opus");
        matched("cs", "claude-sonnet");
        // Missing characters do not.
        assert!(!fuzzy_match("qzx", "claude-opus").matches);
        // Query longer than text does not.
        assert!(!fuzzy_match("claude-opus-model", "opus").matches);
    }

    #[test]
    fn empty_query_matches_everything_with_zero_score() {
        assert_eq!(fuzzy_match("", "anything").score, 0.0);
        assert!(fuzzy_match("", "anything").matches);
    }

    #[test]
    fn exact_match_beats_partial() {
        let exact = matched("opus", "opus");
        let partial = matched("opus", "claude-opus");
        assert!(
            exact < partial,
            "exact {exact} should beat partial {partial}"
        );
    }

    #[test]
    fn contiguous_occurrence_beats_scattered_greedy() {
        // "opus" scattered over "openrouter..." must lose to the real
        // "opus" occurrence ("fuzzy.ts:67-69").
        let scattered = matched("opus", "openrouter openrouter/anthropic/claude-opus");
        let direct = matched("opus", "anthropic/claude-opus");
        assert!(
            scattered > direct,
            "scattered {scattered} should score worse than direct {direct}"
        );
    }

    #[test]
    fn word_boundaries_and_consecutive_letters_rewarded() {
        let boundary = matched("so", "claude-sonnet");
        let inner = matched("so", "person");
        assert!(
            boundary < inner,
            "boundary {boundary} should beat inner {inner}"
        );
    }

    #[test]
    fn alphanumeric_swap_matches_with_penalty() {
        // "5gpt" only matches "gpt-5" after swapping to "gpt5".
        assert!(fuzzy_match("5gpt", "gpt-5").matches);
        // Non-alphanumeric queries and single-class queries get no swap.
        assert!(!fuzzy_match("a-b1", "b-a1").matches);
        assert!(!fuzzy_match("abc", "cba").matches);
    }

    #[test]
    fn filter_splits_tokens_and_sorts_best_first() {
        let items = vec![
            "anthropic/claude-opus-4",
            "anthropic/claude-sonnet-4",
            "openai/gpt-5",
        ];
        let filtered = fuzzy_filter(&items, "claude opus", |t| (*t).to_string());
        // Subsequence matching lets "opus" scatter into "claude-sonnet-4"
        // too; the sort must still rank the real match first.
        assert_eq!(
            filtered,
            vec!["anthropic/claude-opus-4", "anthropic/claude-sonnet-4"]
        );

        let filtered = fuzzy_filter(&items, "", |t| (*t).to_string());
        assert_eq!(filtered.len(), 3);

        let filtered = fuzzy_filter(&items, "gpt", |t| (*t).to_string());
        assert_eq!(filtered, vec!["openai/gpt-5"]);

        let filtered = fuzzy_filter(&items, "nope zzz", |t| (*t).to_string());
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_ignores_slash_separators_in_query() {
        let items = vec!["anthropic/claude-opus-4", "openai/gpt-5"];
        // "anthropic/claude" splits into two tokens, both must match.
        let filtered = fuzzy_filter(&items, "anthropic/claude", |t| (*t).to_string());
        assert_eq!(filtered, vec!["anthropic/claude-opus-4"]);
    }
}
