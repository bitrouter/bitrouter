//! The optional machine-consumable result contract for headless subagents
//! (TUI_SPEC §4, goose's `response.json_schema` model): the caller supplies a
//! JSON Schema, the schema text rides the subagent's prompt, and the reply's
//! final fenced ```json block is extracted and validated. On invalid output
//! the driver sends **one** repair re-prompt, then reports `schema_ok: false`
//! plus the raw text — the orchestrator is never blocked.

use anyhow::{Context, Result};

/// A parsed result-schema contract: the schema value plus its compiled
/// validator.
pub struct ResultContract {
    schema_text: String,
    validator: jsonschema::Validator,
}

impl ResultContract {
    /// Build a contract from the `--result-schema` flag value: inline JSON, or
    /// `@path` to read the schema from a file.
    pub fn from_flag(flag: &str) -> Result<Self> {
        let text = match flag.strip_prefix('@') {
            Some(path) => std::fs::read_to_string(path)
                .with_context(|| format!("reading result schema from {path}"))?,
            None => flag.to_string(),
        };
        let schema: serde_json::Value =
            serde_json::from_str(&text).context("result schema is not valid JSON")?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|e| anyhow::anyhow!("result schema is not a valid JSON Schema: {e}"))?;
        Ok(Self {
            schema_text: serde_json::to_string_pretty(&schema).unwrap_or(text),
            validator,
        })
    }

    /// The contract clause appended to the subagent's task prompt.
    pub fn instruction(&self) -> String {
        format!(
            "\n\nWhen you are done, end your reply with your final result as a single \
             JSON object inside a ```json fenced code block (the last such block in \
             your reply is taken as the result). It must match this JSON Schema:\n\
             ```json\n{}\n```",
            self.schema_text
        )
    }

    /// The repair re-prompt sent once when the reply's result was missing or
    /// invalid.
    pub fn repair_prompt(&self, problem: &str) -> String {
        format!(
            "Your previous reply did not contain a valid result ({problem}). Reply with \
             ONLY a ```json fenced code block containing the corrected result object, \
             matching the schema you were given."
        )
    }

    /// Extract and validate the result from one reply's accumulated message
    /// text. `Ok(value)` on success; `Err(problem)` describes what was wrong
    /// (no block, bad JSON, or schema violations).
    pub fn check(&self, reply: &str) -> std::result::Result<serde_json::Value, String> {
        let candidate = extract_json(reply).ok_or("no JSON result found in the reply")?;
        let value: serde_json::Value = serde_json::from_str(&candidate)
            .map_err(|e| format!("result block is not valid JSON: {e}"))?;
        let errors: Vec<String> = self
            .validator
            .iter_errors(&value)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(value)
        } else {
            Err(format!("schema violations: {}", errors.join("; ")))
        }
    }
}

/// The candidate result JSON in a reply: the **last** ```json fenced block, or
/// — when no fence is present — the whole trimmed reply if it looks like a
/// JSON object (agents sometimes answer with bare JSON).
fn extract_json(reply: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut in_block: Option<String> = None;
    for line in reply.lines() {
        let trimmed = line.trim();
        match &mut in_block {
            None => {
                let fence = trimmed.strip_prefix("```");
                if let Some(info) = fence
                    && matches!(info.trim(), "json" | "JSON")
                {
                    in_block = Some(String::new());
                }
            }
            Some(buf) => {
                if trimmed.starts_with("```") {
                    last = in_block.take();
                } else {
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
    }
    if last.is_some() {
        return last;
    }
    let trimmed = reply.trim();
    (trimmed.starts_with('{') && trimmed.ends_with('}')).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> ResultContract {
        ResultContract::from_flag(
            r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#,
        )
        .expect("valid schema")
    }

    #[test]
    fn from_flag_rejects_bad_json_and_bad_schema() {
        assert!(ResultContract::from_flag("not json").is_err());
        assert!(
            ResultContract::from_flag(r#"{"type":"no-such-type"}"#).is_err(),
            "an invalid schema must fail up front, not at check time"
        );
    }

    #[test]
    fn instruction_carries_the_schema() {
        let c = contract();
        let inst = c.instruction();
        assert!(inst.contains("```json"), "fenced block requested");
        assert!(inst.contains("\"ok\""), "schema text rides the prompt");
    }

    #[test]
    fn check_takes_the_last_json_block_and_validates() {
        let c = contract();
        let reply =
            "Working…\n```json\n{\"ok\": \"draft\"}\n```\nFinal:\n```json\n{\"ok\": true}\n```\n";
        assert_eq!(
            c.check(reply).expect("valid"),
            serde_json::json!({"ok": true}),
            "the LAST block wins"
        );
    }

    #[test]
    fn check_accepts_bare_json_reply() {
        let c = contract();
        assert!(c.check("{\"ok\": false}").is_ok());
    }

    #[test]
    fn check_reports_missing_invalid_and_violating_results() {
        let c = contract();
        assert!(c.check("no json here").unwrap_err().contains("no JSON"));
        assert!(
            c.check("```json\n{broken\n```")
                .unwrap_err()
                .contains("not valid JSON")
        );
        let err = c.check("```json\n{\"ok\": 3}\n```").unwrap_err();
        assert!(err.contains("schema violations"), "{err}");
    }

    #[test]
    fn repair_prompt_names_the_problem() {
        let c = contract();
        let p = c.repair_prompt("no JSON result found in the reply");
        assert!(p.contains("no JSON result found"));
    }
}
