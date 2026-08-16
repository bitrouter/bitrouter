//! MCP registry client — discovery over the official MCP Registry
//! (<https://registry.modelcontextprotocol.io>, unauthenticated v0.1 REST API).
//!
//! Mirrors the ACP integration ([`crate::agent_registry`]): a thin client that
//! classifies install support and emits config stubs, while `mcp_servers:` in
//! `bitrouter.yaml` remains the sole source of truth for what can launch.
//!
//! Install-support preference order (per the registry's own publication
//! model):
//!
//! 1. `remotes[streamable-http]` — **zero-install remote entry** (nothing
//!    executes locally; preferred when present).
//! 2. `packages[npm|pypi]` + stdio — auto-stub: `npx -y <id>@<version>` /
//!    `uvx <id>@<version>`.
//! 3. `packages[oci|mcpb|…]` — manual tier: listed, never auto-stubbed
//!    (no runner mapping; `mcpb` carries a `fileSha256` a future `--run`
//!    flow could verify).
//!
//! The registry is in **preview** — parsing is lenient (unknown fields are
//! ignored), only `status == "active"` + `isLatest` entries are surfaced, and
//! every fetch has a short timeout: discovery is a convenience and must not
//! hang the CLI. Responses are cached under `$XDG_CACHE_HOME/bitrouter/
//! mcp-registry/` (24h TTL, stale fallback on network failure) per the
//! registry's consumption guidance (scrape infrequently, persist your own
//! copy; no uptime guarantees). No background sync.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

/// Base URL of the official MCP registry's v0.1 REST API.
pub const REGISTRY_BASE_URL: &str = "https://registry.modelcontextprotocol.io";

/// How long any single registry request may take before the CLI gives up.
/// Discovery is a convenience; a slow registry must not hang `bitrouter mcp`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Page size for `/v0.1/servers` pagination (the registry's maximum).
const PAGE_SIZE: u32 = 100;

/// Hard cap on pages followed per command, bounding total fetch time.
const MAX_PAGES: usize = 10;

/// Cache freshness window — 24 hours, matching the provider-registry cache.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

// ===== registry document types (lenient: unknown fields ignored) =====

/// One `{"server": …, "_meta": …}` pair from the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    pub server: RegistryServer,
    #[serde(rename = "_meta", default)]
    pub meta: EntryMeta,
}

/// The `_meta` block. Only the official-registry namespace is modelled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryMeta {
    #[serde(
        rename = "io.modelcontextprotocol.registry/official",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub official: Option<OfficialMeta>,
}

/// Publication state assigned by the official registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfficialMeta {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(rename = "isLatest", default)]
    pub is_latest: Option<bool>,
}

/// The `server` object (a `server.json` document). Only the fields the CLI
/// consumes are modelled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryServer {
    /// Reverse-DNS name, e.g. `com.pulsemcp/remote-filesystem`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    #[serde(default)]
    pub remotes: Vec<Remote>,
    #[serde(default)]
    pub packages: Vec<Package>,
}

/// Source repository pointer, used for attribution and manual-install hints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    #[serde(default)]
    pub url: Option<String>,
}

/// A remote (hosted) transport entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    /// `streamable-http`, `sse`, …
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<KeyValue>,
}

/// Shared shape for `environmentVariables` and remote `headers` (the
/// schema's `KeyValueInput`): a named input with an optional fixed value,
/// default, and requirement marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyValue {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,
    #[serde(rename = "isSecret", default)]
    pub is_secret: bool,
}

/// A package (locally executed) distribution entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// `npm`, `pypi`, `oci`, `nuget`, `mcpb`, …
    #[serde(rename = "registryType")]
    pub registry_type: String,
    pub identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<PackageTransport>,
    #[serde(rename = "runtimeArguments", default)]
    pub runtime_arguments: Vec<Argument>,
    #[serde(rename = "packageArguments", default)]
    pub package_arguments: Vec<Argument>,
    #[serde(rename = "environmentVariables", default)]
    pub environment_variables: Vec<KeyValue>,
}

/// The package's transport descriptor; only stdio packages are stub-able.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageTransport {
    #[serde(rename = "type")]
    pub kind: String,
}

/// One `runtimeArguments` / `packageArguments` entry, modelled flat (the
/// schema's `positional` and `named` variants share the input fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Argument {
    /// `positional` | `named`.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Flag name for named arguments (e.g. `--port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Verbatim value (may carry `{var}` templates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Default value supplied by the registry when no fixed value is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Placeholder id for user-supplied positional arguments.
    #[serde(rename = "valueHint", default, skip_serializing_if = "Option::is_none")]
    pub value_hint: Option<String>,
}

impl Argument {
    /// The command-line fragment this argument renders to. Positional
    /// arguments with only a `valueHint` render as a `<hint>` placeholder the
    /// user must replace when reviewing the stub.
    fn cli_fragment(&self) -> Option<String> {
        let value = self.value.as_ref().or(self.default.as_ref());
        match self.kind.as_deref() {
            Some("named") => {
                let name = self.name.as_ref()?;
                Some(match value {
                    Some(v) => format!("{name}={}", template_to_env_refs(v)),
                    None => name.clone(),
                })
            }
            // Positional (and drift-tolerant fallback for unknown kinds):
            // verbatim value, else a `<valueHint>` placeholder.
            _ => value
                .map(|v| template_to_env_refs(v))
                .or_else(|| self.value_hint.as_ref().map(|h| format!("<{h}>"))),
        }
    }
}

// ===== install support classification =====

/// How a registry entry can be installed, for listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSupport {
    /// A `streamable-http` remote exists — zero-install config entry.
    Remote,
    /// An npm/pypi stdio package — `mcp add` emits a ready stub.
    Stub(&'static str),
    /// oci/mcpb/… packages only — listed but not auto-stubbed.
    Manual,
    /// Neither remotes nor packages.
    None,
}

impl RegistryServer {
    /// The first `streamable-http` remote, when present.
    fn streamable_remote(&self) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.kind == "streamable-http")
    }

    /// The first version-pinned npm/pypi package explicitly wired for stdio.
    /// Preference: npm, then pypi, matching the runner preference order.
    fn stdio_package(&self) -> Option<&Package> {
        let is_stdio = |p: &&Package| {
            p.transport.as_ref().is_some_and(|t| t.kind == "stdio")
                && p.version.as_deref().is_some_and(|v| !v.is_empty())
                && !p.identifier.is_empty()
        };
        self.packages
            .iter()
            .filter(is_stdio)
            .find(|p| p.registry_type == "npm")
            .or_else(|| {
                self.packages
                    .iter()
                    .filter(is_stdio)
                    .find(|p| p.registry_type == "pypi")
            })
    }

    /// Install support classification for listings. Remotes win over
    /// packages (zero-install is the safest tier).
    pub fn install_support(&self) -> InstallSupport {
        if self.streamable_remote().is_some() {
            return InstallSupport::Remote;
        }
        if let Some(pkg) = self.stdio_package() {
            return match pkg.registry_type.as_str() {
                "npm" => InstallSupport::Stub("npx"),
                "pypi" => InstallSupport::Stub("uvx"),
                _ => InstallSupport::None,
            };
        }
        if !self.packages.is_empty() {
            return InstallSupport::Manual;
        }
        InstallSupport::None
    }
}

