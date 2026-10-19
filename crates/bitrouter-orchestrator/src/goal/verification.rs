//! Actual local checks with owned process cleanup, separate from worker claims.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{GoalError, identifier, invalid, message};

pub mod permission;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    pub id: String,
    pub description: String,
    pub recipe: CheckRecipe,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    1200
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckRecipe {
    CargoTest,
    CargoClippy,
    CargoFmt,
    NpmTest,
    PnpmTest,
    BunTest,
    FileExists { path: String },
    FileContains { path: String, text: String },
}

impl CheckSpec {
    /// Command that requires explicit effect permission, or `None` for a file check.
    pub fn command(&self) -> Option<&'static str> {
        match self.recipe {
            CheckRecipe::CargoTest => Some("cargo test --all-features"),
            CheckRecipe::CargoClippy => Some("cargo clippy --all-features -- -D warnings"),
            CheckRecipe::CargoFmt => Some("cargo fmt -- --check"),
            CheckRecipe::NpmTest => Some("npm test"),
            CheckRecipe::PnpmTest => Some("pnpm test"),
            CheckRecipe::BunTest => Some("bun test"),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        identifier(&self.id)?;
        message(&self.description)?;
        if self.timeout_seconds == 0 || self.timeout_seconds > 3600 {
            return Err(invalid("check timeout must be 1..=3600 seconds"));
        }
        match &self.recipe {
            CheckRecipe::FileExists { path } | CheckRecipe::FileContains { path, .. } => {
                relative(path)?;
            }
            _ => {}
        }
        if let CheckRecipe::FileContains { text, .. } = &self.recipe {
            message(text)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOutput {
    pub completed: bool,
    pub passed: bool,
    pub exit_status: Option<i32>,
    pub timed_out: bool,
    pub environment: serde_json::Value,
    pub output: serde_json::Value,
}

fn relative(value: &str) -> Result<&Path, GoalError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(invalid("check path must remain within its workspace"));
    }
    Ok(path)
}

impl CheckOutput {
    pub fn supports(&self, spec: &CheckSpec) -> bool {
        self.completed
            && self.passed
            && !self.timed_out
            && match &spec.recipe {
                CheckRecipe::FileExists { .. } | CheckRecipe::FileContains { .. } => true,
                _ => self.exit_status == Some(0),
            }
    }
}

/// Run a bounded file check. Command recipes fail closed without a permit;
/// use [`run_approved`] after explicit command permission admission.
/// Caller must persist check admission and its applicable artifact beforehand,
/// and retain the outcome even when source conditions change.
///
/// # Errors
/// Rejects executable recipes without permission and invalid file-check inputs.
pub async fn run(
    workspace: &Path,
    check: &CheckSpec,
    attempt_id: &str,
    cancellation: &CancellationToken,
) -> Result<CheckOutput, String> {
    run_inner(workspace, check, attempt_id, cancellation, None).await
}

/// Execute a command with a consumed, artifact-bound permission.
///
/// # Errors
/// Rejects mismatched permission scope or invalid check/workspace inputs.
pub async fn run_approved(
    workspace: &Path,
    check: &CheckSpec,
    attempt_id: &str,
    cancellation: &CancellationToken,
    permit: permission::CommandPermit,
) -> Result<CheckOutput, String> {
    run_inner(workspace, check, attempt_id, cancellation, Some(permit)).await
}

async fn run_inner(
    workspace: &Path,
    check: &CheckSpec,
    attempt_id: &str,
    cancellation: &CancellationToken,
    permit: Option<permission::CommandPermit>,
) -> Result<CheckOutput, String> {
    check.validate().map_err(|error| error.to_string())?;
    let root = workspace
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let environment = serde_json::json!({"source":"local_bro_check","os":std::env::consts::OS,"arch":std::env::consts::ARCH,"workspace":root,"recipe":check.recipe});
    if cancellation.is_cancelled() {
        return Err("check cancelled before execution".into());
    }
    if let Some(command) = check.command() {
        let permit = permit.ok_or("command check requires explicit effect permission")?;
        if permit.scope.specification != *check
            || permit.attempt_id != attempt_id
            || permit.scope.workspace != workspace
        {
            return Err("command permission does not match this check invocation".into());
        }
        let tools = crate::tools::WorkspaceTools::new(&root, crate::agent::ToolMode::Coding)
            .map_err(|error| error.to_string())?;
        let (output, effect) = tools
            .verify_command(
                command.into(),
                check.timeout_seconds,
                cancellation,
                attempt_id,
            )
            .await;
        let value = serde_json::to_value(&output).map_err(|error| error.to_string())?;
        let observed = match &output {
            bitrouter_ai::types::ToolResultOutput::Json { value } => Some(value),
            _ => None,
        };
        let exit_status = observed
            .and_then(|value| value.get("exit_status"))
            .and_then(serde_json::Value::as_i64)
            .and_then(|status| i32::try_from(status).ok());
        let timed_out = observed
            .and_then(|value| value.get("timed_out"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let completed = effect == crate::store::EffectStatus::Completed && !timed_out;
        return Ok(CheckOutput {
            completed,
            passed: completed && exit_status == Some(0),
            exit_status,
            timed_out,
            environment: serde_json::json!({"host":environment,"command":command,"permission":permit.scope,"interpreter":observed.and_then(|value| value.get("interpreter"))}),
            output: value,
        });
    }
    let (path, text) = match &check.recipe {
        CheckRecipe::FileExists { path } => (path, None),
        CheckRecipe::FileContains { path, text } => (path, Some(text.as_str())),
        _ => return Err("check has no execution recipe".into()),
    };
    let file = root.join(relative(path).map_err(|error| error.to_string())?);
    let canonical = match file.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            return Ok(CheckOutput {
                completed: true,
                passed: false,
                exit_status: None,
                timed_out: false,
                environment,
                output: serde_json::json!({"path":path,"error":error.to_string()}),
            });
        }
    };
    if !canonical.starts_with(&root) {
        return Err("check target escaped workspace through a symlink".into());
    }
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
        return Err("file check requires a bounded regular file".into());
    }
    let bytes = tokio::fs::read(canonical)
        .await
        .map_err(|error| error.to_string())?;
    let passed = text
        .is_none_or(|text| std::str::from_utf8(&bytes).is_ok_and(|content| content.contains(text)));
    use sha2::{Digest, Sha256};
    Ok(CheckOutput {
        completed: true,
        passed,
        exit_status: None,
        timed_out: false,
        environment,
        output: serde_json::json!({"path":path,"sha256":hex::encode(Sha256::digest(&bytes)),"bytes":bytes.len(),"matched":passed}),
    })
}

#[cfg(test)]
mod tests;
