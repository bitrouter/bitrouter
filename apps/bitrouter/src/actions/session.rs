//! The session surface of the shared actions.
//!
//! Lives in `crate::actions`, not `crate::chat`: deciding what a session offers
//! reads the `ACTIONS` table and what the controller advertised, and the chat
//! guard keeps `chat/` from naming anything daemon-wide. The driver is *handed*
//! the finished list and names nothing else — the same move `route_control`
//! already makes.

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_mcp::actions::models::ModelsQuery;
use bitrouter_mcp::actions::route::{RouteInput, RouteQuery};
use bitrouter_mcp::actions::status::StatusQuery;
use bitrouter_mcp::actions::{ACTIONS, Requires};
use bitrouter_mcp::backend::CallerAuth;
use bitrouter_mcp::error::ToolError;
use bitrouter_sdk::acp::client::{AcpClient, RouteMethod};
use bitrouter_tui::machine::Command;

use crate::output::CliReport;
use crate::paths::ConfigSource;

/// What `/route` says at a session with no route surface to open a picker over.
const NOT_ROUTABLE: &str = "this session cannot be rerouted (the controller advertises no route \
                            control: running direct, or without a trusted local daemon binding)";
/// What `/route reset` says at the same session. Distinct from [`NOT_ROUTABLE`]
/// because a controller may advertise `set` without `reset`, and a reader told
/// "cannot be rerouted" would not know which of the two they had.
const NOT_RESETTABLE: &str = "this session has no route lease to drop (the controller advertises \
                              no route reset)";

/// One line of help per row, for `/commands`.
///
/// App-side rather than on the row because no other surface reads it: the MCP
/// tool has its own description and the CLI leaf has clap's. A guard fails a
/// row that carries a `tui_command` and has no arm here, so this cannot fall
/// behind the table.
pub fn summary_for(action: &str) -> &'static str {
    match action {
        "status" => "whether the daemon is up, and what it has spent",
        "list_models" => "the models this config can route to, optionally by provider",
        "route" => "show where a model would be routed, without sending anything",
        "commands" => "list the commands this session offers",
        "route_set" => "choose the route for the rest of the session",
        "route_reset" => "drop the route lease, so the daemon's default applies",
        _ => "",
    }
}

/// The `ACTIONS` rows that carry a `tui_command`, in table order, with each
/// row's requirement resolved against what this controller advertised.
///
/// Resolved once, at launch, and handed to the reducer as plain data. A row
/// whose requirement is unmet is still returned — with the reason — because a
/// control the session cannot run should say why rather than vanish.
pub fn offered_commands(client: &AcpClient) -> Vec<Command> {
    let capability = client.route_control();
    ACTIONS
        .iter()
        .filter_map(|row| {
            let name = row.tui_command?;
            let unavailable = match (row.requires, row.id) {
                (Requires::Nothing, _) => None,
                // `reset` is advertised separately from `list`/`set`, so it is
                // asked about separately.
                (Requires::Binding, "route_reset") => {
                    (!capability.allows(RouteMethod::Reset)).then_some(NOT_RESETTABLE)
                }
                // The picker lists with one method and sets with another, so
                // both must be there for it to be worth opening.
                (Requires::Binding, _) => (!(capability.allows(RouteMethod::List)
                    && capability.allows(RouteMethod::Set)))
                .then_some(NOT_ROUTABLE),
            };
            Some(Command {
                name,
                action: row.id,
                summary: summary_for(row.id),
                unavailable,
            })
        })
        .collect()
}

/// The session's view of the shared actions: the same ports the stdio MCP
/// profile is built from, handed to the chat driver as one value.
///
/// Not a new trait. The four port traits already exist and the MCP profile is
/// already assembled by bundling `Arc<dyn _>` of them; the session is the same
/// bundle with a third consumer. A second implementation of an action would
/// therefore have to be a second `impl StatusQuery`, which is the point — the
/// CLI leaf, the MCP tool and the slash command cannot answer differently
/// because there is only one thing for them to ask.
pub struct SessionPorts {
    status: Arc<dyn StatusQuery>,
    models: Arc<dyn ModelsQuery>,
    route: Arc<dyn RouteQuery>,
}

