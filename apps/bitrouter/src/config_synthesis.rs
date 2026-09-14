//! Declarative config-file synthesis for launched harnesses.
//!
//! Four catalog harnesses (opencode, pi, hermes, openclaw) expose neither a
//! base-URL environment variable nor a CLI override, so routing them means
//! writing a config file into a per-launch scratch directory and pointing the
//! harness at it with environment variables. Until now that was four hand-
//! written arms of `Harness::launch_overlay`.
//!
//! # Why this is a closed schema and not a template language
//!
//! The four files are *not* one string with different placeholders — they
//! disagree structurally. opencode wants its models as a **map** keyed by id,
//! pi as an **array** of `{id}`, openclaw as an array of fully-specified
//! model objects (its config validation rejects anything less), and hermes
//! carries no model list at all. Each writes its default model to a different
//! path in a different format, and two of them append CLI arguments.
//!
//! Expressing that as data needs iteration and conditionals — a template
//! language. But `AGENT_REGISTRY_SPEC` has this data arriving over the
//! network, and evaluating a fetched template that writes files is a much
//! larger surface than substituting scalars into one. So the variation is
//! modelled as a **closed set of enums** instead: every knob below has a fixed
//! set of values this binary understands, nothing is evaluated, and a registry
//! entry can only select among behaviours that were reviewed here.
//!
//! The practical consequence for contributors is the point of the exercise: a
//! new config-file harness whose shape matches an existing one — a JSON
//! skeleton, an array of model ids, a provider-prefixed default — needs no
//! Rust at all. Only a genuinely new *shape* does.
//!
//! MCP injection for the env/args harnesses (claude's `--mcp-config` file,
//! codex's `-c mcp_servers.*` overrides) is deliberately **not** modelled
//! here: that is MCP injection layered on top of env routing, not config-file
//! routing, and folding the two would widen this schema for no registry gain.

use std::path::Path;

use anyhow::Context;
use serde_json::{Map, Value};

use crate::harness::{McpServer, McpTransport, RoutingOverlay, headers_map, v1_base_url};

/// One harness's config-file routing, as data.
#[derive(Debug, Clone, Copy)]
pub struct ConfigSynthesis {
    /// Subdirectory under the launch state dir the config lives in, when the
    /// harness wants a directory of its own (`HERMES_HOME` and friends).
    /// `None` writes directly into the state dir.
    pub dir: Option<&'static str>,
    /// Filename within that directory.
    pub file: &'static str,
    /// The config's fixed structure, as JSON text. String leaves may contain
    /// `{base_url_v1}` and `{auth}`. Any key whose value is filled in below
    /// must already appear here, in the position the harness expects — the
    /// renderer replaces values but only ever *appends* new keys.
    pub skeleton: &'static str,
    /// Where the daemon's model catalog is written, and in what shape.
    pub models: Option<ModelList>,
    /// Where the default model is written. Appended, so it lands at the end
    /// of its parent object.
    pub default_model: Option<DefaultModel>,
    /// How injected MCP servers are written.
    pub mcp: McpShape,
    /// Environment variables to set. Values may contain `{dir}` (the resolved
    /// config directory), `{file}` (the config file's full path), and
    /// `{auth}`.
    pub env: &'static [(&'static str, &'static str)],
    /// Arguments appended to the harness invocation.
    pub args: ArgSpec,
}

/// Where and how the model catalog is rendered into the config.
#[derive(Debug, Clone, Copy)]
pub struct ModelList {
    /// JSON pointer to the key holding the collection. The key must exist in
    /// the skeleton so its position is preserved.
    pub at: &'static str,
    pub shape: ModelShape,
    pub order: ModelOrder,
}

