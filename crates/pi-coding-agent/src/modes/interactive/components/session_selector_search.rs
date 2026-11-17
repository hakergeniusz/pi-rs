//! Session search (`session-selector-search.ts`). Tiered matching over the
//! session list: an exact hit in any field beats a scattered subsequence in a
//! short field, so tiers dominate the fuzzy scores they carry.

use crate::session::store::SessionInfo;
use pi_tui::fuzzy::fuzzy_match;

/// Match tiers, best first: the query text appears in the session name, in
/// the session id/messages/cwd, or only scattered across name/cwd.
const TIER_NAME: f64 = 0.0;
const TIER_TEXT: f64 = 1.0;
const TIER_FUZZY: f64 = 2.0;
const TIER_WEIGHT: f64 = 1_000_000.0;

/// Whitespace-collapsed, lowercased text (phrase matching normalizes this
/// way so multi-word phrases survive line wraps in the stored messages).
fn normalize_whitespace_lower(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut last_was_space = false;
    for character in text.to_lowercase().chars() {
        let is_space = character.is_whitespace();
        if is_space {
            if last_was_space {
                continue;
            }
            normalized.push(' ');
        } else {
            normalized.push(character);
        }
        last_was_space = is_space;
    }
    normalized.trim_end().to_string()
}

fn tier_score(tier: f64, score: f64) -> f64 {
    tier * TIER_WEIGHT + score
}

