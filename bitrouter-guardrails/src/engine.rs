use bitrouter_core::models::language::{
    call_options::LanguageModelCallOptions,
    content::LanguageModelContent,
    generate_result::LanguageModelGenerateResult,
    prompt::{
        LanguageModelAssistantContent, LanguageModelMessage, LanguageModelToolResult,
        LanguageModelToolResultOutput, LanguageModelToolResultOutputContent,
        LanguageModelUserContent,
    },
    stream_part::LanguageModelStreamPart,
};

use crate::{
    config::GuardrailConfig,
    pattern::{CompiledPattern, builtin_patterns, downgoing_pattern_ids, upgoing_pattern_ids},
    rule::{Action, InspectionResult, REDACTED_PLACEHOLDER, Violation},
};

/// The guardrail engine.
///
/// Pre-compiles all built-in patterns on construction and evaluates
/// incoming/outgoing content against the configured rules.
#[derive(Debug, Clone)]
pub struct Guardrail {
    config: GuardrailConfig,
    patterns: Vec<CompiledPattern>,
}

impl Guardrail {
    /// Create a new guardrail engine from the given configuration.
    pub fn new(config: GuardrailConfig) -> Self {
        Self {
            config,
            patterns: builtin_patterns(),
        }
    }

    /// Returns `true` when the guardrail engine is disabled and will skip all
    /// checks.
    pub fn is_disabled(&self) -> bool {
        !self.config.enabled
    }

    // ── Upgoing (outbound) inspection ────────────────────────────────

    /// Inspect a text string for upgoing pattern matches and apply configured
    /// rules. Returns the inspection result containing any violations and
    /// the (possibly redacted) text.
    pub fn inspect_upgoing_text(&self, text: &str) -> InspectionResult {
        if self.is_disabled() {
            return InspectionResult {
                violations: vec![],
                blocked: false,
                content: text.to_owned(),
            };
        }

        let upgoing_ids = upgoing_pattern_ids();
        let mut violations = Vec::new();
        let mut content = text.to_owned();
        let mut blocked = false;

        for pat in &self.patterns {
            if !upgoing_ids.contains(&pat.id) {
                continue;
            }
            let action = self.config.upgoing_action(pat.id);
            for m in pat.regex.find_iter(text) {
                let matched = m.as_str().to_owned();
                match action {
                    Action::Warn => {
                        tracing::warn!(
                            pattern = ?pat.id,
                            matched = %matched,
                            "guardrail: upgoing content matched sensitive pattern (warn)"
                        );
                    }
                    Action::Redact => {
                        content = content.replace(&matched, REDACTED_PLACEHOLDER);
                        tracing::info!(
                            pattern = ?pat.id,
                            "guardrail: upgoing content redacted"
                        );
                    }
                    Action::Block => {
                        blocked = true;
                        tracing::warn!(
                            pattern = ?pat.id,
                            "guardrail: upgoing content blocked"
                        );
                    }
                }
                violations.push(Violation {
                    pattern_id: pat.id,
                    description: pat.description,
                    action,
                    matched,
                });
            }
        }

        InspectionResult {
            violations,
            blocked,
            content,
        }
    }

    /// Inspect a text string for downgoing pattern matches and apply
    /// configured rules.
    pub fn inspect_downgoing_text(&self, text: &str) -> InspectionResult {
        if self.is_disabled() {
            return InspectionResult {
                violations: vec![],
                blocked: false,
                content: text.to_owned(),
            };
        }

        let downgoing_ids = downgoing_pattern_ids();
        let mut violations = Vec::new();
        let mut content = text.to_owned();
        let mut blocked = false;

        for pat in &self.patterns {
            if !downgoing_ids.contains(&pat.id) {
                continue;
            }
            let action = self.config.downgoing_action(pat.id);
            for m in pat.regex.find_iter(text) {
                let matched = m.as_str().to_owned();
                match action {
                    Action::Warn => {
                        tracing::warn!(
                            pattern = ?pat.id,
                            matched = %matched,
                            "guardrail: downgoing content matched suspicious pattern (warn)"
                        );
                    }
                    Action::Redact => {
                        content = content.replace(&matched, REDACTED_PLACEHOLDER);
                        tracing::info!(
                            pattern = ?pat.id,
                            "guardrail: downgoing content redacted"
                        );
                    }
                    Action::Block => {
                        blocked = true;
                        tracing::warn!(
                            pattern = ?pat.id,
                            "guardrail: downgoing content blocked"
                        );
                    }
                }
                violations.push(Violation {
                    pattern_id: pat.id,
                    description: pat.description,
                    action,
                    matched,
                });
            }
        }

        InspectionResult {
            violations,
            blocked,
            content,
        }
    }

