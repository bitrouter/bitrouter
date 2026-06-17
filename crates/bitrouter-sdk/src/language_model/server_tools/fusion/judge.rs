//! The Fusion judge: its output schema, system prompt, and a tolerant parser.
//!
//! The judge *compares* the panel answers — it does not merge them or
//! majority-vote. Its lift comes from reasoning about disagreement.
//!
//! Reference: <https://openrouter.ai/docs/guides/features/server-tools/fusion>

use serde::{Deserialize, Serialize};

/// The five-field structured analysis the judge emits.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JudgeAnalysis {
    /// Points all or most models agree on (treated as higher-confidence).
    #[serde(default)]
    pub consensus: Vec<String>,
    /// Direct disagreements between panel answers.
    #[serde(default)]
    pub contradictions: Vec<String>,
    /// Claims only some models covered.
    #[serde(default)]
    pub partial_coverage: Vec<String>,
    /// Valuable insights from a single model.
    #[serde(default)]
    pub unique_insights: Vec<String>,
    /// Gaps no model addressed.
    #[serde(default)]
    pub blind_spots: Vec<String>,
}

impl JudgeAnalysis {
    /// Parse the judge's text, tolerating ```json fences and surrounding prose
    /// by extracting the outermost `{ … }` object.
    pub fn parse_lenient(text: &str) -> Result<Self, String> {
        let trimmed = strip_fence(text);
        let start = trimmed.find('{').ok_or("judge produced no JSON object")?;
        let end = trimmed.rfind('}').ok_or("judge produced no JSON object")?;
        if end < start {
            return Err("judge produced no JSON object".to_string());
        }
        serde_json::from_str(&trimmed[start..=end])
            .map_err(|e| format!("judge JSON parse failed: {e}"))
    }
}

fn strip_fence(text: &str) -> &str {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t).trim()
}

/// The JSON-schema contract handed to the judge as `response_format`.
pub fn analysis_schema() -> serde_json::Value {
    let arr = serde_json::json!({ "type": "array", "items": { "type": "string" } });
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["consensus", "contradictions", "partial_coverage", "unique_insights", "blind_spots"],
        "properties": {
            "consensus": arr,
            "contradictions": arr,
            "partial_coverage": arr,
            "unique_insights": arr,
            "blind_spots": arr,
        }
    })
}

/// The judge system prompt. Emphasizes comparison over merging.
pub fn judge_system_prompt() -> &'static str {
    "You are an impartial judge comparing several independent answers to the same \
     prompt. Do NOT merge them and do NOT majority-vote. Identify, as JSON: \
     consensus (points all or most answers agree on — higher confidence), \
     contradictions (direct disagreements), partial_coverage (claims only some \
     answers make), unique_insights (valuable points from a single answer), and \
     blind_spots (gaps none addressed). Return only the JSON object."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_five_analysis_fields() {
        let schema = analysis_schema();
        let props = schema["properties"].as_object().unwrap();
        for k in [
            "consensus",
            "contradictions",
            "partial_coverage",
            "unique_insights",
            "blind_spots",
        ] {
            assert!(props.contains_key(k), "schema missing {k}");
        }
    }

    #[test]
    fn parses_judge_json_even_when_fenced() {
        let raw = "```json\n{\"consensus\":[\"a\"],\"contradictions\":[],\
                   \"partial_coverage\":[],\"unique_insights\":[],\"blind_spots\":[]}\n```";
        assert_eq!(
            JudgeAnalysis::parse_lenient(raw).unwrap().consensus,
            vec!["a".to_string()]
        );
    }

    #[test]
    fn parses_with_surrounding_prose() {
        let raw = "Here is my analysis:\n{\"consensus\":[],\"contradictions\":[\"x vs y\"],\
                   \"partial_coverage\":[],\"unique_insights\":[],\"blind_spots\":[]}\nDone.";
        let a = JudgeAnalysis::parse_lenient(raw).unwrap();
        assert_eq!(a.contradictions, vec!["x vs y".to_string()]);
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let a = JudgeAnalysis::parse_lenient("{\"consensus\":[\"only this\"]}").unwrap();
        assert_eq!(a.consensus, vec!["only this".to_string()]);
        assert!(a.blind_spots.is_empty());
    }

    #[test]
    fn no_json_object_is_an_error() {
        assert!(JudgeAnalysis::parse_lenient("no json here").is_err());
    }
}
