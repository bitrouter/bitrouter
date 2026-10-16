//! Service-owned admission policy over credential-free provider evidence.

use bitrouter_ai::core::evidence::{ModelTokenLimits, NativeProtocolValidation, NativeRoute};

/// A provider that cannot accept a request-level output cap may still fit
/// a reservation covering its entire configured model output ceiling.
/// Missing or zero metadata never establishes that bound.
pub(crate) fn output_reservation_covers_model(limits: &ModelTokenLimits, reservation: u32) -> bool {
    limits
        .max_output_tokens
        .is_some_and(|ceiling| ceiling > 0 && ceiling <= u64::from(reservation))
}

/// A configured counter is required only for a locally viable candidate.
/// Rejected candidates must carry no count intent or outcome.
pub(crate) fn requires_input_count(route: &NativeRoute) -> bool {
    route.constraints.input_token_counting.is_some()
        && !matches!(
            route.protocol_validation,
            NativeProtocolValidation::Rejected { .. }
        )
}

#[cfg(test)]
mod tests {
    use super::requires_input_count;
    use bitrouter_ai::core::evidence::{
        InputTokenCounting, NativeProtocolValidation, NativeRoute, NativeRouteConstraints,
    };
    use bitrouter_ai::types::ApiProtocol;

    #[test]
    fn counting_requires_configuration_but_not_verified_protocol_compatibility() {
        let cases = [
            (NativeProtocolValidation::Unverified, true),
            (NativeProtocolValidation::Compatible, true),
            (
                NativeProtocolValidation::Rejected {
                    reason: "unsupported_request".into(),
                },
                false,
            ),
        ];
        for (protocol_validation, configured_requires_count) in cases {
            let mut route = NativeRoute {
                provider: "fixture-provider".into(),
                model: "fixture-model".into(),
                protocol: ApiProtocol::Responses,
                constraints: NativeRouteConstraints::default(),
                output_token_limit_supported: None,
                input_count: None,
                protocol_validation,
                continuation: Default::default(),
            };
            assert!(!requires_input_count(&route));
            route.constraints.input_token_counting = Some(InputTokenCounting::Responses);
            assert_eq!(
                requires_input_count(&route),
                configured_requires_count,
                "{:?}",
                route.protocol_validation
            );
        }
    }
}
