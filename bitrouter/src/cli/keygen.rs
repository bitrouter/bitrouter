//! `bitrouter keygen` subcommand — sign a JWT with the active master key.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_core::jwt::claims::{BitrouterClaims, BudgetRange, BudgetScope, TokenScope};
use bitrouter_core::jwt::token;

use crate::cli::account::load_active_keypair;

/// Parsed keygen options (from CLI flags).
pub struct KeygenOpts {
    pub scope: TokenScope,
    pub exp: Option<String>,
    pub models: Option<Vec<String>>,
    pub budget: Option<u64>,
    pub budget_scope: Option<BudgetScope>,
    pub budget_range: Option<String>,
    pub name: Option<String>,
}

/// Run the `keygen` subcommand.
pub fn run(keys_dir: &Path, opts: KeygenOpts) -> Result<(), String> {
    let (prefix, kp) = load_active_keypair(keys_dir)?;
    let pubkey = kp.public_key_b64();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))?
        .as_secs();

    let exp = parse_expiration(&opts.exp, now)?;

    // Admin tokens must have an expiration.
    if opts.scope == TokenScope::Admin && exp.is_none() {
        return Err(
            "Admin-scope tokens require --exp (e.g., --exp 5m). This limits replay window."
                .to_string(),
        );
    }

    let budget_range = opts
        .budget_range
        .as_deref()
        .map(parse_budget_range)
        .transpose()?;

    let claims = BitrouterClaims {
        iss: pubkey,
        iat: Some(now),
        exp,
        scope: opts.scope,
        models: opts.models,
        budget: opts.budget,
        budget_scope: opts.budget_scope,
        budget_range,
    };

    let jwt =
        token::sign(&claims, kp.signing_key()).map_err(|e| format!("failed to sign JWT: {e}"))?;

    // Save to disk if a name was provided.
    if let Some(ref name) = opts.name {
        let tokens_dir = keys_dir.join(&prefix).join("tokens");
        fs::create_dir_all(&tokens_dir)
            .map_err(|e| format!("failed to create tokens directory: {e}"))?;

        let filename = format!("{name}.jwt");
        fs::write(tokens_dir.join(&filename), &jwt)
            .map_err(|e| format!("failed to save token: {e}"))?;

        eprintln!("Saved to: {}/{filename}", tokens_dir.display());
    }

    println!("{jwt}");
    Ok(())
}

/// Parse an expiration string into an absolute UNIX timestamp.
///
/// Supports:
/// - `"never"` → `None`
/// - Relative durations: `"5m"`, `"1h"`, `"30d"`, `"1y"`
/// - Absolute UNIX timestamp as integer string
fn parse_expiration(exp: &Option<String>, now: u64) -> Result<Option<u64>, String> {
    let s = match exp {
        Some(s) => s.as_str(),
        None => return Ok(None),
    };

    if s == "never" {
        return Ok(None);
    }

    // Try as absolute UNIX timestamp.
    if let Ok(ts) = s.parse::<u64>() {
        return Ok(Some(ts));
    }

    // Parse relative duration: <number><unit>
    let (num_str, unit) = s.split_at(s.len().saturating_sub(1));
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid expiration format: \"{s}\" (try \"5m\", \"1h\", \"30d\")"))?;

    let seconds = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        "y" => num * 365 * 86400,
        _ => {
            return Err(format!(
                "unknown duration unit \"{unit}\" in \"{s}\" (use s/m/h/d/y)"
            ));
        }
    };

    Ok(Some(now + seconds))
}

/// Parse a budget range string.
///
/// Formats: `"rounds:10"`, `"duration:3600s"`
fn parse_budget_range(s: &str) -> Result<BudgetRange, String> {
    if let Some(count_str) = s.strip_prefix("rounds:") {
        let count: u32 = count_str
            .parse()
            .map_err(|_| format!("invalid round count: \"{count_str}\""))?;
        return Ok(BudgetRange::Rounds { count });
    }

    if let Some(rest) = s.strip_prefix("duration:") {
        let secs_str = rest.strip_suffix('s').unwrap_or(rest);
        let seconds: u64 = secs_str
            .parse()
            .map_err(|_| format!("invalid duration seconds: \"{secs_str}\""))?;
        return Ok(BudgetRange::Duration { seconds });
    }

    Err(format!(
        "invalid budget range: \"{s}\" (use \"rounds:N\" or \"duration:Xs\")"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exp_none() {
        assert_eq!(parse_expiration(&None, 1000).ok(), Some(None));
    }

    #[test]
    fn parse_exp_never() {
        assert_eq!(
            parse_expiration(&Some("never".to_string()), 1000).ok(),
            Some(None)
        );
    }

    #[test]
    fn parse_exp_minutes() {
        assert_eq!(
            parse_expiration(&Some("5m".to_string()), 1000).ok(),
            Some(Some(1300))
        );
    }

    #[test]
    fn parse_exp_hours() {
        assert_eq!(
            parse_expiration(&Some("1h".to_string()), 0).ok(),
            Some(Some(3600))
        );
    }

    #[test]
    fn parse_exp_days() {
        assert_eq!(
            parse_expiration(&Some("30d".to_string()), 0).ok(),
            Some(Some(30 * 86400))
        );
    }

    #[test]
    fn parse_exp_absolute() {
        assert_eq!(
            parse_expiration(&Some("1700000000".to_string()), 0).ok(),
            Some(Some(1700000000))
        );
    }

    #[test]
    fn parse_budget_range_rounds() {
        let r = parse_budget_range("rounds:10");
        assert!(matches!(r, Ok(BudgetRange::Rounds { count: 10 })));
    }

    #[test]
    fn parse_budget_range_duration() {
        let r = parse_budget_range("duration:3600s");
        assert!(matches!(r, Ok(BudgetRange::Duration { seconds: 3600 })));
    }

    #[test]
    fn parse_budget_range_duration_no_s() {
        let r = parse_budget_range("duration:60");
        assert!(matches!(r, Ok(BudgetRange::Duration { seconds: 60 })));
    }

    #[test]
    fn parse_budget_range_invalid() {
        assert!(parse_budget_range("invalid").is_err());
    }
}
