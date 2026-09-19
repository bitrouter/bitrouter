//! Declarations for trusted request-check extensions compiled into the host.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::error::{BitrouterError, Result};
use crate::extension::request_check::validate_revision;

/// A request-check implementation explicitly linked by a custom host.
///
/// The existing `native.revision` configuration shape remains supported. Former
/// HTTP declarations are rejected with a migration diagnostic, never ignored.
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
        struct Fields {
            native: Option<NativeCheckerConfig>,
            #[serde(flatten)]
            unsupported: BTreeMap<String, de::IgnoredAny>,
        }

        let fields = Fields::deserialize(deserializer)?;
        if fields.unsupported.keys().any(|key| {
            matches!(
                key.as_str(),
                "endpoint" | "credential_env" | "contract_version"
            )
        }) {
            return Err(de::Error::custom(
                "HTTP request-check extensions are no longer supported; compile the extension into a custom host, register it through ExtensionApi, and replace the HTTP fields with native.revision; do not remove the router's check binding",
            ));
        }
        if let Some(key) = fields.unsupported.keys().next() {
            return Err(de::Error::unknown_field(key, &["native"]));
        }
        fields
            .native
            .map(|native| Self::Native { native })
            .ok_or_else(|| de::Error::missing_field("native"))
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
