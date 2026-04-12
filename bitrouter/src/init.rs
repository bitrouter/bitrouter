use std::collections::HashMap;

use crate::runtime::RuntimePaths;
use bitrouter_config::{
    CustomProviderInit, InitOptions, ToolProviderInit, builtin_provider_defs,
    detect_providers_from_env,
};
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};

#[cfg(feature = "tui")]
use bitrouter_config::builtin_agent_defs;
#[cfg(feature = "tui")]
use bitrouter_providers::acp::discovery::discover_agents;
#[cfg(feature = "tui")]
use bitrouter_providers::acp::types::AgentAvailability;

/// Outcome of the init wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    /// Config was written successfully.
    Configured,
    /// User cancelled or selected no providers.
    Cancelled,
}

/// Display name and config key for each builtin provider.
const PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("google", "Google"),
];

pub fn run_init(paths: &RuntimePaths) -> Result<InitOutcome, Box<dyn std::error::Error>> {
    let theme = ColorfulTheme::default();

    // Check if running in a terminal
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("error: `bitrouter init` requires an interactive terminal.");
        eprintln!("Edit ~/.bitrouter/bitrouter.yaml and ~/.bitrouter/.env manually.");
        std::process::exit(1);
    }

    println!();
    println!("  BitRouter Setup");
    println!("  ───────────────");
    println!();

    // Check existing config
    let config_exists = paths.config_file.exists()
        && std::fs::read_to_string(&paths.config_file)
            .map(|s| !s.trim_start().starts_with('#'))
            .unwrap_or(false);

    let overwrite = if config_exists {
        Confirm::with_theme(&theme)
            .with_prompt("Existing configuration found. Overwrite?")
            .default(false)
            .interact()?
    } else {
        true
    };

    if config_exists && !overwrite {
        println!("Setup cancelled. Existing configuration preserved.");
        return Ok(InitOutcome::Cancelled);
    }

    // ── Step 0: Wallet (non-skippable) ─────────────────────────────
    println!("  Step 1 · Wallet");
    println!("  Your wallet is your BitRouter identity — used for");
    println!("  authentication, spend tracking, and API key management.");
    println!();

    let wallet_name = create_or_reuse_wallet(&theme)?;

    // ── Step 1: Models ─────────────────────────────────────────────
    println!();
    println!("  Step 2 · Models");
    println!();

    let model_choice = Select::with_theme(&theme)
        .with_prompt("How do you want to connect to LLMs?")
        .items([
            "BitRouter Cloud (no API keys needed, billed through wallet)",
            "Bring Your Own Keys (OpenAI, Anthropic, Google, custom)",
        ])
        .default(0)
        .interact()?;

    let use_default_models = model_choice == 0;
    let mut selected_providers: Vec<&str> = Vec::new();
    let mut custom_providers: Vec<CustomProviderInit> = Vec::new();
    let mut api_keys: HashMap<String, String> = HashMap::new();

    if !use_default_models {
        let detected = detect_providers_from_env();
        let detected_names: Vec<&str> = detected.iter().map(|d| d.name.as_str()).collect();

        if !detected.is_empty() {
            println!();
            println!("  Detected API keys in environment:");
            for d in &detected {
                println!("    ✓ {}", provider_display_name(&d.name),);
            }
            println!();
        }

        // Builtin provider selection
        for &(key, display) in PROVIDERS {
            let is_detected = detected_names.contains(&key);
            let enable = Confirm::with_theme(&theme)
                .with_prompt(format!("Configure {display}?"))
                .default(is_detected)
                .interact()?;
            if enable {
                selected_providers.push(key);
            }
        }

        // Custom provider setup
        println!();
        println!("  Custom providers (OpenAI-compatible or Anthropic-compatible)");
        println!();

        loop {
            let add_custom = Confirm::with_theme(&theme)
                .with_prompt("Add a custom provider?")
                .default(false)
                .interact()?;

            if !add_custom {
                break;
            }

            if let Some(cp) = prompt_custom_provider(&theme)? {
                custom_providers.push(cp);
            }
            println!();
        }

        if selected_providers.is_empty() && custom_providers.is_empty() {
            println!();
            println!("No providers selected. Run `bitrouter` again anytime.");
            return Ok(InitOutcome::Cancelled);
        }

        // Collect API keys
        let defs = builtin_provider_defs();

        if !selected_providers.is_empty() {
            println!();
        }

        for &name in &selected_providers {
            let fallback = name.to_uppercase();
            let prefix = defs
                .get(name)
                .and_then(|bp| bp.config.env_prefix.as_deref())
                .unwrap_or(&fallback);
            let key_var = format!("{prefix}_API_KEY");

            let env_key = std::env::var(&key_var).ok().filter(|v| !v.is_empty());

            let key = if let Some(existing) = &env_key {
                let masked = mask_key(existing);
                let use_existing = Confirm::with_theme(&theme)
                    .with_prompt(format!(
                        "{} API key detected ({masked}). Use this?",
                        provider_display_name(name)
                    ))
                    .default(true)
                    .interact()?;

                if use_existing {
                    existing.clone()
                } else {
                    prompt_api_key(&theme, name)?
                }
            } else {
                prompt_api_key(&theme, name)?
            };

            api_keys.insert(name.to_owned(), key);
        }

        // Collect API keys for custom providers
        for cp in &custom_providers {
            let env_key = std::env::var(&cp.env_key_var)
                .ok()
                .filter(|v| !v.is_empty());

            let key = if let Some(existing) = &env_key {
                let masked = mask_key(existing);
                let use_existing = Confirm::with_theme(&theme)
                    .with_prompt(format!(
                        "{} API key detected ({masked}). Use this?",
                        cp.name
                    ))
                    .default(true)
                    .interact()?;

                if use_existing {
                    existing.clone()
                } else {
                    prompt_api_key(&theme, &cp.name)?
                }
            } else {
                prompt_api_key(&theme, &cp.name)?
            };

            api_keys.insert(cp.name.clone(), key);
        }
    }

    // ── Step 2: Tools (skippable) ──────────────────────────────────
    println!();
    println!("  Step 3 · Tools");
    println!();

    let configure_tools = Confirm::with_theme(&theme)
        .with_prompt("Configure tool providers? (skip to use built-in defaults)")
        .default(false)
        .interact()?;

    let mut use_default_tools = false;
    let mut tool_providers: Vec<ToolProviderInit> = Vec::new();

    if configure_tools {
        let tool_choice = Select::with_theme(&theme)
            .with_prompt("How do you want to connect to tools?")
            .items([
                "BitRouter Cloud (wallet-authenticated, coming soon)",
                "Add custom MCP servers",
            ])
            .default(0)
            .interact()?;

        if tool_choice == 0 {
            use_default_tools = true;
            println!("  BitRouter Cloud tools will be available when services launch.");
        } else {
            loop {
                let add_mcp = Confirm::with_theme(&theme)
                    .with_prompt("Add an MCP server?")
                    .default(tool_providers.is_empty())
                    .interact()?;

                if !add_mcp {
                    break;
                }

                if let Some(tp) = prompt_tool_provider(&theme)? {
                    println!("  → {} ({})", tp.name, tp.url);
                    tool_providers.push(tp);
                }
                println!();
            }
        }
    }

    // ── Listen address ─────────────────────────────────────────────
    println!();
    let listen_str: String = Input::with_theme(&theme)
        .with_prompt("Listen address")
        .default("127.0.0.1:8787".into())
        .interact_text()?;
    let listen_addr = listen_str.parse().ok();

    // ── Step 3: Agents (requires tui feature) ─────────────────────
    #[cfg(feature = "tui")]
    let (discovered_agent_names, agent_routing_configured) =
        run_agent_step(&theme, &listen_str, &api_keys)?;
    #[cfg(not(feature = "tui"))]
    let (discovered_agent_names, agent_routing_configured): (Vec<String>, bool) =
        (Vec::new(), false);

    // ── Summary ────────────────────────────────────────────────────
    let model_summary = if use_default_models {
        "BitRouter Cloud".to_owned()
    } else {
        let mut names: Vec<String> = selected_providers
            .iter()
            .map(|n| provider_display_name(n).to_owned())
            .chain(
                custom_providers
                    .iter()
                    .map(|cp| format!("{} (derives: {})", cp.name, cp.derives)),
            )
            .collect();
        if names.is_empty() {
            "(none)".to_owned()
        } else {
            names.sort();
            names.join(", ")
        }
    };

    let tool_summary = if use_default_tools {
        "BitRouter Cloud + built-in defaults".to_owned()
    } else if tool_providers.is_empty() {
        "built-in defaults".to_owned()
    } else {
        let names: Vec<&str> = tool_providers.iter().map(|tp| tp.name.as_str()).collect();
        format!("built-in defaults + {}", names.join(", "))
    };

    let agent_summary = if discovered_agent_names.is_empty() {
        "none detected".to_owned()
    } else if agent_routing_configured {
        format!(
            "{} (routing via env vars)",
            discovered_agent_names.join(", ")
        )
    } else {
        format!(
            "{} detected, routing not configured",
            discovered_agent_names.len()
        )
    };

    println!();
    println!("  Configuration summary");
    println!("  ─────────────────────");
    println!("  Wallet:    {wallet_name}");
    println!("  Home:      {}", paths.home_dir.display());
    println!("  Listen:    {listen_str}");
    println!("  Models:    {model_summary}");
    println!("  Tools:     {tool_summary}");
    println!("  Agents:    {agent_summary}");
    println!("  Config:    {}", paths.config_file.display());
    println!("  Env file:  {}", paths.env_file.display());
    println!();

    let confirm = Confirm::with_theme(&theme)
        .with_prompt("Write configuration?")
        .default(true)
        .interact()?;

    if !confirm {
        println!("Setup cancelled.");
        return Ok(InitOutcome::Cancelled);
    }

    // Write config
    let options = InitOptions {
        providers: selected_providers.iter().map(|s| s.to_string()).collect(),
        api_keys,
        custom_providers,
        listen_addr,
        home_dir: paths.home_dir.clone(),
        wallet_name,
        use_default_models,
        tool_providers,
        use_default_tools,
    };

    let result = bitrouter_config::write_init_config(&options, overwrite)?;

    println!();
    println!("  ✓ Configuration written!");
    println!();
    println!(
        "  Models: {}",
        if use_default_models {
            "BitRouter Cloud".to_owned()
        } else {
            format!(
                "{} provider{} configured: {}",
                result.providers_configured.len(),
                if result.providers_configured.len() == 1 {
                    ""
                } else {
                    "s"
                },
                model_summary,
            )
        }
    );
    println!();

    Ok(InitOutcome::Configured)
}

