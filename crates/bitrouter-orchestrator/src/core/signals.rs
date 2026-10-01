//! Versioned harness facts and immutable material resolution. This module has
//! no filesystem access: a material reference never authorizes a workspace read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::checkpoint::sha256;
use super::protocol::{
    CoreError, ErrorCode, HarnessManifest, MaterialRef, SignalUpdate, validate_id,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalState {
    pub revision: u64,
    pub observed_at: Option<String>,
    pub source: Option<String>,
    pub materials: BTreeMap<String, MaterialRef>,
    /// Retain immutable identities even if a later inventory omits a version.
    /// Content bodies stay in the current inventory and frozen model steps.
    pub versions: BTreeMap<String, BTreeMap<String, MaterialRef>>,
    pub facts: BTreeMap<String, Value>,
    pub requests: BTreeMap<String, MaterialRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterialRequest {
    pub request_id: String,
    pub signal_revision: u64,
    pub reference: MaterialRef,
    pub resolved: bool,
    pub unavailable_reason: Option<String>,
}

impl SignalState {
    pub fn apply(
        &mut self,
        update: &SignalUpdate,
        session_id: &str,
        harness_id: &str,
    ) -> Result<(), CoreError> {
        if update.scope != session_id || update.source != harness_id {
            return Err(reject(
                ErrorCode::UnauthorizedScope,
                "signals must name the bound session and harness",
            ));
        }
        if update.signal_revision <= self.revision || update.observed_at.is_empty() {
            return Err(reject(
                ErrorCode::StaleRevision,
                "signals require a newer revision and observation time",
            ));
        }
        if update.workspace_revision != update.manifest.workspace_revision {
            return Err(reject(
                ErrorCode::OperationConflict,
                "signal and manifest workspace revisions differ",
            ));
        }
        let mut materials = BTreeMap::new();
        for material in &update.materials {
            validate_material(material, &update.manifest)?;
            if let Some(previous) = self
                .versions
                .get(&material.material_id)
                .and_then(|versions| versions.get(&material.version))
                && !same_reference(previous, material)
            {
                return Err(reject(
                    ErrorCode::OperationConflict,
                    "a material version cannot change identity or provenance",
                ));
            }
            let mut material = material.clone();
            // Inventory refreshes need not upload identical content again.
            if material.content.is_none()
                && let Some(previous) = self.materials.get(&material.material_id)
                && same_reference(previous, &material)
            {
                material.content = previous.content.clone();
            }
            if materials
                .insert(material.material_id.clone(), material)
                .is_some()
            {
                return Err(reject(
                    ErrorCode::OperationConflict,
                    "duplicate material inventory identity",
                ));
            }
        }
        validate_inventory(&materials, &update.manifest)?;
        for material in materials.values() {
            let mut reference = material.clone();
            reference.content = None;
            self.versions
                .entry(material.material_id.clone())
                .or_default()
                .insert(material.version.clone(), reference);
        }
        self.revision = update.signal_revision;
        self.observed_at = Some(update.observed_at.clone());
        self.source = Some(update.source.clone());
        self.materials = materials;
        self.facts = update.facts.clone();
        Ok(())
    }

    pub fn resolve(
        &mut self,
        request_id: &str,
        material: Option<&MaterialRef>,
        unavailable_reason: Option<&str>,
        manifest: &HarnessManifest,
    ) -> Result<(), CoreError> {
        let request = self
            .requests
            .get(request_id)
            .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown material request"))?;
        if request.resolved {
            return Err(reject(
                ErrorCode::OperationConflict,
                "material request already resolved",
            ));
        }
        let current = self
            .materials
            .get(&request.reference.material_id)
            .ok_or_else(|| {
                reject(
                    ErrorCode::StaleRevision,
                    "requested material left the inventory",
                )
            })?;
        if !same_reference(current, &request.reference) {
            return Err(reject(
                ErrorCode::StaleRevision,
                "material response belongs to an older inventory version",
            ));
        }
        match (material, unavailable_reason) {
            (Some(material), None) => {
                validate_material(material, manifest)?;
                if material.content.is_none() || !same_reference(material, &request.reference) {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "material response must resolve the exact requested content",
                    ));
                }
                let mut inventory = self.materials.clone();
                inventory.insert(material.material_id.clone(), material.clone());
                validate_inventory(&inventory, manifest)?;
                self.materials = inventory;
            }
            (None, Some(reason)) if !reason.is_empty() => {}
            _ => {
                return Err(reject(
                    ErrorCode::ArtifactUnavailable,
                    "material response needs content or an unavailable reason",
                ));
            }
        }
        let request = self
            .requests
            .get_mut(request_id)
            .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "material request disappeared"))?;
        request.resolved = true;
        request.unavailable_reason = unavailable_reason.map(str::to_owned);
        Ok(())
    }
}

fn validate_inventory(
    materials: &BTreeMap<String, MaterialRef>,
    manifest: &HarnessManifest,
) -> Result<(), CoreError> {
    let bytes = materials
        .values()
        .map(|material| {
            material
                .artifact
                .as_ref()
                .map(|artifact| artifact.bytes)
                .or_else(|| {
                    material
                        .content
                        .as_ref()
                        .map(|content| content.len() as u64)
                })
                .unwrap_or(0)
        })
        .fold(0u64, u64::saturating_add);
    if bytes > manifest.artifact_quota_bytes {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "material inventory exceeds its aggregate quota",
        ));
    }
    Ok(())
}

pub fn same_reference(a: &MaterialRef, b: &MaterialRef) -> bool {
    a.material_id == b.material_id
        && a.version == b.version
        && a.sha256 == b.sha256
        && a.media_type == b.media_type
        && a.provenance == b.provenance
        && a.required == b.required
        && a.artifact == b.artifact
}

pub fn validate_material(
    material: &MaterialRef,
    manifest: &HarnessManifest,
) -> Result<(), CoreError> {
    validate_id(&material.material_id)?;
    if material.version.is_empty()
        || material.provenance.is_empty()
        || material.sha256.len() != 64
        || !material
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(reject(
            ErrorCode::ArtifactUnavailable,
            "material identity, digest, version and provenance are required",
        ));
    }
    if !matches!(
        material.media_type.as_str(),
        "text/plain" | "text/markdown" | "application/json"
    ) {
        return Err(reject(
            ErrorCode::UnsupportedCapability,
            "managed context materials currently require UTF-8 text",
        ));
    }
    if let Some(content) = &material.content {
        if content.len() as u64 > manifest.artifact_quota_bytes
            || sha256(content.as_bytes()) != material.sha256
        {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "material content exceeds quota or differs from its digest",
            ));
        }
        if material.media_type == "application/json"
            && serde_json::from_str::<Value>(content).is_err()
        {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "JSON material is not valid JSON",
            ));
        }
    }
    if let Some(artifact) = &material.artifact {
        validate_id(&artifact.artifact_id)?;
        if artifact.sha256 != material.sha256
            || artifact.media_type != material.media_type
            || artifact.bytes > manifest.artifact_quota_bytes
            || material
                .content
                .as_ref()
                .is_some_and(|content| content.len() as u64 != artifact.bytes)
        {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "material artifact does not match its content reference",
            ));
        }
    }
    Ok(())
}

fn reject(code: ErrorCode, message: &str) -> CoreError {
    CoreError::rejected(code, message)
}
