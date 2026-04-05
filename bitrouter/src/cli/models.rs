//! `bitrouter models` subcommand — list routable models from config.

use bitrouter_config::{BitrouterConfig, RoutingStrategy};

/// Run the `models list` subcommand — prints all configured models.
pub fn run_list(config: &BitrouterConfig) -> Result<(), Box<dyn std::error::Error>> {
    if config.models.is_empty() {
        // Fallback: list per-provider model catalogs (same as ConfigRoutingTable).
        let mut any = false;
        for (provider_name, provider) in &config.providers {
            if let Some(models) = &provider.models {
                for model_id in models.keys() {
                    println!("  {model_id}  [{provider_name}]");
                    any = true;
                }
            }
        }
        if !any {
            println!("  (no models configured)");
        }
        return Ok(());
    }

    let mut entries: Vec<_> = config.models.iter().collect();
    entries.sort_by_key(|(name, _)| (*name).clone());

    for (model_name, model_config) in entries {
        let providers: Vec<&str> = model_config
            .endpoints
            .iter()
            .map(|ep| ep.provider.as_str())
            .collect();
        let strategy = match model_config.strategy {
            RoutingStrategy::Priority => "priority",
            RoutingStrategy::LoadBalance => "load_balance",
        };
        println!("  {model_name}  [{}]  {strategy}", providers.join(","));
    }

    Ok(())
}
