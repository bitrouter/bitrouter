//! Read-once shared startup inputs, before any local daemon or home discovery.

use anyhow::{Context, Result, ensure};
#[cfg(feature = "multi-instance")]
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;

mod probe;

#[cfg(feature = "multi-instance")]
use super::gateway_state::{GatewayStateConfig, SharedGatewayConfig};
use crate::paths::ConfigSource;
#[cfg(feature = "multi-instance")]
use crate::telemetry::config::OtelConfig;

const MAX_SELECTED_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(feature = "multi-instance")]
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Server {
    pub listen: String,
    pub log_level: String,
    pub skip_auth: bool,
}

#[cfg(feature = "multi-instance")]
impl Default for Server {
    fn default() -> Self {
        let defaults = super::ServerConfig::default();
        Self {
            listen: defaults.listen,
            log_level: defaults.log_level,
            skip_auth: false,
        }
    }
}

#[cfg(feature = "multi-instance")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Database {
    url: String,
}

#[cfg(feature = "multi-instance")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    enabled: bool,
}

#[cfg(feature = "multi-instance")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    server: Server,
    database: Database,
    gateway_state: GatewayStateConfig,
    telemetry: Option<OtelConfig>,
    inherit_defaults: Option<bool>,
    registry: Option<Registry>,
}

/// Process configuration carries no provider, policy or credential authority.
#[cfg(feature = "multi-instance")]
#[derive(Clone)]
pub(crate) struct Process {
    pub server: Server,
    pub database_url: String,
    pub shared: SharedGatewayConfig,
    pub telemetry: Option<OtelConfig>,
}

pub(crate) struct LocalInput {
    pub config: super::Config,
    pub document: serde_json::Value,
}

pub(crate) enum Selection {
    Local(Option<Box<LocalInput>>),
    #[cfg(feature = "multi-instance")]
    Shared(Box<Process>),
}

/// Both modes retain their selected input. Local substitution is validated as
/// local before any daemon/home work, then reused without reading or expanding again.
pub(crate) async fn select(source: &ConfigSource) -> Result<Selection> {
    let ConfigSource::File(path) = source else {
        return Ok(Selection::Local(None));
    };
    let file = tokio::fs::File::open(path)
        .await
        .context("opening selected gateway configuration")?;
    let text = read_selected(file).await?;
    parse_lookups(&text, |key| std::env::var(key).ok(), super::env_lookup)
}

async fn read_selected(reader: impl tokio::io::AsyncRead + Unpin) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_SELECTED_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .context("reading selected gateway configuration")?;
    ensure!(
        bytes.len() as u64 <= MAX_SELECTED_BYTES,
        "selected gateway configuration exceeds 16 MiB"
    );
    String::from_utf8(bytes).context("gateway configuration is not UTF-8")
}

