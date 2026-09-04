//! Reports for `models` and `providers list`.

use bitrouter_mcp::actions::models::{ModelsReport, ModelsSource};
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};

/// The human view of `bitrouter models`.
///
/// The report type itself is
/// [`bitrouter_mcp::actions::models::ModelsReport`](ModelsReport): the
/// `list_models` tool returns the same type, so `bitrouter models --json` and
/// the tool's structured content are the same bytes. Rendering stays here — a
/// local trait on a foreign type is legal, and it keeps [`Human`] out of the
/// crate.
impl CliReport for ModelsReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.models.is_empty() {
            return h.line("(no routable models)");
        }
        for m in &self.models {
            h.line(&format!("{}\t{}", m.id, m.providers.join(", ")))?;
        }
        // Which view answered, stated only where it is a caveat: a live
        // catalog needs no annotation, a projected one does — a provider whose
        // credential only resolves at daemon start-up is missing from it.
        if self.resolved_via == ModelsSource::Config {
            return h.note(
                "Listed from config — no daemon answered. \
                 Run `bitrouter start` for the live catalog.",
            );
        }
        Ok(())
    }
}

/// One configured provider.
#[derive(Serialize)]
pub struct ProviderRow {
    pub id: String,
    pub models: usize,
    pub active: bool,
    pub api_base: String,
}

/// Result of `bitrouter providers list`.
#[derive(Serialize)]
pub struct ProvidersReport {
    pub providers: Vec<ProviderRow>,
}

impl CliReport for ProvidersReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.providers.is_empty() {
            return h.line("(no providers configured)");
        }
        let mut t = Table::new(["ID", "MODELS", "ACTIVE", "API_BASE"]);
        for p in &self.providers {
            t.push([
                p.id.clone(),
                p.models.to_string(),
                if p.active { "yes".into() } else { "no".into() },
                p.api_base.clone(),
            ]);
        }
        h.table(&t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    fn model(id: &str, providers: &[&str]) -> bitrouter_sdk::language_model::routing::ModelInfo {
        bitrouter_sdk::language_model::routing::ModelInfo {
            id: id.into(),
            providers: providers.iter().map(|p| (*p).into()).collect(),
        }
    }

    #[test]
    fn models_empty_is_empty_array() {
        let r = ModelsReport::from_config(vec![]);
        let v: serde_json::Value =
            serde_json::from_slice(&Output::new(Format::Json).render_to_vec(&r)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"models": [], "resolved_via": "config"})
        );
    }

    /// `--json` carries the whole fallback chain, which is the shape the MCP
    /// tool advertises as its `output_schema`.
    #[test]
    fn models_json_carries_every_provider() {
        let r = ModelsReport::live(vec![model("gpt-5", &["openai", "azure"])]);
        let v: serde_json::Value =
            serde_json::from_slice(&Output::new(Format::Json).render_to_vec(&r)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "models": [{ "id": "gpt-5", "providers": ["openai", "azure"] }],
                "resolved_via": "live"
            })
        );
    }

    /// A live catalog reads plainly; a projected one says so, because the two
    /// are not the same answer.
    #[test]
    fn a_config_listing_is_annotated_and_a_live_one_is_not() {
        let live = String::from_utf8(
            Output::new(Format::Human)
                .render_to_vec(&ModelsReport::live(vec![model("gpt-5", &["openai"])])),
        )
        .unwrap();
        assert!(live.contains("gpt-5\topenai"), "{live:?}");
        assert!(!live.contains("no daemon answered"), "{live:?}");

        let from_config = String::from_utf8(Output::new(Format::Human).render_to_vec(
            &ModelsReport::from_config(vec![model("gpt-5", &["openai"])]),
        ))
        .unwrap();
        assert!(
            from_config.contains("no daemon answered"),
            "{from_config:?}"
        );
    }

    #[test]
    fn providers_table_human() {
        let r = ProvidersReport {
            providers: vec![ProviderRow {
                id: "openai".into(),
                models: 42,
                active: true,
                api_base: "https://api.openai.com".into(),
            }],
        };
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.starts_with("ID"), "{h:?}");
        assert!(h.contains("openai"), "{h:?}");
        assert!(h.contains("yes"), "{h:?}");
    }
}