/// A concrete configuration target derived from a registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Zero-install remote entry: an HTTP transport with static headers.
    Remote {
        url: String,
        headers: Vec<KeyValueStub>,
    },
    /// A stdio invocation via a package runner.
    Stdio {
        command: &'static str,
        args: Vec<String>,
        env: Vec<KeyValueStub>,
    },
}

/// One env var / header line in a stub. `active` lines land in the YAML
/// mapping; inactive ones are emitted as comments (optional inputs with no
/// default — listing them documents the knob without forcing a value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValueStub {
    pub name: String,
    pub value: String,
    pub active: bool,
    /// `required` / `secret` / description, joined for the trailing comment.
    pub comment: Option<String>,
}

impl RegistryServer {
    /// The invocation for this server, honouring the install-support
    /// preference order. Returns `None` for manual-tier and empty entries.
    pub fn invocation(&self) -> Option<Invocation> {
        if let Some(remote) = self.streamable_remote() {
            return Some(Invocation::Remote {
                url: template_to_env_refs(&remote.url),
                headers: remote.headers.iter().map(header_stub).collect(),
            });
        }
        let pkg = self.stdio_package()?;
        let (command, mut args): (&'static str, Vec<String>) = match pkg.registry_type.as_str() {
            "npm" => ("npx", vec!["-y".to_string()]),
            "pypi" => ("uvx", Vec::new()),
            _ => return None,
        };
        args.extend(
            pkg.runtime_arguments
                .iter()
                .filter_map(Argument::cli_fragment),
        );
        // `npx -y` is our baseline; drop a registry-supplied duplicate.
        if command == "npx" {
            args.dedup();
        }
        args.push(match &pkg.version {
            Some(v) => format!("{}@{v}", pkg.identifier),
            None => pkg.identifier.clone(),
        });
        args.extend(
            pkg.package_arguments
                .iter()
                .filter_map(Argument::cli_fragment),
        );
        Some(Invocation::Stdio {
            command,
            args,
            env: pkg.environment_variables.iter().map(env_stub).collect(),
        })
    }
}

/// Map a remote-header declaration to a stub line. Declared headers are
/// auth-shaped, so they always render as active lines: a fixed value is
/// template-converted; a missing value becomes a `""` placeholder.
fn header_stub(kv: &KeyValue) -> KeyValueStub {
    let value = kv
        .value
        .as_ref()
        .map(|v| template_to_env_refs(v))
        .or_else(|| kv.default.clone())
        .unwrap_or_default();
    KeyValueStub {
        name: kv.name.clone(),
        value,
        active: true,
        comment: stub_comment(kv),
    }
}

/// Map an `environmentVariables` declaration to a stub line. Required vars
/// with no fixed value render as `""` placeholders (active); optional vars
/// with no default render as comments; anything with a fixed value or
/// default renders active so the stub works out of the box.
fn env_stub(kv: &KeyValue) -> KeyValueStub {
    let (value, has_value) = match (&kv.value, &kv.default) {
        (Some(v), _) => (template_to_env_refs(v), true),
        (None, Some(d)) => (d.clone(), true),
        (None, None) => (String::new(), false),
    };
    KeyValueStub {
        name: kv.name.clone(),
        value,
        active: has_value || kv.is_required,
        comment: stub_comment(kv),
    }
}

