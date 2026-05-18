//! `bitrouter models` subcommand — list routable models from config.

use std::io::{self, Write};

use bitrouter_config::{BitrouterConfig, RoutingStrategy};
use serde::Serialize;

use super::OutputFormat;

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub name: String,
    pub providers: Vec<String>,
    pub strategy: String,
}

#[derive(Debug, Serialize)]
pub struct ModelListData {
    pub models: Vec<ModelEntry>,
}

pub fn query_list(config: &BitrouterConfig) -> ModelListData {
    if config.models.is_empty() {
        let mut models = Vec::new();
        for (provider_name, provider) in &config.providers {
            if let Some(model_map) = &provider.models {
                for model_id in model_map.keys() {
                    models.push(ModelEntry {
                        name: model_id.clone(),
                        providers: vec![provider_name.clone()],
                        strategy: "priority".to_owned(),
                    });
                }
            }
        }
        models.sort_by(|a, b| a.name.cmp(&b.name));
        return ModelListData { models };
    }

    let mut entries: Vec<_> = config.models.iter().collect();
    entries.sort_by_key(|(name, _)| (*name).clone());

    let models = entries
        .into_iter()
        .map(|(model_name, model_config)| {
            let providers: Vec<String> = model_config
                .endpoints
                .iter()
                .map(|ep| ep.provider.clone())
                .collect();
            let strategy = match model_config.strategy {
                RoutingStrategy::Priority => "priority",
                RoutingStrategy::LoadBalance => "load_balance",
            };
            ModelEntry {
                name: model_name.clone(),
                providers,
                strategy: strategy.to_owned(),
            }
        })
        .collect();

    ModelListData { models }
}

pub fn render_list_text(data: &ModelListData, w: &mut impl Write) -> io::Result<()> {
    if data.models.is_empty() {
        writeln!(w, "  (no models configured)")?;
        return Ok(());
    }
    for entry in &data.models {
        writeln!(
            w,
            "  {}  [{}]  {}",
            entry.name,
            entry.providers.join(","),
            entry.strategy
        )?;
    }
    Ok(())
}

/// Run the `models list` subcommand.
pub fn run_list(
    config: &BitrouterConfig,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = query_list(config);
    match output {
        OutputFormat::Text => render_list_text(&data, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &data)?,
    }
    Ok(())
}
