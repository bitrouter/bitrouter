//! Declarations for trusted request-check extensions compiled into the host.

use serde::{Deserialize, Deserializer, Serialize};

use crate::gateway::error::BitrouterError;
use crate::gateway::error::Result;
use crate::gateway::extensions::request_check::validate_revision;

/// A request-check implementation explicitly linked by a custom host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum CheckerConfig {
    /// A callback registered under this checker id by the host.
    Native {
        /// Expected code/rules revision; registration must match exactly.
        native: NativeCheckerConfig,
    },
}

impl<'de> Deserialize<'de> for CheckerConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            native: NativeCheckerConfig,
        }

        let Fields { native } = Fields::deserialize(deserializer)?;
        Ok(Self::Native { native })
    }
}

/// Startup identity for trusted, compiled-in request-check code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeCheckerConfig {
    /// Operator-chosen code/rules revision. Change it when behavior changes.
    pub revision: String,
}

impl CheckerConfig {
    pub(super) fn validate(&self, checker_id: &str) -> Result<()> {
        let Self::Native { native } = self;
        validate_revision(&native.revision).map_err(|_| {
            BitrouterError::bad_request(format!(
                "checker '{checker_id}' native revision must be 1–128 ASCII letters, digits, or . _ + - /"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CheckerConfig, NativeCheckerConfig};

    #[test]
    fn native_revision_is_bounded_diagnostic_metadata() {
        for (revision, valid) in [
            ("rules-v1".to_owned(), true),
            ("rules/v1.2+build_3".to_owned(), true),
            ("x".repeat(128), true),
            ("rules:v1".to_owned(), false),
            ("".to_owned(), false),
            ("has spaces".to_owned(), false),
            ("规则".to_owned(), false),
            ("x".repeat(129), false),
        ] {
            let config = CheckerConfig::Native {
                native: NativeCheckerConfig {
                    revision: revision.clone(),
                },
            };
            assert_eq!(config.validate("safety").is_ok(), valid, "{revision}");
        }
    }
}
