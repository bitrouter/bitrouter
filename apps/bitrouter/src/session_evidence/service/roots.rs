//! Keep native profiles separate even when one ACP connection creates sessions
//! with different subprocess environments. Operation ids bind async responses.

use super::*;

impl ControllerEvidence {
    async fn register_root(&self, root: NativeRoot) -> Result<RootContext> {
        let _guard = self.root_gate.lock().await;
        {
            let state = self.state.lock().await;
            if let Some(context) = state.roots.get(&root.namespace) {
                return Ok(context.clone());
            }
            ensure!(state.roots.len() < MAX_GRAPH_ITEMS, "native profile limit");
        }
        let spool = self.spool.join(format!(
            "root-{}",
            root.namespace
                .strip_prefix("sha256:")
                .context("native namespace digest")?
        ));
        private_directory(&spool).await?;
        let spool = tokio::fs::canonicalize(spool).await?;
        let journal = Arc::new(
            Journal::new(
                self.store.clone(),
                SourceDescriptor {
                    namespace: root.namespace.clone(),
                    harness: root.harness,
                    format: SourceFormat::Acp,
                    locator: format!("controller:{}", self.controller_id),
                    node: None,
                },
                self.producer_version.clone(),
            )
            .await?,
        );
        journal
            .append(
                json!({"method":"controller/root_registered","phase":"metadata",
            "payload":{"native_root":root.directory,"namespace":root.namespace,
                "spool":spool,"harness":root.harness},
            "observed_at":chrono::Utc::now().to_rfc3339()}),
            )
            .await?;
        let namespace = root.namespace.clone();
        let context = RootContext {
            collector: NativeCollector::new(self.store.clone(), root)?,
            spool,
            journal,
        };
        self.state
            .lock()
            .await
            .roots
            .insert(namespace, context.clone());
        Ok(context)
    }

    pub(super) async fn prepare_claude_session(
        &self,
        operation_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        super::super::types::identifier(operation_id)?;
        let requested_id = params.get("sessionId").and_then(Value::as_str);
        if matches!(method, "session/load" | "session/resume") {
            let state = self.state.lock().await;
            if requested_id.is_some_and(|id| state.uncertain_queries.contains(id)) {
                let context = state
                    .roots
                    .get(&self.collector.root().namespace)
                    .context("controller profile unavailable")?
                    .clone();
                drop(state);
                // The adapter may reuse a Query and ignore these options. While
                // its lifetime is unknown, do not read settings or inject hooks
                // that could change a request the native adapter would accept.
                context.journal.append(json!({"operation_id":operation_id,"method":method,
                    "phase":"prepared","native_scope":"unresolved","payload":{"sessionId":requested_id},
                    "observed_at":chrono::Utc::now().to_rfc3339()})).await?;
                return Ok(params);
            }
        }
        let cwd = Path::new(
            params
                .get("cwd")
                .and_then(Value::as_str)
                .context("Claude session cwd is missing")?,
        );
        ensure!(cwd.is_absolute(), "Claude session cwd must be absolute");
        let fingerprint = claude_query_fingerprint(&params)?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let reused = if matches!(method, "session/load" | "session/resume") {
            let state = self.state.lock().await;
            session_id
                .as_ref()
                .and_then(|id| state.loaded.get(id))
                .filter(|query| query.fingerprint == fingerprint)
                .and_then(|query| state.roots.get(&query.namespace))
                .cloned()
        } else {
            None
        };
        // getOrCreateSession reuses an existing Query when cwd and MCP servers
        // match. In that case it ignores even explicitly changed env/settings.
        // Reading those ignored settings could itself change native behavior.
        if let Some(context) = reused {
            // An idle native process can disappear without an RPC error. If a
            // newly created Query would use another profile, its returned id
            // cannot distinguish recreation from reuse. Keep that ambiguity
            // explicit rather than treating this cache as native liveness proof.
            if !self
                .configured_claude_root(&params)
                .as_ref()
                .is_ok_and(|root| root.namespace == context.collector.root().namespace)
            {
                self.remember_prepared(operation_id, method, &params, &context, fingerprint, true)
                    .await?;
                let mut state = self.state.lock().await;
                if let Some(id) = session_id {
                    state.uncertain_queries.insert(id.clone());
                    state.loaded.remove(&id);
                    state.sessions.remove(&id);
                }
                return Ok(params);
            }
            self.remember_prepared(operation_id, method, &params, &context, fingerprint, true)
                .await?;
            return Ok(params);
        }
        let root = self.configured_claude_root(&params)?;
        let context = self.register_root(root).await?;
        let model_settings = claude_model_settings(self.claude_model_config.as_deref())?;
        let instrumented = super::super::claude_hooks::instrument(
            params,
            &context.spool,
            &self.executable,
            model_settings.as_ref(),
        )
        .await?;
        self.remember_prepared(
            operation_id,
            method,
            &instrumented,
            &context,
            fingerprint,
            false,
        )
        .await?;
        Ok(instrumented)
    }

