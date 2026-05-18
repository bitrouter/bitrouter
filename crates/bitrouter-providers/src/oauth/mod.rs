//! OAuth 2.0 Device Authorization Grant (RFC 8628) + on-disk token store.
//!
//! Standards reference: <https://www.rfc-editor.org/rfc/rfc8628>.
//!
//! The device-code flow is generic — it's keyed only by `(device_authorization_endpoint, token_endpoint, client_id, scope)`.
//! Provider-specific extras (the GitHub Copilot internal-token exchange,
//! Anthropic Claude code OAuth's PKCE, etc.) layer on top in their own
//! modules.

pub mod device_code;
pub mod token_store;

pub use device_code::{DeviceCodeFlow, DeviceCodeParams, DeviceCodeResponse, FlowError, FlowEvent};
pub use token_store::{OAuthToken, TokenStore, TokenStoreError};
