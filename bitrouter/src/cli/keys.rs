//! `bitrouter keys` subcommand — manage locally-stored JWTs.

use std::fs;
use std::path::Path;

use bitrouter_core::jwt::claims::BitrouterClaims;
use bitrouter_core::jwt::token;

use crate::cli::account::load_active_keypair;

/// Run the `keys` subcommand.
pub fn run(
    keys_dir: &Path,
    list: bool,
    show: Option<String>,
    rm: Option<String>,
) -> Result<(), String> {
    if let Some(ref id) = show {
        show_token(keys_dir, id)
    } else if let Some(ref id) = rm {
        remove_token(keys_dir, id)
    } else if list {
        list_tokens(keys_dir)
    } else {
        // Default to listing.
        list_tokens(keys_dir)
    }
}

/// List saved JWTs for the active account.
fn list_tokens(keys_dir: &Path) -> Result<(), String> {
    let (prefix, _kp) = load_active_keypair(keys_dir)?;
    let tokens_dir = keys_dir.join(&prefix).join("tokens");

    let entries = list_jwt_files(&tokens_dir)?;
    if entries.is_empty() {
        println!("No saved tokens for account {prefix}.");
        println!("Run `bitrouter keygen --name <label>` to create one.");
        return Ok(());
    }

    for (name, path) in &entries {
        let summary = read_token_summary(path);
        println!("  {name}");
        println!("       {summary}");
    }

    Ok(())
}

/// Show decoded claims of a stored JWT.
fn show_token(keys_dir: &Path, id: &str) -> Result<(), String> {
    let (prefix, _kp) = load_active_keypair(keys_dir)?;
    let tokens_dir = keys_dir.join(&prefix).join("tokens");

    let jwt_path = resolve_token_path(&tokens_dir, id)?;
    let jwt_str =
        fs::read_to_string(&jwt_path).map_err(|e| format!("failed to read token file: {e}"))?;
    let jwt_str = jwt_str.trim();

    let claims =
        token::decode_unverified(jwt_str).map_err(|e| format!("failed to decode token: {e}"))?;

    let pretty = serde_json::to_string_pretty(&claims)
        .map_err(|e| format!("failed to format claims: {e}"))?;

    println!("{pretty}");
    println!();
    println!("Raw JWT:");
    println!("{jwt_str}");

    Ok(())
}

/// Delete a stored JWT.
fn remove_token(keys_dir: &Path, id: &str) -> Result<(), String> {
    let (prefix, _kp) = load_active_keypair(keys_dir)?;
    let tokens_dir = keys_dir.join(&prefix).join("tokens");

    let jwt_path = resolve_token_path(&tokens_dir, id)?;
    let name = jwt_path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    fs::remove_file(&jwt_path).map_err(|e| format!("failed to remove token: {e}"))?;
    println!("Removed token: {name}");

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// List .jwt files in the tokens directory. Returns (name_without_ext, path).
fn list_jwt_files(tokens_dir: &Path) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    if !tokens_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    let entries =
        fs::read_dir(tokens_dir).map_err(|e| format!("failed to read tokens directory: {e}"))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jwt")
            && let Some(name) = path.file_stem().and_then(|n| n.to_str())
        {
            files.push((name.to_string(), path));
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Resolve a token identifier to a file path. The id can be:
/// - A name (filename without .jwt extension)
/// - A numeric index
fn resolve_token_path(
    tokens_dir: &std::path::Path,
    id: &str,
) -> Result<std::path::PathBuf, String> {
    let entries = list_jwt_files(tokens_dir)?;

    if entries.is_empty() {
        return Err("No saved tokens.".to_string());
    }

    // Try numeric index.
    if let Ok(idx) = id.parse::<usize>() {
        return entries
            .get(idx)
            .map(|(_, p)| p.clone())
            .ok_or_else(|| format!("Index {idx} out of range (0..{})", entries.len()));
    }

    // Try name match.
    let matches: Vec<_> = entries
        .iter()
        .filter(|(name, _)| name.starts_with(id))
        .collect();

    match matches.len() {
        0 => Err(format!("No token matching \"{id}\"")),
        1 => Ok(matches[0].1.clone()),
        n => Err(format!(
            "Ambiguous identifier \"{id}\" — matches {n} tokens. Be more specific."
        )),
    }
}

/// Read a token file and produce a one-line summary.
fn read_token_summary(path: &Path) -> String {
    let jwt_str = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return "error reading file".to_string(),
    };

    let claims: BitrouterClaims = match token::decode_unverified(jwt_str.trim()) {
        Ok(c) => c,
        Err(_) => return "invalid token".to_string(),
    };

    let scope = match claims.scope {
        bitrouter_core::jwt::claims::TokenScope::Admin => "admin",
        bitrouter_core::jwt::claims::TokenScope::Api => "api",
    };

    let exp_info = match claims.exp {
        Some(ts) => format!("exp={ts}"),
        None => "no-exp".to_string(),
    };

    let models_info = match claims.models {
        Some(ref m) if !m.is_empty() => format!("models={}", m.join(",")),
        _ => "all-models".to_string(),
    };

    format!(
        "scope={scope}  chain={}  {exp_info}  {models_info}",
        claims.chain
    )
}