/// Exact (case-insensitive) text match. Messages and ids are only matched
/// exactly: scattered matches in them are noise.
fn match_literal_token(session: &SessionInfo, value: &str) -> Option<f64> {
    let value_lower = value.to_lowercase();
    if let Some(name) = &session.name {
        let name_lower = name.to_lowercase();
        if name_lower.contains(&value_lower) {
            return Some(tier_score(TIER_NAME, fuzzy_match(value, name).score));
        }
    }
    if session.id.to_lowercase().contains(&value_lower)
        || session
            .all_messages_text
            .to_lowercase()
            .contains(&value_lower)
        || session.cwd.to_lowercase().contains(&value_lower)
    {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

fn match_fuzzy_token(session: &SessionInfo, value: &str) -> Option<f64> {
    if let Some(literal) = match_literal_token(session, value) {
        return Some(literal);
    }
    let mut best: Option<f64> = None;
    let name = session.name.as_deref().unwrap_or("");
    for field in [name, session.cwd.as_str()] {
        let matched = fuzzy_match(value, field);
        if matched.matches && best.is_none_or(|score| matched.score < score) {
            best = Some(matched.score);
        }
    }
    best.map(|score| tier_score(TIER_FUZZY, score))
}

fn match_phrase_token(session: &SessionInfo, phrase: &str) -> Option<f64> {
    if let Some(name) = &session.name {
        let normalized = normalize_whitespace_lower(name);
        if normalized.contains(phrase) {
            return Some(tier_score(
                TIER_NAME,
                fuzzy_match(phrase, &normalized).score,
            ));
        }
    }
    let messages = normalize_whitespace_lower(&session.all_messages_text);
    if session.id.to_lowercase().contains(phrase) || messages.contains(phrase) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if normalize_whitespace_lower(&session.cwd).contains(phrase) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

fn match_regex(session: &SessionInfo, regex: &regex::Regex) -> Option<f64> {
    if let Some(name) = &session.name && regex.is_match(name) {
        return Some(tier_score(TIER_NAME, 0.0));
    }
    if regex.is_match(&session.id)
        || regex.is_match(&session.all_messages_text)
        || regex.is_match(&session.cwd)
    {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

/// `hasSessionName` (`session-selector-search.ts:93`): the session carries a
/// non-blank user-defined name.
pub fn has_session_name(session: &SessionInfo) -> bool {
    session
        .name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

/// A parsed query: `re:<pattern>` regex mode, or whitespace-separated tokens
/// with `"phrase"` support.
pub struct ParsedQuery {
    pub tokens: Vec<QueryToken>,
    pub regex: Option<regex::Regex>,
    pub error: Option<String>,
}

pub enum QueryToken {
    /// Scattered-or-exact match across name/cwd (then exact elsewhere).
    Fuzzy(String),
    /// Whitespace-normalized substring match.
    Phrase(String),
}

/// `parseSearchQuery` (`session-selector-search.ts:101-165`): `re:` switches
/// to regex mode; otherwise tokens split on whitespace with double-quote
/// phrase grouping. Unbalanced quotes fall back to plain tokenization.
pub fn parse_search_query(query: &str) -> ParsedQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedQuery {
            tokens: Vec::new(),
            regex: None,
            error: None,
        };
    }
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedQuery {
                tokens: Vec::new(),
                regex: None,
                error: Some("Empty regex".to_string()),
            };
        }
        return match regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(regex) => ParsedQuery {
                tokens: Vec::new(),
                regex: Some(regex),
                error: None,
            },
            Err(error) => ParsedQuery {
                tokens: Vec::new(),
                regex: None,
                error: Some(error.to_string()),
            },
        };
    }
    let mut tokens: Vec<QueryToken> = Vec::new();
    let mut buffer = String::new();
    let mut in_quote = false;
    let mut had_unclosed_quote = false;
    {
        let flush = |tokens: &mut Vec<QueryToken>, buffer: &mut String| {
            let value = buffer.trim().to_string();
            buffer.clear();
            if !value.is_empty() {
                tokens.push(QueryToken::Fuzzy(value));
            }
        };
        for character in trimmed.chars() {
            if character == '"' {
                if in_quote {
                    let value = buffer.trim().to_string();
                    buffer.clear();
                    if !value.is_empty() {
                        tokens.push(QueryToken::Phrase(value));
                    }
                    in_quote = false;
                } else {
                    flush(&mut tokens, &mut buffer);
                    in_quote = true;
                }
                continue;
            }
            if !in_quote && character.is_whitespace() {
                flush(&mut tokens, &mut buffer);
                continue;
            }
            buffer.push(character);
        }
    }
    if in_quote {
        had_unclosed_quote = true;
    }
    if had_unclosed_quote {
        // Unbalanced quotes: fall back to plain whitespace tokenization.
        return ParsedQuery {
            tokens: trimmed
                .split_whitespace()
                .filter(|token| !token.is_empty())
                .map(|token| QueryToken::Fuzzy(token.to_string()))
                .collect(),
            regex: None,
            error: None,
        };
    }
    if !buffer.trim().is_empty() {
        let value = buffer.trim().to_string();
        if in_quote {
            tokens.push(QueryToken::Phrase(value));
        } else {
            tokens.push(QueryToken::Fuzzy(value));
        }
    }
    ParsedQuery {
        tokens,
        regex: None,
        error: None,
    }
}

/// `matchSession` (`session-selector-search.ts:171-199`): every token must
/// match (each field separately, so a token never crosses field boundaries).
pub fn match_session(session: &SessionInfo, parsed: &ParsedQuery) -> Option<f64> {
    if let Some(regex) = &parsed.regex {
        return match_regex(session, regex);
    }
    let mut total = 0.0;
    for token in &parsed.tokens {
        let score = match token {
            QueryToken::Phrase(phrase) => {
                let normalized = normalize_whitespace_lower(phrase);
                if normalized.is_empty() {
                    continue;
                }
                match_phrase_token(session, &normalized)
            }
            QueryToken::Fuzzy(value) => match_fuzzy_token(session, value),
        };
        let score = score?;
        total += score;
    }
    Some(total)
}

/// The name filter from the header toggle: all sessions, or only named ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameFilter {
    All,
    Named,
}

/// The sort modes behind the header's Sort: control. `Threaded` shows the
/// fork tree when no query is active; `Recent` keeps listing order;
/// `Relevance` sorts query hits by score (and is what a query does to
/// `Threaded` too).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortMode {
    Threaded,
    Recent,
    Relevance,
}

/// `filterAndSortSessions` (`session-selector-search.ts:201-238`). With a
/// query, `Threaded` and `Relevance` both sort by score (ties: modified
/// descending); `Recent` filters only.
pub fn filter_and_sort_sessions(
    sessions: &[SessionInfo],
    query: &str,
    sort_mode: SortMode,
    name_filter: NameFilter,
) -> Vec<SessionInfo> {
    let name_filtered: Vec<SessionInfo> = match name_filter {
        NameFilter::All => sessions.to_vec(),
        NameFilter::Named => sessions
            .iter()
            .filter(|session| has_session_name(session))
            .cloned()
            .collect(),
    };
    if query.trim().is_empty() {
        return name_filtered;
    }
    let parsed = parse_search_query(query);
    if parsed.error.is_some() {
        return Vec::new();
    }
    if sort_mode == SortMode::Recent {
        return name_filtered
            .into_iter()
            .filter(|session| match_session(session, &parsed).is_some())
            .collect();
    }
    let mut scored: Vec<(f64, SessionInfo)> = name_filtered
        .into_iter()
        .filter_map(|session| match_session(&session, &parsed).map(|score| (score, session)))
        .collect();
    scored.sort_by(|(score_a, session_a), (score_b, session_b)| {
        score_a
            .total_cmp(score_b)
            .then_with(|| session_b.modified.cmp(&session_a.modified))
    });
    scored.into_iter().map(|(_, session)| session).collect()
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::assert_is_empty))]
mod tests {
    use super::*;