/// Join the requirement/secret markers and description into one comment.
fn stub_comment(kv: &KeyValue) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if kv.is_required && kv.value.is_none() {
        parts.push("required");
    }
    if kv.is_secret {
        parts.push("secret");
    }
    let mut out = parts.join(", ");
    if let Some(desc) = &kv.description {
        let desc = one_line(desc);
        if !desc.is_empty() {
            if !out.is_empty() {
                out.push_str(" — ");
            }
            out.push_str(&desc);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Collapse whitespace runs so a multi-line registry description stays on
/// one comment line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Convert `{var}` templates in registry values to bitrouter's `${VAR}`
/// env-reference form (uppercased, non-alphanumerics mapped to `_`), so a
/// pasted stub resolves from the operator's environment. Braced spans that
/// aren't identifier-shaped are left untouched.
pub fn template_to_env_refs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let inner = &after[..close];
                if !inner.is_empty()
                    && inner
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    let var: String = inner
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() {
                                c.to_ascii_uppercase()
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    out.push_str(&format!("${{{var}}}"));
                    rest = &after[close + 1..];
                } else {
                    out.push('{');
                    rest = after;
                }
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ===== visibility filter =====

/// Whether an entry is surfaced: published as `active` by the official
/// registry and marked as the latest version of its name.
pub fn visible(entry: &ServerEntry) -> bool {
    let Some(official) = &entry.meta.official else {
        return false;
    };
    official.status.as_deref() == Some("active") && official.is_latest == Some(true)
}

// ===== parsing =====

/// One page of `/v0.1/servers` results.
#[derive(Debug)]
pub struct ServerListPage {
    pub entries: Vec<ServerEntry>,
    pub next_cursor: Option<String>,
}

/// Parse a `/v0.1/servers` response body.
pub fn parse_server_list(json: &str) -> Result<ServerListPage> {
    #[derive(Deserialize)]
    struct ListBody {
        #[serde(default)]
        servers: Vec<ServerEntry>,
        #[serde(default)]
        metadata: Option<ListMetadata>,
    }
    #[derive(Deserialize)]
    struct ListMetadata {
        #[serde(rename = "nextCursor", default)]
        next_cursor: Option<String>,
    }
    let body: ListBody = serde_json::from_str(json).context("parsing MCP registry server list")?;
    Ok(ServerListPage {
        entries: body.servers,
        next_cursor: body.metadata.and_then(|m| m.next_cursor),
    })
}

/// Parse a `/v0.1/servers/{name}/versions/latest` response body.
pub fn parse_server_detail(json: &str) -> Result<ServerEntry> {
    serde_json::from_str(json).context("parsing MCP registry server detail")
}

// ===== rows for `mcp list` / `mcp search` =====

/// One row in the registry table.
#[derive(Debug, Clone)]
pub struct RegistryRow {
    /// Reverse-DNS registry name (the `mcp add` argument).
    pub name: String,
    pub version: String,
    /// `remote` / `npx` / `uvx` / `manual` / `-`.
    pub install: &'static str,
    pub description: String,
}

/// Rows for visible entries, sorted by name.
pub fn registry_rows(entries: &[ServerEntry]) -> Vec<RegistryRow> {
    let mut rows: Vec<RegistryRow> = entries
        .iter()
        .filter(|e| visible(e))
        .map(|e| RegistryRow {
            name: e.server.name.clone(),
            version: e.server.version.clone().unwrap_or_else(|| "-".to_string()),
            install: match e.server.install_support() {
                InstallSupport::Remote => "remote",
                InstallSupport::Stub(runner) => runner,
                InstallSupport::Manual => "manual",
                InstallSupport::None => "-",
            },
            description: e
                .server
                .description
                .clone()
                .or_else(|| e.server.title.clone())
                .unwrap_or_default(),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

// ===== `mcp add` stub =====

/// The result of [`add_stub`]: the derived `mcp_servers:` key and the YAML.
#[derive(Debug, Clone)]
pub struct AddStub {
    pub id: String,
    pub yaml: String,
}

/// `bitrouter mcp add <name>` — emit a YAML stub for a registry entry the
/// user pastes under `mcp_servers:` after review. Manual-tier entries
/// (oci/mcpb-only) are refused with a pointer to the project, mirroring the
/// ACP binary-only refusal. Stub-paste is SEP-1024-compliant by
/// construction: the user reviews the full command before it ever runs.
pub fn add_stub(entry: &ServerEntry) -> Result<AddStub, String> {
    let server = &entry.server;
    let id = stub_id(&server.name);
    let Some(invocation) = server.invocation() else {
        let hint = server
            .repository
            .as_ref()
            .and_then(|r| r.url.as_deref())
            .unwrap_or("the project's documentation");
        return Err(match server.install_support() {
            InstallSupport::Manual => format!(
                "'{}' has no safely stub-able, version-pinned npm/pypi stdio package (unsupported packages include oci/mcpb) — install it manually from {hint} and add an `mcp_servers:` entry pointing at the installed server.",
                server.name
            ),
            _ => format!(
                "'{}' publishes neither a remote endpoint nor an installable package — see {hint}.",
                server.name
            ),
        });
    };
    let comment = server
        .description
        .clone()
        .or_else(|| server.title.clone())
        .unwrap_or_else(|| server.name.clone());
    let version = server.version.as_deref().unwrap_or("unknown");
    let source = format!("MCP registry {}@{version}", server.name);
    Ok(AddStub {
        id: id.clone(),
        yaml: render_stub(&id, &one_line(&comment), &source, &invocation),
    })
}

/// Derive the `mcp_servers:` key from a reverse-DNS registry name: the
/// segment after the last `/`, restricted to URL-safe characters (the key
/// becomes the `POST /mcp/<name>` path segment and must satisfy
/// `McpServerConfig`'s name rules — non-empty, no `/`, not `sse`).
pub fn stub_id(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['-', '.', '_']);
    match trimmed {
        "" => "mcp-server".to_string(),
        // `sse` is a reserved server id in the config schema.
        "sse" => "sse-mcp".to_string(),
        id => id.to_string(),
    }
}

fn render_stub(id: &str, comment: &str, source: &str, invocation: &Invocation) -> String {
    use crate::agents::yaml_scalar;
    let mut out = String::new();
    out.push_str(&format!(
        "# {comment} — paste under `mcp_servers:` in bitrouter.yaml.\n"
    ));
    out.push_str(&format!("# Source: {source}\n"));
    out.push_str(&format!("{id}:\n"));
    out.push_str(&format!("  name: {id}\n"));
    out.push_str("  transport:\n");
    match invocation {
        Invocation::Remote { url, headers } => {
            out.push_str("    type: http\n");
            out.push_str(&format!("    url: {}\n", yaml_scalar(url)));
            if !headers.is_empty() {
                out.push_str("    headers:\n");
                for h in headers {
                    if let Some(comment) = &h.comment {
                        out.push_str(&format!("      # {comment}\n"));
                    }
                    out.push_str(&format!(
                        "      {}: {}\n",
                        yaml_scalar(&h.name),
                        yaml_scalar(&h.value)
                    ));
                }
            }
        }
        Invocation::Stdio { command, args, env } => {
            out.push_str("    type: stdio\n");
            out.push_str(&format!("    command: {command}\n"));
            out.push_str("    args:\n");
            for a in args {
                out.push_str(&format!("      - {}\n", yaml_scalar(a)));
            }
            if !env.is_empty() {
                out.push_str("    env:\n");
                for kv in env {
                    if kv.active {
                        if let Some(comment) = &kv.comment {
                            out.push_str(&format!("      # {comment}\n"));
                        }
                        out.push_str(&format!(
                            "      {}: {}\n",
                            yaml_scalar(&kv.name),
                            yaml_scalar(&kv.value)
                        ));
                    } else {
                        // Optional knob with no default — document it as a
                        // commented-out line rather than forcing a value.
                        match &kv.comment {
                            Some(comment) => out.push_str(&format!("      # {comment}\n")),
                            None => out.push_str("      # optional\n"),
                        }
                        out.push_str(&format!("      # {}: \"\"\n", kv.name));
                    }
                }
            }
        }
    }
    out
}

// ===== HTTP client + cache =====

/// What a fetch produced, plus whether it came from the on-disk cache.
#[derive(Debug)]
pub struct FetchOutcome<T> {
    pub data: T,
    pub from_cache: bool,
}

/// The registry HTTP client: short timeout, cursor pagination, XDG cache.
pub struct RegistryClient {
    http: reqwest::Client,
    base_url: String,
    cache_dir: Option<PathBuf>,
    timeout: Duration,
}

impl RegistryClient {
    /// A client against [`REGISTRY_BASE_URL`] with the default XDG cache
    /// (disabled when no cache directory can be resolved).
    pub fn new() -> Result<Self> {
        Self::with_base(REGISTRY_BASE_URL.to_string(), default_cache_dir())
    }

    /// A client against an explicit base URL and cache directory. Tests use
    /// this to point at a mock server; production uses [`Self::new`].
    fn with_base(base_url: String, cache_dir: Option<PathBuf>) -> Result<Self> {
        Self::with_base_and_timeout(base_url, cache_dir, FETCH_TIMEOUT)
    }

    fn with_base_and_timeout(
        base_url: String,
        cache_dir: Option<PathBuf>,
        timeout: Duration,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("bitrouter/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .build()
            .context("building MCP registry http client")?;
        Ok(Self {
            http,
            base_url,
            cache_dir,
            timeout,
        })
    }

    /// `mcp search <query>` / `mcp list` — fetch visible (active + latest)
    /// entries, following cursors until `limit` rows are gathered, the
    /// registry runs out of pages, or the page cap is reached.
    pub async fn servers(
        &self,
        query: Option<&str>,
        limit: usize,
    ) -> Result<FetchOutcome<Vec<ServerEntry>>> {
        let cache = self.cache_dir.as_ref().map(|d| {
            let identity = format!("{}\0{limit}", query.unwrap_or(""));
            d.join(format!("servers--{}.json", cache_key(&identity)))
        });
        let outcome = fetch_with_cache(cache, || async {
            tokio::time::timeout(self.timeout, self.fetch_servers(query, limit))
                .await
                .with_context(|| {
                    format!(
                        "MCP registry command timed out after {}s",
                        self.timeout.as_secs_f64()
                    )
                })?
        })
        .await?;
        Ok(FetchOutcome {
            data: truncate(outcome.data, limit),
            from_cache: outcome.from_cache,
        })
    }

    /// `mcp add <name>` — fetch one server's latest version.
    pub async fn latest(&self, name: &str) -> Result<FetchOutcome<ServerEntry>> {
        let cache = self
            .cache_dir
            .as_ref()
            .map(|d| d.join(format!("server--{}.json", cache_key(name))));
        let outcome = fetch_with_cache(cache, || self.fetch_latest(name)).await?;
        if outcome.data.server.name != name || !visible(&outcome.data) {
            anyhow::bail!("cached MCP registry entry does not match active server '{name}'");
        }
        Ok(outcome)
    }

    /// Paginate `/v0.1/servers`, keeping only visible entries.
    async fn fetch_servers(&self, query: Option<&str>, limit: usize) -> Result<Vec<ServerEntry>> {
        let mut out: Vec<ServerEntry> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let url = servers_url(&self.base_url, query, cursor.as_deref(), PAGE_SIZE)?;
            let body = self.get(&url).await?;
            let page = parse_server_list(&body)?;
            out.extend(page.entries.into_iter().filter(visible));
            if out.len() >= limit {
                break;
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    /// Fetch `/v0.1/servers/{name}/versions/latest`.
    async fn fetch_latest(&self, name: &str) -> Result<ServerEntry> {
        let url = latest_url(&self.base_url, name)?;
        let body = self.get(&url).await.with_context(|| {
            format!("looking up '{name}' — check the id with `bitrouter mcp search <query>`")
        })?;
        let entry = parse_server_detail(&body)?;
        if !visible(&entry) {
            anyhow::bail!(
                "'{name}' is not an active, latest entry in the MCP registry (it may be deprecated)"
            );
        }
        Ok(entry)
    }

    async fn get(&self, url: &str) -> Result<String> {
        self.http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("fetching MCP registry {url}"))?
            .text()
            .await
            .context("reading MCP registry body")
    }
}

fn truncate<T>(mut v: Vec<T>, limit: usize) -> Vec<T> {
    v.truncate(limit);
    v
}

/// Build a `/v0.1/servers` URL with search / cursor / limit params.
fn servers_url(
    base: &str,
    query: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("{base}/v0.1/servers"))
        .context("building MCP registry URL")?;
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(q) = query {
            pairs.append_pair("search", q);
        }
        if let Some(c) = cursor {
            pairs.append_pair("cursor", c);
        }
        pairs.append_pair("limit", &limit.to_string());
        pairs.append_pair("version", "latest");
    }
    Ok(url.into())
}

/// Build a `/v0.1/servers/{name}/versions/latest` URL. Registry names are
/// reverse-DNS (`org/name`); only the slash needs encoding.
fn latest_url(base: &str, name: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("{base}/v0.1/servers"))
        .context("building MCP registry URL")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("MCP registry base URL cannot hold path segments"))?
        .push(name)
        .push("versions")
        .push("latest");
    Ok(url.into())
}

/// Filename-safe cache key fragment: lowercase, non-alphanumeric runs
/// collapse to one `-`.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "all".to_string()
    } else {
        out
    }
}