/// The container shape a harness expects its model list in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelShape {
    /// `{"<id>": {}}` — opencode keys its models by id.
    MapOfEmpty,
    /// `[{"id": "<id>"}]`.
    ArrayOfId,
    /// `[{id, name, reasoning, input, cost, contextWindow, maxTokens}]` —
    /// openclaw's config validation rejects entries missing these, so the
    /// full record is synthesized with neutral values.
    ArrayOfProfile,
}

/// Whether the pinned model leads the list or follows the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOrder {
    /// Catalog order, with the pinned model appended only if it is not
    /// already present.
    CatalogThenModel,
    /// The pinned model first, then the whole catalog unfiltered.
    ModelThenCatalog,
}

/// Where the default model is written, and how it is spelled.
#[derive(Debug, Clone, Copy)]
pub struct DefaultModel {
    /// JSON pointer; intermediate objects are created as needed.
    pub at: &'static str,
    pub format: DefaultFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultFormat {
    /// The bare model id.
    Bare,
    /// `bitrouter/<id>` — harnesses that address a model through its provider.
    ProviderPrefixed,
}

/// How injected MCP servers are rendered.
#[derive(Debug, Clone, Copy)]
pub enum McpShape {
    /// The harness has no injectable MCP mechanism.
    None,
    /// A map of server name to entry, at `at`.
    NamedMap {
        at: &'static str,
        entry: McpEntryShape,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpEntryShape {
    /// opencode: an explicit `type` discriminant plus `enabled`, with the
    /// stdio command and its arguments folded into one invocation array.
    OpencodeTyped,
    /// The common shape: `{command, args}` for stdio, `{url, headers}` for
    /// HTTP, with the transport selected by which keys are present.
    CommandArgsOrUrl,
}

/// Arguments a harness needs appended to its invocation.
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    /// Always appended.
    pub always: &'static [&'static str],
    /// Appended only when a default model exists. `{default_model}` is
    /// substituted.
    pub with_default_model: &'static [&'static str],
}