/// Create a new wallet or reuse an existing one.
///
/// Returns the wallet name. Aborts the setup if wallet creation fails.
fn create_or_reuse_wallet(theme: &ColorfulTheme) -> Result<String, Box<dyn std::error::Error>> {
    // Check for existing wallets
    let has_existing = ows_lib::list_wallets(None)
        .map(|w| !w.is_empty())
        .unwrap_or(false);

    if has_existing {
        let wallets = ows_lib::list_wallets(None).unwrap_or_default();
        let first_name = wallets
            .first()
            .map(|w| w.name.clone())
            .unwrap_or_else(|| "default".to_owned());

        let use_existing = Confirm::with_theme(theme)
            .with_prompt(format!("Use existing wallet '{first_name}'?"))
            .default(true)
            .interact()?;

        if use_existing {
            println!("  ✓ Using wallet '{first_name}'");
            return Ok(first_name);
        }
    }

    let name: String = Input::with_theme(theme)
        .with_prompt("Wallet name")
        .default("default".into())
        .interact_text()?;

    match crate::cli::wallet::create(&name, None, false) {
        Ok(()) => {
            println!("  ✓ Wallet '{name}' created");
            Ok(name)
        }
        Err(e) => Err(format!(
            "Wallet creation failed: {e}\n\
             A wallet is required for BitRouter. Fix the issue and run setup again."
        )
        .into()),
    }
}

