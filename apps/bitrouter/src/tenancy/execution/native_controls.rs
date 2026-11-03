//! Opaque provider controls under current ordered authority, without ID custody.

use crate::gateway::{
    error::BitrouterError,
    model::{conversion::ReportAdmission, protocol::values::extra_value_for_protocol},
};
use bitrouter_ai::{
    conversion::{ConversionEffect, ConversionLocation, ConversionReason, ConversionReport},
    tenancy::operation::ErrorCode,
    types::{ApiProtocol, Prompt},
};

const FIELDS: [&str; 4] = [
    "previous_response_id",
    "conversation",
    "context_management",
    "cachedContent",
];

#[cfg(test)]
mod tests;

fn belongs(field: &str, protocol: &ApiProtocol) -> bool {
    // Native fields keep their source wire semantics; they do not select an
    // endpoint or prove ownership of provider-managed state.
    // https://platform.openai.com/docs/api-reference/responses/create
    // https://platform.claude.com/docs/en/api/messages
    matches!(
        (field, protocol),
        (
            "previous_response_id" | "conversation",
            ApiProtocol::Responses
        ) | ("context_management", ApiProtocol::Messages)
    )
}

pub(super) fn validate_input(prompt: Option<&Prompt>, enabled: bool) -> Result<(), ErrorCode> {
    if !enabled {
        return super::ordered_prompt(prompt);
    }
    let prompt = prompt.ok_or(ErrorCode::CapabilityMismatch)?;
    let source = prompt
        .params
        .extra_protocol
        .as_ref()
        .ok_or(ErrorCode::CapabilityMismatch)?;
    if !matches!(source, ApiProtocol::Responses | ApiProtocol::Messages)
        || FIELDS.iter().any(|field| {
            prompt
                .params
                .extra
                .get(*field)
                .is_some_and(|v| !v.is_null())
                && !belongs(field, source)
        })
        || prompt
            .params
            .supplemental_extra
            .get("cachedContent")
            .is_some_and(|v| !v.is_null())
    {
        return Err(ErrorCode::CapabilityMismatch);
    }
    Ok(())
}

fn compatible(prompt: &Prompt, target: &ApiProtocol) -> bool {
    FIELDS.iter().all(|field| {
        let source = prompt.params.extra.get(*field).filter(|v| !v.is_null());
        // A foreign protocol must not turn a source-native control into absence.
        let drops_source = source.is_some()
            && prompt
                .params
                .extra_protocol
                .as_ref()
                .is_some_and(|p| p != target);
        // Use the renderer's per-target precedence: source null masks a default
        // only on that source protocol. It cannot mask foreign supplemental data.
        let effective = extra_value_for_protocol(&prompt.params, target, field);
        !drops_source && effective.is_none_or(|v| v.is_null() || belongs(field, target))
    })
}

impl super::TenantExecution {
    pub(crate) fn provider_native_controls(&self) -> bool {
        self.submission.binding.native_controls
    }

    pub(crate) fn admit_native_controls(
        &self,
        prompt: &Prompt,
        protocol: &ApiProtocol,
    ) -> Result<(), BitrouterError> {
        if !self.provider_native_controls() {
            return Ok(());
        }
        validate_input(Some(prompt), true).map_err(|code| self.reject(code))?;
        if !compatible(prompt, protocol) {
            let mut report = ConversionReport::default();
            report.push_projection(
                protocol,
                ConversionLocation::GenerationOptions,
                ConversionReason::NativeContinuityUnrepresentable,
                ConversionEffect::TaskSemantics,
            );
            return Err(BitrouterError::Incompatible { report });
        }
        Ok(())
    }

    pub(crate) fn validate_native_controls_body(
        &self,
        prompt: &Prompt,
        protocol: &ApiProtocol,
        body: &serde_json::Value,
    ) -> Result<(), BitrouterError> {
        if !self.provider_native_controls() {
            return Ok(());
        }
        self.admit_native_controls(prompt, protocol)?;
        if !body.is_object()
            || FIELDS.iter().any(|field| {
                body.get(*field) != extra_value_for_protocol(&prompt.params, protocol, field)
            })
        {
            return Err(self.reject(ErrorCode::CapabilityMismatch));
        }
        Ok(())
    }
}
