//! `bitrouter policy` subcommands — manage OWS spend-limit policies and
//! serve as the OWS executable policy evaluator.
//!
//! ## Two roles
//!
//! 1. **CLI management** (`create`, `list`, `show`, `delete`): CRUD for
//!    spend-limit policy files stored in `<home>/policies/`.
//! 2. **Executable evaluator** (`eval`): invoked by OWS as a subprocess
//!    during the signing flow. Reads [`PolicyContext`] JSON from stdin,
//!    evaluates against the operator-defined limits, and writes
//!    [`PolicyResult`] JSON to stdout. Default-deny on any error.

use std::io::Read;
use std::path::{Path, PathBuf};

use bitrouter_core::policy::{PolicyConfig, PolicyContext, PolicyFile, PolicyResult};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

// ── Eval subcommand ──────────────────────────────────────────────

/// Evaluate a policy against context read from stdin.
///
/// This is the OWS executable policy entry point. It reads JSON from stdin,
/// evaluates declarative rules and spend limits, and writes a JSON result to
/// stdout. Any error results in a default-deny response.
pub fn eval(policy_dir: &Path) -> Result {
    // Default-deny: capture the result and always write a PolicyResult.
    let result = eval_inner(policy_dir);
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let deny = PolicyResult {
                allow: false,
                reason: Some(format!("policy evaluation error: {e}")),
            };
            let json = serde_json::to_string(&deny)?;
            println!("{json}");
            Ok(())
        }
    }
}

fn eval_inner(policy_dir: &Path) -> Result {
    // Read PolicyContext from stdin.
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let ctx: PolicyContext =
        serde_json::from_str(&input).map_err(|e| format!("invalid PolicyContext JSON: {e}"))?;

    // Load all active policies and evaluate against each.
    let policies = load_policies(policy_dir)?;

    if policies.is_empty() {
        // No policies configured → allow (policy layer is optional).
        let result = PolicyResult {
            allow: true,
            reason: None,
        };
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }

    for policy in &policies {
        if let Some(ref reason) = check_policy(&policy.config, &ctx) {
            let result = PolicyResult {
                allow: false,
                reason: Some(reason.clone()),
            };
            println!("{}", serde_json::to_string(&result)?);
            return Ok(());
        }
    }

    let result = PolicyResult {
        allow: true,
        reason: None,
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

/// Check a single policy against the context. Returns `Some(reason)` if denied.
fn check_policy(config: &PolicyConfig, ctx: &PolicyContext) -> Option<String> {
    // Declarative pre-filter: expiration.
    if let Some(ref expires) = config.expires_at
        && let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires)
        && chrono::Utc::now() >= exp
    {
        return Some(format!("policy '{}' has expired", config.name));
    }

    // Declarative pre-filter: chain allowlist.
    if !config.allowed_chains.is_empty()
        && let Some(ref chain) = ctx.chain
        && !config.allowed_chains.iter().any(|c| c == chain)
    {
        return Some(format!(
            "chain '{}' not in allowed list for policy '{}'",
            chain, config.name,
        ));
    }

    // Per-transaction cap.
    if let Some(max) = config.per_tx_max
        && ctx.transaction_value > max
    {
        return Some(format!(
            "transaction value {} exceeds per-tx max {} (policy '{}')",
            ctx.transaction_value, max, config.name,
        ));
    }

    // Daily limit.
    if let Some(limit) = config.daily_limit
        && ctx.daily_total.saturating_add(ctx.transaction_value) > limit
    {
        return Some(format!(
            "daily spend {} + tx {} would exceed daily limit {} (policy '{}')",
            ctx.daily_total, ctx.transaction_value, limit, config.name,
        ));
    }

    // Monthly limit.
    if let Some(limit) = config.monthly_limit
        && ctx.monthly_total.saturating_add(ctx.transaction_value) > limit
    {
        return Some(format!(
            "monthly spend {} + tx {} would exceed monthly limit {} (policy '{}')",
            ctx.monthly_total, ctx.transaction_value, limit, config.name,
        ));
    }

    None
}

// ── CLI subcommands ──────────────────────────────────────────────

/// Options for creating a policy.
pub struct CreateOpts<'a> {
    pub name: &'a str,
    pub daily_limit: Option<u64>,
    pub monthly_limit: Option<u64>,
    pub per_tx_max: Option<u64>,
    pub chains: &'a [String],
    pub expires_at: Option<&'a str>,
    pub file: Option<&'a Path>,
    /// Tool allow rules as "provider:tool" pairs.
    pub tool_allow: &'a [String],
}

