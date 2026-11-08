//! Explicit discovery for provider-native data on ordered submissions.

use bitrouter_ai::tenancy::native_controls::OrderedNativeControlsCapabilitiesV1;

use super::TenancyClient;
use crate::error::Result;

impl TenancyClient {
    /// Discover the operator's explicit provider-native control policy.
    ///
    /// Provider references grant no historical ownership. Shared provider
    /// credentials may expose a shared state namespace across principals.
    ///
    /// # Errors
    /// Returns authentication, unsupported endpoint, bounds or contract errors.
    /// No inference request, fallback discovery or automatic retry is performed.
    pub async fn ordered_native_controls_capabilities(
        &self,
    ) -> Result<OrderedNativeControlsCapabilitiesV1> {
        self.get("tenancy/ordered/native-controls/capabilities", None)
            .await
    }
}
