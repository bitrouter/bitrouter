//! `bitrouter account` subcommand — manage local Ed25519 keypairs.

use std::fs;
use std::path::{Path, PathBuf};

use bitrouter_core::auth::keys::{MasterKeyJson, MasterKeypair};

/// Run the `account` subcommand.
pub fn run(keys_dir: &Path, generate: bool, list: bool, set: Option<String>) -> Result<(), String> {
    if generate {
        generate_key(keys_dir)
    } else if list {
        list_keys(keys_dir)
    } else if let Some(ref id) = set {
        set_active(keys_dir, id)
    } else {
        // Default to listing when no flag is given.
        list_keys(keys_dir)
    }
}

/// Generate a new Ed25519 keypair and set it as the active account.
fn generate_key(keys_dir: &Path) -> Result<(), String> {
    let kp = MasterKeypair::generate();
    let prefix = kp.public_key_prefix();
    let sol_addr = kp.solana_pubkey_b58();
    let evm_addr = kp
        .evm_address_string()
        .map_err(|e| format!("failed to derive EVM address: {e}"))?;

    let key_dir = keys_dir.join(&prefix);
    fs::create_dir_all(&key_dir).map_err(|e| format!("failed to create key directory: {e}"))?;

    let json = kp.to_json();
    let json_str =
        serde_json::to_string_pretty(&json).map_err(|e| format!("failed to serialize key: {e}"))?;
    fs::write(key_dir.join("master.json"), json_str)
        .map_err(|e| format!("failed to write master.json: {e}"))?;

    // Create tokens directory for this account.
    fs::create_dir_all(key_dir.join("tokens"))
        .map_err(|e| format!("failed to create tokens directory: {e}"))?;

    // Set as active.
    write_active(keys_dir, &prefix)?;

    println!("Generated web3 master key");
    println!("  solana:  {sol_addr}");
    println!("  evm:     {evm_addr}");
    println!("  prefix:  {prefix}");
    println!("  path:    {}", key_dir.display());
    println!("  active:  yes");
    Ok(())
}

/// List all local account keypairs.
fn list_keys(keys_dir: &Path) -> Result<(), String> {
    let active = read_active(keys_dir);
    let entries = list_key_dirs(keys_dir)?;

    if entries.is_empty() {
        println!("No accounts found. Run `bitrouter account -g` to generate one.");
        return Ok(());
    }

    for (i, (prefix, dir)) in entries.iter().enumerate() {
        let marker = if active.as_deref() == Some(prefix.as_str()) {
            " *"
        } else {
            ""
        };

        println!("  [{i}] {prefix}{marker}");
        match load_addresses(dir) {
            Ok((sol, evm)) => {
                println!("       sol: {sol}");
                println!("       evm: {evm}");
            }
            Err(_) => println!("       addresses: ???"),
        }
    }

    Ok(())
}

/// Set the active account by index or pubkey prefix.
fn set_active(keys_dir: &Path, id: &str) -> Result<(), String> {
    let entries = list_key_dirs(keys_dir)?;

    if entries.is_empty() {
        return Err("No accounts found. Run `bitrouter account -g` first.".to_string());
    }

    // Try as numeric index first.
    if let Ok(idx) = id.parse::<usize>() {
        if let Some((prefix, _)) = entries.get(idx) {
            write_active(keys_dir, prefix)?;
            println!("Active account set to: {prefix}");
            return Ok(());
        }
        return Err(format!("Index {idx} out of range (0..{})", entries.len()));
    }

    // Try as pubkey prefix match.
    let matches: Vec<_> = entries
        .iter()
        .filter(|(prefix, _)| prefix.starts_with(id))
        .collect();

    match matches.len() {
        0 => Err(format!("No account matching prefix \"{id}\"")),
        1 => {
            let prefix = &matches[0].0;
            write_active(keys_dir, prefix)?;
            println!("Active account set to: {prefix}");
            Ok(())
        }
        n => Err(format!(
            "Ambiguous prefix \"{id}\" — matches {n} accounts. Use a longer prefix."
        )),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Path to the "active" marker file.
fn active_file(keys_dir: &Path) -> PathBuf {
    keys_dir.join("active")
}

/// Read the active account prefix, if any.
fn read_active(keys_dir: &Path) -> Option<String> {
    fs::read_to_string(active_file(keys_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the active account prefix.
fn write_active(keys_dir: &Path, prefix: &str) -> Result<(), String> {
    fs::create_dir_all(keys_dir).map_err(|e| format!("failed to create keys directory: {e}"))?;
    fs::write(active_file(keys_dir), prefix)
        .map_err(|e| format!("failed to write active file: {e}"))
}

/// List key directories sorted alphabetically. Returns (prefix, path) pairs.
fn list_key_dirs(keys_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !keys_dir.exists() {
        return Ok(Vec::new());
    }

    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let entries =
        fs::read_dir(keys_dir).map_err(|e| format!("failed to read keys directory: {e}"))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if path.is_dir()
            && path.join("master.json").exists()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            dirs.push((name.to_string(), path));
        }
    }

    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(dirs)
}

/// Load CAIP-10 wallet addresses from a key directory's master.json.
///
/// Returns `(solana_caip10, evm_caip10)` strings for display.
fn load_addresses(key_dir: &Path) -> Result<(String, String), String> {
    use bitrouter_core::auth::chain::Chain;

    let data = fs::read_to_string(key_dir.join("master.json"))
        .map_err(|e| format!("failed to read master.json: {e}"))?;
    let json: MasterKeyJson =
        serde_json::from_str(&data).map_err(|e| format!("invalid master.json: {e}"))?;
    let kp = MasterKeypair::from_json(&json)
        .map_err(|e| format!("invalid keypair in master.json: {e}"))?;
    let sol = kp
        .caip10(&Chain::solana_mainnet())
        .map_err(|e| format!("solana caip10: {e}"))?
        .format();
    let evm = kp
        .caip10(&Chain::base())
        .map_err(|e| format!("evm caip10: {e}"))?
        .format();
    Ok((sol, evm))
}

/// Load the active MasterKeypair from the keys directory.
/// Returns `(prefix, keypair)` or an error.
pub fn load_active_keypair(keys_dir: &Path) -> Result<(String, MasterKeypair), String> {
    let prefix = read_active(keys_dir).ok_or_else(|| {
        "No active account. Run `bitrouter account -g` to generate one.".to_string()
    })?;

    let key_dir = keys_dir.join(&prefix);
    let data = fs::read_to_string(key_dir.join("master.json"))
        .map_err(|e| format!("failed to read master.json for active account: {e}"))?;
    let json: MasterKeyJson =
        serde_json::from_str(&data).map_err(|e| format!("invalid master.json: {e}"))?;
    let kp = MasterKeypair::from_json(&json)
        .map_err(|e| format!("invalid keypair in master.json: {e}"))?;

    Ok((prefix, kp))
}