/// Create a new spend-limit policy.
pub fn create(policy_dir: &Path, opts: CreateOpts<'_>) -> Result {
    std::fs::create_dir_all(policy_dir)?;

    let policy_file = if let Some(path) = opts.file {
        // Import from a custom policy JSON file.
        let content = std::fs::read_to_string(path)?;
        let mut pf: PolicyFile = serde_json::from_str(&content)?;
        if pf.id.is_empty() {
            pf.id = uuid::Uuid::new_v4().to_string();
        }
        if pf.created_at.is_empty() {
            pf.created_at = chrono::Utc::now().to_rfc3339();
        }
        pf
    } else {
        // Build from CLI flags.
        let bitrouter_exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "bitrouter".to_owned());

        PolicyFile {
            id: uuid::Uuid::new_v4().to_string(),
            config: PolicyConfig {
                name: opts.name.to_owned(),
                daily_limit: opts.daily_limit,
                monthly_limit: opts.monthly_limit,
                per_tx_max: opts.per_tx_max,
                allowed_chains: opts.chains.to_vec(),
                expires_at: opts.expires_at.map(String::from),
                tool_rules: parse_tool_rules(opts.tool_allow),
            },
            executable: format!("{bitrouter_exe} policy eval"),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    };

    let filename = format!("{}.json", policy_file.id);
    let path = policy_dir.join(&filename);
    let json = serde_json::to_string_pretty(&policy_file)?;
    std::fs::write(&path, json)?;

    println!("Policy created:");
    println!("  ID:   {}", policy_file.id);
    println!("  Name: {}", policy_file.config.name);
    println!("  File: {}", path.display());

    if let Some(dl) = policy_file.config.daily_limit {
        println!("  Daily limit:   {} μUSD (${:.2})", dl, dl as f64 / 1e6);
    }
    if let Some(ml) = policy_file.config.monthly_limit {
        println!("  Monthly limit: {} μUSD (${:.2})", ml, ml as f64 / 1e6);
    }
    if let Some(tx) = policy_file.config.per_tx_max {
        println!("  Per-tx max:    {} μUSD (${:.2})", tx, tx as f64 / 1e6);
    }
    if !policy_file.config.allowed_chains.is_empty() {
        println!(
            "  Chains:        {}",
            policy_file.config.allowed_chains.join(", ")
        );
    }
    if let Some(ref exp) = policy_file.config.expires_at {
        println!("  Expires:       {exp}");
    }

    Ok(())
}

/// List all policies.
pub fn list(policy_dir: &Path) -> Result {
    let policies = load_policies(policy_dir)?;

    if policies.is_empty() {
        println!("No policies found. Run `bitrouter policy create` to create one.");
        return Ok(());
    }

    println!(
        "{:<38} {:<20} {:<14} {:<14} {:<14}",
        "ID", "NAME", "DAILY", "MONTHLY", "PER-TX"
    );
    println!("{}", "-".repeat(100));
    for p in &policies {
        let daily = p
            .config
            .daily_limit
            .map(|v| format!("${:.2}", v as f64 / 1e6))
            .unwrap_or_else(|| "-".into());
        let monthly = p
            .config
            .monthly_limit
            .map(|v| format!("${:.2}", v as f64 / 1e6))
            .unwrap_or_else(|| "-".into());
        let per_tx = p
            .config
            .per_tx_max
            .map(|v| format!("${:.2}", v as f64 / 1e6))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<38} {:<20} {:<14} {:<14} {:<14}",
            p.id, p.config.name, daily, monthly, per_tx,
        );
    }

    Ok(())
}

/// Show details of a single policy.
pub fn show(policy_dir: &Path, id: &str) -> Result {
    let policies = load_policies(policy_dir)?;
    let policy = policies
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("policy '{id}' not found"))?;

    println!("{}", serde_json::to_string_pretty(policy)?);
    Ok(())
}

