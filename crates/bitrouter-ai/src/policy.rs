//! Model-only routing policy contracts. Compilation and selection are service-owned.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// The exact supported policy contract, independent of software and resource revisions.
pub const SPEC_VERSION: &str = "policy/1.0.0-beta.1";

/// Closed discriminator: unsupported contracts cannot silently become defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SpecVersion {
    /// Initial independently versioned model-only policy contract.
    #[serde(rename = "policy/1.0.0-beta.1")]
    Beta1,
}

// A label's wire spelling is shared by Serde, route keys and the JSON schema.
macro_rules! route_labels {
    (
        $(#[$enum_doc:meta])*
        pub enum $name:ident {
            $($(#[$variant_doc:meta])* $variant:ident => $label:literal),+ $(,)?
        }
    ) => {
        $(#[$enum_doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        pub enum $name {
            $($(#[$variant_doc])* #[serde(rename = $label)] $variant),+
        }

        impl $name {
            #[cfg(feature = "schema")]
            const LABELS: &'static [&'static str] = &[$($label),+];

            /// Return the canonical wire label.
            pub const fn key(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }

            /// Decode a label using its exact wire spelling.
            pub fn parse_key(value: &str) -> Option<Self> {
                match value {
                    $($label => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

route_labels! {
    /// The intent of the request.
    pub enum TaskFamily {
        /// Generate or implement code.
        CodeGeneration => "code:generation",
        /// Diagnose and fix a code defect.
        CodeDebugging => "code:debugging",
        /// Review code and report findings.
        CodeReview => "code:review",
        /// Work on SQL or database behavior.
        CodeSqlDatabase => "code:sql_database",
        /// Work on frontend or user interface behavior.
        CodeFrontendUi => "code:frontend_ui",
        /// Work on deployment or configuration.
        CodeDevopsConfig => "code:devops_config",
        /// Analyze a repository.
        CodeRepositoryAnalysis => "code:repository_analysis",
        /// Plan a sequence of dependent work.
        AgentMultiStepPlanning => "agent:multi_step_planning",
        /// Carry out a workflow.
        AgentWorkflowExecution => "agent:workflow_execution",
        /// Research external information.
        AgentWebResearch => "agent:web_research",
        /// Manage retained context or memory.
        AgentMemoryOperations => "agent:memory_operations",
        /// General agent work.
        AgentGeneral => "agent:general",
        /// Insufficient information to choose a semantic label.
        Unknown => "unknown",
    }
}

route_labels! {
    /// The role of the next inference step.
    pub enum NextStep {
        /// Coordinate or decompose work.
        Orchestrate => "orchestrate",
        /// Produce or modify the solution.
        Implement => "implement",
        /// Perform a narrow, well-specified operation.
        Mechanical => "mechanical",
        /// Check the solution against requirements.
        Verify => "verify",
        /// Deliver the completed result.
        Finalize => "finalize",
        /// Insufficient information to choose a semantic label.
        Unknown => "unknown",
    }
}

impl Default for TaskFamily {
    fn default() -> Self {
        Self::Unknown
    }
}

route_labels! {
    /// Risk of selecting a different generation model for the request.
    pub enum Risk {
        /// The request is eligible for the normal model selection rules.
        Normal => "normal",
        /// The request requires conservative model selection.
        Guarded => "guarded",
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// A model-routing classification, encoded as `task-family|next-step|risk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteClass {
    /// The intent of the request.
    pub task_family: TaskFamily,
    /// The role of the next inference step.
    pub next_step_role: NextStep,
    /// The risk of model selection.
    pub risk: Risk,
}

impl RouteClass {
    /// Construct a classification from validated labels.
    pub const fn new(task_family: TaskFamily, next_step_role: NextStep, risk: Risk) -> Self {
        Self {
            task_family,
            next_step_role,
            risk,
        }
    }

    /// Return the same step and risk with an unknown task family.
    pub const fn unknown_baseline(self) -> Self {
        Self::new(TaskFamily::Unknown, self.next_step_role, self.risk)
    }

    /// Decode a canonical classification.
    pub fn parse_key(value: &str) -> Option<Self> {
        let mut fields = value.split('|');
        let task_family = TaskFamily::parse_key(fields.next()?)?;
        let next_step_role = NextStep::parse_key(fields.next()?)?;
        let risk = Risk::parse_key(fields.next()?)?;
        fields
            .next()
            .is_none()
            .then_some(Self::new(task_family, next_step_role, risk))
    }
}

/// Error returned when a route key cannot be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a canonical task-family|next-step|risk route key")]
pub struct InvalidRouteClass;

impl FromStr for RouteClass {
    type Err = InvalidRouteClass;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_key(value).ok_or(InvalidRouteClass)
    }
}

impl fmt::Display for RouteClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}|{}|{}",
            self.task_family.key(),
            self.next_step_role.key(),
            self.risk
        )
    }
}

impl Serialize for RouteClass {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RouteClass {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for RouteClass {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RouteClass".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": format!("^({})[|]({})[|]({})$",
                TaskFamily::LABELS.join("|"), NextStep::LABELS.join("|"), Risk::LABELS.join("|"))
        })
    }
}

/// Portable policy source. Tenant identity, activation and revision live in the API envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Required, exact contract discriminator; never inferred from other fields.
    pub spec_version: SpecVersion,
    /// Named policies selected by explicit router bindings.
    #[serde(deserialize_with = "unique_map")]
    pub policies: BTreeMap<String, Policy>,
}

/// Model selection without context transformations, agent execution or learning state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Named generation targets. A target is a model and optional reasoning effort.
    #[serde(deserialize_with = "unique_map")]
    pub tiers: BTreeMap<String, Target>,
    /// Required fallback when no admitted classification matches.
    pub default_tier: String,
    /// Exact classifier configuration digest; required when routes are nonempty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_digest: Option<String>,
    /// Canonical `task-family|next-step|risk` classification to tier.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "unique_map"
    )]
    #[cfg_attr(feature = "schema", schemars(schema_with = "route_map_schema"))]
    pub routes: BTreeMap<RouteClass, String>,
    /// Tool-safe fallback when a tool-bearing request selects an unsafe tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_tier: Option<String>,
    /// Tiers permitted for tool-bearing requests when a tool fallback is configured.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "tool_tiers"
    )]
    pub tool_safe_tiers: Vec<String>,
}