/// Prompt the user to define a custom provider.
fn prompt_custom_provider(
    theme: &ColorfulTheme,
) -> Result<Option<CustomProviderInit>, Box<dyn std::error::Error>> {
    let name: String = Input::with_theme(theme)
        .with_prompt("Provider name (e.g. openrouter, together, ollama)")
        .interact_text()?;

    let name = name.trim().to_lowercase().replace(' ', "-");
    if name.is_empty() {
        return Ok(None);
    }

    // Check for conflicts with builtins
    let defs = builtin_provider_defs();
    if defs.contains_key(&name) {
        eprintln!(
            "  '{name}' is a built-in provider. Configure it in the built-in section instead."
        );
        return Ok(None);
    }

    let derives: String = Input::with_theme(theme)
        .with_prompt("Compatible API protocol")
        .default("openai".into())
        .validate_with(|input: &String| -> Result<(), String> {
            match input.as_str() {
                "openai" | "anthropic" => Ok(()),
                _ => Err("Must be 'openai' or 'anthropic'".into()),
            }
        })
        .interact_text()?;

    let default_base = match derives.as_str() {
        "anthropic" => "https://api.example.com".to_owned(),
        _ => "https://api.example.com/v1".to_owned(),
    };

    let api_base: String = Input::with_theme(theme)
        .with_prompt("API base URL")
        .default(default_base)
        .interact_text()?;

    let env_prefix = name.to_uppercase().replace('-', "_");
    let env_key_var = format!("{env_prefix}_API_KEY");

    println!("  → {name} (derives: {derives}, base: {api_base}, env: {env_key_var})");

    Ok(Some(CustomProviderInit {
        name,
        derives,
        api_base,
        env_key_var,
    }))
}