    // ── High-level call-options / result inspection ──────────────────

    /// Inspect **outbound** call options (prompt messages). Returns `Err`
    /// with a human-readable reason if any pattern triggered `Block`.
    /// When `Redact` is active, matched substrings in text messages are
    /// replaced in-place.
    pub fn inspect_call_options(
        &self,
        options: &mut LanguageModelCallOptions,
    ) -> Result<Vec<Violation>, String> {
        if self.is_disabled() {
            return Ok(vec![]);
        }

        let mut all_violations = Vec::new();

        for msg in &mut options.prompt {
            match msg {
                LanguageModelMessage::System { content, .. } => {
                    let result = self.inspect_upgoing_text(content);
                    if result.blocked {
                        return Err(format!(
                            "guardrail blocked upgoing system message: {}",
                            violation_summary(&result.violations)
                        ));
                    }
                    *content = result.content;
                    all_violations.extend(result.violations);
                }
                LanguageModelMessage::User { content, .. } => {
                    for item in content.iter_mut() {
                        if let LanguageModelUserContent::Text { text, .. } = item {
                            let result = self.inspect_upgoing_text(text);
                            if result.blocked {
                                return Err(format!(
                                    "guardrail blocked upgoing user message: {}",
                                    violation_summary(&result.violations)
                                ));
                            }
                            *text = result.content;
                            all_violations.extend(result.violations);
                        }
                    }
                }
                LanguageModelMessage::Assistant { content, .. } => {
                    for item in content.iter_mut() {
                        match item {
                            LanguageModelAssistantContent::Text { text, .. } => {
                                let result = self.inspect_upgoing_text(text);
                                if result.blocked {
                                    return Err(format!(
                                        "guardrail blocked upgoing assistant message: {}",
                                        violation_summary(&result.violations)
                                    ));
                                }
                                *text = result.content;
                                all_violations.extend(result.violations);
                            }
                            LanguageModelAssistantContent::Reasoning { text, .. } => {
                                let result = self.inspect_upgoing_text(text);
                                if result.blocked {
                                    return Err(format!(
                                        "guardrail blocked upgoing assistant reasoning: {}",
                                        violation_summary(&result.violations)
                                    ));
                                }
                                *text = result.content;
                                all_violations.extend(result.violations);
                            }
                            _ => {}
                        }
                    }
                }
                LanguageModelMessage::Tool { content, .. } => {
                    for item in content.iter_mut() {
                        if let LanguageModelToolResult::ToolResult { output, .. } = item {
                            self.inspect_tool_result_output_upgoing(output, &mut all_violations)?;
                        }
                    }
                }
            }
        }

        Ok(all_violations)
    }