/// A readable, collision-resistant cache key. The slug is diagnostic only;
/// the SHA-256 suffix preserves the exact query/name identity.
fn cache_key(s: &str) -> String {
    let slug: String = slugify(s).chars().take(48).collect();
    let digest = Sha256::digest(s.as_bytes());
    format!("{slug}--{}", hex::encode(digest))
}

/// Resolve the MCP registry cache directory under
/// `$XDG_CACHE_HOME/bitrouter/mcp-registry/` (falling back to
/// `~/.cache/bitrouter/…` on Unix or `%LOCALAPPDATA%\bitrouter\cache\…` on
/// Windows). `None` disables caching.
fn default_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("bitrouter").join("mcp-registry"));
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return Some(
            PathBuf::from(dir)
                .join("bitrouter")
                .join("cache")
                .join("mcp-registry"),
        );
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(
            PathBuf::from(home)
                .join(".cache")
                .join("bitrouter")
                .join("mcp-registry"),
        );
    }
    None
}

/// The on-disk cache envelope: `{fetched_at, data}`.
#[derive(Debug, Serialize, Deserialize)]
struct Cached<T> {
    fetched_at: u64,
    data: T,
}

/// The fetch-with-cache flow shared by `servers` and `latest`: a fresh
/// cache hit wins with no network at all; a successful fetch refreshes the
/// cache; a network failure falls back to a stale entry (with a stderr
/// warning) and only errors when nothing is cached.
async fn fetch_with_cache<T, F, Fut>(cache: Option<PathBuf>, fetch: F) -> Result<FetchOutcome<T>>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if let Some(path) = &cache
        && let Some(fresh) = cache_read_fresh::<T>(path)
    {
        return Ok(FetchOutcome {
            data: fresh,
            from_cache: true,
        });
    }
    match fetch().await {
        Ok(data) => {
            if let Some(path) = &cache {
                cache_write(path, &data);
            }
            Ok(FetchOutcome {
                data,
                from_cache: false,
            })
        }
        Err(net_err) => {
            if let Some(path) = &cache
                && permits_stale_fallback(&net_err)
                && let Some(stale) = cache_read_any::<T>(path)
            {
                eprintln!(
                    "warning: MCP registry unreachable ({net_err:#}); serving cached data (may be stale)"
                );
                return Ok(FetchOutcome {
                    data: stale,
                    from_cache: true,
                });
            }
            Err(net_err)
        }
    }
}

/// Stale data is safe only for transient transport/server failures. An
/// authoritative client error (notably 404 for a deleted registry entry) or
/// a local parse/validation error must surface instead of reviving old data.
fn permits_stale_fallback(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
        {
            return true;
        }
        let Some(error) = cause.downcast_ref::<reqwest::Error>() else {
            return false;
        };
        if let Some(status) = error.status() {
            return status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        }
        error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a cache entry that exists AND is within the TTL. Best-effort: any
/// I/O or parse failure reads as a miss (the network path then decides).
fn cache_read_fresh<T: DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let (cached, age) = cache_read_with_age::<T>(path)?;
    (age <= CACHE_TTL).then_some(cached.data)
}

/// Read a cache entry regardless of freshness — the stale fallback after a
/// network failure.
fn cache_read_any<T: DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    cache_read_with_age::<T>(path).map(|(cached, _)| cached.data)
}

fn cache_read_with_age<T: DeserializeOwned>(
    path: &std::path::Path,
) -> Option<(Cached<T>, Duration)> {
    let bytes = std::fs::read(path).ok()?;
    let cached: Cached<T> = serde_json::from_slice(&bytes).ok()?;
    let age = Duration::from_secs(now_secs().saturating_sub(cached.fetched_at));
    Some((cached, age))
}

