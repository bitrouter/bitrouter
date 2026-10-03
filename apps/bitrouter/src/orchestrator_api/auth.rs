//! The managed control surface always requires a real virtual-key principal.
//! Ordinary inference's skip_auth setting does not authenticate a harness.

use bitrouter_sdk::caller::CallerContext;
use chrono::Utc;
use http::{HeaderMap, HeaderValue};
use sea_orm::DatabaseConnection;

use super::{ApiError, CoreError, ErrorCode};
use crate::auth::{db, hook::credential_from_headers, keys};

#[derive(Clone)]
pub(super) struct Principal {
    pub caller: CallerContext,
    headers: HeaderMap,
    db: DatabaseConnection,
}

impl Principal {
    pub async fn authenticate(
        db: &DatabaseConnection,
        headers: &HeaderMap,
    ) -> Result<Self, ApiError> {
        let credential = credential_from_headers(headers)
            .filter(|value| keys::looks_like_virtual_key(value))
            .ok_or_else(ApiError::unauthorized)?;
        let record = db::find_key_by_hash(db, &keys::hash_key(&credential))
            .await
            .map_err(|_| ApiError::unavailable())?
            .ok_or_else(ApiError::unauthorized)?;
        if !record.active || record.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
            return Err(ApiError::unauthorized());
        }
        // Retain only authentication, not arbitrary ingress headers. Never
        // serialize credentials into a session snapshot or return them to peers.
        let mut headers = HeaderMap::new();
        let mut value = HeaderValue::from_str(&format!("Bearer {credential}"))
            .map_err(|_| ApiError::unauthorized())?;
        value.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, value);
        Ok(Self {
            caller: CallerContext::new(record.id, record.user_id),
            headers,
            db: db.clone(),
        })
    }

    pub fn scope(&self) -> (String, String) {
        (
            self.caller.user_id().into(),
            self.caller.api_key_id().into(),
        )
    }

    pub fn headers(&self) -> HeaderMap {
        self.headers.clone()
    }

    pub async fn revalidate(&self) -> Result<(), CoreError> {
        let current = Self::authenticate(&self.db, &self.headers)
            .await
            .map_err(|error| error.0)?;
        if current.scope() != self.scope() {
            return Err(CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "managed principal changed",
            ));
        }
        Ok(())
    }
}
