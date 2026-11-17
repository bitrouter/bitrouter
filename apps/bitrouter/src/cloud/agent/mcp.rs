//! Opt-in local stdio bridge. Trust destinations/storage are startup settings,
//! not tool arguments supplied by a model. No tokens/proofs enter tool results.
use super::{Controller, KeyStorage, Status};
use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Connect {
    profile: Option<String>,
    name: String,
    #[serde(default)]
    renew: bool,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Selected {
    profile: String,
}
struct Bridge {
    controller: Controller,
    issuer: String,
    resource: String,
    storage: KeyStorage,
    tool_router: ToolRouter<Self>,
}
fn output(result: Result<Status>) -> Result<String, ErrorData> {
    let status = result.map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    serde_json::to_string(&status)
        .map_err(|_| ErrorData::internal_error("could not encode safe Agent status", None))
}
#[tool_router]
impl Bridge {
    #[tool(
        description = "Start or resume browser pairing for an independent BitRouter Agent. Return the link/code to the human; this tool cannot approve. No API keys or browser session are returned."
    )]
    async fn agent_connect(
        &self,
        Parameters(request): Parameters<Connect>,
    ) -> Result<String, ErrorData> {
        let profile = request
            .profile
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        output(
            self.controller
                .begin(
                    &profile,
                    &request.name,
                    &self.issuer,
                    &self.resource,
                    self.storage,
                    request.renew,
                )
                .await
                .map_err(|error| anyhow::anyhow!("Agent profile {profile}: {error}")),
        )
    }
    #[tool(
        description = "Read this local Agent profile. After browser approval, durably save credentials and confirm; rotate near-expiry credentials. Returns safe status and IDs only."
    )]
    async fn agent_status(
        &self,
        Parameters(request): Parameters<Selected>,
    ) -> Result<String, ErrorData> {
        output(self.controller.status(&request.profile).await)
    }
    #[tool(
        description = "Revoke this Agent connection and erase its local secrets after server acknowledgement. Does not revoke browser sessions, developer login or API keys."
    )]
    async fn agent_disconnect(
        &self,
        Parameters(request): Parameters<Selected>,
    ) -> Result<String, ErrorData> {
        output(self.controller.disconnect(&request.profile).await)
    }
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Local BitRouter Agent auth only. Show the pairing code to the owning human for browser approval. Credentials stay outside the project. Paid inference is not enabled by this bridge.")
    }
}
pub(super) async fn serve(issuer: String, resource: String, storage: KeyStorage) -> Result<()> {
    let issuer = super::wire::origin(&issuer)?;
    let resource = super::wire::resource(&resource)?;
    let mut controller = Controller::default()?;
    controller.authority = Some((issuer.clone(), resource.clone()));
    let bridge = Bridge {
        controller,
        issuer,
        resource,
        storage,
        tool_router: Bridge::tool_router(),
    };
    bridge
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
pub(super) fn fixture(root: std::path::PathBuf) -> impl ServerHandler {
    Bridge {
        controller: Controller {
            root,
            authority: Some((
                "https://console.example".into(),
                "https://api.example".into(),
            )),
        },
        issuer: "https://console.example".into(),
        resource: "https://api.example".into(),
        storage: KeyStorage::File,
        tool_router: Bridge::tool_router(),
    }
}
