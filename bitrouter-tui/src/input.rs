use crate::model::InputTarget;

/// Parse @-mentions from input text and resolve them against known agent names.
///
/// Rules:
/// - `@all` → `InputTarget::All`
/// - `@claude @codex` → `InputTarget::Specific(["claude", "codex"])`
/// - No @-mentions → `InputTarget::Default`
/// - Unrecognised @-names are silently ignored.
pub fn parse_mentions(text: &str, agent_names: &[String]) -> InputTarget {
    let mut found: Vec<String> = Vec::new();
    let mut has_all = false;

    for token in text.split_whitespace() {
        if let Some(name) = token.strip_prefix('@') {
            let lower = name.to_lowercase();
            if lower == "all" {
                has_all = true;
            } else if agent_names.iter().any(|a| a.to_lowercase() == lower)
                && !found.iter().any(|f| f.to_lowercase() == lower)
            {
                // Preserve the canonical agent name casing.
                if let Some(canonical) = agent_names.iter().find(|a| a.to_lowercase() == lower) {
                    found.push(canonical.clone());
                }
            }
        }
    }

    if has_all {
        InputTarget::All
    } else if found.is_empty() {
        InputTarget::Default
    } else {
        InputTarget::Specific(found)
    }
}

/// Strip all `@name` and `@all` mention tokens from the text, leaving the
/// clean prompt for sending to agents.
pub fn strip_mentions(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for token in text.split_whitespace() {
        if token.starts_with('@') {
            continue;
        }
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(token);
    }
    result
}

/// Given the full input line and the cursor column position, return the
/// @-prefix being typed if the cursor is immediately after an `@word`.
///
/// Returns `None` if the cursor isn't in an @-mention position.
pub fn autocomplete_prefix(line: &str, cursor_col: usize) -> Option<String> {
    let char_count = line.chars().count();
    if cursor_col == 0 || cursor_col > char_count {
        return None;
    }

    // Convert char index to byte index for slicing.
    let end_byte = char_to_byte(line, cursor_col);

    // Walk backwards from cursor (in chars) to find '@'.
    let chars: Vec<(usize, char)> = line.char_indices().take(cursor_col).collect();
    let mut i = chars.len();
    while i > 0 {
        i -= 1;
        let (byte_pos, ch) = chars[i];
        if ch == '@' {
            // The '@' must be at the start of a word (preceded by whitespace or BOL).
            if byte_pos == 0
                || chars
                    .get(i.wrapping_sub(1))
                    .is_some_and(|(_, c)| c.is_whitespace())
            {
                let prefix = &line[byte_pos + 1..end_byte];
                return Some(prefix.to_string());
            }
            return None;
        }
        if ch.is_whitespace() {
            return None;
        }
    }
    None
}

/// Convert a char index to a byte index within a string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

/// Filter agent names by prefix (case-insensitive). Also includes `"all"`
/// if it matches the prefix.
pub fn filter_candidates(prefix: &str, agent_names: &[String]) -> Vec<String> {
    let lower = prefix.to_lowercase();
    let mut result: Vec<String> = agent_names
        .iter()
        .filter(|name| name.to_lowercase().starts_with(&lower))
        .cloned()
        .collect();

    if "all".starts_with(&lower) {
        result.push("all".to_string());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> Vec<String> {
        vec![
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
        ]
    }

    // ── parse_mentions ──────────────────────────────────────────────

    #[test]
    fn no_mentions_returns_default() {
        assert_eq!(
            parse_mentions("hello world", &agents()),
            InputTarget::Default
        );
    }

    #[test]
    fn single_mention() {
        assert_eq!(
            parse_mentions("@claude do something", &agents()),
            InputTarget::Specific(vec!["claude".to_string()])
        );
    }

    #[test]
    fn multiple_mentions() {
        assert_eq!(
            parse_mentions("@claude @codex refactor auth", &agents()),
            InputTarget::Specific(vec!["claude".to_string(), "codex".to_string()])
        );
    }

    #[test]
    fn all_mention() {
        assert_eq!(
            parse_mentions("@all run tests", &agents()),
            InputTarget::All
        );
    }

    #[test]
    fn all_overrides_specific() {
        assert_eq!(
            parse_mentions("@claude @all run tests", &agents()),
            InputTarget::All
        );
    }

    #[test]
    fn unknown_mention_ignored() {
        assert_eq!(
            parse_mentions("@unknown do stuff", &agents()),
            InputTarget::Default
        );
    }

    #[test]
    fn case_insensitive_mention() {
        assert_eq!(
            parse_mentions("@Claude do stuff", &agents()),
            InputTarget::Specific(vec!["claude".to_string()])
        );
    }

    #[test]
    fn duplicate_mention_deduped() {
        assert_eq!(
            parse_mentions("@claude @claude do stuff", &agents()),
            InputTarget::Specific(vec!["claude".to_string()])
        );
    }

    // ── strip_mentions ──────────────────────────────────────────────

    #[test]
    fn strip_removes_mentions() {
        assert_eq!(
            strip_mentions("@claude @codex refactor the auth"),
            "refactor the auth"
        );
    }

    #[test]
    fn strip_removes_all() {
        assert_eq!(strip_mentions("@all run tests"), "run tests");
    }

    #[test]
    fn strip_preserves_non_mentions() {
        assert_eq!(strip_mentions("hello world"), "hello world");
    }

    #[test]
    fn strip_empty_input() {
        assert_eq!(strip_mentions(""), "");
    }

    // ── autocomplete_prefix ─────────────────────────────────────────

    #[test]
    fn autocomplete_at_start() {
        assert_eq!(autocomplete_prefix("@cl", 3), Some("cl".to_string()));
    }

    #[test]
    fn autocomplete_after_space() {
        assert_eq!(autocomplete_prefix("hello @co", 9), Some("co".to_string()));
    }

    #[test]
    fn autocomplete_just_at_sign() {
        assert_eq!(autocomplete_prefix("@", 1), Some(String::new()));
    }

    #[test]
    fn no_autocomplete_mid_word() {
        assert_eq!(autocomplete_prefix("hello@cl", 8), None);
    }

    #[test]
    fn no_autocomplete_cursor_zero() {
        assert_eq!(autocomplete_prefix("@cl", 0), None);
    }

    #[test]
    fn no_autocomplete_no_at() {
        assert_eq!(autocomplete_prefix("hello", 5), None);
    }

    // ── filter_candidates ───────────────────────────────────────────

    #[test]
    fn filter_by_prefix() {
        let result = filter_candidates("cl", &agents());
        assert_eq!(result, vec!["claude".to_string()]);
    }

    #[test]
    fn filter_co_matches_codex() {
        let result = filter_candidates("co", &agents());
        assert_eq!(result, vec!["codex".to_string()]);
    }

    #[test]
    fn filter_empty_returns_all_plus_all() {
        let result = filter_candidates("", &agents());
        assert_eq!(
            result,
            vec![
                "claude".to_string(),
                "codex".to_string(),
                "opencode".to_string(),
                "all".to_string(),
            ]
        );
    }

    #[test]
    fn filter_a_includes_all() {
        let result = filter_candidates("a", &agents());
        assert!(result.contains(&"all".to_string()));
    }
}
