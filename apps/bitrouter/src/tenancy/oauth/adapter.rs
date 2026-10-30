//! Bound existing OAuthSession/Claude Code consumer; no alternate account lookup.
use super::{
    ModelStamp,
    exchange::{Exchange, Operation},
    storage::State,
};
use crate::gateway::{
    model::error::ModelError,
    providers::{
        auth::{
            credentials::Credential,
            store::{
                CredentialKey, CredentialStore, CredentialTransaction, Durability, OAuthSession,
                StoreError,
            },
        },
        claude_code::ClaudeCodeAuthApplier,
    },
};
use async_trait::async_trait;
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct BoundOAuthStore {
    exchange: Arc<Exchange>,
    used: AtomicBool,
}
impl BoundOAuthStore {
    pub(in crate::tenancy) fn new(exchange: Arc<Exchange>) -> Arc<Self> {
        Arc::new(Self {
            exchange,
            used: AtomicBool::new(false),
        })
    }
    pub(crate) fn applier(self: &Arc<Self>) -> ClaudeCodeAuthApplier {
        ClaudeCodeAuthApplier::new(OAuthSession::new_inline(
            self.clone(),
            Arc::new(super::refresh::Refresher::new(self.exchange.clone())),
            crate::gateway::providers::auth::oauth::REFRESH_WINDOW,
        ))
    }
    pub(crate) fn stamp(&self) -> Result<ModelStamp, ErrorCode> {
        let row = self.exchange.current()?;
        if !matches!(row.metadata.state, State::Ready) {
            return Err(ErrorCode::Forbidden);
        }
        self.exchange.execution.revalidate()?;
        Ok(ModelStamp {
            metadata: row.metadata.clone(),
            execution: self.exchange.execution.id().clone(),
        })
    }
    fn fail(&self, code: ErrorCode) -> StoreError {
        self.exchange.execution.reject(code);
        StoreError::Unavailable
    }
}
struct Transaction {
    exchange: Arc<Exchange>,
    credential: Credential,
    pending: Option<Credential>,
    stage_error: bool,
}
#[async_trait]
impl CredentialStore for BoundOAuthStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> Result<Box<dyn CredentialTransaction>, StoreError> {
        let metadata = &self.exchange.original.metadata;
        if key.provider != metadata.profile.provider
            || key.account != metadata.profile.account
            || self.used.swap(true, Ordering::AcqRel)
        {
            return Err(self.fail(ErrorCode::Forbidden));
        }
        self.exchange
            .execution
            .revalidate()
            .map_err(|code| self.fail(code))?;
        let token = self
            .exchange
            .original
            .token(
                &self
                    .exchange
                    .execution
                    .oauth_crypto()
                    .map_err(|code| self.fail(code))?,
            )
            .map_err(|_| self.fail(ErrorCode::AuthorityUnavailable))?;
        Ok(Box::new(Transaction {
            exchange: self.exchange.clone(),
            credential: Credential::Oauth(token),
            pending: None,
            stage_error: false,
        }))
    }
}
#[async_trait]
impl CredentialTransaction for Transaction {
    fn credential(&self) -> Option<&Credential> {
        Some(&self.credential)
    }
    fn pending(&self) -> Option<&Credential> {
        self.pending.as_ref()
    }
    fn stage(&mut self, credential: Credential) {
        let valid = match &credential {
            Credential::Oauth(token) => self.exchange.stage(token.clone()),
            _ => Err(ErrorCode::CapabilityMismatch),
        };
        self.stage_error = valid.is_err();
        if let Err(code) = valid {
            self.exchange.execution.reject(code);
        }
        self.pending = Some(credential);
    }
    async fn prepare_refresh(&mut self) -> Result<(), StoreError> {
        self.exchange.reserve().map_err(|code| {
            self.exchange.execution.reject(code);
            StoreError::Unavailable
        })?;
        self.exchange
            .submit(Operation::Start)
            .await
            .map_err(|code| {
                self.exchange.execution.reject(code);
                StoreError::Unavailable
            })
    }
    async fn commit(&mut self) -> Result<Durability, StoreError> {
        if self.stage_error {
            return Err(StoreError::Unavailable);
        }
        self.exchange
            .submit(Operation::Commit)
            .await
            .map_err(|code| {
                self.exchange.execution.reject(code);
                StoreError::Unavailable
            })?;
        if let Some(credential) = self.pending.take() {
            self.credential = credential;
        }
        Ok(Durability::Persistent)
    }
    async fn release(&mut self) -> Result<(), StoreError> {
        let row = self
            .exchange
            .current()
            .map_err(|_| StoreError::Unavailable)?;
        if matches!(row.metadata.state, State::RefreshStarted(_)) {
            self.exchange
                .submit(Operation::Release)
                .await
                .map_err(|code| {
                    self.exchange.execution.reject(code);
                    StoreError::Unavailable
                })?;
        }
        Ok(())
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        self.exchange.abandoned();
    }
}

pub(in crate::tenancy) fn model_error(exchange: &Exchange, code: ErrorCode) -> ModelError {
    exchange.execution.reject(code);
    ModelError::CredentialStorage {
        failure: StoreError::Unavailable,
    }
}