    fn session(id: &str, name: Option<&str>, cwd: &str, messages: &str) -> SessionInfo {
        SessionInfo {
            path: std::path::PathBuf::from(format!("/sessions/{id}.jsonl")),
            id: id.to_string(),
            cwd: cwd.to_string(),
            name: name.map(str::to_string),
            parent_session_path: None,
            created: chrono::Utc::now(),
            modified: chrono::Utc::now(),
            message_count: 2,
            first_message: messages.to_string(),
            all_messages_text: messages.to_string(),
        }
    }

    #[test]
    fn name_hits_rank_above_message_hits() {
        let named = session("a", Some("deploy notes"), "/tmp", "unrelated text");
        let textual = session("b", None, "/tmp", "talking about deploy notes");
        let parsed = parse_search_query("deploy");
        let score_named = match_session(&named, &parsed).expect("named matches");
        let score_text = match_session(&textual, &parsed).expect("text matches");
        assert!(score_named < score_text, "{score_named} vs {score_text}");
    }

    #[test]
    fn every_token_must_match() {
        let item = session("a", Some("deploy notes"), "/tmp", "other stuff");
        let parsed = parse_search_query("deploy missing");
        assert!(match_session(&item, &parsed).is_none());
    }

    #[test]
    fn quoted_phrases_match_normalized_whitespace() {
        let item = session("a", None, "/tmp", "fix the\n  node cve bug");
        let parsed = parse_search_query("\"node cve\"");
        assert!(match_session(&item, &parsed).is_some());
    }

    #[test]
    fn regex_mode_matches_case_insensitively() {
        let item = session("a", Some("Deploy"), "/tmp", "nothing");
        let parsed = parse_search_query("re:^dep");
        assert!(parsed.error.is_none());
        assert!(match_session(&item, &parsed).is_some());
        let broken = parse_search_query("re:([");
        assert!(broken.error.is_some());
        let empty = parse_search_query("re:");
        assert_eq!(empty.error.as_deref(), Some("Empty regex"));
    }

    #[test]
    fn unbalanced_quotes_fall_back_to_tokens() {
        let parsed = parse_search_query("foo \"bar baz");
        assert_eq!(parsed.tokens.len(), 3);
        assert!(
            parsed
                .tokens
                .iter()
                .all(|token| matches!(token, QueryToken::Fuzzy(_)))
        );
    }

    #[test]
    fn relevance_sort_prefers_low_scores_then_recency() {
        let mut older = session("old", Some("alpha"), "/tmp", "x");
        older.modified = chrono::Utc::now() - chrono::Duration::hours(2);
        let newer = session("new", None, "/tmp", "alpha beta");
        let sorted = filter_and_sort_sessions(
            &[newer.clone(), older.clone()],
            "alpha",
            SortMode::Relevance,
            NameFilter::All,
        );
        assert_eq!(sorted[0].id, "old");
        assert_eq!(sorted[1].id, "new");
    }

    #[test]
    fn recent_sort_keeps_listing_order() {
        let a = session("a", Some("alpha"), "/tmp", "x");
        let b = session("b", None, "/tmp", "alpha");
        let sorted = filter_and_sort_sessions(
            &[a.clone(), b.clone()],
            "alpha",
            SortMode::Recent,
            NameFilter::All,
        );
        assert_eq!(sorted[0].id, "a");
        assert_eq!(sorted[1].id, "b");
    }

    #[test]
    fn named_filter_keeps_only_named_sessions() {
        let a = session("a", Some("alpha"), "/tmp", "x");
        let b = session("b", None, "/tmp", "alpha");
        let sorted = filter_and_sort_sessions(&[a, b], "", SortMode::Recent, NameFilter::Named);
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].id, "a");
    }

    #[test]
    fn has_session_name_rejects_blank_names() {
        let mut item = session("a", Some("  "), "/tmp", "x");
        assert!(!has_session_name(&item));
        item.name = Some("real".to_string());
        assert!(has_session_name(&item));
    }
}
