//! Reports for `models` and `providers list`.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};

/// One routable model and the providers that can serve it.
#[derive(Serialize)]
pub struct ModelRow {
    pub id: String,
    pub providers: Vec<String>,
}

/// Result of `bitrouter models`.
#[derive(Serialize)]
pub struct ModelsReport {
    pub models: Vec<ModelRow>,
}

impl CliReport for ModelsReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.models.is_empty() {
            return h.line("(no routable models)");
        }
        for m in &self.models {
            h.line(&format!("{}\t{}", m.id, m.providers.join(", ")))?;
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

    #[test]
    fn models_empty_is_empty_array() {
        let r = ModelsReport { models: vec![] };
        let v: serde_json::Value =
            serde_json::from_slice(&Output::new(Format::Json).render_to_vec(&r)).unwrap();
        assert_eq!(v, serde_json::json!({"models": []}));
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
