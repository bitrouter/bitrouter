//! Data-layer services.
//!
//! These are plain async methods on a [`DatabaseConnection`](sea_orm::DatabaseConnection).
//! They perform CRUD operations and know nothing about HTTP or authentication.

pub mod account;
pub mod revocation;
pub mod session;
pub mod virtual_key;

pub use account::AccountService;
pub use revocation::DbRevocationSet;
pub use session::SessionService;
pub use virtual_key::{
    CreateVirtualKeyRequest, CreateVirtualKeyResponse, VIRTUAL_KEY_PREFIX, VirtualKeyService,
    hash_virtual_key, is_virtual_key,
};