/// Write a cache entry (atomic rename so a crash mid-write never leaves a
/// half-truncated file). Best-effort: failures only cost the next fetch.
fn cache_write<T: Serialize>(path: &std::path::Path, data: &T) {
    let write = || -> std::io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "cache path has no parent")
        })?;
        std::fs::create_dir_all(parent)?;
        let payload = Cached {
            fetched_at: now_secs(),
            data,
        };
        let bytes = serde_json::to_vec(&payload)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::debug!(path = %path.display(), error = %e, "MCP registry cache write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-shape fixture: two versions of one server (only the newer is
    /// `isLatest`), a remote-only entry with templated headers, an
    /// oci-only entry, a deprecated entry, and one with no `_meta`.
    const LIST_FIXTURE: &str = r#"{
      "servers": [
        {
          "server": {
            "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
            "name": "com.pulsemcp/remote-filesystem",
            "description": "MCP server for remote filesystem operations.",
            "repository": { "url": "https://github.com/pulsemcp/mcp-servers", "source": "github" },
            "version": "0.1.4",
            "packages": [
              {
                "registryType": "npm",
                "identifier": "remote-filesystem-mcp-server",
                "version": "0.1.4",
                "runtimeHint": "npx",
                "transport": { "type": "stdio" },
                "runtimeArguments": [ { "value": "-y", "type": "positional" } ],
                "environmentVariables": [
                  { "name": "GCS_BUCKET", "description": "Bucket name.", "isRequired": true },
                  { "name": "GCS_PRIVATE_KEY", "description": "Private key.", "isSecret": true, "isRequired": true },
                  { "name": "GCS_MAKE_PUBLIC", "default": "false", "description": "Make uploads public." },
                  { "name": "GCS_ROOT_PATH", "description": "Root path prefix." }
                ],
                "unknownFutureField": 42
              }
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": false } }
        },
        {
          "server": {
            "name": "com.pulsemcp/remote-filesystem",
            "description": "MCP server for remote filesystem operations.",
            "version": "0.1.5",
            "packages": [
              {
                "registryType": "npm",
                "identifier": "remote-filesystem-mcp-server",
                "version": "0.1.5",
                "transport": { "type": "stdio" },
                "runtimeArguments": [ { "value": "-y", "type": "positional" } ],
                "environmentVariables": [
                  { "name": "GCS_BUCKET", "description": "Bucket name.", "isRequired": true },
                  { "name": "GCS_MAKE_PUBLIC", "default": "false", "description": "Make uploads public." }
                ]
              }
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": true } }
        },
        {
          "server": {
            "name": "ai.smithery/context7fork",
            "title": "Context7 Fork",
            "description": "Docs lookup.",
            "version": "1.0.13",
            "remotes": [
              {
                "type": "streamable-http",
                "url": "https://server.smithery.ai/@x/context7fork/mcp",
                "headers": [
                  { "name": "Authorization", "value": "Bearer {smithery_api_key}", "description": "Bearer token." }
                ]
              }
            ],
            "packages": [
              { "registryType": "npm", "identifier": "unused-here", "transport": { "type": "stdio" } }
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": true } }
        },
        {
          "server": {
            "name": "io.github.some/oci-only",
            "description": "Container only.",
            "version": "2.0.0",
            "packages": [
              { "registryType": "oci", "identifier": "ghcr.io/some/oci-only", "version": "2.0.0", "transport": { "type": "stdio" } }
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": true } }
        },
        {
          "server": { "name": "old.example/deprecated", "version": "1.0.0", "remotes": [ { "type": "streamable-http", "url": "https://x.example/mcp" } ] },
          "_meta": { "io.modelcontextprotocol.registry/official": { "status": "deprecated", "isLatest": true } }
        },
        {
          "server": { "name": "no.meta/entry", "version": "1.0.0" }
        }
      ],
      "metadata": { "nextCursor": "com.pulsemcp/remote-filesystem:0.1.5", "count": 6 }
    }"#;

    /// A pypi entry with packageArguments and a user-supplied positional.
    const DETAIL_FIXTURE: &str = r#"{
      "server": {
        "name": "io.github.06ketan/substack-ops",
        "description": "Substack operations.",
        "version": "0.3.5",
        "packages": [
          {
            "registryType": "pypi",
            "identifier": "substack-ops",
            "version": "0.3.5",
            "runtimeHint": "uvx",
            "transport": { "type": "stdio" },
            "packageArguments": [
              { "value": "mcp", "type": "positional" },
              { "value": "serve", "type": "positional" },
              { "type": "named", "name": "--root", "value": "{root_dir}" },
              { "type": "positional", "valueHint": "newsletter_slug" }
            ],
            "environmentVariables": [
              { "name": "SUBSTACK_API_KEY", "isRequired": true, "isSecret": true, "description": "API key." }
            ]
          }
        ]
      },
      "_meta": { "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": true } }
    }"#;

    fn entries() -> Vec<ServerEntry> {
        parse_server_list(LIST_FIXTURE)
            .expect("fixture parses")
            .entries
    }

    #[test]
    fn parses_real_shape_and_ignores_unknown_fields() {
        let page = parse_server_list(LIST_FIXTURE).expect("fixture parses");
        assert_eq!(page.entries.len(), 6);
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("com.pulsemcp/remote-filesystem:0.1.5")
        );
        let fs = &page.entries[0];
        assert_eq!(fs.server.name, "com.pulsemcp/remote-filesystem");
        assert_eq!(fs.server.packages[0].environment_variables.len(), 4);
    }

    #[test]
    fn visible_keeps_only_active_and_latest() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let names: Vec<_> = vis.iter().map(|e| e.server.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "com.pulsemcp/remote-filesystem",
                "ai.smithery/context7fork",
                "io.github.some/oci-only"
            ],
            "non-latest, deprecated, and no-meta entries must be filtered"
        );
        // The surviving filesystem entry is the 0.1.5 (latest) one.
        assert_eq!(vis[0].server.version.as_deref(), Some("0.1.5"));
    }

    #[test]
    fn classification_prefers_remote_then_runner_then_manual() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let by_name = |n: &str| vis.iter().find(|e| e.server.name == n).expect("entry");
        assert_eq!(
            by_name("ai.smithery/context7fork").server.install_support(),
            InstallSupport::Remote,
            "a remote beats the same entry's npm package"
        );
        assert_eq!(
            by_name("com.pulsemcp/remote-filesystem")
                .server
                .install_support(),
            InstallSupport::Stub("npx")
        );
        assert_eq!(
            by_name("io.github.some/oci-only").server.install_support(),
            InstallSupport::Manual
        );
    }

    #[test]
    fn package_stubs_require_explicit_stdio_and_pinned_version() {
        let package = |transport: Option<&str>, version: Option<&str>| Package {
            registry_type: "npm".to_string(),
            identifier: "example-mcp".to_string(),
            version: version.map(str::to_string),
            transport: transport.map(|kind| PackageTransport {
                kind: kind.to_string(),
            }),
            runtime_arguments: Vec::new(),
            package_arguments: Vec::new(),
            environment_variables: Vec::new(),
        };
        let server = |package| RegistryServer {
            name: "com.example/server".to_string(),
            title: None,
            description: None,
            version: Some("1.0.0".to_string()),
            repository: None,
            remotes: Vec::new(),
            packages: vec![package],
        };

        for unsafe_package in [
            package(None, Some("1.0.0")),
            package(Some("stdio"), None),
            package(Some("stdio"), Some("")),
        ] {
            let server = server(unsafe_package);
            assert_eq!(server.install_support(), InstallSupport::Manual);
            assert_eq!(server.invocation(), None);
        }
    }

    #[test]
    fn rows_classify_and_sort() {
        let rows = registry_rows(&entries());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "ai.smithery/context7fork");
        assert_eq!(rows[0].install, "remote");
        assert_eq!(rows[1].install, "npx");
        assert_eq!(rows[2].install, "manual");
    }

    #[test]
    fn stdio_invocation_pins_version_and_dedups_dash_y() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let fs = vis
            .iter()
            .find(|e| e.server.name == "com.pulsemcp/remote-filesystem")
            .expect("entry");
        let inv = fs.server.invocation().expect("stdio maps");
        let Invocation::Stdio { command, args, env } = inv else {
            panic!("expected stdio invocation");
        };
        assert_eq!(command, "npx");
        assert_eq!(args, vec!["-y", "remote-filesystem-mcp-server@0.1.5"]);
        let bucket = env.iter().find(|e| e.name == "GCS_BUCKET").expect("env");
        assert!(
            bucket.active,
            "required var renders as an active placeholder"
        );
        assert_eq!(bucket.value, "");
        assert_eq!(bucket.comment.as_deref(), Some("required — Bucket name."));
    }

    #[test]
    fn env_stub_tiers_required_default_optional() {
        let old = &entries()[0].server.packages[0];
        let stubs: Vec<KeyValueStub> = old.environment_variables.iter().map(env_stub).collect();
        let by_name = |n: &str| stubs.iter().find(|s| s.name == n).expect("stub");
        assert!(by_name("GCS_BUCKET").active);
        let secret = by_name("GCS_PRIVATE_KEY");
        assert!(secret.active);
        assert_eq!(
            secret.comment.as_deref(),
            Some("required, secret — Private key.")
        );
        let defaulted = by_name("GCS_MAKE_PUBLIC");
        assert!(defaulted.active);
        assert_eq!(defaulted.value, "false");
        let optional = by_name("GCS_ROOT_PATH");
        assert!(!optional.active, "optional without default comments out");
    }

    #[test]
    fn remote_invocation_converts_header_templates() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let remote = vis
            .iter()
            .find(|e| e.server.name == "ai.smithery/context7fork")
            .expect("entry");
        let inv = remote.server.invocation().expect("remote maps");
        let Invocation::Remote { url, headers } = inv else {
            panic!("expected remote invocation");
        };
        assert_eq!(url, "https://server.smithery.ai/@x/context7fork/mcp");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].name, "Authorization");
        assert_eq!(headers[0].value, "Bearer ${SMITHERY_API_KEY}");
    }

    #[test]
    fn template_conversion_uppercases_and_sanitises() {
        assert_eq!(
            template_to_env_refs("Bearer {smithery_api_key}"),
            "Bearer ${SMITHERY_API_KEY}"
        );
        assert_eq!(template_to_env_refs("{a-b.c}"), "${A_B_C}");
        assert_eq!(template_to_env_refs("no templates"), "no templates");
        assert_eq!(template_to_env_refs("100% {"), "100% {");
        assert_eq!(template_to_env_refs("{not a var}"), "{not a var}");
        assert_eq!(template_to_env_refs("{}"), "{}");
    }

    #[test]
    fn stub_id_derives_url_safe_key() {
        assert_eq!(
            stub_id("com.pulsemcp/remote-filesystem"),
            "remote-filesystem"
        );
        assert_eq!(stub_id("io.github.06ketan/substack-ops"), "substack-ops");
        assert_eq!(stub_id("plainname"), "plainname");
        assert_eq!(stub_id("org/sse"), "sse-mcp", "sse is a reserved id");
        assert_eq!(stub_id("org/we ird"), "we-ird");
        assert_eq!(stub_id("org/"), "mcp-server");
    }

    #[test]
    fn pypi_invocation_appends_package_arguments() {
        let entry = parse_server_detail(DETAIL_FIXTURE).expect("fixture parses");
        let inv = entry.server.invocation().expect("uvx maps");
        let Invocation::Stdio { command, args, .. } = inv else {
            panic!("expected stdio invocation");
        };
        assert_eq!(command, "uvx");
        assert_eq!(
            args,
            vec![
                "substack-ops@0.3.5",
                "mcp",
                "serve",
                "--root=${ROOT_DIR}",
                "<newsletter_slug>"
            ]
        );
    }

    #[test]
    fn arguments_render_registry_defaults() {
        let named: Argument = serde_json::from_value(serde_json::json!({
            "type": "named",
            "name": "--port",
            "default": "3000"
        }))
        .expect("named argument");
        assert_eq!(named.cli_fragment().as_deref(), Some("--port=3000"));

        let positional: Argument = serde_json::from_value(serde_json::json!({
            "type": "positional",
            "valueHint": "workspace",
            "default": "/srv/project"
        }))
        .expect("positional argument");
        assert_eq!(positional.cli_fragment().as_deref(), Some("/srv/project"));
    }

    #[test]
    fn add_stub_stdio_round_trips_through_config_schema() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let fs = vis
            .iter()
            .find(|e| e.server.name == "com.pulsemcp/remote-filesystem")
            .expect("entry");
        let stub = add_stub(fs).expect("stub");
        assert_eq!(stub.id, "remote-filesystem");
        assert!(stub.yaml.contains("remote-filesystem:"));
        assert!(stub.yaml.contains("command: npx"));
        assert!(stub.yaml.contains("remote-filesystem-mcp-server@0.1.5"));
        let body: std::collections::HashMap<
            String,
            bitrouter_sdk::mcp::transport::McpServerConfig,
        > = serde_saphyr::from_str(&stub.yaml)
            .expect("stub must deserialise into the config schema");
        let entry = body.get("remote-filesystem").expect("key present");
        assert_eq!(entry.name, "remote-filesystem");
        entry.validate().expect("stub must pass config validation");
        match &entry.transport {
            bitrouter_sdk::mcp::transport::McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args[0], "-y");
                assert!(env.contains_key("GCS_BUCKET"));
                // A registry default that parses as a YAML bool must still
                // land as a *string* in the pasted config.
                assert_eq!(
                    env.get("GCS_MAKE_PUBLIC").map(String::as_str),
                    Some("false")
                );
            }
            other => panic!("expected stdio transport, got {other:?}"),
        }
        assert!(stub.yaml.contains("GCS_MAKE_PUBLIC: \"false\""));
    }

    #[test]
    fn add_stub_remote_round_trips_through_config_schema() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let remote = vis
            .iter()
            .find(|e| e.server.name == "ai.smithery/context7fork")
            .expect("entry");
        let stub = add_stub(remote).expect("stub");
        assert_eq!(stub.id, "context7fork");
        let body: std::collections::HashMap<
            String,
            bitrouter_sdk::mcp::transport::McpServerConfig,
        > = serde_saphyr::from_str(&stub.yaml)
            .expect("stub must deserialise into the config schema");
        let entry = body.get("context7fork").expect("key present");
        entry.validate().expect("stub must pass config validation");
        match &entry.transport {
            bitrouter_sdk::mcp::transport::McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://server.smithery.ai/@x/context7fork/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer ${SMITHERY_API_KEY}")
                );
            }
            other => panic!("expected http transport, got {other:?}"),
        }
    }

    #[test]
    fn add_stub_pypi_fixture_has_required_env_placeholder() {
        let entry = parse_server_detail(DETAIL_FIXTURE).expect("fixture parses");
        let stub = add_stub(&entry).expect("stub");
        assert_eq!(stub.id, "substack-ops");
        assert!(stub.yaml.contains("command: uvx"));
        assert!(stub.yaml.contains("# required, secret — API key."));
        assert!(stub.yaml.contains("SUBSTACK_API_KEY: \"\""));
        let body: std::collections::HashMap<
            String,
            bitrouter_sdk::mcp::transport::McpServerConfig,
        > = serde_saphyr::from_str(&stub.yaml).expect("schema round-trip");
        body.get("substack-ops")
            .expect("key")
            .validate()
            .expect("valid");
    }

    #[test]
    fn add_stub_refuses_manual_tier_with_pointer() {
        let vis: Vec<_> = entries().into_iter().filter(visible).collect();
        let oci = vis
            .iter()
            .find(|e| e.server.name == "io.github.some/oci-only")
            .expect("entry");
        let err = add_stub(oci).unwrap_err();
        assert!(err.contains("oci/mcpb"), "{err}");
        assert!(err.contains("mcp_servers:"), "{err}");
    }

    #[test]
    fn add_stub_refuses_empty_entry() {
        let entry = ServerEntry {
            server: RegistryServer {
                name: "x.example/empty".to_string(),
                title: None,
                description: None,
                version: None,
                repository: None,
                remotes: Vec::new(),
                packages: Vec::new(),
            },
            meta: EntryMeta::default(),
        };
        let err = add_stub(&entry).unwrap_err();
        assert!(
            err.contains("neither a remote endpoint nor an installable package"),
            "{err}"
        );
    }

    #[test]
    fn slugify_normalises_cache_keys() {
        assert_eq!(
            slugify("com.pulsemcp/remote-filesystem"),
            "com-pulsemcp-remote-filesystem"
        );
        assert_eq!(slugify("File System"), "file-system");
        assert_eq!(slugify("all"), "all");
        assert_eq!(slugify(""), "all");
        assert_eq!(slugify("--weird--"), "weird");
    }

    #[test]
    fn urls_carry_search_cursor_and_limit() {
        let url = servers_url(
            "https://registry.example",
            Some("file system"),
            Some("cur:1"),
            100,
        )
        .expect("url");
        assert_eq!(
            url,
            "https://registry.example/v0.1/servers?search=file+system&cursor=cur%3A1&limit=100&version=latest"
        );
        let bare = servers_url("https://registry.example", None, None, 25).expect("url");
        assert_eq!(
            bare,
            "https://registry.example/v0.1/servers?limit=25&version=latest"
        );
    }

    #[test]
    fn latest_url_encodes_the_slash() {
        let url =
            latest_url("https://registry.example", "com.pulsemcp/remote-filesystem").expect("url");
        assert_eq!(
            url,
            "https://registry.example/v0.1/servers/com.pulsemcp%2Fremote-filesystem/versions/latest"
        );
    }

    // ===== network + cache behaviour (wiremock) =====

    mod http {
        use super::*;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn tmp_cache(label: &str) -> PathBuf {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "bitrouter-mcp-registry-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("tmp cache dir");
            dir
        }

        fn client(server: &MockServer, cache: &std::path::Path) -> RegistryClient {
            RegistryClient::with_base(server.uri(), Some(cache.to_path_buf())).expect("client")
        }

        fn age_only_cache_entry(cache: &std::path::Path) {
            let entries: Vec<_> = std::fs::read_dir(cache)
                .expect("cache dir")
                .collect::<std::io::Result<_>>()
                .expect("cache entries");
            assert_eq!(entries.len(), 1, "test expects exactly one cache file");
            let path = entries[0].path();
            let mut raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read cache"))
                    .expect("cache json");
            raw["fetched_at"] = serde_json::json!(0);
            std::fs::write(&path, serde_json::to_vec(&raw).expect("serialize cache"))
                .expect("age cache");
        }

        /// Two-list-page fixture: page 1 carries a cursor, page 2 terminates.
        #[tokio::test]
        async fn servers_paginates_filters_and_caches() {
            let server = MockServer::start().await;
            let cache = tmp_cache("pages");
            let page1 = r#"{"servers": [
                    { "server": {"name": "a.example/one", "version": "1.0.0", "remotes": [{"type": "streamable-http", "url": "https://a.example/mcp"}]},
                      "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}} },
                    { "server": {"name": "a.example/old", "version": "0.9.0"},
                      "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": false}} }
                ], "metadata": {"nextCursor": "a.example/one:1.0.0"}}"#;
            let page2 = r#"{"servers": [
                    { "server": {"name": "b.example/two", "version": "2.0.0"},
                      "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}} }
                ], "metadata": {}}"#;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .and(query_param("limit", "100"))
                .and(query_param("cursor", "a.example/one:1.0.0"))
                .respond_with(ResponseTemplate::new(200).set_body_string(page2))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .and(query_param("limit", "100"))
                .respond_with(ResponseTemplate::new(200).set_body_string(page1))
                .expect(1)
                .mount(&server)
                .await;

            let c = client(&server, &cache);
            let out = c.servers(None, 50).await.expect("fetch");
            assert!(!out.from_cache);
            let names: Vec<_> = out.data.iter().map(|e| e.server.name.as_str()).collect();
            assert_eq!(names, ["a.example/one", "b.example/two"]);

            // Second call is served from the cache — the mocks expect(1)
            // verify no further HTTP request is made.
            let out = c.servers(None, 50).await.expect("cached");
            assert!(out.from_cache);
            assert_eq!(out.data.len(), 2);
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn server_list_cache_does_not_truncate_larger_limits() {
            let server = MockServer::start().await;
            let cache = tmp_cache("limit");
            let active = |name: String| {
                serde_json::json!({
                    "server": {"name": name, "version": "1.0.0"},
                    "_meta": {"io.modelcontextprotocol.registry/official": {
                        "status": "active", "isLatest": true
                    }}
                })
            };
            let first: Vec<_> = (0..100)
                .map(|i| active(format!("com.example/server-{i:03}")))
                .collect();
            let page1 = serde_json::json!({
                "servers": first,
                "metadata": {"nextCursor": "cursor-100"}
            });
            let page2 = serde_json::json!({
                "servers": [active("com.example/server-100".to_string())],
                "metadata": {}
            });
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .and(query_param("cursor", "cursor-100"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page2))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page1))
                .mount(&server)
                .await;

            let c = client(&server, &cache);
            let small = c.servers(None, 50).await.expect("small fetch");
            assert_eq!(small.data.len(), 50);
            let large = c.servers(None, 150).await.expect("larger fetch");
            assert!(!large.from_cache, "a smaller cached result is incomplete");
            assert_eq!(large.data.len(), 101);
            assert_eq!(large.data[100].server.name, "com.example/server-100");
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn pagination_obeys_one_timeout_for_the_whole_command() {
            let server = MockServer::start().await;
            let page1 = r#"{"servers": [{
                    "server": {"name": "com.example/one", "version": "1.0.0"},
                    "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}}
                }], "metadata": {"nextCursor": "cursor-1"}}"#;
            let page2 = r#"{"servers": [{
                    "server": {"name": "com.example/two", "version": "1.0.0"},
                    "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}}
                }], "metadata": {}}"#;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .and(query_param("cursor", "cursor-1"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(70))
                        .set_body_string(page2),
                )
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(70))
                        .set_body_string(page1),
                )
                .mount(&server)
                .await;

            let c = RegistryClient::with_base_and_timeout(
                server.uri(),
                None,
                Duration::from_millis(100),
            )
            .expect("client");
            let err = c.servers(None, 2).await.unwrap_err();
            let timed_out = err.chain().any(|cause| {
                if cause
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
                {
                    return true;
                }
                let Some(error) = cause.downcast_ref::<reqwest::Error>() else {
                    return false;
                };
                error.is_timeout()
            });
            assert!(timed_out, "{err:#}");
        }

        #[tokio::test]
        async fn latest_cache_keys_do_not_alias_distinct_registry_names() {
            let server = MockServer::start().await;
            let cache = tmp_cache("name-collision");
            for name in ["com.example/a-b", "com.example/a_b"] {
                let encoded = name.replace('/', "%2F");
                let detail = serde_json::json!({
                    "server": {"name": name, "description": "test", "version": "1.0.0"},
                    "_meta": {"io.modelcontextprotocol.registry/official": {
                        "status": "active", "isLatest": true
                    }}
                });
                Mock::given(method("GET"))
                    .and(path(format!("/v0.1/servers/{encoded}/versions/latest")))
                    .respond_with(ResponseTemplate::new(200).set_body_json(detail))
                    .expect(1)
                    .mount(&server)
                    .await;
            }

            let c = client(&server, &cache);
            let first = c.latest("com.example/a-b").await.expect("first fetch");
            assert_eq!(first.data.server.name, "com.example/a-b");
            let second = c.latest("com.example/a_b").await.expect("second fetch");
            assert_eq!(second.data.server.name, "com.example/a_b");
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn servers_falls_back_to_stale_cache_on_network_failure() {
            let cache = tmp_cache("stale");
            let live = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"servers": [{
                        "server": {"name": "cached.example/one", "version": "1.0.0"},
                        "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}}
                    }], "metadata": {}}"#,
                ))
                .mount(&live)
                .await;
            client(&live, &cache)
                .servers(None, 50)
                .await
                .expect("populate cache");
            age_only_cache_entry(&cache);

            // No mock server listening on this port → connection refused.
            let c =
                RegistryClient::with_base("http://127.0.0.1:1".to_string(), Some(cache.clone()))
                    .expect("client");
            let out = c.servers(None, 50).await.expect("stale fallback");
            assert!(out.from_cache);
            assert_eq!(out.data[0].server.name, "cached.example/one");
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn servers_errors_when_network_fails_without_cache() {
            let cache = tmp_cache("nocache");
            let c =
                RegistryClient::with_base("http://127.0.0.1:1".to_string(), Some(cache.clone()))
                    .expect("client");
            let err = c.servers(None, 50).await.unwrap_err();
            assert!(err.to_string().contains("fetching MCP registry"), "{err}");
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn latest_fetches_detail_and_caches_by_name() {
            let server = MockServer::start().await;
            let cache = tmp_cache("detail");
            Mock::given(method("GET"))
                .and(path(
                    "/v0.1/servers/com.pulsemcp%2Fremote-filesystem/versions/latest",
                ))
                .respond_with(ResponseTemplate::new(200).set_body_string(DETAIL_LATEST))
                .expect(1)
                .mount(&server)
                .await;
            let c = client(&server, &cache);
            let out = c
                .latest("com.pulsemcp/remote-filesystem")
                .await
                .expect("fetch");
            assert!(!out.from_cache);
            assert_eq!(out.data.server.version.as_deref(), Some("0.1.5"));
            let cached = c
                .latest("com.pulsemcp/remote-filesystem")
                .await
                .expect("cached");
            assert!(cached.from_cache);
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn latest_rejects_non_latest_detail() {
            let server = MockServer::start().await;
            let cache = tmp_cache("notlatest");
            Mock::given(method("GET"))
                .and(path("/v0.1/servers/x%2Fy/versions/latest"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"server": {"name": "x/y", "version": "1.0.0"},
                        "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": false}}}"#,
                ))
                .mount(&server)
                .await;
            let c = client(&server, &cache);
            let err = c.latest("x/y").await.unwrap_err();
            assert!(err.to_string().contains("deprecated"), "{err}");
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn latest_404_hints_at_search() {
            let server = MockServer::start().await;
            let cache = tmp_cache("404");
            Mock::given(method("GET"))
                .and(path("/v0.1/servers/no%2Fsuch/versions/latest"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_string(r#"{"error":"not found"}"#),
                )
                .mount(&server)
                .await;
            let c = client(&server, &cache);
            let err = c.latest("no/such").await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("bitrouter mcp search"), "{msg}");
            let _ = std::fs::remove_dir_all(&cache);
        }

        #[tokio::test]
        async fn latest_does_not_revive_stale_entry_after_404() {
            let cache = tmp_cache("removed");
            let live = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers/no%2Fsuch/versions/latest"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"server": {"name": "no/such", "description": "test", "version": "1.0.0"},
                        "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}}}"#,
                ))
                .mount(&live)
                .await;
            let c = client(&live, &cache);
            c.latest("no/such").await.expect("populate cache");

            age_only_cache_entry(&cache);

            let removed = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v0.1/servers/no%2Fsuch/versions/latest"))
                .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"deleted"}"#))
                .mount(&removed)
                .await;
            let c = client(&removed, &cache);
            let err = c.latest("no/such").await.unwrap_err();
            assert!(format!("{err:#}").contains("bitrouter mcp search"));
            let _ = std::fs::remove_dir_all(&cache);
        }

        const DETAIL_LATEST: &str = r#"{
          "server": {
            "name": "com.pulsemcp/remote-filesystem",
            "description": "MCP server for remote filesystem operations.",
            "version": "0.1.5",
            "packages": [
              { "registryType": "npm", "identifier": "remote-filesystem-mcp-server", "version": "0.1.5", "transport": { "type": "stdio" } }
            ]
          },
          "_meta": { "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": true } }
        }"#;
    }
}