    fn configured_claude_root(&self, params: &Value) -> Result<NativeRoot> {
        let cwd = Path::new(
            params
                .get("cwd")
                .and_then(Value::as_str)
                .context("Claude session cwd is missing")?,
        );
        ensure!(cwd.is_absolute(), "Claude session cwd must be absolute");
        let mut env = self.root_env.clone();
        // The adapter merges these options into the SDK subprocess environment
        // and sets its cwd to params.cwd. Do not persist the remaining env: it
        // can contain provider credentials and is not identity evidence.
        // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
        if let Some(overrides) = params.pointer("/_meta/claudeCode/options/env") {
            let overrides = overrides
                .as_object()
                .context("Claude env must be an object")?;
            for key in ["CLAUDE_CONFIG_DIR", "HOME", "USERPROFILE"] {
                if let Some(value) = overrides.get(key) {
                    env.insert(
                        key.into(),
                        value
                            .as_str()
                            .context("native root env must be a string")?
                            .into(),
                    );
                }
            }
        }
        // Use the captured effective environment, including removed inherited
        // variables, rather than reading this controller's ambient env again.
        let stripped = ["CLAUDE_CONFIG_DIR", "HOME", "USERPROFILE"].map(str::to_owned);
        native_root_at(Harness::ClaudeCode, &env, &stripped, cwd)
    }

    async fn remember_prepared(
        &self,
        operation_id: &str,
        method: &str,
        params: &Value,
        context: &RootContext,
        fingerprint: String,
        reused: bool,
    ) -> Result<()> {
        let root = context.collector.root();
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        context
            .journal
            .append(json!({"operation_id":operation_id,"method":method,
            "phase":"prepared","payload":{"sessionId":session_id,"cwd":params.get("cwd"),
                "native_root":root.directory,"namespace":root.namespace,
                "query_fingerprint":fingerprint,"query_may_be_reused":reused},
            "observed_at":chrono::Utc::now().to_rfc3339()}))
            .await?;
        let mut state = self.state.lock().await;
        ensure!(
            state.pending.contains_key(operation_id) || state.pending.len() < MAX_GRAPH_ITEMS,
            "pending native operation limit"
        );
        state.pending.insert(
            operation_id.into(),
            PendingScope {
                namespace: root.namespace.clone(),
                session_id,
                query_fingerprint: Some(fingerprint),
                method: method.into(),
                query_reused: reused,
            },
        );
        Ok(())
    }

