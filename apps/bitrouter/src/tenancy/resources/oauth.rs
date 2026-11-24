//! Same-slot seed lineage and live encrypted OAuth bootstrap under H custody.
use super::{
    Resources,
    manifest::{Artifact, Definition, Import},
};
use crate::gateway::providers::auth::credentials::Credential;
use crate::tenancy::oauth::{
    crypto::Crypto,
    storage::{self, Metadata, Row, State},
};
use anyhow::{Context, Result, ensure};
use sea_orm::ConnectionTrait;

impl Resources {
    pub(in crate::tenancy) fn oauth_crypto(&self) -> Result<Crypto> {
        Ok(Crypto::new(
            self.installation.clone(),
            self.key
                .clone()
                .context("owned OAuth wrapping key unavailable")?,
        ))
    }
    pub(super) async fn authenticate_oauth(
        &self,
        db: &impl ConnectionTrait,
        artifact: &Artifact,
    ) -> Result<()> {
        if let Definition::Credential {
            credential: Credential::Oauth(seed),
            oauth: Some(profile),
            oauth_generation: 0,
        } = &artifact.definition
        {
            let crypto = self.oauth_crypto()?;
            let row = storage::load(db, &self.installation, &artifact.reference)
                .await?
                .context("retained OAuth slot missing")?;
            ensure!(
                row.metadata.profile == *profile
                    && row.metadata.seed == crypto.seed(&artifact.reference, profile, seed)?,
                "OAuth immutable lineage mismatch"
            );
            row.token(&crypto)?;
        }
        Ok(())
    }

    /// Authenticate exactly the selected live slot and its immutable seed lineage.
    /// This never restores the seed or changes a started/quarantined generation.
    pub(in crate::tenancy) async fn authenticate_oauth_slot(
        &self,
        db: &impl ConnectionTrait,
        row: &Row,
    ) -> Result<()> {
        let crypto = self
            .oauth_crypto()
            .context(crate::tenancy::store::directory::InvalidRecord)?;
        row.token(&crypto)
            .context(crate::tenancy::store::directory::InvalidRecord)?;
        let artifact = self
            .load(db, &row.metadata.reference)
            .await
            .map_err(|code| {
                let error = anyhow::anyhow!("OAuth retained resource unavailable");
                if code == bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable {
                    error
                } else {
                    error.context(crate::tenancy::store::directory::InvalidRecord)
                }
            })?;
        let validated = || -> Result<()> {
            let Definition::Credential {
                credential: Credential::Oauth(seed),
                oauth: Some(profile),
                oauth_generation: 0,
            } = artifact.definition
            else {
                anyhow::bail!("OAuth slot resource kind mismatch");
            };
            ensure!(
                row.metadata.installation == self.installation
                    && artifact.reference == row.metadata.reference
                    && row.metadata.profile == profile
                    && row.metadata.seed
                        == crypto.seed(&row.metadata.reference, &profile, &seed)?,
                "OAuth slot lineage mismatch"
            );
            Ok(())
        };
        validated().context(crate::tenancy::store::directory::InvalidRecord)
    }
    pub(super) async fn preflight_oauth(
        &self,
        db: &impl ConnectionTrait,
        imports: &[Import],
    ) -> Result<()> {
        for import in imports {
            if let Definition::Credential { oauth: Some(_), .. } = &import.definition {
                let existing = storage::lookup(db, &self.installation, &import.reference).await?;
                if let Some(existing) = existing {
                    ensure!(
                        existing.metadata.reference == import.reference,
                        "owned OAuth authority replacement is not supported"
                    );
                } else if crate::tenancy::configuration_store::resource(db, &import.reference)
                    .await?
                    .is_some()
                {
                    anyhow::bail!("retained OAuth slot missing; seed cannot restore live state");
                }
            }
        }
        Ok(())
    }
    pub(super) async fn retain_oauth(
        &self,
        db: &impl ConnectionTrait,
        import: &Import,
    ) -> Result<()> {
        let Definition::Credential {
            credential: Credential::Oauth(seed),
            oauth: Some(profile),
            oauth_generation: 0,
        } = &import.definition
        else {
            return Ok(());
        };
        let crypto = self.oauth_crypto()?;
        let seed_digest = crypto.seed(&import.reference, profile, seed)?;
        if let Some(row) = storage::load(db, &self.installation, &import.reference).await? {
            ensure!(
                row.metadata.profile == *profile && row.metadata.seed == seed_digest,
                "OAuth immutable seed conflict"
            );
            row.token(&crypto)?;
            return Ok(()); // Preserve generation, nonce, started/quarantine state.
        }
        let metadata = Metadata {
            format: 1,
            installation: self.installation.clone(),
            reference: import.reference.clone(),
            slot: storage::slot(&self.installation, &import.reference)?,
            profile: profile.clone(),
            seed: seed_digest,
            generation: 0,
            state: State::Ready,
        };
        storage::insert(db, &Row::new(&crypto, metadata, seed)?).await
    }
    /// Authenticate all slots and their exact retained resource; no seed restore.
    pub(in crate::tenancy) async fn audit_oauth_in(
        &self,
        db: &impl ConnectionTrait,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let mut after = None;
        loop {
            check()?;
            let page = storage::page(db, &self.installation, after.as_deref()).await?;
            if page.is_empty() {
                break;
            }
            for row in page {
                check()?;
                self.authenticate_oauth_slot(db, &row).await?;
                after = Some(row.metadata.slot);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::tenancy) async fn validate_oauth(&self, db: &impl ConnectionTrait) -> Result<()> {
        self.audit_oauth_in(db, &|| Ok(())).await
    }
}
