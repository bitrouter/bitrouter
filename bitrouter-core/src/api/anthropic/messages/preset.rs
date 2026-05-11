//! Apply an [`AppliedPreset`] onto an Anthropic Messages request.
//!
//! Anthropic carries the system prompt as a top-level field
//! (`Option<SystemPrompt>`). Shallow merge: preset's system is used only
//! when the request leaves the field unset; numeric fields follow the
//! same "preset fills None" rule. `max_tokens` is required on the request
//! struct (not Option) so the preset cannot override it.

use crate::routers::routing_table::AppliedPreset;

use super::types::{AnthropicThinking, MessagesRequest, SystemPrompt};

/// Shallow-merges `preset` defaults onto `request`.
pub fn apply(request: &mut MessagesRequest, preset: &AppliedPreset) {
    if preset.is_empty() {
        return;
    }

    if request.temperature.is_none() {
        request.temperature = preset.temperature;
    }
    if request.top_p.is_none() {
        request.top_p = preset.top_p;
    }
    if request.top_k.is_none() {
        request.top_k = preset.top_k;
    }
    if request.stop_sequences.is_none() {
        request.stop_sequences = preset.stop_sequences.clone();
    }
    // Anthropic thinking: only inject when the request omits it. Budget is
    // clamped here against `max_tokens` to satisfy Anthropic's constraint
    // (`budget_tokens < max_tokens`); a 256-token margin leaves room for the
    // visible response.
    if request.thinking.is_none()
        && let Some(effort) = preset.reasoning_effort
    {
        request.thinking = Some(effort_to_thinking(effort, request.max_tokens));
    }

    // System prompt: only set when request has none.
    if request.system.is_none()
        && let Some(system) = &preset.system
    {
        request.system = Some(SystemPrompt::Text(system.clone()));
    }
}

/// Maps a normalized effort onto Anthropic's `thinking` configuration.
///
/// Returns `Disabled` for [`ReasoningEffort::Minimal`]; otherwise enables
/// thinking with the per-effort budget, clamped so `budget_tokens` stays
/// strictly below `max_tokens` (Anthropic constraint — thinking and visible
/// output share the same output pool) with a 256-token margin.
pub fn effort_to_thinking(
    effort: crate::models::language::call_options::ReasoningEffort,
    max_tokens: u32,
) -> AnthropicThinking {
    let Some(budget) = effort.anthropic_budget_tokens() else {
        return AnthropicThinking::Disabled;
    };
    let cap = max_tokens.saturating_sub(256).max(1024);
    AnthropicThinking::Enabled {
        budget_tokens: budget.min(cap),
        display: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::anthropic::messages::types::AnthropicMessage;
    use crate::models::language::call_options::ReasoningEffort;

    fn empty_request() -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: None,
            }],
            max_tokens: 8192,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            metadata: None,
            thinking: None,
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
        assert_eq!(req.temperature, Some(0.2));
    }

    #[test]
    fn request_temperature_wins() {
        let mut req = empty_request();
        req.temperature = Some(0.9);
        let preset = AppliedPreset {
            temperature: Some(0.2),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert_eq!(req.temperature, Some(0.9));
    }

    #[test]
    fn preset_system_used_when_request_has_none() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        match req.system {
            Some(SystemPrompt::Text(t)) => assert_eq!(t, "Reason carefully."),
            _ => panic!("expected text system prompt"),
        }
    }

    #[test]
    fn preset_reasoning_effort_emits_thinking_when_request_unset() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::Medium),
            ..Default::default()
        };
        apply(&mut req, &preset);
        match req.thinking {
            Some(AnthropicThinking::Enabled { budget_tokens, .. }) => {
                assert_eq!(budget_tokens, 4096);
            }
            _ => panic!("expected enabled thinking"),
        }
    }

    #[test]
    fn preset_minimal_effort_disables_thinking() {
        let mut req = empty_request();
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::Minimal),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert!(matches!(req.thinking, Some(AnthropicThinking::Disabled)));
    }

    #[test]
    fn preset_high_effort_clamps_budget_below_max_tokens() {
        let mut req = empty_request();
        req.max_tokens = 4096; // High preset budget (16384) must be clamped.
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        match req.thinking {
            Some(AnthropicThinking::Enabled { budget_tokens, .. }) => {
                assert!(budget_tokens < req.max_tokens);
                assert!(budget_tokens >= 1024);
            }
            _ => panic!("expected enabled thinking"),
        }
    }

    #[test]
    fn request_thinking_wins() {
        let mut req = empty_request();
        req.thinking = Some(AnthropicThinking::Disabled);
        let preset = AppliedPreset {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        apply(&mut req, &preset);
        assert!(matches!(req.thinking, Some(AnthropicThinking::Disabled)));
    }

    #[test]
    fn request_system_wins() {
        let mut req = empty_request();
        req.system = Some(SystemPrompt::Text("Be friendly.".into()));
        let preset = AppliedPreset {
            system: Some("Reason carefully.".into()),
            ..Default::default()
        };
        apply(&mut req, &preset);
        match req.system {
            Some(SystemPrompt::Text(t)) => assert_eq!(t, "Be friendly."),
            _ => panic!("expected text system prompt"),
        }
    }
}