impl SessionPorts {
    /// The same three constructors `bitrouter status`, `bitrouter models` and
    /// `bitrouter route` call, with the same arguments.
    pub fn open(source: ConfigSource, socket: PathBuf) -> Self {
        Self {
            status: Arc::new(crate::actions::status::DaemonStatus::new(
                socket.clone(),
                Some(source.clone()),
            )),
            models: Arc::new(crate::actions::models::RoutableModels::new(
                source.clone(),
                Some(socket.clone()),
            )),
            route: Arc::new(crate::actions::route::RouteAction::new(
                source,
                Some(socket),
            )),
        }
    }

    /// Resolve one row id and its arguments to a report.
    ///
    /// The only place a slash command becomes a port call. The reducer emits
    /// only ids the app gave it, so an unknown id here is a bug in the list the
    /// app built — reported as an error rather than a panic, because a mistyped
    /// table should not take the session down.
    pub async fn run(
        &self,
        action: &str,
        args: &[String],
    ) -> Result<Box<dyn CliReport>, ToolError> {
        // Local and single-tenant, exactly as the stdio MCP profile: the local
        // implementations document that they ignore the caller.
        let caller = CallerAuth::default();
        match action {
            "status" => {
                // Nothing to take, so what was typed is refused rather than
                // silently dropped.
                if !args.is_empty() {
                    return Err(ToolError::new("usage: /status"));
                }
                Ok(Box::new(self.status.status(&caller).await?))
            }
            // The filter is applied to the report, not asked of the port —
            // the same `filtered` the CLI leaf calls, so both surfaces mean
            // the same thing by "declared by this provider".
            "list_models" => Ok(Box::new(
                self.models
                    .list_models(&caller)
                    .await?
                    .filtered(args.first().map(String::as_str)),
            )),
            "route" => {
                let Some(model) = args.first() else {
                    return Err(ToolError::new("usage: /preview <model>"));
                };
                Ok(Box::new(
                    self.route
                        .route(RouteInput {
                            model: model.clone(),
                            prompt: None,
                        })
                        .await?,
                ))
            }
            other => Err(ToolError::new(format!("no session action `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::actions::status::DaemonStatus;

    /// Write `yaml` as a `bitrouter.yaml` in a fresh temp dir and return the
    /// source pointing at it.
    fn config_source(dir: &Path, yaml: &str) -> ConfigSource {
        let path = dir.join("bitrouter.yaml");
        std::fs::write(&path, yaml).expect("write config");
        ConfigSource::File(path)
    }

    /// One active provider declaring one model.
    const ONE_MODEL: &str = r#"
providers:
  demo:
    api_base: https://api.example.test
    api_key: sk-test
    active: true
    models:
      - id: demo-model
"#;

    /// The third surface answers with the second's bytes.
    ///
    /// `bitrouter status` constructs `DaemonStatus` and calls `report()`;
    /// `/status` goes through `SessionPorts`. This holds `open` to constructing
    /// the action the leaf constructs, from the same source and the same
    /// socket — a mismatch shows up as a different `socket` in the report.
    ///
    /// What it does not catch is a live-daemon divergence: no daemon runs under
    /// test, which is the same limit `both_surfaces_produce_the_same_report`
    /// has. A shared type stops the shapes diverging, not the contents.
    #[tokio::test]
    async fn the_session_surface_answers_with_the_cli_leafs_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        let socket = dir.path().join("bitrouter.sock");

        let leaf = DaemonStatus::new(socket.clone(), Some(source.clone()))
            .report()
            .await
            .expect("cli surface");
        let ports = SessionPorts::open(source, socket);
        let session = ports.run("status", &[]).await.expect("session surface");

        assert_eq!(
            serde_json::to_value(&leaf).expect("leaf json"),
            serde_json::to_value(session.as_ref()).expect("session json"),
            "the session surface must answer with the leaf's bytes"
        );
    }

    /// And it reaches the screen as the CLI's own plain rendering — the same
    /// renderer, with the palette off, so there is one human view of a report
    /// rather than one per surface.
    #[tokio::test]
    async fn the_session_surface_renders_what_the_cli_renders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        let socket = dir.path().join("bitrouter.sock");

        let leaf = DaemonStatus::new(socket.clone(), Some(source.clone()))
            .report()
            .await
            .expect("cli surface");
        let ports = SessionPorts::open(source, socket);
        let session = ports.run("status", &[]).await.expect("session surface");

        let human = crate::output::Output::new(crate::output::Format::Human);
        let expected = human.render_to_vec(&leaf);
        assert_eq!(
            human.render_to_vec(session.as_ref()),
            expected,
            "the notice's bytes are the CLI's plain rendering of the same report"
        );
        assert!(
            !expected.is_empty(),
            "a report that renders to nothing would make this test vacuous"
        );
    }

    /// `/models` answers with what `bitrouter models --provider` answers,
    /// filter included — the filter is the report's, so both surfaces read
    /// "declared by this provider" the same way.
    #[tokio::test]
    async fn the_models_surface_answers_with_the_cli_leafs_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        let socket = dir.path().join("bitrouter.sock");

        let leaf =
            crate::actions::models::RoutableModels::new(source.clone(), Some(socket.clone()))
                .report()
                .await
                .expect("cli surface")
                .filtered(Some("demo"));
        let ports = SessionPorts::open(source, socket);
        let session = ports
            .run("list_models", &["demo".to_string()])
            .await
            .expect("session surface");

        assert_eq!(
            serde_json::to_value(&leaf).expect("leaf json"),
            serde_json::to_value(session.as_ref()).expect("session json"),
        );
        assert!(!leaf.models.is_empty(), "the fixture declares one model");
    }

    /// `/preview <model>` answers with what `bitrouter route <model>` answers.
    #[tokio::test]
    async fn the_preview_surface_answers_with_the_cli_leafs_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        let socket = dir.path().join("bitrouter.sock");

        let leaf = crate::actions::route::RouteAction::new(source.clone(), Some(socket.clone()))
            .report(RouteInput {
                model: "demo-model".to_string(),
                prompt: None,
            })
            .await
            .expect("cli surface");
        let ports = SessionPorts::open(source, socket);
        let session = ports
            .run("route", &["demo-model".to_string()])
            .await
            .expect("session surface");

        assert_eq!(
            serde_json::to_value(&leaf).expect("leaf json"),
            serde_json::to_value(session.as_ref()).expect("session json"),
        );
    }

    /// `/preview` with nothing to preview says how to use it. A missing
    /// argument is a typo, not a crash.
    #[tokio::test]
    async fn preview_without_a_model_answers_with_its_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ports = SessionPorts::open(
            config_source(dir.path(), ONE_MODEL),
            dir.path().join("bitrouter.sock"),
        );
        match ports.run("route", &[]).await {
            Ok(_) => panic!("a model is required"),
            Err(error) => assert!(format!("{error}").contains("usage: /preview"), "{error}"),
        }
    }

    /// An id the table does not carry is an error, not a panic: a mistyped
    /// command list must not take the session down.
    #[tokio::test]
    async fn an_unknown_action_is_reported_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        let ports = SessionPorts::open(source, dir.path().join("bitrouter.sock"));
        // `Box<dyn CliReport>` is not `Debug`, so this cannot be `expect_err`.
        match ports.run("not_a_row", &[]).await {
            Ok(_) => panic!("an unknown id must not resolve to a report"),
            Err(error) => assert!(format!("{error}").contains("not_a_row"), "{error}"),
        }
    }
}