    /// Recursively inspect tool result output for upgoing patterns.
    fn inspect_tool_result_output_upgoing(
        &self,
        output: &mut LanguageModelToolResultOutput,
        violations: &mut Vec<Violation>,
    ) -> Result<(), String> {
        match output {
            LanguageModelToolResultOutput::Text { value, .. } => {
                let result = self.inspect_upgoing_text(value);
                if result.blocked {
                    return Err(format!(
                        "guardrail blocked upgoing tool result: {}",
                        violation_summary(&result.violations)
                    ));
                }
                *value = result.content;
                violations.extend(result.violations);
            }
            LanguageModelToolResultOutput::ErrorText { value, .. } => {
                let result = self.inspect_upgoing_text(value);
                if result.blocked {
                    return Err(format!(
                        "guardrail blocked upgoing tool error: {}",
                        violation_summary(&result.violations)
                    ));
                }
                *value = result.content;
                violations.extend(result.violations);
            }
            LanguageModelToolResultOutput::Content { value, .. } => {
                for content_item in value.iter_mut() {
                    if let LanguageModelToolResultOutputContent::Text { text, .. } = content_item {
                        let result = self.inspect_upgoing_text(text);
                        if result.blocked {
                            return Err(format!(
                                "guardrail blocked upgoing tool content: {}",
                                violation_summary(&result.violations)
                            ));
                        }
                        *text = result.content;
                        violations.extend(result.violations);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Inspect **inbound** generate result (response content). Returns `Err`
    /// with a reason if any pattern triggered `Block`. When `Redact` is
    /// active, matched text content is replaced in-place.
    pub fn inspect_generate_result(
        &self,
        result: &mut LanguageModelGenerateResult,
    ) -> Result<Vec<Violation>, String> {
        if self.is_disabled() {
            return Ok(vec![]);
        }

        let mut all_violations = Vec::new();

        match &mut result.content {
            LanguageModelContent::Text { text, .. } => {
                let inspection = self.inspect_downgoing_text(text);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing text: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *text = inspection.content;
                all_violations.extend(inspection.violations);
            }
            LanguageModelContent::Reasoning { text, .. } => {
                let inspection = self.inspect_downgoing_text(text);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing reasoning: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *text = inspection.content;
                all_violations.extend(inspection.violations);
            }
            LanguageModelContent::ToolCall { tool_input, .. } => {
                let inspection = self.inspect_downgoing_text(tool_input);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing tool call: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *tool_input = inspection.content;
                all_violations.extend(inspection.violations);
            }
            _ => {}
        }

        Ok(all_violations)
    }

    /// Inspect a single **inbound** stream part. Returns `Ok(violations)` on
    /// pass, `Err(reason)` if the part should be blocked. Text deltas are
    /// mutated in-place when `Redact` is active.
    pub fn inspect_stream_part(
        &self,
        part: &mut LanguageModelStreamPart,
    ) -> Result<Vec<Violation>, String> {
        if self.is_disabled() {
            return Ok(vec![]);
        }

        match part {
            LanguageModelStreamPart::TextDelta { delta, .. } => {
                let inspection = self.inspect_downgoing_text(delta);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing stream text: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *delta = inspection.content;
                Ok(inspection.violations)
            }
            LanguageModelStreamPart::ReasoningDelta { delta, .. } => {
                let inspection = self.inspect_downgoing_text(delta);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing stream reasoning: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *delta = inspection.content;
                Ok(inspection.violations)
            }
            LanguageModelStreamPart::ToolInputDelta { delta, .. } => {
                let inspection = self.inspect_downgoing_text(delta);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing stream tool input: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *delta = inspection.content;
                Ok(inspection.violations)
            }
            LanguageModelStreamPart::ToolCall { tool_input, .. } => {
                let inspection = self.inspect_downgoing_text(tool_input);
                if inspection.blocked {
                    return Err(format!(
                        "guardrail blocked downgoing stream tool call: {}",
                        violation_summary(&inspection.violations)
                    ));
                }
                *tool_input = inspection.content;
                Ok(inspection.violations)
            }
            _ => Ok(vec![]),
        }
    }
}

/// Format a concise summary of violations for error messages.
fn violation_summary(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(|v| format!("{:?}", v.pattern_id))
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::PatternId;
    use bitrouter_core::models::language::{
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        prompt::{LanguageModelMessage, LanguageModelUserContent},
        stream_part::LanguageModelStreamPart,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    };

    fn default_usage() -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: None,
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: None,
                text: None,
                reasoning: None,
            },
            raw: None,
        }
    }

    // ── Disabled engine ──────────────────────────────────────────────

    #[test]
    fn disabled_guardrail_is_noop() {
        let config = GuardrailConfig {
            enabled: false,
            ..Default::default()
        };
        let g = Guardrail::new(config);
        assert!(g.is_disabled());

        let result = g.inspect_upgoing_text("sk-abc123def456ghi789jkl012");
        assert!(result.is_clean());
        assert_eq!(result.content, "sk-abc123def456ghi789jkl012");
    }

    // ── Warn (default) ──────────────────────────────────────────────

    #[test]
    fn default_config_warns_on_api_key() {
        let g = Guardrail::new(GuardrailConfig::default());
        let text = "my key is sk-abc123def456ghi789jkl012 ok";
        let result = g.inspect_upgoing_text(text);
        assert!(!result.blocked);
        // Content is unchanged under Warn
        assert_eq!(result.content, text);
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].pattern_id, PatternId::ApiKeys);
        assert_eq!(result.violations[0].action, Action::Warn);
    }

