//! Apply an [`AppliedPreset`] onto a Google GenerateContent request.
//!
//! Google nests generation params inside `generation_config`. The system
//! prompt is `system_instruction: Option<GoogleContent>` at the top level.
//! Shallow merge: preset fills any field the request leaves unset.

use crate::routers::routing_table::AppliedPreset;

use super::types::{
    GenerateContentRequest, GoogleContent, GoogleGenerationConfig, GooglePart, GoogleThinkingConfig,
};

/// Shallow-merges `preset` defaults onto `request`.
pub fn apply(request: &mut GenerateContentRequest, preset: &AppliedPreset) {
    if preset.is_empty() {
        return;
    }

    // Generation params live under `generation_config`. Create the nested
    // struct only if at least one preset param is set.
    let needs_gen_config = preset.temperature.is_some()
        || preset.top_p.is_some()
        || preset.top_k.is_some()
        || preset.max_tokens.is_some()
        || preset.stop_sequences.is_some()
        || preset.presence_penalty.is_some()
        || preset.frequency_penalty.is_some()
        || preset.reasoning_effort.is_some();
    if needs_gen_config {
        let cfg = request
            .generation_config
            .get_or_insert_with(GoogleGenerationConfig::default);
        if cfg.temperature.is_none() {
            cfg.temperature = preset.temperature;
        }
        if cfg.top_p.is_none() {
            cfg.top_p = preset.top_p;
        }
        if cfg.top_k.is_none() {
            cfg.top_k = preset.top_k;
        }
        if cfg.max_output_tokens.is_none() {
            cfg.max_output_tokens = preset.max_tokens;
        }
        if cfg.stop_sequences.is_none() {
            cfg.stop_sequences = preset.stop_sequences.clone();
        }
        if cfg.presence_penalty.is_none() {
            cfg.presence_penalty = preset.presence_penalty;
        }
        if cfg.frequency_penalty.is_none() {
            cfg.frequency_penalty = preset.frequency_penalty;
        }
        // Inject thinking only when the request omits the entire object.
        // A request that supplied `thinkingConfig: {}` (no fields) is
        // treated as explicit opt-out of preset thinking, matching the
        // "request wins" semantics other fields use.
        if cfg.thinking_config.is_none()
            && let Some(effort) = preset.reasoning_effort
        {
            cfg.thinking_config = Some(GoogleThinkingConfig {
                thinking_budget: Some(effort.google_thinking_budget()),
                thinking_level: None,
                include_thoughts: None,
            });
        }
    }

    // System: only when request has no system_instruction.
    if request.system_instruction.is_none()
        && let Some(system) = &preset.system
    {
        request.system_instruction = Some(GoogleContent {
            role: None,
            parts: Some(vec![GooglePart {
                text: Some(system.clone()),
                inline_data: None,
                function_call: None,
                function_response: None,
                thought: None,
            }]),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::language::call_options::ReasoningEffort;

    fn empty_request() -> GenerateContentRequest {
        GenerateContentRequest {
            model: "gemini-2.5-flash".into(),
            contents: Vec::new(),
            system_instruction: None,
            generation_config: None,
            stream: None,
            tools: None,
            tool_config: None,
        }
    }

    #[test]
    fn preset_fills_unset_temperature() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            temperature: Some(0.2),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let cfg = req.generation_config.expect("generation_config created");
        assert_eq!(cfg.temperature, Some(0.2));
    }

    #[test]
    fn request_generation_config_temperature_wins() {
        let mut req = empty_request();
        req.generation_config = Some(GoogleGenerationConfig {
            temperature: Some(0.9),
            ..Default::default()
        });
        let preset = AppliedPreset {
            temperature: Some(0.2),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let cfg = req.generation_config.unwrap();
        assert_eq!(cfg.temperature, Some(0.9));
    }

    #[test]
    fn preset_reasoning_effort_writes_thinking_budget() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let cfg = req.generation_config.expect("generation_config");
        let thinking = cfg.thinking_config.expect("thinking_config");
        assert_eq!(thinking.thinking_budget, Some(16384));
    }

    #[test]
    fn request_thinking_config_wins() {
        let mut req = empty_request();
        req.generation_config = Some(GoogleGenerationConfig {
            thinking_config: Some(GoogleThinkingConfig {
                thinking_budget: Some(0),
                thinking_level: None,
                include_thoughts: None,
            }),
            ..Default::default()
        });
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let cfg = req.generation_config.unwrap();
        let thinking = cfg.thinking_config.unwrap();
        assert_eq!(thinking.thinking_budget, Some(0));
    }

    #[test]
    fn preset_system_instruction_added_when_absent() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        let sys = req.system_instruction.expect("system_instruction added");
        let parts = sys.parts.expect("parts");
        assert_eq!(parts[0].text.as_deref(), Some("Reason carefully."));
    }
}