    pub(super) async fn observation_context(
        &self,
        observation: &SessionObservation,
    ) -> Result<(RootContext, &'static str)> {
        let state = self.state.lock().await;
        let session_id = observation.payload.get("sessionId").and_then(Value::as_str);
        if observation.phase == "request" {
            super::super::types::identifier(&observation.operation_id)?;
            ensure!(
                !session_id.is_some_and(|id| state.ambiguous_sessions.contains(id))
                    || matches!(
                        observation.method.as_str(),
                        "session/close" | "session/delete"
                    ),
                "ACP session id belongs to multiple native profiles"
            );
            // A load can register its new Query before finishing history replay.
            // Response order alone therefore cannot order concurrent transitions
            // of the same session. Reject overlap before forwarding it upstream.
            ensure!(
                !session_id.is_some_and(|id| state.pending.values().any(|pending| pending
                    .session_id
                    .as_deref()
                    == Some(id)
                    && (lifecycle(&pending.method)
                        || (lifecycle(&observation.method)
                            && !matches!(
                                observation.method.as_str(),
                                "session/close" | "session/delete"
                            ))))),
                "native session transition overlaps another operation"
            );
        }
        let pending = (observation.phase == "response")
            .then(|| state.pending.get(&observation.operation_id))
            .flatten();
        // A lifecycle transition can retain the old Query or create a new one
        // before its RPC result. Notifications from either may arrive while
        // sessions still contains the old profile. Preserve them unbound.
        let transition_in_flight = observation.phase == "notification"
            && session_id.is_some_and(|id| {
                state.pending.values().any(|pending| {
                    pending.session_id.as_deref() == Some(id) && lifecycle(&pending.method)
                })
            });
        let uncertain = transition_in_flight
            || session_id
                .into_iter()
                .chain(
                    pending
                        .filter(|scope| scope.method != "session/fork")
                        .and_then(|scope| scope.session_id.as_deref()),
                )
                .any(|id| state.uncertain_queries.contains(id));
        if uncertain {
            return Ok((
                state
                    .roots
                    .get(&self.collector.root().namespace)
                    .context("controller profile unavailable")?
                    .clone(),
                "unresolved",
            ));
        }
        let (namespace, scope) = if let Some(pending) = pending {
            (pending.namespace.as_str(), "operation")
        } else if let Some(namespace) = session_id
            .filter(|id| !state.ambiguous_sessions.contains(*id))
            .and_then(|id| state.sessions.get(id))
        {
            (namespace.as_str(), "session")
        } else {
            (self.collector.root().namespace.as_str(), "controller")
        };
        Ok((
            state
                .roots
                .get(namespace)
                .context("native profile unavailable")?
                .clone(),
            scope,
        ))
    }

