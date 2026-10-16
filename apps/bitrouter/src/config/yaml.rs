//! Shared scalar encoding for generated agent and MCP configuration.

/// Quote a YAML scalar when it starts with a character YAML treats as
/// special at the start of a plain scalar (`@`, `` ` ``, `!`, `&`, `*`, `|`,
/// `>`, `'`, `"`, `%`, `#`, `?`, `:`, `-`, `,`, `{`, `}`, `[`, `]`), or
/// contains a space or `#`. Conservative: when in doubt, double-quote.
/// Shared with the MCP registry stub renderer (`crate::mcp_registry`).
pub(crate) fn yaml_scalar(s: &str) -> String {
    if s.is_empty() {
        // An unquoted empty plain scalar parses as YAML null, which would
        // silently corrupt a command/arg field. Always quote.
        return "\"\"".to_string();
    }
    let first_special = s
        .chars()
        .next()
        .map(|c| {
            matches!(
                c,
                '@' | '`'
                    | '!'
                    | '&'
                    | '*'
                    | '|'
                    | '>'
                    | '\''
                    | '"'
                    | '%'
                    | '#'
                    | '?'
                    | ':'
                    | '-'
                    | ','
                    | '{'
                    | '}'
                    | '['
                    | ']'
            )
        })
        .unwrap_or(false);
    let needs_quotes = first_special
        || s.chars().any(|c| c.is_whitespace() || c.is_control())
        || s.contains('#')
        || s.contains(':')
        || parses_as_non_string(s);
    if needs_quotes {
        // JSON string syntax is a valid YAML double-quoted scalar and handles
        // every control character, not just backslashes and quotes.
        serde_json::Value::String(s.to_string()).to_string()
    } else {
        s.to_string()
    }
}

/// True when YAML would parse an unquoted `s` as a non-string scalar
/// (bool / null / number) — emitting it bare would change the type of a
/// String-typed config field (`env` values, header values, args). The stub
/// renderers only emit strings, so these always get quoted.
fn parses_as_non_string(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true"
            | "false"
            | "yes"
            | "no"
            | "on"
            | "off"
            | "null"
            | "~"
            | ".nan"
            | ".inf"
            | "+.inf"
            | "-.inf"
    ) {
        return true;
    }
    if s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok() {
        return true;
    }
    // Underscored / hex / octal integers per the YAML core schema.
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if !digits.is_empty()
        && digits.chars().any(|c| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return true;
    }
    if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    if let Some(oct) = digits
        .strip_prefix("0o")
        .or_else(|| digits.strip_prefix("0O"))
    {
        return !oct.is_empty() && oct.chars().all(|c| matches!(c, '0'..='7'));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::yaml_scalar;

    #[test]
    fn yaml_scalar_quotes_empty_string() {
        // Empty unquoted plain scalar would parse as YAML null and silently
        // corrupt a command / arg field. Regression check: the empty case
        // must be double-quoted.
        assert_eq!(yaml_scalar(""), "\"\"");
    }

    #[test]
    fn yaml_scalar_quotes_non_string_scalars() {
        // `env` / header values and args are String-typed config fields: an
        // unquoted `false` / `123` would parse as bool/int and fail schema
        // validation on the pasted stub. Regression check: YAML non-string
        // scalars are always quoted.
        for s in [
            "false", "true", "yes", "no", "on", "off", "null", "Null", "~", "123", "-1", "+1",
            "1.5", ".5", "1e3", "0x1F", "0o17", "1_000", ".nan", "-.inf",
        ] {
            let quoted = yaml_scalar(s);
            assert!(
                quoted.starts_with('"') && quoted.ends_with('"'),
                "{s} must be quoted, got {quoted}"
            );
        }
        // Strings that merely look numeric-ish stay plain.
        for s in ["1.2.3", "v2", "npx", "false-positive"] {
            assert_eq!(yaml_scalar(s), s, "{s} must stay unquoted");
        }
    }

    #[test]
    fn yaml_scalar_round_trips_control_characters() -> Result<(), Box<dyn std::error::Error>> {
        for value in ["line one\nline two", "tab\tseparated", "carriage\rreturn"] {
            let yaml = format!("value: {}\n", yaml_scalar(value));
            let parsed: std::collections::HashMap<String, String> = serde_saphyr::from_str(&yaml)?;
            assert_eq!(parsed.get("value").map(String::as_str), Some(value));
        }
        Ok(())
    }
}