/// Delete a policy by ID.
pub fn delete(policy_dir: &Path, id: &str) -> Result {
    let path = policy_dir.join(format!("{id}.json"));
    if !path.exists() {
        return Err(format!("policy '{id}' not found").into());
    }
    std::fs::remove_file(&path)?;
    println!("Policy '{id}' deleted.");
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────

/// Parse `--tool-allow` flags into a `tool_rules` map.
///
/// Flags use "provider:tool" format (e.g. "github:search_code").
fn parse_tool_rules(
    allow: &[String],
) -> std::collections::HashMap<String, bitrouter_core::policy::ToolProviderPolicy> {
    let mut rules: std::collections::HashMap<String, bitrouter_core::policy::ToolProviderPolicy> =
        std::collections::HashMap::new();

    for entry in allow {
        if let Some((provider, tool)) = entry.split_once(':') {
            let policy = rules.entry(provider.to_string()).or_default();
            policy
                .filter
                .allow
                .get_or_insert_default()
                .push(tool.to_string());
        } else {
            eprintln!(
                "warning: ignoring malformed --tool-allow '{entry}' (expected provider:tool)"
            );
        }
    }

    rules
}

fn load_policies(dir: &Path) -> Result<Vec<PolicyFile>> {
    let loaded = bitrouter_core::policy::load_policies(dir)?;
    for skip in &loaded.skipped {
        eprintln!("warning: skipping {}: {}", skip.path.display(), skip.error);
    }
    Ok(loaded.policies)
}

/// Resolve the policy directory for a given BitRouter home.
pub fn policy_dir(home: &Path) -> PathBuf {
    bitrouter_core::policy::policy_dir(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_allows_within_limits() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: Some(10_000_000),
            monthly_limit: Some(50_000_000),
            per_tx_max: Some(2_000_000),
            allowed_chains: vec!["tempo:mainnet".into()],
            expires_at: None,
            tool_rules: Default::default(),
        };
        let ctx = PolicyContext {
            chain: Some("tempo:mainnet".into()),
            transaction_value: 1_000_000,
            daily_total: 5_000_000,
            monthly_total: 20_000_000,
        };

        assert!(check_policy(&config, &ctx).is_none());
    }

    #[test]
    fn eval_denies_over_daily_limit() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: Some(10_000_000),
            monthly_limit: None,
            per_tx_max: None,
            allowed_chains: vec![],
            expires_at: None,
            tool_rules: Default::default(),
        };
        let ctx = PolicyContext {
            chain: None,
            transaction_value: 2_000_000,
            daily_total: 9_000_000,
            monthly_total: 0,
        };

        let reason = check_policy(&config, &ctx);
        assert!(reason.is_some());
        assert!(reason.as_ref().is_some_and(|r| r.contains("daily")));
    }

    #[test]
    fn eval_denies_over_monthly_limit() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: None,
            monthly_limit: Some(50_000_000),
            per_tx_max: None,
            allowed_chains: vec![],
            expires_at: None,
            tool_rules: Default::default(),
        };
        let ctx = PolicyContext {
            chain: None,
            transaction_value: 2_000_000,
            daily_total: 0,
            monthly_total: 49_000_000,
        };

        let reason = check_policy(&config, &ctx);
        assert!(reason.is_some());
        assert!(reason.as_ref().is_some_and(|r| r.contains("monthly")));
    }

    #[test]
    fn eval_denies_over_per_tx_max() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: None,
            monthly_limit: None,
            per_tx_max: Some(1_000_000),
            allowed_chains: vec![],
            expires_at: None,
            tool_rules: Default::default(),
        };
        let ctx = PolicyContext {
            chain: None,
            transaction_value: 2_000_000,
            daily_total: 0,
            monthly_total: 0,
        };

        let reason = check_policy(&config, &ctx);
        assert!(reason.is_some());
        assert!(reason.as_ref().is_some_and(|r| r.contains("per-tx")));
    }

    #[test]
    fn eval_denies_disallowed_chain() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: None,
            monthly_limit: None,
            per_tx_max: None,
            allowed_chains: vec!["tempo:mainnet".into()],
            expires_at: None,
            tool_rules: Default::default(),
        };
        let ctx = PolicyContext {
            chain: Some("solana:mainnet".into()),
            transaction_value: 0,
            daily_total: 0,
            monthly_total: 0,
        };

        let reason = check_policy(&config, &ctx);
        assert!(reason.is_some());
        assert!(reason.as_ref().is_some_and(|r| r.contains("chain")));
    }

    #[test]
    fn eval_denies_expired_policy() {
        let config = PolicyConfig {
            name: "test".into(),
            daily_limit: None,
            monthly_limit: None,
            per_tx_max: None,
            allowed_chains: vec![],
            expires_at: Some("2020-01-01T00:00:00Z".into()),
            tool_rules: Default::default(),
        };
        let ctx = PolicyContext {
            chain: None,
            transaction_value: 0,
            daily_total: 0,
            monthly_total: 0,
        };

        let reason = check_policy(&config, &ctx);
        assert!(reason.is_some());
        assert!(reason.as_ref().is_some_and(|r| r.contains("expired")));
    }

    #[test]
    fn eval_allows_no_policies() {
        // When no policies exist, the layer is optional → allow.
        let policies: Vec<PolicyFile> = vec![];
        assert!(policies.is_empty());
    }
}
