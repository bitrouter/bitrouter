//! `bitrouter keygen` subcommand — sign a JWT with the active master key.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_core::auth::chain::Chain;
use bitrouter_core::auth::claims::{BitrouterClaims, BudgetRange, BudgetScope, TokenScope};
use bitrouter_core::auth::token;
use bitrouter_core::routers::upstream::ToolServerAccessGroups;

use crate::cli::account::load_active_keypair;

const DEFAULT_CHAIN_NAME: &str = "solana";
const LOCAL_ADMIN_JWT_EXPIRATION: &str = "5m";

/// Parsed keygen options (from CLI flags).
pub struct KeygenOpts {
    pub chain: String,
    pub scope: TokenScope,
    pub exp: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub budget: Option<u64>,
    pub budget_scope: Option<BudgetScope>,
    pub budget_range: Option<String>,
    pub name: Option<String>,
    /// Access groups loaded from config (for expanding group patterns in --tools).
    pub mcp_groups: ToolServerAccessGroups,
}

struct SignJwtOpts {
    chain_name: String,
    scope: TokenScope,
    exp_input: Option<String>,
    models: Option<Vec<String>>,
    tools: Option<Vec<String>>,
    budget: Option<u64>,
    budget_scope: Option<BudgetScope>,
    budget_range_input: Option<String>,
}

/// Run the `keygen` subcommand.
pub fn run(keys_dir: &Path, opts: KeygenOpts) -> Result<(), String> {
    let (prefix, kp) = load_active_keypair(keys_dir)?;

    // Expand group patterns (e.g. "dev_tools/*" → "github/*", "jira/*")
    let tools = opts
        .tools
        .map(|patterns| opts.mcp_groups.expand_patterns(&patterns));

    let jwt = sign_jwt(
        &kp,
        SignJwtOpts {
            chain_name: opts.chain,
            scope: opts.scope,
            exp_input: opts.exp,
            models: opts.models,
            tools,
            budget: opts.budget,
            budget_scope: opts.budget_scope,
            budget_range_input: opts.budget_range,
        },
    )?;

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

/// Generate a short-lived admin JWT for local daemon management requests.
pub fn generate_local_admin_jwt(keys_dir: &Path) -> Result<String, String> {
    let (_prefix, kp) = load_active_keypair(keys_dir)?;
    let exp = LOCAL_ADMIN_JWT_EXPIRATION.to_owned();
    sign_jwt(
        &kp,
        SignJwtOpts {
            chain_name: DEFAULT_CHAIN_NAME.to_owned(),
            scope: TokenScope::Admin,
            exp_input: Some(exp),
            models: None,
            tools: None,
            budget: None,
            budget_scope: None,
            budget_range_input: None,
        },
    )
}

fn sign_jwt(
    kp: &bitrouter_core::auth::keys::MasterKeypair,
    opts: SignJwtOpts,
) -> Result<String, String> {
    let chain = parse_chain(&opts.chain_name)?;
    let caip10 = kp
        .caip10(&chain)
        .map_err(|e| format!("failed to derive CAIP-10 identity: {e}"))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))?
        .as_secs();

    let exp = parse_expiration(opts.exp_input.as_deref(), now)?;

    if opts.scope == TokenScope::Admin && exp.is_none() {
        return Err(
            "Admin-scope tokens require --exp (e.g., --exp 5m). This limits replay window."
                .to_string(),
        );
    }

    let budget_range = opts
        .budget_range_input
        .as_deref()
        .map(parse_budget_range)
        .transpose()?;

    let claims = BitrouterClaims {
        iss: caip10.format(),
        chain: chain.caip2(),
        iat: Some(now),
        exp,
        scope: opts.scope,
        models: opts.models,
        tools: opts.tools,
        budget: opts.budget,
        budget_scope: opts.budget_scope,
        budget_range,
    };

    token::sign(&claims, kp).map_err(|e| format!("failed to sign JWT: {e}"))
}

/// Parse a chain name into a [`Chain`].
fn parse_chain(s: &str) -> Result<Chain, String> {
    match s {
        "solana" => Ok(Chain::solana_mainnet()),
        "base" => Ok(Chain::base()),
        other => Err(format!(
            "unsupported chain \"{other}\" — use \"solana\" or \"base\""
        )),
    }
}

/// Parse an expiration string into an absolute UNIX timestamp.
///
/// Supports:
/// - `"never"` → `None`
/// - Relative durations: `"5m"`, `"1h"`, `"30d"`, `"1y"`
/// - Absolute UNIX timestamp as integer string
fn parse_expiration(exp: Option<&str>, now: u64) -> Result<Option<u64>, String> {
    let s = match exp {
        Some(s) => s,
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
        assert_eq!(parse_expiration(None, 1000).ok(), Some(None));
    }

    #[test]
    fn parse_exp_never() {
        assert_eq!(parse_expiration(Some("never"), 1000).ok(), Some(None));
    }

    #[test]
    fn parse_exp_minutes() {
        assert_eq!(parse_expiration(Some("5m"), 1000).ok(), Some(Some(1300)));
    }

    #[test]
    fn parse_exp_hours() {
        assert_eq!(parse_expiration(Some("1h"), 0).ok(), Some(Some(3600)));
    }

    #[test]
    fn parse_exp_days() {
        assert_eq!(
            parse_expiration(Some("30d"), 0).ok(),
            Some(Some(30 * 86400))
        );
    }

    #[test]
    fn parse_exp_absolute() {
        assert_eq!(
            parse_expiration(Some("1700000000"), 0).ok(),
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

    #[test]
    fn sign_jwt_generates_admin_token_with_expiration() {
        let kp = bitrouter_core::auth::keys::MasterKeypair::generate();
        let exp = LOCAL_ADMIN_JWT_EXPIRATION.to_owned();
        let jwt = sign_jwt(
            &kp,
            SignJwtOpts {
                chain_name: DEFAULT_CHAIN_NAME.to_owned(),
                scope: TokenScope::Admin,
                exp_input: Some(exp),
                models: None,
                tools: None,
                budget: None,
                budget_scope: None,
                budget_range_input: None,
            },
        );
        assert!(jwt.is_ok());
        if let Ok(jwt) = jwt {
            let claims = bitrouter_core::auth::token::decode_unverified(&jwt);
            assert!(claims.is_ok());
            if let Ok(claims) = claims {
                assert_eq!(claims.scope, TokenScope::Admin);
                assert!(claims.exp.is_some());
            }
        }
    }
}
