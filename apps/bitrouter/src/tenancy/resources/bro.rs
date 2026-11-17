//! Explicit local BRO construction; stock service entrypoints stay model-only.
use crate::tenancy::{Provisioning, Service};
use anyhow::Result;
use std::sync::Arc;

impl Service {
    #[cfg(all(feature = "core", feature = "harness"))]
    #[cfg(all(test, unix))]
    pub(in crate::tenancy) async fn open_owned_bro_profile(
        database: crate::db::OwnedDatabase,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
    ) -> Result<Self> {
        Self::open_owned_bro_profile_observed_startup(
            database,
            config,
            continuations,
            crate::telemetry::receipt_latency::Sink::default(),
            #[cfg(test)]
            None,
        )
        .await
    }

    pub(crate) async fn open_owned_bro_profile_observed(
        database: crate::db::OwnedDatabase,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
        receipt_observer: crate::telemetry::receipt_latency::Sink,
    ) -> Result<Self> {
        Self::open_owned_bro_profile_observed_startup(
            database,
            config,
            continuations,
            receipt_observer,
            #[cfg(test)]
            None,
        )
        .await
    }

    pub(in crate::tenancy) async fn open_owned_bro_profile_observed_startup(
        database: crate::db::OwnedDatabase,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
        receipt_observer: crate::telemetry::receipt_latency::Sink,
        #[cfg(test)] startup_test: Option<crate::tenancy::StartupTest>,
    ) -> Result<Self> {
        config.validate_lifetime()?;
        config.capacity.validate()?;
        let (db, owner, claim) = database.claim()?;
        let initial = crate::tenancy::store::initial_header_with(&db, chrono::Utc::now(), || {
            crate::tenancy::storage::exclusive_write(&owner, 16 * 1024)
        })
        .await?;
        let resources = Arc::new(super::Resources::load_key_bro_capture(
            &initial.installation,
            config,
        )?);
        let import = super::Resources::read_import(config.resource_import.as_deref()).await?;
        let service = Self::start_resources_with_continuations(
            db,
            Some(owner),
            initial,
            resources,
            Provisioning {
                import,
                continuations,
                claim: Some(claim),
                receipt_observer,
                config,
            },
            #[cfg(test)]
            startup_test,
        )
        .await?;
        if service.http_continuation.state.enabled() {
            service.http_registry.start_reconciler();
        }
        Ok(service)
    }
}