    // ── Redact ──────────────────────────────────────────────────────

    #[test]
    fn redact_replaces_api_key_with_placeholder() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::ApiKeys, Action::Redact);
        let g = Guardrail::new(config);

        let text = "key: sk-abc123def456ghi789jkl012 done";
        let result = g.inspect_upgoing_text(text);
        assert!(!result.blocked);
        assert!(result.content.contains(REDACTED_PLACEHOLDER));
        assert!(!result.content.contains("sk-abc123"));
        assert_eq!(result.violations.len(), 1);
    }

    #[test]
    fn redact_replaces_multiple_matches() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::PiiEmails, Action::Redact);
        let g = Guardrail::new(config);

        let text = "contact alice@example.com or bob@test.org";
        let result = g.inspect_upgoing_text(text);
        assert!(!result.blocked);
        assert!(!result.content.contains("alice@example.com"));
        assert!(!result.content.contains("bob@test.org"));
        assert_eq!(result.violations.len(), 2);
    }

    // ── Block ───────────────────────────────────────────────────────

    #[test]
    fn block_sets_blocked_flag() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::PrivateKeys, Action::Block);
        let g = Guardrail::new(config);

        let text = "here is -----BEGIN RSA PRIVATE KEY-----\nMIIE... end";
        let result = g.inspect_upgoing_text(text);
        assert!(result.blocked);
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].action, Action::Block);
    }

    // ── Downgoing inspection ────────────────────────────────────────

    #[test]
    fn downgoing_warns_on_suspicious_command() {
        let g = Guardrail::new(GuardrailConfig::default());
        let text = "try running rm -rf / to clean up";
        let result = g.inspect_downgoing_text(text);
        assert!(!result.blocked);
        assert_eq!(result.violations.len(), 1);
        assert_eq!(
            result.violations[0].pattern_id,
            PatternId::SuspiciousCommands
        );
    }

    #[test]
    fn downgoing_block_stops_suspicious_command() {
        let mut config = GuardrailConfig::default();
        config
            .downgoing
            .insert(PatternId::SuspiciousCommands, Action::Block);
        let g = Guardrail::new(config);

        let text = "run this: rm -rf / for cleanup";
        let result = g.inspect_downgoing_text(text);
        assert!(result.blocked);
    }

    // ── Call options inspection ──────────────────────────────────────

    #[test]
    fn inspect_call_options_redacts_user_text() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::ApiKeys, Action::Redact);
        let g = Guardrail::new(config);

        let mut options = LanguageModelCallOptions {
            prompt: vec![LanguageModelMessage::User {
                content: vec![LanguageModelUserContent::Text {
                    text: "my key sk-abc123def456ghi789jkl012 here".to_owned(),
                    provider_options: None,
                }],
                provider_options: None,
            }],
            ..default_call_options()
        };

        let violations = g.inspect_call_options(&mut options).unwrap();
        assert_eq!(violations.len(), 1);

        // The text in the prompt should have been redacted
        if let LanguageModelMessage::User { content, .. } = &options.prompt[0] {
            if let LanguageModelUserContent::Text { text, .. } = &content[0] {
                assert!(text.contains(REDACTED_PLACEHOLDER));
                assert!(!text.contains("sk-abc123"));
            } else {
                panic!("expected Text");
            }
        } else {
            panic!("expected User");
        }
    }

    #[test]
    fn inspect_call_options_blocks_private_key() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::PrivateKeys, Action::Block);
        let g = Guardrail::new(config);

        let mut options = LanguageModelCallOptions {
            prompt: vec![LanguageModelMessage::System {
                content: "-----BEGIN PRIVATE KEY-----\nMIIE...".to_owned(),
                provider_options: None,
            }],
            ..default_call_options()
        };

        let err = g.inspect_call_options(&mut options).unwrap_err();
        assert!(err.contains("blocked"));
    }

    // ── Generate result inspection ──────────────────────────────────

    #[test]
    fn inspect_generate_result_blocks_suspicious_tool_call() {
        let mut config = GuardrailConfig::default();
        config
            .downgoing
            .insert(PatternId::SuspiciousCommands, Action::Block);
        let g = Guardrail::new(config);

        let mut gen_result = LanguageModelGenerateResult {
            content: LanguageModelContent::ToolCall {
                tool_call_id: "tc1".to_owned(),
                tool_name: "bash".to_owned(),
                tool_input: "rm -rf /".to_owned(),
                provider_executed: None,
                dynamic: None,
                provider_metadata: None,
            },
            finish_reason: LanguageModelFinishReason::Stop,
            usage: default_usage(),
            provider_metadata: None,
            request: None,
            response_metadata: None,
            warnings: None,
        };

        let err = g.inspect_generate_result(&mut gen_result).unwrap_err();
        assert!(err.contains("blocked"));
    }

    // ── Stream part inspection ──────────────────────────────────────

    #[test]
    fn inspect_stream_part_redacts_text_delta() {
        let mut config = GuardrailConfig::default();
        config
            .downgoing
            .insert(PatternId::SuspiciousCommands, Action::Redact);
        let g = Guardrail::new(config);

        let mut part = LanguageModelStreamPart::TextDelta {
            id: "d1".to_owned(),
            delta: "do: rm -rf / please".to_owned(),
            provider_metadata: None,
        };

        let violations = g.inspect_stream_part(&mut part).unwrap();
        assert_eq!(violations.len(), 1);

        if let LanguageModelStreamPart::TextDelta { delta, .. } = &part {
            assert!(delta.contains(REDACTED_PLACEHOLDER));
            assert!(!delta.contains("rm -rf /"));
        }
    }

    #[test]
    fn inspect_stream_part_noop_for_non_text_parts() {
        let g = Guardrail::new(GuardrailConfig::default());
        let mut part = LanguageModelStreamPart::StreamStart { warnings: vec![] };
        let violations = g.inspect_stream_part(&mut part).unwrap();
        assert!(violations.is_empty());
    }

    // ── Multiple patterns ───────────────────────────────────────────

    #[test]
    fn multiple_patterns_all_detected() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::ApiKeys, Action::Redact);
        config.upgoing.insert(PatternId::PiiEmails, Action::Redact);
        let g = Guardrail::new(config);

        let text = "key=sk-abc123def456ghi789jkl012 email=user@example.com";
        let result = g.inspect_upgoing_text(text);
        assert_eq!(result.violations.len(), 2);
        assert!(result.content.contains(REDACTED_PLACEHOLDER));
        assert!(!result.content.contains("sk-abc123"));
        assert!(!result.content.contains("user@example.com"));
    }

    // ── Helpers ─────────────────────────────────────────────────────

    fn default_call_options() -> LanguageModelCallOptions {
        LanguageModelCallOptions {
            prompt: vec![],
            stream: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            presence_penalty: None,
            frequency_penalty: None,
            response_format: None,
            seed: None,
            tools: None,
            tool_choice: None,
            include_raw_chunks: None,
            abort_signal: None,
            headers: None,
            provider_options: None,
        }
    }
}