/// Prompt the user to define a custom MCP tool provider.
fn prompt_tool_provider(
    theme: &ColorfulTheme,
) -> Result<Option<ToolProviderInit>, Box<dyn std::error::Error>> {
    let name: String = Input::with_theme(theme)
        .with_prompt("MCP server name (e.g. my-tools, internal-mcp)")
        .interact_text()?;

    let name = name.trim().to_lowercase().replace(' ', "-");
    if name.is_empty() {
        return Ok(None);
    }

    let url: String = Input::with_theme(theme)
        .with_prompt("MCP server URL")
        .interact_text()?;

    let has_auth = Confirm::with_theme(theme)
        .with_prompt("Does this server require authentication?")
        .default(false)
        .interact()?;

    let auth_header = if has_auth {
        let header: String = dialoguer::Password::with_theme(theme)
            .with_prompt("Authorization header value (e.g. Bearer sk-...)")
            .interact()?;
        if header.is_empty() {
            None
        } else {
            Some(header)
        }
    } else {
        None
    };

    Ok(Some(ToolProviderInit {
        name,
        url,
        auth_header,
    }))
}

fn prompt_api_key(
    theme: &ColorfulTheme,
    provider_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let key: String = dialoguer::Password::with_theme(theme)
        .with_prompt(format!("{} API key", provider_display_name(provider_name)))
        .interact()?;

    if key.is_empty() {
        eprintln!(
            "  Warning: empty API key for {}",
            provider_display_name(provider_name)
        );
    }
    Ok(key)
}

fn provider_display_name(key: &str) -> &str {
    PROVIDERS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, d)| *d)
        .unwrap_or(key)
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_owned();
    }
    let prefix = &key[..4];
    let suffix = &key[key.len() - 4..];
    format!("{prefix}...{suffix}")
}