#[cfg(test)]
fn parse_with(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Selection> {
    parse_lookups(text, &lookup, &lookup)
}

fn parse_lookups(
    text: &str,
    shared_lookup: impl Fn(&str) -> Option<String>,
    local_lookup: impl Fn(&str) -> Option<String>,
) -> Result<Selection> {
    // Existing local YAML substitutes before parsing, including unquoted flow
    // values such as {listen: ${LISTEN}}. A structural probe can mask those
    // tokens without consulting their environment or changing the local input.
    let references = std::cell::RefCell::new(std::collections::HashMap::new());
    let prefix = format!("BRPROBE_{}_", uuid::Uuid::new_v4().simple());
    let probe = super::substitute_with(text, |name| {
        let mut references = references.borrow_mut();
        let token = format!("{prefix}{}", references.len());
        Some(references.entry(name.to_owned()).or_insert(token).clone())
    })
    .map_err(|_| anyhow::anyhow!("invalid selected gateway environment reference"))?;
    // Stable tokens preserve YAML anchors and aliases. A missing variable with
    // different fallbacks can produce distinct real keys but identical probe
    // keys. Only synthetic probe-key collisions are tolerated; literal duplicate
    // selectors must fail before looking up any real environment values.
    let probe = probe::parse(&probe, &prefix)
        .map_err(|_| anyhow::anyhow!("invalid selected gateway configuration"))?;
    ensure!(
        probe.is_object(),
        "gateway mode must be selected by a YAML mapping"
    );
    ensure!(
        probe
            .as_object()
            .is_none_or(|fields| fields.keys().all(|key| !references
                .borrow()
                .values()
                .any(|token| key.contains(token)))),
        "gateway process field names cannot be environment references"
    );
    if let Some(state) = probe.get("gateway_state") {
        ensure!(
            state.is_object() && state.get("mode").is_some_and(serde_json::Value::is_string),
            "gateway_state must explicitly select a literal mode"
        );
    }
    match probe
        .pointer("/gateway_state/mode")
        .and_then(|mode| mode.as_str())
    {
        None | Some("local") => {
            return local_input(text, local_lookup)
                .map(|input| Selection::Local(Some(Box::new(input))));
        }
        Some("shared") => {}
        Some(_) => anyhow::bail!("gateway_state.mode must be the literal local or shared"),
    }
    #[cfg(not(feature = "multi-instance"))]
    {
        let _ = shared_lookup;
        anyhow::bail!("shared gateway requires the multi-instance feature");
    }
    #[cfg(feature = "multi-instance")]
    parse_shared(text, shared_lookup).map(|process| Selection::Shared(Box::new(process)))
}

fn local_input(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<LocalInput> {
    let resolved = super::substitute_with(text, lookup)?;
    ensure!(
        resolved.len() as u64 <= MAX_SELECTED_BYTES,
        "expanded local configuration exceeds 16 MiB"
    );
    let document: serde_json::Value = serde_saphyr::from_str(&resolved)
        .map_err(|_| anyhow::anyhow!("invalid expanded local configuration"))?;
    let state: super::gateway_state::GatewayStateConfig = document
        .get("gateway_state")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid expanded gateway mode"))?
        .unwrap_or_default();
    ensure!(
        matches!(state, super::gateway_state::GatewayStateConfig::Local {}),
        "environment substitution cannot select shared gateway mode"
    );
    Ok(LocalInput {
        config: super::parse_resolved(&resolved)?,
        document,
    })
}

#[cfg(feature = "multi-instance")]
fn parse_shared(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Process> {
    let mut value: serde_json::Value = serde_saphyr::from_str(text).map_err(|_| {
        anyhow::anyhow!("shared environment references must be quoted YAML strings")
    })?;
    // Reject foreign authority and unsupported fields before resolving even a
    // single environment reference. Numeric bounds must be literal values.
    let raw: Document = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("shared configuration accepts only process inputs"))?;
    ensure!(
        raw.inherit_defaults != Some(true) && raw.registry.is_none_or(|registry| !registry.enabled),
        "shared configuration cannot inherit defaults or enable registry discovery"
    );
    resolve_strings(&mut value, &lookup)?;
    let document: Document = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid shared process configuration"))?;
    document
        .gateway_state
        .validate(&document.database.url, document.server.skip_auth)?;
    let GatewayStateConfig::Shared(shared) = document.gateway_state else {
        anyhow::bail!("shared process selection changed mode");
    };
    ensure!(
        !document.server.listen.is_empty() && document.server.listen.len() <= 1024,
        "invalid shared listener address"
    );
    Ok(Process {
        server: document.server,
        database_url: document.database.url,
        shared,
        telemetry: document
            .telemetry
            .map(|config| config.with_env_from(lookup)),
    })
}

#[cfg(feature = "multi-instance")]
fn resolve_strings(
    value: &mut serde_json::Value,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<()> {
    match value {
        serde_json::Value::String(text) => {
            *text = resolve_scalar(text, lookup)?;
        }
        serde_json::Value::Array(values) => {
            for value in values {
                resolve_strings(value, lookup)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                resolve_strings(value, lookup)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(feature = "multi-instance")]
fn resolve_scalar(text: &str, lookup: &impl Fn(&str) -> Option<String>) -> Result<String> {
    // This is already a decoded YAML value: '#' is literal content, not a
    // comment, and inserted newlines/quotes must never be parsed as YAML again.
    let mut output = String::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let reference = &remaining[start + 2..];
        let end = reference
            .find('}')
            .context("unterminated shared environment reference")?;
        let token = &reference[..end];
        let (name, fallback) = token
            .split_once(":-")
            .map_or((token, None), |(name, value)| (name, Some(value)));
        let value = lookup(name)
            .or_else(|| fallback.map(str::to_owned))
            .context("shared process environment reference is unavailable")?;
        ensure!(
            (output.len() as u64).saturating_add(value.len() as u64) <= MAX_SELECTED_BYTES,
            "expanded shared value exceeds 16 MiB"
        );
        output.push_str(&value);
        remaining = &reference[end + 1..];
    }
    ensure!(
        (output.len() as u64).saturating_add(remaining.len() as u64) <= MAX_SELECTED_BYTES,
        "expanded shared value exceeds 16 MiB"
    );
    output.push_str(remaining);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "multi-instance")]
    const BASE: &str = "database:\n  url: '${DATABASE_URL}'\ngateway_state:\n  mode: shared\n  namespace: process-fixture\n  deployment_key_env: DEPLOYMENT_KEY\n";

    #[cfg(feature = "multi-instance")]
    #[test]
    fn rejects_local_authority_before_environment_lookup() -> Result<()> {
        for extra in [
            "providers: {}",
            "policy: {path: '${PRIVATE_POLICY}'}",
            "plugins: {}",
            "control: {}",
            "server: {control_socket: '${PRIVATE_SOCKET}'}",
            "inherit_defaults: true",
            "registry: {enabled: true}",
        ] {
            let looked_up = std::cell::Cell::new(false);
            assert!(
                parse_with(&format!("{BASE}{extra}\n"), |_| {
                    looked_up.set(true);
                    Some("unexpected".into())
                })
                .is_err()
            );
            assert!(
                !looked_up.get(),
                "resolved environment before rejecting {extra}"
            );
        }
        assert!(matches!(parse_with("{}", |_| None)?, Selection::Local(_)));
        Ok(())
    }

    #[cfg(feature = "multi-instance")]
    #[test]
    fn environment_values_cannot_inject_authority_fields() -> Result<()> {
        let Selection::Shared(process) =
            parse_with(BASE, |_| Some("postgres://localhost/fixture".into()))?
        else {
            anyhow::bail!("shared selection missing");
        };
        assert_eq!(process.database_url, "postgres://localhost/fixture");
        assert!(process.telemetry.is_none());
        assert!(parse_with(BASE, |_| Some("sqlite::memory:\nproviders: {}".into())).is_err());
        assert!(
            parse_with(&format!("{BASE}server: {{skip_auth: true}}\n"), |_| Some(
                "postgres://localhost/fixture".into()
            ))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn local_flow_interpolation_keeps_the_existing_parser_contract() -> Result<()> {
        let text = "server: {listen: ${LISTEN}}";
        assert!(matches!(
            parse_with(text, |_| Some("127.0.0.1:4356".into()))?,
            Selection::Local(_)
        ));
        let local = crate::config::parse_with(text, |_| Some("127.0.0.1:4356".into()))?;
        assert_eq!(local.server.listen, "127.0.0.1:4356");
        for text in [
            "${DOCUMENT}",
            "gateway_state: ${STATE}",
            "gateway_state: {mode: ${MODE}}",
        ] {
            assert!(parse_with(text, |_| None).is_err());
        }
        Ok(())
    }

    #[cfg(feature = "multi-instance")]
    #[tokio::test]
    async fn retains_the_read_document_without_creating_home_state() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("shared.yaml");
        let text = BASE.replace("${DATABASE_URL}", "postgres://localhost/first");
        tokio::fs::write(&path, &text).await?;
        let cwd = std::env::current_dir()?;
        let Selection::Shared(selected) = select(&ConfigSource::File(path.clone())).await? else {
            anyhow::bail!("shared selection missing");
        };
        tokio::fs::write(&path, text.replace("/first", "/second")).await?;
        assert_eq!(selected.database_url, "postgres://localhost/first");
        assert_eq!(std::env::current_dir()?, cwd);
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn expanded_local_values_cannot_inject_shared_mode() -> Result<()> {
        let text =
            "database: {url: 'postgres://localhost/fixture'}\nserver:\n  listen: ${LISTEN}\n";
        let injected = "127.0.0.1:4356\ngateway_state:\n  mode: shared\n  namespace: injected\n  deployment_key_env: INJECTED_KEY\n";
        let expanded = super::super::substitute_with(text, |_| Some(injected.into()))?;
        super::super::parse_resolved(&expanded)?;
        let error = parse_with(text, |_| Some(injected.into()))
            .err()
            .context("injected shared mode was accepted")?;
        assert_eq!(
            error.to_string(),
            "environment substitution cannot select shared gateway mode"
        );
        Ok(())
    }

    #[test]
    fn distinct_local_provider_references_keep_distinct_probe_keys() -> Result<()> {
        let text = "providers:\n  ${FIRST}: {api_base: 'https://one.example/v1', api_key: first}\n  ${SECOND}: {api_base: 'https://two.example/v1', api_key: second}\n";
        let Selection::Local(Some(local)) = parse_with(text, |key| match key {
            "FIRST" => Some("first".into()),
            "SECOND" => Some("second".into()),
            _ => None,
        })?
        else {
            anyhow::bail!("local selection missing");
        };
        assert!(
            local.config.providers.contains_key("first")
                && local.config.providers.contains_key("second")
        );
        Ok(())
    }

    #[test]
    fn local_probe_preserves_aliases_and_fallback_mapping_keys() -> Result<()> {
        let text = "providers:\n  first: &provider_${ANCHOR} {api_base: 'https://one.example/v1', api_key: first}\n  second: *provider_${ANCHOR}\n";
        assert!(matches!(
            parse_with(text, |_| Some("credentials".into()))?,
            Selection::Local(_)
        ));
        let text = "providers:\n  ${MISSING:-first}: {api_base: 'https://one.example/v1', api_key: first}\n  ${MISSING:-second}: {api_base: 'https://two.example/v1', api_key: second}\n";
        assert!(matches!(parse_with(text, |_| None)?, Selection::Local(_)));
        // Probe collision tolerance must not weaken real duplicate validation.
        assert!(parse_with(text, |_| Some("same".into())).is_err());
        for text in [
            "gateway_state: {mode: shared}\ngateway_state: {mode: local}\nserver: {listen: '${PRIVATE}'}",
            "gateway_state: {mode: shared, mode: local}\nserver: {listen: '${PRIVATE}'}",
            "gateway_state:\n  <<: {mode: shared, mode: local}\nserver: {listen: '${PRIVATE}'}",
            "template: &state {mode: shared, mode: local}\ngateway_state: {<<: *state}\nserver: {listen: '${PRIVATE}'}",
        ] {
            let called = std::cell::Cell::new(false);
            assert!(
                parse_with(text, |_| {
                    called.set(true);
                    Some("private".into())
                })
                .is_err()
            );
            assert!(!called.get());
        }
        Ok(())
    }

    #[test]
    fn local_probe_preserves_legal_merge_sequences_and_repeated_merges() -> Result<()> {
        let text = "providers:\n  first:\n    <<: {api_base: 'https://one.example/v1', api_key: first}\n    <<: {models: []}\n  second:\n    <<: [{api_base: 'https://two.example/v1'}, {api_key: second}]\n";
        super::super::parse_with(text, |_| None)?;
        let Selection::Local(Some(input)) = parse_with(text, |_| None)? else {
            anyhow::bail!("local merged input missing");
        };
        assert_eq!(input.config.providers["first"].api_key, "first");
        assert_eq!(input.config.providers["second"].api_key, "second");
        Ok(())
    }

    #[tokio::test]
    async fn local_baseline_uses_the_validated_input_after_file_replacement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("local.yaml");
        tokio::fs::write(&path, "server: {listen: '127.0.0.1:4567'}\n").await?;
        let source = ConfigSource::File(path.clone());
        let selected = select(&source).await?;
        #[cfg(not(feature = "multi-instance"))]
        let Selection::Local(local) = selected;
        #[cfg(feature = "multi-instance")]
        let Selection::Local(local) = selected else {
            anyhow::bail!("local input missing");
        };
        tokio::fs::write(path, "gateway_state: {mode: shared}\n").await?;
        let baseline = crate::reload::load_selected_configuration_baseline(&source, local).await?;
        assert_eq!(baseline.config().server.listen, "127.0.0.1:4567");
        assert!(matches!(
            baseline.config().gateway_state,
            super::super::gateway_state::GatewayStateConfig::Local {}
        ));
        Ok(())
    }

    #[cfg(feature = "multi-instance")]
    #[test]
    fn shared_decoded_scalars_preserve_hashes_and_substitute_once() -> Result<()> {
        let value = resolve_scalar("prefix # ${TOKEN} ${FALLBACK:-end}", &|key| {
            (key == "TOKEN").then(|| "literal\n${UNRESOLVED}".into())
        })?;
        assert_eq!(value, "prefix # literal\n${UNRESOLVED} end");
        Ok(())
    }

    #[tokio::test]
    async fn selected_reader_stops_at_one_detection_byte_before_parsing() -> Result<()> {
        use std::pin::Pin;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::task::{Context, Poll};
        use tokio::io::{AsyncRead, ReadBuf};
        struct Counted {
            inner: tokio::io::Repeat,
            count: Arc<AtomicUsize>,
        }
        impl AsyncRead for Counted {
            fn poll_read(
                mut self: Pin<&mut Self>,
                context: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                let before = buffer.filled().len();
                let result = Pin::new(&mut self.inner).poll_read(context, buffer);
                self.count
                    .fetch_add(buffer.filled().len() - before, Ordering::SeqCst);
                result
            }
        }
        let exact = read_selected(tokio::io::repeat(b' ').take(MAX_SELECTED_BYTES)).await?;
        assert_eq!(exact.len() as u64, MAX_SELECTED_BYTES);
        let count = Arc::new(AtomicUsize::new(0));
        let error = read_selected(Counted {
            inner: tokio::io::repeat(b'!'),
            count: count.clone(),
        })
        .await
        .err()
        .context("unbounded input unexpectedly succeeded")?;
        assert_eq!(
            error.to_string(),
            "selected gateway configuration exceeds 16 MiB"
        );
        assert_eq!(count.load(Ordering::SeqCst) as u64, MAX_SELECTED_BYTES + 1);
        Ok(())
    }

    #[cfg(not(feature = "multi-instance"))]
    #[test]
    fn unavailable_mode_fails_before_environment_lookup() {
        let called = std::cell::Cell::new(false);
        let result = parse_with(
            "gateway_state: {mode: shared}\ndatabase: {url: '${PRIVATE_URL}'}",
            |_| {
                called.set(true);
                None
            },
        );
        assert!(result.is_err());
        assert!(!called.get());
    }
}