impl ConfigSynthesis {
    /// Write the config and return the overlay that points the harness at it.
    pub fn render(
        &self,
        base_url: &str,
        auth: &str,
        model: Option<&str>,
        catalog: &[String],
        mcp: &[McpServer],
        state_dir: &Path,
    ) -> anyhow::Result<RoutingOverlay> {
        let dir = match self.dir {
            Some(name) => state_dir.join(name),
            None => state_dir.to_path_buf(),
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(self.file);

        let mut config: Value =
            serde_json::from_str(self.skeleton).context("parsing the harness config skeleton")?;
        substitute_scalars(&mut config, &v1_base_url(base_url), auth);

        let ids = self.model_ids(model, catalog);
        if let Some(list) = &self.models {
            set_pointer(&mut config, list.at, render_models(list.shape, &ids))?;
        }
        let default = ids_default(model, catalog);
        if let (Some(spec), Some(default)) = (&self.default_model, default.as_deref()) {
            set_pointer(
                &mut config,
                spec.at,
                Value::String(spec.format.render(default)),
            )?;
        }
        if let McpShape::NamedMap { at, entry } = self.mcp
            && !mcp.is_empty()
        {
            set_pointer(&mut config, at, render_mcp(entry, mcp))?;
        }

        std::fs::write(&path, serde_json::to_string_pretty(&config)?)
            .with_context(|| format!("writing {}", path.display()))?;

        let env = self
            .env
            .iter()
            .map(|(name, value)| {
                let rendered = value
                    .replace("{dir}", &dir.display().to_string())
                    .replace("{file}", &path.display().to_string())
                    .replace("{auth}", auth);
                ((*name).to_string(), rendered)
            })
            .collect();

        let mut args: Vec<String> = self.args.always.iter().map(|a| (*a).to_string()).collect();
        if let Some(default) = default {
            args.extend(
                self.args
                    .with_default_model
                    .iter()
                    .map(|a| a.replace("{default_model}", &default)),
            );
        }
        Ok(RoutingOverlay { env, args })
    }

    /// The model ids the config lists, in the order the harness expects.
    fn model_ids(&self, model: Option<&str>, catalog: &[String]) -> Vec<String> {
        let Some(list) = &self.models else {
            return Vec::new();
        };
        match list.order {
            ModelOrder::CatalogThenModel => {
                let mut ids: Vec<String> = catalog.to_vec();
                if let Some(m) = model
                    && !ids.iter().any(|id| id == m)
                {
                    ids.push(m.to_string());
                }
                ids
            }
            ModelOrder::ModelThenCatalog => model
                .into_iter()
                .map(str::to_string)
                .chain(catalog.iter().cloned())
                .collect(),
        }
    }
}

/// The default model: the pinned one, else the head of the catalog.
fn ids_default(model: Option<&str>, catalog: &[String]) -> Option<String> {
    model
        .map(str::to_string)
        .or_else(|| catalog.first().cloned())
}

impl DefaultFormat {
    fn render(self, id: &str) -> String {
        match self {
            Self::Bare => id.to_string(),
            Self::ProviderPrefixed => format!("bitrouter/{id}"),
        }
    }
}

/// Replace `{base_url_v1}` and `{auth}` in every string leaf of the skeleton.
fn substitute_scalars(value: &mut Value, base_url_v1: &str, auth: &str) {
    match value {
        Value::String(text) => {
            *text = text
                .replace("{base_url_v1}", base_url_v1)
                .replace("{auth}", auth);
        }
        Value::Array(items) => {
            for item in items {
                substitute_scalars(item, base_url_v1, auth);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                substitute_scalars(item, base_url_v1, auth);
            }
        }
        _ => {}
    }
}

/// Set a JSON pointer, creating intermediate objects. An existing key keeps
/// its position; a new one is appended, which is what the harness configs
/// expect of their optional trailing blocks.
fn set_pointer(root: &mut Value, pointer: &str, value: Value) -> anyhow::Result<()> {
    let segments: Vec<&str> = pointer
        .strip_prefix('/')
        .with_context(|| format!("config pointer '{pointer}' must start with '/'"))?
        .split('/')
        .collect();
    let (last, parents) = segments
        .split_last()
        .with_context(|| format!("config pointer '{pointer}' is empty"))?;
    let mut cursor = root;
    for segment in parents {
        let map = cursor
            .as_object_mut()
            .with_context(|| format!("config pointer '{pointer}' traverses a non-object"))?;
        cursor = map
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    cursor
        .as_object_mut()
        .with_context(|| format!("config pointer '{pointer}' traverses a non-object"))?
        .insert((*last).to_string(), value);
    Ok(())
}

fn render_models(shape: ModelShape, ids: &[String]) -> Value {
    match shape {
        ModelShape::MapOfEmpty => {
            let mut map = Map::new();
            for id in ids {
                map.entry(id.clone())
                    .or_insert_with(|| serde_json::json!({}));
            }
            Value::Object(map)
        }
        ModelShape::ArrayOfId => Value::Array(
            ids.iter()
                .map(|id| serde_json::json!({ "id": id }))
                .collect(),
        ),
        ModelShape::ArrayOfProfile => Value::Array(
            ids.iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "name": id,
                        "reasoning": false,
                        "input": ["text"],
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                        "contextWindow": 200000,
                        "maxTokens": 8192,
                    })
                })
                .collect(),
        ),
    }
}