/// Step 4: Discover ACP agents on PATH and optionally configure them
/// to route their LLM traffic through BitRouter.
///
/// Two mechanisms are used:
/// 1. **Config-file patches**: write/update each agent's native config to
///    point at BitRouter (per-agent, from the `routing.config_files` YAML).
/// 2. **Env vars at spawn time**: injected automatically by the TUI when
///    connecting to an agent (per-agent, from the `routing.env` YAML).
///
/// Returns `(discovered_names, routing_configured)`.
#[cfg(feature = "tui")]
fn run_agent_step(
    theme: &ColorfulTheme,
    listen_str: &str,
    api_keys: &HashMap<String, String>,
) -> Result<(Vec<String>, bool), Box<dyn std::error::Error>> {
    println!();
    println!("  Step 4 · Agents");
    println!("  Detect locally installed ACP-compatible coding agents");
    println!("  and optionally route their LLM traffic through BitRouter.");
    println!();

    let known = builtin_agent_defs();
    let discovered = discover_agents(&known);

    let on_path: Vec<&str> = discovered
        .iter()
        .filter(|a| matches!(a.availability, AgentAvailability::OnPath(_)))
        .map(|a| a.name.as_str())
        .collect();

    let installable: Vec<&str> = discovered
        .iter()
        .filter(|a| matches!(a.availability, AgentAvailability::Distributable))
        .map(|a| a.name.as_str())
        .collect();

    if on_path.is_empty() && installable.is_empty() {
        println!("  No ACP agents detected on PATH.");
        println!("  Install agents and run `bitrouter reset` to configure.");
        return Ok((Vec::new(), false));
    }

    if !on_path.is_empty() {
        println!("  Installed: {}", on_path.join(", "));
    }
    if !installable.is_empty() {
        println!("  Available: {} (auto-installable)", installable.join(", "));
    }
    println!();

    let all_names: Vec<String> = on_path
        .iter()
        .chain(installable.iter())
        .map(|s| s.to_string())
        .collect();

    let configure = Confirm::with_theme(theme)
        .with_prompt("Configure agents to route through BitRouter?")
        .default(true)
        .interact()?;

    if !configure {
        return Ok((all_names, false));
    }

    // Build routing context from listen address and provider API keys.
    // Map init-wizard key names (e.g. "openai") to standard env var names.
    let mut provider_keys = HashMap::new();
    for (name, key) in api_keys {
        let env_name = match name.as_str() {
            "openai" => "OPENAI_API_KEY",
            "anthropic" => "ANTHROPIC_API_KEY",
            "google" => "GOOGLE_API_KEY",
            _ => continue,
        };
        provider_keys.insert(env_name.to_owned(), key.clone());
    }

    let routing_ctx = bitrouter_config::RoutingContext::new(listen_str, &provider_keys);

    // Apply per-agent config file patches.
    let mut any_patched = false;
    for name in &all_names {
        let config = match known.get(name.as_str()) {
            Some(c) => c,
            None => continue,
        };
        let routing = match &config.routing {
            Some(r) if !r.config_files.is_empty() => r,
            _ => continue,
        };

        let results = routing_ctx.apply_config_patches(&routing.config_files);
        for (path, result) in &results {
            match result {
                Ok(()) => {
                    println!("  {name}: patched {}", path.display());
                    any_patched = true;
                }
                Err(e) => {
                    eprintln!("  {name}: failed to patch {}: {e}", path.display());
                }
            }
        }
    }

    if any_patched {
        println!();
    }

    // Also offer shell profile env vars as a broad fallback.
    let env_vars = routing_env_vars(listen_str);

    println!("  Env vars for agents without native config (injected at spawn):");
    for (var, val) in &env_vars {
        println!("    export {var}={val}");
    }
    println!();

    let write_profile = Confirm::with_theme(theme)
        .with_prompt("Also add these to your shell profile? (fallback for non-TUI usage)")
        .default(false)
        .interact()?;

    if write_profile {
        match append_to_shell_profile(&env_vars) {
            Ok(path) => println!("  ✓ Appended to {path}"),
            Err(e) => eprintln!("  Warning: could not write shell profile: {e}"),
        }
    }

    println!();
    println!("  Agent routing configured. The TUI will inject per-agent");
    println!("  env vars automatically when connecting to agents.");

    Ok((all_names, true))
}

/// Build the set of env vars that point agents at a BitRouter instance.
#[cfg(feature = "tui")]
fn routing_env_vars(listen_str: &str) -> Vec<(String, String)> {
    let base = format!("http://{listen_str}");
    vec![
        ("OPENAI_BASE_URL".to_owned(), format!("{base}/v1")),
        ("ANTHROPIC_BASE_URL".to_owned(), format!("{base}/v1")),
        ("GOOGLE_AI_BASE_URL".to_owned(), format!("{base}/v1beta")),
    ]
}

/// Detect the user's shell and append env var exports to the appropriate rc file.
///
/// Returns the path that was written to.
#[cfg(feature = "tui")]
fn append_to_shell_profile(
    vars: &[(String, String)],
) -> Result<String, Box<dyn std::error::Error>> {
    let home = dirs::home_dir().ok_or("could not determine home directory")?;

    let shell = std::env::var("SHELL").unwrap_or_default();
    let rc_file = if shell.ends_with("zsh") {
        home.join(".zshrc")
    } else if shell.ends_with("fish") {
        // fish uses a different export syntax — fall back to .bashrc-style
        // in a fish-compatible universal var file.
        home.join(".config/fish/conf.d/bitrouter.fish")
    } else {
        home.join(".bashrc")
    };

    let is_fish = shell.ends_with("fish");

    let mut snippet = String::from("\n# BitRouter agent routing\n");
    for (var, val) in vars {
        if is_fish {
            snippet.push_str(&format!("set -gx {var} {val}\n"));
        } else {
            snippet.push_str(&format!("export {var}={val}\n"));
        }
    }

    // Ensure parent directory exists (relevant for fish conf.d)
    if let Some(parent) = rc_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc_file)?;
    file.write_all(snippet.as_bytes())?;

    Ok(rc_file.display().to_string())
}
