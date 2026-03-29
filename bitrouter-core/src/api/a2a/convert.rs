//! Conversions between A2A wire types and canonical tool types.
//!
//! Mirrors the model-side pattern where each API protocol has a `convert.rs`
//! that translates provider-specific types into protocol-neutral core types.
//!
//! A2A skills are inherently unstructured — they describe capabilities in
//! natural language rather than typed schemas, so `input_schema` is always
//! `None` when converting from [`AgentSkill`].

use crate::tools::definition::ToolDefinition;

use super::types::AgentSkill;

// ── AgentSkill → ToolDefinition ──────────────────────────────────

impl From<AgentSkill> for ToolDefinition {
    fn from(skill: AgentSkill) -> Self {
        Self {
            name: skill.name,
            description: Some(skill.description),
            input_schema: None,
            annotations: None,
            input_modes: skill.input_modes,
            output_modes: skill.output_modes,
            examples: skill.examples,
            tags: skill.tags,
        }
    }
}

// ── ToolDefinition → AgentSkill ──────────────────────────────────

impl From<ToolDefinition> for AgentSkill {
    fn from(def: ToolDefinition) -> Self {
        Self {
            id: def.name.clone(),
            name: def.name,
            description: def.description.unwrap_or_default(),
            tags: def.tags,
            examples: def.examples,
            input_modes: def.input_modes,
            output_modes: def.output_modes,
            security: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_skill_converts_to_definition() {
        let skill = AgentSkill {
            id: "summarize".into(),
            name: "Summarize".into(),
            description: "Summarize text content".into(),
            tags: vec!["nlp".into(), "text".into()],
            examples: vec!["Summarize this article".into()],
            input_modes: vec!["text/plain".into()],
            output_modes: vec!["text/plain".into()],
            security: Vec::new(),
        };

        let def = ToolDefinition::from(skill);
        assert_eq!(def.name, "Summarize");
        assert_eq!(def.description.as_deref(), Some("Summarize text content"));
        assert!(def.input_schema.is_none());
        assert_eq!(def.tags, vec!["nlp", "text"]);
        assert_eq!(def.examples, vec!["Summarize this article"]);
        assert_eq!(def.input_modes, vec!["text/plain"]);
    }

    #[test]
    fn definition_round_trips_through_agent_skill() {
        let def = ToolDefinition {
            name: "translate".into(),
            description: Some("Translate text".into()),
            input_schema: None,
            annotations: None,
            input_modes: vec!["text/plain".into()],
            output_modes: vec!["text/plain".into(), "application/json".into()],
            examples: vec!["Translate to French".into()],
            tags: vec!["i18n".into()],
        };

        let skill = AgentSkill::from(def);
        assert_eq!(skill.id, "translate");
        assert_eq!(skill.name, "translate");
        assert_eq!(skill.description, "Translate text");
        assert_eq!(skill.tags, vec!["i18n"]);
    }
}