fn render_mcp(entry: McpEntryShape, servers: &[McpServer]) -> Value {
    let mut entries = Map::new();
    for server in servers {
        let rendered = match (entry, &server.transport) {
            (McpEntryShape::OpencodeTyped, McpTransport::Stdio { command, args }) => {
                let mut invocation = vec![command.clone()];
                invocation.extend(args.iter().cloned());
                serde_json::json!({ "type": "local", "command": invocation, "enabled": true })
            }
            (McpEntryShape::OpencodeTyped, McpTransport::Http { url, headers }) => {
                serde_json::json!({
                    "type": "remote",
                    "url": url,
                    "enabled": true,
                    "headers": headers_map(headers),
                })
            }
            (McpEntryShape::CommandArgsOrUrl, McpTransport::Stdio { command, args }) => {
                serde_json::json!({ "command": command, "args": args })
            }
            (McpEntryShape::CommandArgsOrUrl, McpTransport::Http { url, headers }) => {
                serde_json::json!({ "url": url, "headers": headers_map(headers) })
            }
        };
        entries.insert(server.name.clone(), rendered);
    }
    Value::Object(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{self, McpServer, McpTransport};

    /// The four harnesses whose routing this module took over.
    const SYNTHESIZED: &[&str] = &["opencode", "pi-acp", "hermes-acp", "openclaw"];

    fn mcp_stdio() -> McpServer {
        McpServer {
            name: "bitrouter_tools".to_string(),
            transport: McpTransport::Stdio {
                command: "bro".to_string(),
                args: vec!["mcp".to_string(), "serve".to_string()],
            },
        }
    }

    fn mcp_http() -> McpServer {
        McpServer {
            name: "bitrouter_skills".to_string(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:4356/mcp".to_string(),
                headers: vec![("Authorization".to_string(), "Bearer brk_test".to_string())],
            },
        }
    }

    /// Env values carry absolute paths under the scratch dir, which differs
    /// between the two renders. Normalize so the comparison is about content.
    ///
    /// The separator is normalized too, so one golden set covers every host:
    /// substituting the root leaves the native separator behind, which would
    /// render `{root}\hermes` on Windows against a `{root}/hermes` fixture.
    /// Only env values carry paths here — every fixture's `args` are flags —
    /// so this cannot corrupt a value that means a backslash.
    fn normalize(overlay: &RoutingOverlay, root: &Path) -> (Vec<(String, String)>, Vec<String>) {
        let root = root.display().to_string();
        let env = overlay
            .env
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    value.replace(&root, "{root}").replace('\\', "/"),
                )
            })
            .collect();
        (env, overlay.args.clone())
    }

    /// The rendered-bytes gate.
    ///
    /// This started life as a differential test against the four hand-written
    /// `launch_overlay` arms, and that is what licensed deleting them. Once
    /// they were gone both sides of that comparison called the same code, so
    /// it proved nothing. What replaces it is a golden set captured from the
    /// state those arms were proven equal to: the exact bytes each harness
    /// receives, for the input shapes that distinguish the knobs.
    ///
    /// It is the gate for everything that comes after — moving these shapes
    /// into `registry/agents/` and generating them at build time both have to
    /// reproduce these bytes exactly.
    ///
    /// Set `UPDATE_CONFIG_SYNTHESIS_GOLDEN=1` to rewrite the fixtures after an
    /// intentional change, and read the diff before committing it.
    #[test]
    fn rendered_config_bytes_match_the_golden_fixtures() -> anyhow::Result<()> {
        // One input shape: its fixture suffix, the catalog, the pinned model,
        // and the MCP servers to render.
        type Case = (
            &'static str,
            Vec<String>,
            Option<&'static str>,
            Vec<McpServer>,
        );

        let cases: [Case; 3] = [
            // No catalog: no default model, so the optional blocks are absent.
            ("empty", Vec::new(), None, Vec::new()),
            // A pinned model outside the catalog — exercises append-vs-dedup.
            (
                "pinned-outside-catalog",
                vec!["a/one".to_string(), "b/two".to_string()],
                Some("extra/model"),
                Vec::new(),
            ),
            // A pinned model already in the catalog, plus both MCP transports.
            (
                "full",
                vec!["a/one".to_string(), "b/two".to_string()],
                Some("a/one"),
                vec![mcp_stdio(), mcp_http()],
            ),
        ];

        let update = std::env::var_os("UPDATE_CONFIG_SYNTHESIS_GOLDEN").is_some();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/config_synthesis");
        if update {
            std::fs::create_dir_all(&dir)?;
        }

        for id in SYNTHESIZED {
            let harness =
                harness::by_id(id).ok_or_else(|| anyhow::anyhow!("{id} missing from catalog"))?;
            for (case, catalog, model, mcp) in &cases {
                let scratch = tempfile::tempdir()?;
                let overlay = harness.launch_overlay(
                    "http://127.0.0.1:4356",
                    "brk_test",
                    *model,
                    catalog,
                    mcp,
                    scratch.path(),
                )?;
                let (env, args) = normalize(&overlay, scratch.path());
                let spec = harness
                    .config_synthesis()
                    .ok_or_else(|| anyhow::anyhow!("{id} has no config synthesis"))?;
                let written = spec
                    .dir
                    .map_or_else(
                        || scratch.path().to_path_buf(),
                        |sub| scratch.path().join(sub),
                    )
                    .join(spec.file);
                let body = std::fs::read_to_string(&written)?;

                let actual = serde_json::to_string_pretty(&serde_json::json!({
                    "env": env.iter().map(|(k, v)| [k, v]).collect::<Vec<_>>(),
                    "args": args,
                    "file": spec.file,
                    "config": serde_json::from_str::<Value>(&body)?,
                }))? + "\n";

                let path = dir.join(format!("{id}-{case}.json"));
                if update {
                    std::fs::write(&path, &actual)?;
                    continue;
                }
                let expected = std::fs::read_to_string(&path).with_context(|| {
                    format!(
                        "reading {} - regenerate with UPDATE_CONFIG_SYNTHESIS_GOLDEN=1",
                        path.display()
                    )
                })?;
                assert_eq!(
                    expected, actual,
                    "rendered config drifted for {id} ({case})"
                );
            }
        }
        assert!(
            !update,
            "fixtures rewritten; unset UPDATE_CONFIG_SYNTHESIS_GOLDEN to assert"
        );
        Ok(())
    }

    /// Non-vacuity: the sweep above would pass trivially if the renderer and
    /// the arm both produced nothing. Pin the parts a reader would check by
    /// hand — that a model list actually lands, and in the right container.
    #[test]
    fn rendered_configs_carry_the_catalog_in_each_container_shape() -> anyhow::Result<()> {
        let catalog = vec!["a/one".to_string(), "b/two".to_string()];
        let dir = tempfile::tempdir()?;
        let body = |id: &str| -> anyhow::Result<Value> {
            let spec = harness::by_id(id)
                .and_then(harness::Harness::config_synthesis)
                .ok_or_else(|| anyhow::anyhow!("{id}"))?;
            let sub = dir.path().join(id);
            spec.render(
                "http://127.0.0.1:4356",
                "brk_test",
                None,
                &catalog,
                &[],
                &sub,
            )?;
            let path = spec
                .dir
                .map_or_else(|| sub.clone(), |name| sub.join(name))
                .join(spec.file);
            Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
        };

        let opencode = body("opencode")?;
        assert!(opencode["provider"]["bitrouter"]["models"]["a/one"].is_object());
        assert_eq!(opencode["model"], "bitrouter/a/one");

        let pi = body("pi-acp")?;
        assert_eq!(pi["providers"]["bitrouter"]["models"][0]["id"], "a/one");

        let openclaw = body("openclaw")?;
        assert_eq!(
            openclaw["models"]["providers"]["bitrouter"]["models"][0]["contextWindow"],
            200000
        );
        assert_eq!(openclaw["agents"]["defaults"]["model"], "bitrouter/a/one");

        // hermes carries no model list at all — only the default.
        let hermes = body("hermes-acp")?;
        assert_eq!(hermes["model"]["default"], "a/one");
        assert!(hermes["model"].get("models").is_none());
        Ok(())
    }
}
