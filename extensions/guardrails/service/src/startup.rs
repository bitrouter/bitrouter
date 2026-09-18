//! Sanitized startup configuration loading.

use std::path::Path;

use bitrouter_guardrails::config::{InputConfigError, InputGuardrailConfig};
use bitrouter_guardrails::rules::RuleSet;

/// Rules-file activation failure that does not include rule names, patterns,
/// source lines, or file contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesLoadError {
    /// The file could not be read.
    Read(std::io::ErrorKind),
    /// The YAML did not match the strict input-only schema.
    InvalidDocument,
    /// The strict document parsed but its rules could not be activated.
    InvalidRules(InputConfigError),
}

impl std::fmt::Display for RulesLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(kind) => write!(formatter, "cannot read rules file ({kind:?})"),
            Self::InvalidDocument => {
                formatter.write_str("rules file does not match the input-only schema")
            }
            Self::InvalidRules(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RulesLoadError {}

/// Load strict YAML rules and compile every expression before the server binds.
pub fn load_rules(path: &Path) -> Result<RuleSet, RulesLoadError> {
    let source =
        std::fs::read_to_string(path).map_err(|error| RulesLoadError::Read(error.kind()))?;
    let config = serde_saphyr::from_str::<InputGuardrailConfig>(&source)
        .map_err(|_| RulesLoadError::InvalidDocument)?;
    config.compile().map_err(RulesLoadError::InvalidRules)
}
