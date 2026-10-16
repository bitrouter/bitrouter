//! Local session authorization and requests over the daemon control transport.

use std::collections::BTreeSet;
use std::path::Path;

#[cfg(not(feature = "harness"))]
use anyhow::Context;
use anyhow::Result;

use self::wire::{SessionCommand, SessionGrant, SessionRequest, SessionResponse, SessionScope};

pub mod wire;

/// Request one exact set of capabilities over the owner-scoped local
/// transport. Starting a daemon is deliberately the caller's responsibility.
pub async fn authorize(
    socket_path: &Path,
    client_id: &str,
    scopes: BTreeSet<SessionScope>,
) -> Result<SessionGrant> {
    let requested_scopes = scopes.clone();
    match send_session_request(
        socket_path,
        SessionRequest::authorize(client_id.to_string(), scopes),
    )
    .await?
    {
        SessionResponse::Authorized { grant } => {
            anyhow::ensure!(
                grant.client_id == client_id && grant.scopes == requested_scopes,
                "daemon returned a grant for a different client or scope set"
            );
            Ok(grant)
        }
        _ => Err(anyhow::anyhow!(
            "daemon returned an unexpected grant response variant"
        )),
    }
}

/// Send one authorized session command over the daemon's owner-scoped local
/// transport. This helper never adds scopes or renews a failed grant.
pub async fn request(
    socket_path: &Path,
    grant: &SessionGrant,
    command: SessionCommand,
) -> Result<SessionResponse> {
    send_session_request(socket_path, SessionRequest::authorized(grant, command)).await
}

async fn send_session_request(
    socket_path: &Path,
    request: SessionRequest,
) -> Result<SessionResponse> {
    #[cfg(not(feature = "harness"))]
    let request = crate::daemon::OpaqueSessionPayload::new(
        serde_json::to_value(request).context("encoding local session request")?,
    );
    match crate::daemon::send_command(
        socket_path,
        &crate::daemon::DaemonCommand::Sessions { request },
    )
    .await?
    {
        crate::daemon::DaemonResponse::Sessions { response } => {
            #[cfg(feature = "harness")]
            {
                Ok(response)
            }
            #[cfg(not(feature = "harness"))]
            {
                serde_json::from_value(response.into_value())
                    .context("decoding local session response")
            }
        }
        crate::daemon::DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        _ => Err(anyhow::anyhow!(
            "daemon returned an unexpected session response variant"
        )),
    }
}