    pub(super) async fn finish_observation(
        &self,
        observation: &SessionObservation,
        root: &NativeRoot,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let session_id = observation
            .payload
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if observation.phase == "request" {
            ensure!(
                state.pending.contains_key(&observation.operation_id)
                    || state.pending.len() < MAX_GRAPH_ITEMS,
                "pending native operation limit"
            );
            state.pending.insert(
                observation.operation_id.clone(),
                PendingScope {
                    namespace: root.namespace.clone(),
                    session_id,
                    query_fingerprint: None,
                    method: observation.method.clone(),
                    query_reused: false,
                },
            );
        } else if observation.phase == "response" {
            let pending = state.pending.remove(&observation.operation_id);
            let requested = pending.as_ref().and_then(|scope| scope.session_id.clone());
            if observation.payload.get("error_code").is_some()
                && matches!(
                    observation.method.as_str(),
                    "session/load" | "session/resume"
                )
                && pending
                    .as_ref()
                    .is_some_and(|scope| scope.query_fingerprint.is_some() && !scope.query_reused)
                && let Some(id) = &requested
            {
                // The old Query was torn down, but this error may follow either
                // failed creation or successful creation with failed replay.
                // ACP does not distinguish them. Keep forwarding observations
                // without claiming a native profile until close confirms reset.
                state.loaded.remove(id);
                state.sessions.remove(id);
                state.uncertain_queries.insert(id.clone());
            }
            if root.harness == Harness::ClaudeCode
                && observation.method == "session/prompt"
                && observation.payload.get("error_code").is_some()
                && let Some(id) = &requested
                && state.sessions.contains_key(id)
            {
                // A failed prompt can include native process eviction. Its RPC
                // error does not prove whether the adapter retained the Query.
                state.loaded.remove(id);
                state.sessions.remove(id);
                state.uncertain_queries.insert(id.clone());
            }
            if matches!(
                observation.method.as_str(),
                "session/close" | "session/delete"
            ) && observation.payload.get("error_code").is_none()
            {
                if let Some(id) = session_id.or(requested) {
                    state.loaded.remove(&id);
                    state.sessions.remove(&id);
                    state.ambiguous_sessions.remove(&id);
                    state.uncertain_queries.remove(&id);
                }
                return Ok(());
            }
            if observation.payload.get("error_code").is_some()
                || !matches!(
                    observation.method.as_str(),
                    "session/new" | "session/load" | "session/resume" | "session/fork"
                )
            {
                return Ok(());
            }
            // ACP load/resume results may omit sessionId. Only a successful
            // lifecycle result may bind the original requested native id.
            // https://agentclientprotocol.com/protocol/v1/session-setup
            let session_id = session_id.or_else(|| {
                matches!(
                    observation.method.as_str(),
                    "session/load" | "session/resume"
                )
                .then(|| requested.clone())
                .flatten()
            });
            if let Some(session_id) = session_id {
                if state.uncertain_queries.contains(&session_id) {
                    return Ok(());
                }
                // Only successful lifecycle responses promote ACP ids to native
                // roots. Notification ids can be synthetic subagent views.
                // https://github.com/agentclientprotocol/codex-acp
                let node = NodeKey {
                    namespace: root.namespace.clone(),
                    harness: root.harness,
                    native_id: session_id.clone(),
                    agent_id: None,
                };
                node.validate()?;
                ensure!(
                    state.nodes.contains(&node) || state.nodes.len() < MAX_GRAPH_ITEMS,
                    "native execution node limit"
                );
                let replaced = requested.as_ref() == Some(&session_id)
                    && matches!(
                        observation.method.as_str(),
                        "session/load" | "session/resume"
                    );
                if !replaced
                    && state
                        .sessions
                        .get(&session_id)
                        .is_some_and(|namespace| namespace != &root.namespace)
                {
                    state.ambiguous_sessions.insert(session_id.clone());
                } else {
                    state
                        .sessions
                        .insert(session_id.clone(), root.namespace.clone());
                    state.ambiguous_sessions.remove(&session_id);
                }
                if let Some(fingerprint) = pending.and_then(|scope| scope.query_fingerprint) {
                    state.loaded.insert(
                        session_id,
                        LoadedQuery {
                            namespace: root.namespace.clone(),
                            fingerprint,
                        },
                    );
                }
                state.nodes.insert(node);
            }
        }
        Ok(())
    }
}

fn lifecycle(method: &str) -> bool {
    matches!(
        method,
        "session/new"
            | "session/load"
            | "session/resume"
            | "session/fork"
            | "session/close"
            | "session/delete"
    )
}

fn claude_query_fingerprint(params: &Value) -> Result<String> {
    // Match the adapter's equivalence relation, retaining all MCP config in a
    // digest only. Neither credentials nor server commands enter the journal.
    // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
    let mut servers = match params.get("mcpServers").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_array()
            .context("MCP servers must be an array")?
            .clone(),
        None => vec![],
    };
    ensure!(servers.len() <= MAX_GRAPH_ITEMS, "MCP server limit");
    for server in &servers {
        ensure!(
            server.get("name").and_then(Value::as_str).is_some(),
            "MCP server name missing"
        );
    }
    servers.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    canonical_digest(&(params.get("cwd"), servers))
}

fn claude_model_settings(raw: Option<&str>) -> Result<Option<Value>> {
    // An injected settings object replaces the adapter's model-config fallback.
    // Preserve that effective base before adding lifecycle hooks.
    // https://github.com/agentclientprotocol/claude-agent-acp/blob/main/src/acp-agent.ts
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(raw)?;
    let config = parsed
        .as_object()
        .context("CLAUDE_MODEL_CONFIG must be an object")?;
    let mut settings = serde_json::Map::new();
    for key in ["modelOverrides", "availableModels"] {
        if let Some(value) = config.get(key).filter(|value| match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => value.as_f64() != Some(0.0),
            Value::String(value) => !value.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        }) {
            settings.insert(key.into(), value.clone());
        }
    }
    Ok((!settings.is_empty()).then_some(Value::Object(settings)))
}
