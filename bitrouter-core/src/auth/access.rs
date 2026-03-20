//! Access control — glob-based allowlist matching for models and tools.

/// Returns `true` if the requested name is allowed given a glob-based allowlist.
///
/// When `patterns` is empty, all names are allowed.
/// Each pattern supports `*` as a wildcard that matches any sequence of
/// characters (e.g., `"openai/*"` matches `"openai/gpt-4o"`).
///
/// Used for both model names (e.g., `"openai/gpt-4o"`) and tool names
/// (e.g., `"github/search"`).
fn is_pattern_allowed(requested: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|p| glob_match(p, requested))
}

/// Returns `true` if the requested model is allowed given the allowlist.
pub fn is_model_allowed(requested: &str, patterns: &[String]) -> bool {
    is_pattern_allowed(requested, patterns)
}

/// Returns `true` if the requested tool is allowed given the allowlist.
///
/// Tool names follow `{server}/{tool}` format, so patterns like
/// `"github/*"` or `"*/search"` work naturally.
pub fn is_tool_allowed(requested: &str, patterns: &[String]) -> bool {
    is_pattern_allowed(requested, patterns)
}

/// Simple glob matcher supporting `*` wildcards.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();

    // No wildcard — exact match.
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        if i == 0 {
            // First segment must be a prefix.
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            // Last segment must be a suffix.
            if !text[pos..].ends_with(part) {
                return false;
            }
            pos = text.len();
        } else {
            // Middle segment must appear somewhere after current position.
            match text[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patterns_allow_all() {
        assert!(is_model_allowed("anything", &[]));
    }

    #[test]
    fn exact_match() {
        let patterns = vec!["openai/gpt-4o".to_string()];
        assert!(is_model_allowed("openai/gpt-4o", &patterns));
        assert!(!is_model_allowed("openai/gpt-4o-mini", &patterns));
    }

    #[test]
    fn wildcard_suffix() {
        let patterns = vec!["openai/*".to_string()];
        assert!(is_model_allowed("openai/gpt-4o", &patterns));
        assert!(is_model_allowed("openai/gpt-4o-mini", &patterns));
        assert!(!is_model_allowed("anthropic/claude-3.5", &patterns));
    }

    #[test]
    fn wildcard_prefix() {
        let patterns = vec!["*/claude-*".to_string()];
        assert!(is_model_allowed("anthropic/claude-3.5", &patterns));
        assert!(!is_model_allowed("openai/gpt-4o", &patterns));
    }

    #[test]
    fn multiple_patterns() {
        let patterns = vec!["openai/*".to_string(), "anthropic/claude-*".to_string()];
        assert!(is_model_allowed("openai/gpt-4o", &patterns));
        assert!(is_model_allowed("anthropic/claude-3.5", &patterns));
        assert!(!is_model_allowed("google/gemini-pro", &patterns));
    }

    #[test]
    fn star_matches_everything() {
        let patterns = vec!["*".to_string()];
        assert!(is_model_allowed("anything/at/all", &patterns));
    }

    // ── is_tool_allowed tests ────────────────────────────────────

    #[test]
    fn tool_empty_patterns_allow_all() {
        assert!(is_tool_allowed("github/search", &[]));
    }

    #[test]
    fn tool_exact_match() {
        let patterns = vec!["github/search".to_string()];
        assert!(is_tool_allowed("github/search", &patterns));
        assert!(!is_tool_allowed("github/get_file", &patterns));
    }

    #[test]
    fn tool_wildcard_suffix() {
        let patterns = vec!["github/*".to_string()];
        assert!(is_tool_allowed("github/search", &patterns));
        assert!(is_tool_allowed("github/get_file", &patterns));
        assert!(!is_tool_allowed("slack/post_message", &patterns));
    }

    #[test]
    fn tool_wildcard_prefix() {
        let patterns = vec!["*/search".to_string()];
        assert!(is_tool_allowed("github/search", &patterns));
        assert!(is_tool_allowed("jira/search", &patterns));
        assert!(!is_tool_allowed("github/get_file", &patterns));
    }

    #[test]
    fn tool_multiple_patterns() {
        let patterns = vec!["github/*".to_string(), "slack/post_message".to_string()];
        assert!(is_tool_allowed("github/search", &patterns));
        assert!(is_tool_allowed("slack/post_message", &patterns));
        assert!(!is_tool_allowed("slack/list_channels", &patterns));
    }

    #[test]
    fn tool_star_matches_all() {
        let patterns = vec!["*".to_string()];
        assert!(is_tool_allowed("anything/at/all", &patterns));
    }
}