/// A generation target. No field can request context rewriting or harness behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Explicit model selector resolved inside the authorized routing source.
    pub model: String,
    /// Absence preserves the caller's reasoning-effort constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::types::ReasoningEffort>,
}

impl Target {
    /// Model identifier selected by this target.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Reasoning effort enforced by this target, when specified.
    pub fn effort(&self) -> Option<crate::types::ReasoningEffort> {
        self.effort
    }
}

impl From<&str> for Target {
    fn from(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            effort: None,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.model)?;
        if let Some(effort) = self.effort {
            write!(f, "@{effort}")?;
        }
        Ok(())
    }
}

#[cfg(feature = "schema")]
fn route_map_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "propertyNames": generator.subschema_for::<RouteClass>(),
        "additionalProperties": generator.subschema_for::<String>()
    })
}

fn unique_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct Unique<K, V>(std::marker::PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> serde::de::Visitor<'de> for Unique<K, V> {
        type Value = BTreeMap<K, V>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a mapping with unique keys")
        }
        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_map(self)
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate policy mapping key"));
                }
            }
            Ok(result)
        }
    }
    // YAML deserializers can coerce a null scalar into an empty map. Only
    // omission may select the field's default.
    deserializer.deserialize_option(Unique(std::marker::PhantomData))
}

fn tool_tiers<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    // Likewise, an explicit null is not an empty array.
    Option::<Vec<String>>::deserialize(deserializer)?
        .ok_or_else(|| serde::de::Error::custom("expected a tool-safe tier array, found null"))
}
