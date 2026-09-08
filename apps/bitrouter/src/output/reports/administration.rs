//! Human renderers for safe live-administration reports.

use crate::actions::administration::{
    AgentsReport, ObserveReport, PolicyReport, PolicyView, ProvidersReport,
};
use crate::output::CliReport;
use crate::output::human::{Health, Human, Table};

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn policy_view(view: PolicyView) -> &'static str {
    match view {
        PolicyView::Active => "active",
        PolicyView::Disk => "disk",
    }
}

impl CliReport for ProvidersReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        if self.providers.is_empty() {
            human.line("(no accepted providers)")?;
        } else {
            let mut table = Table::new(["ID", "MODELS", "ACTIVE"]);
            for provider in &self.providers {
                table.push([
                    provider.id.clone(),
                    provider.models.to_string(),
                    yes_no(provider.active).to_string(),
                ]);
            }
            human.table(&table)?;
        }
        human.note(&format!(
            "resolved via {} daemon configuration",
            self.resolved_via
        ))
    }
}

impl CliReport for AgentsReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        if self.agents.is_empty() {
            human.line("(no configured or catalog agents)")?;
        } else {
            let mut table = Table::new(["ID", "CONFIGURED", "CATALOG", "DESCRIPTION"]);
            for agent in &self.agents {
                table.push([
                    agent.id.clone(),
                    yes_no(agent.configured).to_string(),
                    yes_no(agent.in_catalog).to_string(),
                    agent.description.clone(),
                ]);
            }
            human.table(&table)?;
        }
        human.note(if self.readiness_checked {
            "readiness was checked"
        } else {
            "catalog only; this read did not launch or check an agent"
        })
    }
}

impl CliReport for ObserveReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        if !self.daemon_reachable {
            human.status_block(Health::Down, "bitrouter observe — daemon stopped")?;
            human.field("compiled", yes_no(self.compiled_in))?;
            return human.note("Run `bitrouter start` to inspect the live telemetry exporter.");
        }
        let (health, headline) = if self.exporter_wired {
            (Health::Up, "OTel exporter is wired")
        } else if self.compiled_in {
            (
                Health::Down,
                "OTel feature compiled in, exporter not configured",
            )
        } else {
            (Health::Down, "OTel feature not compiled in")
        };
        human.status_block(health, &format!("bitrouter observe — {headline}"))?;
        human.field("compiled", yes_no(self.compiled_in))?;
        human.field("wired", yes_no(self.exporter_wired))?;
        if let Some(sampler) = &self.sampler {
            let sampler = match self.sampler_arg {
                Some(argument) => format!("{sampler} (arg={argument})"),
                None => sampler.clone(),
            };
            human.field("sampler", sampler)?;
        }
        human.field("metrics", if self.metrics_enabled { "on" } else { "off" })?;
        human.field("headers", self.header_count)?;
        human.field("res-attrs", self.resource_attribute_count)?;
        human.field(
            "api-keys",
            format!("{} / {}", self.api_key_count, self.api_key_cap),
        )?;
        human.field(
            "users",
            format!("{} / {}", self.user_id_count, self.user_id_cap),
        )?;
        human.field("in-flight", self.active_spans)
    }
}

impl CliReport for PolicyReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!("policy {}", policy_view(self.view)))?;
        human.field("availability", &self.availability)?;
        if let Some(digest) = &self.digest {
            human.field("digest", digest)?;
        }
        human.field("mode", &self.mode)?;
        if !self.policies.is_empty() {
            human.field("policies", self.policies.join(", "))?;
        }
        for (preset, policy) in &self.bindings {
            human.line(&format!("  @{preset} -> {policy}"))?;
        }
        if !self.definitions.is_empty() {
            human.blank()?;
            let mut table = Table::new(["POLICY", "TIERS", "ROUTES", "CERTIFICATES"]);
            for (name, detail) in &self.definitions {
                table.push([
                    name.clone(),
                    detail.tiers.keys().cloned().collect::<Vec<_>>().join(", "),
                    detail.routes.len().to_string(),
                    detail.certificates.len().to_string(),
                ]);
            }
            human.table(&table)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::administration::ProviderEntry;
    use crate::output::{Format, Output};

    #[test]
    fn provider_report_does_not_render_an_api_base() -> anyhow::Result<()> {
        let report = ProvidersReport {
            resolved_via: "live".to_string(),
            providers: vec![ProviderEntry {
                id: "openai".to_string(),
                models: 2,
                active: true,
            }],
        };
        let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&report))?;
        assert!(rendered.contains("openai"));
        assert!(!rendered.contains("API_BASE"));
        Ok(())
    }
}
