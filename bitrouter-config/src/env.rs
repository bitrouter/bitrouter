use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

/// Loads environment variables from the process environment,
/// optionally supplemented by a `.env` file.
///
/// Precedence (highest wins): process environment > `.env` file.
pub fn load_env(env_file: Option<&Path>) -> HashMap<String, String> {
    let mut env = HashMap::new();

    // Load from .env file first (lower priority)
    if let Some(path) = env_file
        && let Ok(vars) = load_dotenv(path)
    {
        env.extend(vars);
    }

    // Process environment overrides .env file
    for (key, value) in std::env::vars() {
        env.insert(key, value);
    }

    env
}

/// Parses a simple `.env` file (KEY=VALUE per line, `#` comments, optional quoting).
fn load_dotenv(path: &Path) -> std::io::Result<HashMap<String, String>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut vars = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            // Strip optional surrounding quotes
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            vars.insert(key.to_owned(), value.to_owned());
        }
    }

    Ok(vars)
}

/// Substitutes `${VAR_NAME}` patterns in a string using the provided environment map.
///
/// Unresolved variables are replaced with an empty string.
/// Malformed patterns (no closing brace) are emitted literally.
pub fn substitute_env_vars(input: &str, env: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '}' {
                    closed = true;
                    break;
                }
                var_name.push(ch);
            }
            if closed {
                if let Some(value) = env.get(&var_name) {
                    result.push_str(value);
                }
                // missing var → empty string
            } else {
                // malformed → emit literal
                result.push('$');
                result.push('{');
                result.push_str(&var_name);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Recursively substitutes `${VAR}` references in all string values of a YAML value tree.
pub fn substitute_in_value(
    value: serde_json::Value,
    env: &HashMap<String, String>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(substitute_env_vars(&s, env)),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k, substitute_in_value(v, env));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(seq) => serde_json::Value::Array(
            seq.into_iter()
                .map(|v| substitute_in_value(v, env))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_substitution() {
        let env = HashMap::from([("FOO".into(), "bar".into())]);
        assert_eq!(substitute_env_vars("${FOO}", &env), "bar");
        assert_eq!(
            substitute_env_vars("prefix-${FOO}-suffix", &env),
            "prefix-bar-suffix"
        );
    }

    #[test]
    fn missing_var_becomes_empty() {
        let env = HashMap::new();
        assert_eq!(substitute_env_vars("${MISSING}", &env), "");
    }

    #[test]
    fn composable_substitution() {
        let env = HashMap::from([
            ("HOST".into(), "api.example.com".into()),
            ("PORT".into(), "8080".into()),
        ]);
        assert_eq!(
            substitute_env_vars("https://${HOST}:${PORT}/v1", &env),
            "https://api.example.com:8080/v1"
        );
    }

    #[test]
    fn malformed_pattern_emitted_literally() {
        let env = HashMap::new();
        assert_eq!(substitute_env_vars("${UNCLOSED", &env), "${UNCLOSED");
    }

    #[test]
    fn no_substitution_needed() {
        let env = HashMap::new();
        assert_eq!(substitute_env_vars("plain string", &env), "plain string");
    }

    #[test]
    fn yaml_value_substitution() {
        let env = HashMap::from([("KEY".into(), "secret".into())]);
        let input = serde_json::Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("api_key".into(), serde_json::Value::String("${KEY}".into()));
            m.insert("port".into(), serde_json::Value::Number(8080.into()));
            m
        });
        let output = substitute_in_value(input, &env);
        if let serde_json::Value::Object(m) = output {
            assert_eq!(
                m.get("api_key"),
                Some(&serde_json::Value::String("secret".into()))
            );
            // numeric values are untouched
            assert_eq!(m.get("port"), Some(&serde_json::Value::Number(8080.into())));
        } else {
            panic!("expected mapping");
        }
    }
}
