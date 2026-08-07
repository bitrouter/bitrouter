use serde::{Deserialize, Serialize};

use crate::workflow_state::ir::{RouteProjection, RouteRisk, parse_route_risk};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStepRole {
    Orchestrate,
    Implement,
    Mechanical,
    Verify,
    Finalize,
    Unknown,
}

impl NextStepRole {
    fn key(self) -> &'static str {
        match self {
            Self::Orchestrate => "orchestrate",
            Self::Implement => "implement",
            Self::Mechanical => "mechanical",
            Self::Verify => "verify",
            Self::Finalize => "finalize",
            Self::Unknown => "unknown",
        }
    }

    fn parse_key(value: &str) -> Option<Self> {
        match value {
            "orchestrate" => Some(Self::Orchestrate),
            "implement" => Some(Self::Implement),
            "mechanical" => Some(Self::Mechanical),
            "verify" => Some(Self::Verify),
            "finalize" => Some(Self::Finalize),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextActionClass {
    ReasonOrPlan,
    InspectOrRead,
    Mutate,
    ExecuteOrTest,
    WaitOrPoll,
    AnswerOrSummarize,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Mechanical,
    Simple,
    Substantive,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Opening,
    Progressing,
    Stalled,
    Recovering,
    NearDone,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictiveHistoryCompleteness {
    Complete,
    BoundedPrefix,
    Truncated,
    Ambiguous,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictiveEvidence {
    pub code: String,
    pub weight: i16,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictiveRouteIR {
    pub schema_version: u8,
    pub observed: RouteProjection,
    pub next_step_role: NextStepRole,
    pub next_action_class: NextActionClass,
    pub task_complexity: TaskComplexity,
    pub progress_state: ProgressState,
    pub history_completeness: PredictiveHistoryCompleteness,
    pub route_risk: RouteRisk,
    pub confidence: f32,
    pub evidence: Vec<PredictiveEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictiveRouteProjection {
    pub schema_version: u8,
    pub next_step_role: NextStepRole,
    pub risk: RouteRisk,
}

impl PredictiveRouteProjection {
    pub fn key(&self) -> String {
        format!(
            "agent_route/v{}|{}|{}",
            self.schema_version,
            self.next_step_role.key(),
            self.risk
        )
    }

    pub fn parse_key(value: &str) -> Option<Self> {
        let mut segments = value.split('|');
        let (Some(namespace_version), Some(next_step_role), Some(risk), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return None;
        };
        if namespace_version != "agent_route/v1" {
            return None;
        }
        let risk = parse_route_risk(risk)?;

        Some(Self {
            schema_version: 1,
            next_step_role: NextStepRole::parse_key(next_step_role)?,
            risk,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalPolicyProjection {
    Observed(RouteProjection),
    Predictive(PredictiveRouteProjection),
}

impl CanonicalPolicyProjection {
    pub fn parse_key(value: &str) -> Option<Self> {
        RouteProjection::parse_key(value)
            .map(Self::Observed)
            .or_else(|| PredictiveRouteProjection::parse_key(value).map(Self::Predictive))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictive_projection_uses_a_stable_canonical_key() {
        let projection = PredictiveRouteProjection {
            schema_version: 1,
            next_step_role: NextStepRole::Implement,
            risk: RouteRisk::Normal,
        };

        assert_eq!(projection.key(), "agent_route/v1|implement|normal");
        assert_eq!(
            PredictiveRouteProjection::parse_key(&projection.key()),
            Some(projection)
        );
        assert!(CanonicalPolicyProjection::parse_key("agent_trace/v2|edit|normal").is_some());
        assert!(CanonicalPolicyProjection::parse_key("agent_route/v1|implement|normal").is_some());
        assert!(CanonicalPolicyProjection::parse_key("agent_route/v2|implement|normal").is_none());
    }

    #[test]
    fn predictive_enums_serialize_as_stable_snake_case_values() {
        assert!(matches!(
            serde_json::to_string(&NextStepRole::Orchestrate),
            Ok(value) if value == "\"orchestrate\""
        ));
        assert!(matches!(
            serde_json::to_string(&NextActionClass::ExecuteOrTest),
            Ok(value) if value == "\"execute_or_test\""
        ));
        assert!(matches!(
            serde_json::to_string(&TaskComplexity::Substantive),
            Ok(value) if value == "\"substantive\""
        ));
        assert!(matches!(
            serde_json::to_string(&ProgressState::NearDone),
            Ok(value) if value == "\"near_done\""
        ));
        assert!(matches!(
            serde_json::to_string(&PredictiveHistoryCompleteness::BoundedPrefix),
            Ok(value) if value == "\"bounded_prefix\""
        ));
    }

    #[test]
    fn predictive_key_excludes_source_specific_identity() {
        let projection = PredictiveRouteProjection {
            schema_version: 1,
            next_step_role: NextStepRole::Implement,
            risk: RouteRisk::Normal,
        };
        let key = projection.key();

        assert_eq!(key, "agent_route/v1|implement|normal");
        for source_identity in ["codex", "claude_code", "hermes", "smithers", "openclaw"] {
            assert!(!key.contains(source_identity), "{source_identity}");
        }
    }
}
