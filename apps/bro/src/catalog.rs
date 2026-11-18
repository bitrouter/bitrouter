//! Package-local ACP invocations and the verified endpoint contracts of their adapters.
use agent_client_protocol_schema::v1::{LlmProtocol, SetProviderRequest};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use tokio::process::Command;

pub(crate) struct Harness {
    pub id: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub adapter: Option<Adapter>,
}

#[derive(Clone, Copy)]
pub(crate) enum Adapter {
    Claude,
    Codex,
}

include!(concat!(env!("OUT_DIR"), "/catalog_generated.rs"));

pub(crate) fn find(name: &str) -> Result<&'static Harness> {
    let id = match name {
        "claude" | "claude-code" => "claude-acp",
        "codex" => "codex-acp",
        other => other,
    };
    CATALOG
        .iter()
        .find(|h| h.id == id)
        .context("unknown ACP adapter (see bro acp list)")
}

/// One plan drives both the documented process-environment fallback and ACP providers/set.
/// The token never enters argv or Debug output.
pub(crate) struct Endpoint<'a> {
    adapter: Adapter,
    provider_id: &'static str,
    protocol: LlmProtocol,
    base_url: String,
    token: &'a str,
    headers: HashMap<&'static str, String>,
}

impl<'a> Endpoint<'a> {
    pub(crate) fn new(harness: &Harness, endpoint: &str, token: &'a str) -> Result<Self> {
        let adapter = harness
            .adapter
            .context("adapter has no verified gateway-bound headless launch")?;
        ensure!(
            !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
            "invalid gateway token"
        );
        let url =
            url::Url::parse(endpoint).map_err(|_| anyhow::anyhow!("invalid gateway endpoint"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "expected an HTTP(S) gateway endpoint without credentials, query or fragment"
        );
        let path = url.path().trim_end_matches('/');
        let prefix = path.strip_suffix("/v1").unwrap_or(path);
        let root = format!("{}{prefix}", &url[..url::Position::BeforePath]);
        let (provider_id, protocol, base_url) = match adapter {
            Adapter::Claude => ("main", LlmProtocol::Anthropic, root),
            Adapter::Codex => ("openai", LlmProtocol::OpenAi, format!("{root}/v1")),
        };
        Ok(Self {
            adapter,
            provider_id,
            protocol,
            base_url,
            token,
            headers: HashMap::from([
                ("x-bitrouter-harness", harness.id.to_owned()),
                (
                    "x-bitrouter-controller-id",
                    format!("brc_{}", uuid::Uuid::new_v4().simple()),
                ),
            ]),
        })
    }

    pub(crate) fn set_provider(&self) -> SetProviderRequest {
        let mut headers: HashMap<_, _> = self
            .headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        headers.insert("authorization".into(), format!("Bearer {}", self.token));
        SetProviderRequest::new(self.provider_id, self.protocol.clone(), &self.base_url)
            .headers(headers)
    }

    pub(crate) fn matches(
        &self,
        providers: &agent_client_protocol_schema::v1::ListProvidersResponse,
    ) -> bool {
        providers.providers.iter().any(|p| {
            p.provider_id.0.as_ref() == self.provider_id
                && p.current
                    .as_ref()
                    .is_some_and(|c| c.api_type == self.protocol && c.base_url == self.base_url)
        })
    }

    pub(crate) fn apply(&self, cmd: &mut Command, model: Option<&str>) {
        match self.adapter {
            Adapter::Claude => {
                let mut headers: Vec<_> = self
                    .headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect();
                headers.sort_unstable();
                cmd.env("ANTHROPIC_BASE_URL", &self.base_url)
                    .env("ANTHROPIC_AUTH_TOKEN", self.token)
                    .env("ANTHROPIC_CUSTOM_HEADERS", headers.join("\n"));
                if let Some(model) = model {
                    cmd.env("ANTHROPIC_MODEL", model);
                }
            }
            Adapter::Codex => {
                let mut config = serde_json::json!({
                    "model_provider":"bitrouter",
                    "model_providers":{"bitrouter":{"name":"BitRouter","base_url":self.base_url,
                        "wire_api":"responses","env_key":"BITROUTER_API_KEY","http_headers":self.headers}}
                });
                if let Some(model) = model {
                    config["model"] = model.into();
                }
                cmd.env("BITROUTER_API_KEY", self.token)
                    .env("CODEX_CONFIG", config.to_string())
                    .env("MODEL_PROVIDER", "bitrouter");
            }
        }
    }
}
