//! Aggregating [`Executor`] — fan-out across many upstream MCP servers behind
//! one virtual `POST /mcp` endpoint.
//!
//! Wraps an inner [`Executor`] that only knows how to handle
//! [`McpTarget::Direct`]; this layer is responsible for resolving
//! [`McpTarget::Aggregate`] into N direct calls, merging their results, and
//! applying the per-server `tool_prefix` rewrites the MCP-gateway ecosystem
//! has converged on (MetaMCP, mcphub, Pluggedin, Docker MCP Gateway,
//! Cloudflare portals).
//!
//! Per-method behaviour (see issue #483 for the dispatch table):
//!
//! | Inbound | Behaviour |
//! |---|---|
//! | `tools/list` | fan-out → concat tools, prepend `tool_prefix` to each name |
//! | `resources/list` | fan-out → concat (no prefix, URIs are globally addressable) |
//! | `resources/templates/list` | fan-out → concat |
//! | `prompts/list` | fan-out → concat, prepend `tool_prefix` to each name |
//! | `tools/call` | strip prefix from `params.name`, dispatch to owning member |
//! | `resources/read` | resolve the **single** owning member, or error; skill URIs round-trip the label |
//! | `prompts/get` | strip prefix from `params.name`, dispatch to owning member |
//! | `skills/list` | fan-out → namespace each entry's URIs under its member's label, concat |
//! | `skills/get` | strip label from `params.uri`, dispatch to owning member, re-namespace |
//!
//! Failure semantics for fan-out: **partial-success** by default. Servers that
//! responded contribute their results; servers that failed are listed under
//! `result._bitrouterErrors = [{server, error}]`. The `_bitrouter` prefix
//! namespaces gateway-injected fields so they cannot collide with anything the
//! upstream method's result schema may carry now or later.
//!
//! ## `resources/read` never guesses
//!
//! Resource URIs are not prefixed the way tool names are — they are whatever
//! the upstream chose, so two members can legitimately serve the same URI.
//! This dispatcher used to try each member in turn and return the first
//! success, which let configuration order silently decide which server
//! answered. That is the impersonation surface SEP-2640 names for skills, and
//! a silent misroute for every other resource.
//!
//! `AggregatingExecutor::resolve_owner` replaces it. It resolves exactly one
//! owning member or returns an error naming the ambiguity; it never picks. See
//! that method for the two resolution tiers.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};

use super::skills::{
    SKILL_SCHEME, SKILLS_GET_METHOD, SKILLS_LIST_METHOD, namespace_entry, namespace_uri,
    strip_label,
};
use super::{
    AggregateMember, Executor, McpRequest, McpResponse, McpStreamPart, McpTarget, ServerSelector,
};
use crate::error::{BitrouterError, Result};

/// Whether `uri` falls under `template`'s literal prefix — the text before its
/// first RFC 6570 expansion.
///
/// Full template matching is out of scope: this is a candidate filter, and a
/// URI matching several members' templates is reported as an ambiguity rather
/// than resolved by a guess.
fn matches_template_prefix(uri: &str, template: &str) -> bool {
    let literal = template.split('{').next().unwrap_or(template);
    !literal.is_empty() && uri.starts_with(literal)
}

struct OwnerMatches<'m> {
    owners: Vec<&'m AggregateMember>,
    failures: Vec<String>,
}

/// Fan-out wrapper over an inner [`Executor`]. Passes [`McpTarget::Direct`]
/// straight through to the inner; handles [`McpTarget::Aggregate`] by issuing
/// per-member direct calls and merging the results.
pub struct AggregatingExecutor<E: Executor> {
    inner: Arc<E>,
}

impl<E: Executor> AggregatingExecutor<E> {
    /// Wrap an inner executor.
    pub fn new(inner: Arc<E>) -> Self {
        Self { inner }
    }

    /// Build a per-member direct request from the aggregate request. Used by
    /// fan-out and prefix-routed methods.
    fn direct_request(request: &McpRequest, member: &AggregateMember) -> McpRequest {
        // The aggregate's `request_id` is the inbound id from the JSON-RPC
        // client. Per-member sub-calls reuse it so settled/observe hooks can
        // correlate the whole fan-out as one logical request.
        McpRequest {
            request_id: request.request_id.clone(),
            selector: ServerSelector::Direct(member.server_name.clone()),
            method: request.method.clone(),
            params: request.params.clone(),
            caller: request.caller.clone(),
            headers: request.headers.clone(),
        }
    }

    fn direct_target(member: &AggregateMember) -> McpTarget {
        McpTarget::Direct {
            server_name: member.server_name.clone(),
            transport: member.transport.clone(),
        }
    }

    async fn fanout_list(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
        list_key: &str,
        prefix_field: Option<&str>,
    ) -> Result<McpResponse> {
        // Fan out concurrently — cold-cache latency is Σ→max(per-server). Errors
        // are collected as data (partial-success semantics), so there is no
        // short-circuit and `join_all` is correct here. Order of members is
        // preserved in the merged result because `join_all` returns results in
        // input order regardless of completion order.
        let calls = members.iter().map(|member| {
            let sub_req = Self::direct_request(request, member);
            let target = Self::direct_target(member);
            async move { (member, self.inner.execute(&target, &sub_req).await) }
        });
        let outcomes = futures::future::join_all(calls).await;

        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut errors: Vec<serde_json::Value> = Vec::new();
        for (member, outcome) in outcomes {
            match outcome {
                Ok(resp) => match resp.result.get(list_key).and_then(|v| v.as_array()) {
                    Some(arr) => {
                        for entry in arr {
                            let mut entry = entry.clone();
                            if let Some(field) = prefix_field
                                && let Some(obj) = entry.as_object_mut()
                                && let Some(name) = obj.get_mut(field).and_then(|v| v.as_str())
                            {
                                let prefixed = format!("{}{name}", member.tool_prefix);
                                obj.insert(field.to_string(), prefixed.into());
                            }
                            items.push(entry);
                        }
                    }
                    None => errors.push(serde_json::json!({
                        "server": member.server_name,
                        "error": format!(
                            "upstream response missing or non-array '{list_key}'",
                        ),
                    })),
                },
                Err(e) => errors.push(serde_json::json!({
                    "server": member.server_name,
                    "error": e.to_string(),
                })),
            }
        }
        let mut result = serde_json::json!({ list_key: items });
        if !errors.is_empty() {
            result["_bitrouterErrors"] = serde_json::Value::Array(errors);
        }
        Ok(McpResponse {
            request_id: request.request_id.clone(),
            result,
        })
    }

    /// Resolve a prefix-routed request (`tools/call` / `prompts/get`) into the
    /// per-member direct request and target. Returns the rewritten `name`
    /// stripped of its prefix.
    ///
    /// Longest-prefix wins so `a__` does not steal calls intended for `ab__`
    /// when both servers are registered.
    fn resolve_prefixed(
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<(McpRequest, McpTarget)> {
        let name = request
            .params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "mcp aggregate {}: params.name is required",
                    request.method
                ))
            })?;
        let (member, stripped) = members
            .iter()
            .filter_map(|m| name.strip_prefix(&m.tool_prefix).map(|s| (m, s)))
            .max_by_key(|(m, _)| m.tool_prefix.len())
            .ok_or_else(|| {
                BitrouterError::NotFound(format!(
                    "mcp aggregate {}: no member prefix matches '{name}'",
                    request.method
                ))
            })?;
        let mut sub_req = Self::direct_request(request, member);
        if let Some(obj) = sub_req.params.as_object_mut() {
            obj.insert("name".to_string(), stripped.to_string().into());
        }
        Ok((sub_req, Self::direct_target(member)))
    }

    /// Strip the `tool_prefix` from `params.name` and dispatch to the owning
    /// member. Used by `tools/call` and `prompts/get`.
    async fn prefixed_dispatch(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        let (sub_req, target) = Self::resolve_prefixed(members, request)?;
        self.inner.execute(&target, &sub_req).await
    }

    /// `skills/list` — fan out, namespace every entry under its member's
    /// label, and concat.
    ///
    /// Entries that cannot be namespaced (no `uri`, or a scheme this gateway
    /// does not aggregate) are **skipped with a note**, never passed through
    /// unrewritten: an unrewritten entry is exactly the cross-origin collision
    /// the namespacing exists to prevent. This is also the mitigation for an
    /// upstream whose URI structure predates the SEP — it is reported as
    /// skipped rather than silently mis-rewritten.
    ///
    /// v1 returns no `nextCursor`: each member's own pagination is followed to
    /// exhaustion by the inner executor, and the merged result is one page.
    async fn fanout_skills(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        let calls = members.iter().map(|member| {
            let sub_req = Self::direct_request(request, member);
            let target = Self::direct_target(member);
            async move { (member, self.inner.execute(&target, &sub_req).await) }
        });

        let mut skills: Vec<serde_json::Value> = Vec::new();
        let mut errors: Vec<serde_json::Value> = Vec::new();
        for (member, outcome) in futures::future::join_all(calls).await {
            match outcome {
                Ok(resp) => {
                    let Some(entries) = resp.result.get("skills").and_then(|v| v.as_array()) else {
                        errors.push(serde_json::json!({
                            "server": member.server_name,
                            "error": "upstream response missing or non-array 'skills'",
                        }));
                        continue;
                    };
                    let mut skipped = 0usize;
                    for entry in entries {
                        match namespace_entry(&member.server_name, entry) {
                            Some(namespaced) => skills.push(namespaced),
                            None => skipped += 1,
                        }
                    }
                    if skipped > 0 {
                        errors.push(serde_json::json!({
                            "server": member.server_name,
                            "error": format!(
                                "{skipped} skill(s) skipped: only '{SKILL_SCHEME}' URIs can be \
                                 namespaced across members. Reach them on this server's direct \
                                 route instead.",
                            ),
                        }));
                    }
                }
                Err(e) => errors.push(serde_json::json!({
                    "server": member.server_name,
                    "error": e.to_string(),
                })),
            }
        }
        let mut result = serde_json::json!({ "skills": skills });
        if !errors.is_empty() {
            result["_bitrouterErrors"] = serde_json::Value::Array(errors);
        }
        Ok(McpResponse {
            request_id: request.request_id.clone(),
            result,
        })
    }

    /// `skills/get` — strip the label off `params.uri`, dispatch to the owning
    /// member, and namespace the returned entry back.
    async fn get_skill(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        let uri = request
            .params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BitrouterError::bad_request(
                    "mcp aggregate skills/get: params.uri is required".to_string(),
                )
            })?;
        let (member, upstream_uri) = members
            .iter()
            .find_map(|m| strip_label(&m.server_name, uri).map(|rest| (m, rest)))
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "mcp aggregate skills/get: '{uri}' names no configured member. Aggregated \
                     skill URIs are '{SKILL_SCHEME}<server>/<skill-path>/SKILL.md'."
                ))
            })?;

        let mut sub_req = Self::direct_request(request, member);
        if let Some(params) = sub_req.params.as_object_mut() {
            params.insert("uri".to_string(), upstream_uri.clone().into());
        }
        let response = self
            .inner
            .execute(&Self::direct_target(member), &sub_req)
            .await?;

        let entry = response
            .result
            .get("skill")
            .ok_or_else(|| BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate skills/get: '{}' returned no 'skill' object",
                    member.server_name
                ),
            })?;
        let returned_uri = entry
            .get("uri")
            .and_then(|value| value.as_str())
            .ok_or_else(|| BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate skills/get: '{}' returned a skill without a string uri",
                    member.server_name
                ),
            })?;
        if returned_uri != upstream_uri {
            return Err(BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate skills/get: '{}' returned '{returned_uri}' when \
                     '{upstream_uri}' was requested",
                    member.server_name
                ),
            });
        }
        let namespaced = namespace_entry(&member.server_name, entry).ok_or_else(|| {
            BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate skills/get: '{}' returned a malformed skill entry that \
                     cannot be namespaced under '{SKILL_SCHEME}'",
                    member.server_name
                ),
            }
        })?;
        if namespaced.get("uri").and_then(|value| value.as_str()) != Some(uri) {
            return Err(BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate skills/get: '{}' returned a skill that does not map back to \
                     '{uri}'",
                    member.server_name
                ),
            });
        }
        Ok(McpResponse {
            request_id: request.request_id.clone(),
            result: serde_json::json!({ "skill": namespaced }),
        })
    }

    /// `resources/read` — resolve the one member that owns `params.uri` and
    /// dispatch there.
    async fn read_resource(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        let uri = request
            .params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BitrouterError::bad_request(
                    "mcp aggregate resources/read: params.uri is required".to_string(),
                )
            })?;
        let (member, upstream_uri) = self.resolve_owner(members, request, uri).await?;
        // Tier 1 rewrote the URI; tier 2 forwarded it verbatim. Only the
        // rewritten case needs undoing on the way back.
        let was_namespaced = upstream_uri != uri;
        let mut sub_req = Self::direct_request(request, member);
        if let Some(obj) = sub_req.params.as_object_mut() {
            obj.insert("uri".to_string(), upstream_uri.into());
        }
        let mut response = self
            .inner
            .execute(&Self::direct_target(member), &sub_req)
            .await?;

        // Put the gateway's namespace back on every returned `contents[].uri`.
        //
        // The upstream answers in its own namespace, but the client asked in
        // ours, and SEP-2640 has hosts verify a read against the entry's
        // `resources` set — which this gateway namespaced. Returning the
        // upstream's URI would look to a conforming host like a read of a file
        // outside the skill, which the SEP says to treat as a verification
        // failure. This is the same re-namespacing `skills/get` applies to its
        // reply, for the same reason.
        if was_namespaced
            && let Some(contents) = response
                .result
                .get_mut("contents")
                .and_then(|v| v.as_array_mut())
        {
            for item in contents {
                if let Some(obj) = item.as_object_mut()
                    && let Some(u) = obj.get("uri").and_then(|v| v.as_str())
                    && let Some(renamespaced) = namespace_uri(&member.server_name, u)
                {
                    obj.insert("uri".to_string(), renamespaced.into());
                }
            }
        }
        Ok(response)
    }

    /// The single member that owns `uri`, or an error that names the
    /// ambiguity rather than resolving it by guessing.
    ///
    /// Two tiers:
    ///
    /// 1. **Label-prefixed skill URIs.** `skill://<label>/…` whose leading
    ///    segment names a configured member routes to that member with no
    ///    upstream calls. This is the inverse of the namespacing the gateway
    ///    applies when aggregating `skills/list`, and it is the only tier that
    ///    works for skills — SEP-2640 makes a skill readable whether or not it
    ///    appears in any `resources/list`.
    /// 2. **Everything else** resolves against an ownership index built from
    ///    the members' own enumerations (see [`Self::index_owners`]).
    ///
    /// A URI no member enumerates is an error rather than a scan: the
    /// aggregate endpoint serves what its members publish, and the direct
    /// route (`POST /mcp/{server}`) remains available for anything else.
    async fn resolve_owner<'m>(
        &self,
        members: &'m [AggregateMember],
        request: &McpRequest,
        uri: &str,
    ) -> Result<(&'m AggregateMember, String)> {
        // Tier 1 — a URI this gateway namespaced. The label is ours, not the
        // upstream's, so it MUST come back off before the sub-request goes out:
        // the member has never heard of `skill://<label>/…`.
        if let Some((member, upstream_uri)) = members
            .iter()
            .find_map(|m| strip_label(&m.server_name, uri).map(|rest| (m, rest)))
        {
            return Ok((member, upstream_uri));
        }
        // Tier 2 — an upstream's own URI, which we never rewrote. Forward it
        // exactly as the client sent it.
        let owners = self.index_owners(members, request, uri).await?;
        match owners.as_slice() {
            [only] => Ok((only, uri.to_string())),
            [] => Err(BitrouterError::bad_request(format!(
                "mcp aggregate resources/read: no configured member enumerates '{uri}'. \
                 The aggregate endpoint serves the resources its members publish; \
                 read this one from its server's direct route (POST /mcp/{{server}})."
            ))),
            many => Err(BitrouterError::bad_request(format!(
                "mcp aggregate resources/read: '{uri}' is served by {} members ({}); \
                 the aggregate endpoint will not choose between them. Use the direct \
                 route (POST /mcp/{{server}}) to name the one you mean.",
                many.len(),
                many.iter()
                    .map(|m| m.server_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ))),
        }
    }

    /// Members that enumerate `uri`, in member order.
    ///
    /// Exact `resources/list` matches are consulted first; only when none
    /// match are `resources/templates/list` templates considered, so the
    /// common case costs one list call per member. Both go through the inner
    /// executor — the caching layer in the assembled stack
    /// (`AggregatingExecutor → CachingExecutor → RmcpExecutor`) — so warm
    /// lookups do not reach upstream at all. Each tier fans out exactly once
    /// per `resources/read`, so a single request never enumerates twice.
    async fn index_owners<'m>(
        &self,
        members: &'m [AggregateMember],
        request: &McpRequest,
        uri: &str,
    ) -> Result<Vec<&'m AggregateMember>> {
        let exact = self
            .owners_matching(members, request, "resources/list", "resources", |entry| {
                entry.get("uri").and_then(|v| v.as_str()) == Some(uri)
            })
            .await;
        if !exact.failures.is_empty() {
            return Err(BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate resources/read: ownership of '{uri}' is indeterminate; \
                     resources/list failed for {}",
                    exact.failures.join(", ")
                ),
            });
        }
        if !exact.owners.is_empty() {
            return Ok(exact.owners);
        }
        let templates = self
            .owners_matching(
                members,
                request,
                "resources/templates/list",
                "resourceTemplates",
                |entry| {
                    entry
                        .get("uriTemplate")
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| matches_template_prefix(uri, t))
                },
            )
            .await;
        if !templates.failures.is_empty() {
            return Err(BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "mcp aggregate resources/read: ownership of '{uri}' is indeterminate; \
                     resources/templates/list failed for {}",
                    templates.failures.join(", ")
                ),
            });
        }
        Ok(templates.owners)
    }

    /// Members whose `method` enumeration contains an entry satisfying
    /// `predicate`, plus every member whose enumeration was indeterminate.
    /// Callers must reject an ownership decision while failures are present:
    /// dropping a failed member could turn a real collision into an apparent
    /// unique owner and silently misroute the read.
    async fn owners_matching<'m>(
        &self,
        members: &'m [AggregateMember],
        request: &McpRequest,
        method: &str,
        list_key: &str,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> OwnerMatches<'m> {
        let calls = members.iter().map(|member| {
            let mut sub_req = Self::direct_request(request, member);
            sub_req.method = method.to_string();
            sub_req.params = serde_json::json!({});
            let target = Self::direct_target(member);
            async move { (member, self.inner.execute(&target, &sub_req).await) }
        });
        let mut owners = Vec::new();
        let mut failures = Vec::new();
        for (member, outcome) in futures::future::join_all(calls).await {
            match outcome {
                Ok(resp) => {
                    let Some(entries) = resp.result.get(list_key).and_then(|v| v.as_array()) else {
                        failures.push(format!(
                            "{} (missing or non-array '{list_key}')",
                            member.server_name
                        ));
                        continue;
                    };
                    let matched = entries.iter().any(&predicate);
                    if matched {
                        owners.push(member);
                    }
                }
                Err(e) => failures.push(format!("{} ({e})", member.server_name)),
            }
        }
        OwnerMatches { owners, failures }
    }

    async fn dispatch_aggregate(
        &self,
        members: &[AggregateMember],
        request: &McpRequest,
    ) -> Result<McpResponse> {
        if members.is_empty() {
            // Empty aggregate (e.g. every server set `aggregate: false`) —
            // return an empty list shape so list calls keep parsing on the
            // client. Non-list methods surface as "method not found".
            return match request.method.as_str() {
                "tools/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "tools": [] }),
                }),
                "resources/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "resources": [] }),
                }),
                "resources/templates/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "resourceTemplates": [] }),
                }),
                "prompts/list" => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "prompts": [] }),
                }),
                // The gateway declares the skills extension optimistically
                // (it cannot know its members' capabilities at handshake
                // time), so an empty catalog must answer with an empty list
                // rather than an error.
                SKILLS_LIST_METHOD => Ok(McpResponse {
                    request_id: request.request_id.clone(),
                    result: serde_json::json!({ "skills": [] }),
                }),
                other => Err(BitrouterError::NotFound(format!(
                    "mcp aggregate {other}: no member servers configured"
                ))),
            };
        }
        match request.method.as_str() {
            "tools/list" => {
                self.fanout_list(members, request, "tools", Some("name"))
                    .await
            }
            "resources/list" => self.fanout_list(members, request, "resources", None).await,
            "resources/templates/list" => {
                self.fanout_list(members, request, "resourceTemplates", None)
                    .await
            }
            "prompts/list" => {
                self.fanout_list(members, request, "prompts", Some("name"))
                    .await
            }
            "tools/call" | "prompts/get" => self.prefixed_dispatch(members, request).await,
            "resources/read" => self.read_resource(members, request).await,
            SKILLS_LIST_METHOD => self.fanout_skills(members, request).await,
            SKILLS_GET_METHOD => self.get_skill(members, request).await,
            other => Err(BitrouterError::NotFound(format!(
                "mcp aggregate {other}: not supported on the aggregate endpoint"
            ))),
        }
    }
}

#[async_trait]
impl<E: Executor + 'static> Executor for AggregatingExecutor<E> {
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
        match target {
            McpTarget::Direct { .. } => self.inner.execute(target, request).await,
            McpTarget::Aggregate { members } => self.dispatch_aggregate(members, request).await,
        }
    }

    async fn execute_streaming(
        &self,
        target: &McpTarget,
        request: &McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        match target {
            McpTarget::Direct { .. } => self.inner.execute_streaming(target, request).await,
            McpTarget::Aggregate { members } => {
                // `tools/call` (and `prompts/get`) is the only aggregate-mode
                // method that meaningfully streams — it routes to a single
                // member by prefix, so the inner stream passes through.
                if matches!(request.method.as_str(), "tools/call" | "prompts/get") {
                    let (sub_req, target) = Self::resolve_prefixed(members, request)?;
                    return self.inner.execute_streaming(&target, &sub_req).await;
                }
                // For fan-out methods (list-shaped, `resources/read`), there
                // is no meaningful intermediate stream — buffer the merged
                // response and emit a single `Final`.
                let response = self.dispatch_aggregate(members, request).await?;
                Ok(stream::once(async move { Ok(McpStreamPart::Final(response)) }).boxed())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::mcp::transport::McpTransport;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct CannedExecutor {
        responses: Mutex<HashMap<String, Result<serde_json::Value>>>,
        /// Every `(server, params.uri)` the aggregator actually dispatched.
        ///
        /// Asserting only on *which member* answered cannot catch a wrong
        /// `params.uri`, which is how a gateway-namespaced URI once reached an
        /// upstream that had never heard of it. Tests assert on this.
        seen_uris: Mutex<Vec<(String, String)>>,
    }

    impl CannedExecutor {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                seen_uris: Mutex::new(Vec::new()),
            }
        }

        /// The `params.uri` values dispatched to `server`, in call order.
        fn uris_for(&self, server: &str) -> Vec<String> {
            self.seen_uris
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| s == server)
                .map(|(_, u)| u.clone())
                .collect()
        }
        fn with(self, server: &str, value: serde_json::Value) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(server.to_string(), Ok(value));
            self
        }
        fn with_err(self, server: &str, err: BitrouterError) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert(server.to_string(), Err(err));
            self
        }
    }

    #[async_trait]
    impl Executor for CannedExecutor {
        async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
            let name = match target {
                McpTarget::Direct { server_name, .. } => server_name.clone(),
                McpTarget::Aggregate { .. } => panic!("inner saw aggregate"),
            };
            if let Some(uri) = request.params.get("uri").and_then(|v| v.as_str()) {
                self.seen_uris
                    .lock()
                    .unwrap()
                    .push((name.clone(), uri.to_string()));
            }
            let mut map = self.responses.lock().unwrap();
            // Allow per-server multi-method canned responses by keying on
            // "{server}:{method}" when the test prepared one, falling back to
            // the bare server key.
            let composite = format!("{name}:{}", request.method);
            let entry = map
                .remove(&composite)
                .or_else(|| map.remove(&name))
                .unwrap_or_else(|| {
                    Err(BitrouterError::internal(format!(
                        "no canned response for '{name}' / '{composite}'"
                    )))
                });
            entry.map(|result| McpResponse {
                request_id: request.request_id.clone(),
                result,
            })
        }
    }

    fn member(name: &str) -> AggregateMember {
        AggregateMember {
            server_name: name.into(),
            tool_prefix: format!("{name}__"),
            transport: McpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: Default::default(),
            },
        }
    }

    fn agg_req(method: &str, params: serde_json::Value) -> McpRequest {
        McpRequest::aggregate(method, params, CallerContext::new("k", "u"))
    }

    #[tokio::test]
    async fn tools_list_fanout_prefixes_names_and_concats() {
        let inner = CannedExecutor::new()
            .with(
                "a",
                serde_json::json!({"tools": [{"name": "search"}, {"name": "fetch"}]}),
            )
            .with("b", serde_json::json!({"tools": [{"name": "noop"}]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("tools/list", serde_json::json!({})))
            .await
            .unwrap();
        let names: Vec<String> = resp.result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a__search", "a__fetch", "b__noop"]);
        assert!(resp.result.get("_bitrouterErrors").is_none());
    }

    #[tokio::test]
    async fn tools_list_partial_failure_surfaces_under_errors() {
        let inner = CannedExecutor::new()
            .with("a", serde_json::json!({"tools": [{"name": "ok"}]}))
            .with_err(
                "b",
                BitrouterError::Upstream {
                    status: 502,
                    message: "boom".into(),
                },
            );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("tools/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.result["tools"][0]["name"], "a__ok");
        let errors = resp.result["_bitrouterErrors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["server"], "b");
    }

    #[tokio::test]
    async fn tools_call_strips_prefix_and_dispatches_to_member() {
        let inner = CannedExecutor::new().with("a", serde_json::json!({"ok": true}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "tools/call",
                    serde_json::json!({ "name": "a__search", "arguments": {} }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["ok"], true);
    }

    #[tokio::test]
    async fn tools_call_uses_longest_matching_prefix() {
        // Two servers — "a" with prefix "a__" and "ab" with prefix "ab__". A
        // call to "ab__tool" must route to "ab" (longer prefix) even though
        // "a__" is also a valid `strip_prefix` candidate. Without
        // longest-prefix-wins this would silently misroute to "a" with the
        // stripped name "b__tool".
        let inner = CannedExecutor::new()
            .with("ab", serde_json::json!({"server": "ab"}))
            .with("a", serde_json::json!({"server": "a"}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let mut m_a = member("a");
        m_a.tool_prefix = "a__".into();
        let mut m_ab = member("ab");
        m_ab.tool_prefix = "ab__".into();
        let target = McpTarget::Aggregate {
            members: vec![m_a, m_ab],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "tools/call",
                    serde_json::json!({ "name": "ab__tool", "arguments": {} }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["server"], "ab");
    }

    #[tokio::test]
    async fn tools_call_unknown_prefix_is_404() {
        let inner = CannedExecutor::new();
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        let err = exec
            .execute(
                &target,
                &agg_req("tools/call", serde_json::json!({ "name": "ghost__search" })),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 404);
    }

    /// Canned `resources/list` result naming `uris`.
    fn listing(uris: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "resources": uris.iter().map(|u| serde_json::json!({"uri": u})).collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn resources_read_dispatches_to_the_one_member_that_lists_it() {
        // "b" owns the URI and is *second* in member order, so a first-wins
        // scan would have answered from "a".
        let inner = CannedExecutor::new()
            .with("a:resources/list", listing(&["x://other"]))
            .with("b:resources/list", listing(&["x://owned"]))
            .with("b", serde_json::json!({"contents": ["data"]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req("resources/read", serde_json::json!({ "uri": "x://owned" })),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["contents"][0], "data");
    }

    #[tokio::test]
    async fn resources_read_refuses_to_choose_between_two_owners() {
        let inner = CannedExecutor::new()
            .with("a:resources/list", listing(&["x://shared"]))
            .with("b:resources/list", listing(&["x://shared"]));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let err = exec
            .execute(
                &target,
                &agg_req("resources/read", serde_json::json!({ "uri": "x://shared" })),
            )
            .await
            .unwrap_err();
        // Both candidates are named, and neither was dispatched to — the
        // canned executor has no read response for either, so a dispatch would
        // have surfaced as a different error.
        let msg = err.to_string();
        assert!(msg.contains('a') && msg.contains('b'), "names both: {msg}");
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn resources_read_does_not_guess_when_enumeration_fails() {
        let inner = CannedExecutor::new()
            .with_err(
                "a:resources/list",
                BitrouterError::Upstream {
                    status: 502,
                    message: "list unavailable".into(),
                },
            )
            .with("b:resources/list", listing(&["x://shared"]))
            .with("b", serde_json::json!({"contents": ["must not dispatch"]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };

        let err = exec
            .execute(
                &target,
                &agg_req("resources/read", serde_json::json!({ "uri": "x://shared" })),
            )
            .await
            .expect_err("ownership is indeterminate while one member cannot enumerate");
        assert_eq!(err.status(), 502);
        assert!(err.to_string().contains("indeterminate"), "{err}");
    }

    #[tokio::test]
    async fn resources_read_unlisted_uri_is_invalid_params() {
        let inner = CannedExecutor::new()
            .with("a:resources/list", listing(&["x://other"]))
            .with(
                "a:resources/templates/list",
                serde_json::json!({"resourceTemplates": []}),
            );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        let err = exec
            .execute(
                &target,
                &agg_req("resources/read", serde_json::json!({ "uri": "x://ghost" })),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(
            err.to_string().contains("/mcp/"),
            "names the direct route: {err}"
        );
    }

    #[tokio::test]
    async fn resources_read_falls_back_to_templates() {
        let inner = CannedExecutor::new()
            .with("a:resources/list", listing(&[]))
            .with("b:resources/list", listing(&[]))
            .with(
                "a:resources/templates/list",
                serde_json::json!({"resourceTemplates": []}),
            )
            .with(
                "b:resources/templates/list",
                serde_json::json!({"resourceTemplates": [{"uriTemplate": "db://rows/{id}"}]}),
            )
            .with("b", serde_json::json!({"contents": ["row"]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "resources/read",
                    serde_json::json!({ "uri": "db://rows/7" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["contents"][0], "row");
    }

    #[tokio::test]
    async fn skill_uri_routes_by_label_without_enumerating() {
        // No `resources/list` is canned: if resolution consulted the index it
        // would fail to find an owner and 404. SEP-2640 makes a skill readable
        // whether or not it is listed, so the label must be enough.
        let inner = CannedExecutor::new().with("acme", serde_json::json!({"contents": ["skill"]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("acme"), member("other")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "resources/read",
                    serde_json::json!({ "uri": "skill://acme/refunds/SKILL.md" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["contents"][0], "skill");
    }

    /// Regression: routing to the right member is not enough — the gateway's
    /// own label must come off `params.uri` before the sub-request goes out.
    ///
    /// This escaped unit testing and was caught by a live end-to-end run: the
    /// upstream answered `-32602` for `skill://acme/…`, a URI only the gateway
    /// ever invents, and the aggregate surfaced it as a 502. The earlier test
    /// asserted only which member replied, which a canned executor will happily
    /// do no matter what URI it was handed.
    #[tokio::test]
    async fn label_routed_read_strips_the_label_before_dispatching() {
        let inner = Arc::new(
            CannedExecutor::new().with("acme", serde_json::json!({"contents": ["skill"]})),
        );
        let exec = AggregatingExecutor::new(inner.clone());
        let target = McpTarget::Aggregate {
            members: vec![member("acme"), member("other")],
        };
        exec.execute(
            &target,
            &agg_req(
                "resources/read",
                serde_json::json!({ "uri": "skill://acme/refunds/references/GUIDE.md" }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            inner.uris_for("acme"),
            vec!["skill://refunds/references/GUIDE.md".to_string()],
            "the upstream must see its own URI, not the gateway-namespaced one",
        );
    }

    /// A label-routed read answers in the namespace the client asked in.
    ///
    /// SEP-2640 has hosts verify a read against the entry's `resources` set,
    /// which this gateway namespaced; returning the upstream's own URI would
    /// look like a read of a file outside the skill — a verification failure.
    #[tokio::test]
    async fn label_routed_read_renamespaces_the_returned_uri() {
        let inner = Arc::new(CannedExecutor::new().with(
            "acme",
            serde_json::json!({"contents": [
                {"uri": "skill://refunds/references/GUIDE.md", "text": "body"}
            ]}),
        ));
        let exec = AggregatingExecutor::new(inner);
        let target = McpTarget::Aggregate {
            members: vec![member("acme")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "resources/read",
                    serde_json::json!({ "uri": "skill://acme/refunds/references/GUIDE.md" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.result["contents"][0]["uri"], "skill://acme/refunds/references/GUIDE.md",
            "the client asked in the gateway namespace and must be answered in it",
        );
        assert_eq!(resp.result["contents"][0]["text"], "body");
    }

    /// A tier-2 read was never rewritten going out, so it must not be
    /// rewritten coming back.
    #[tokio::test]
    async fn index_routed_read_leaves_the_returned_uri_alone() {
        let inner = Arc::new(
            CannedExecutor::new()
                .with("a:resources/list", listing(&["file:///x/y.txt"]))
                .with(
                    "a",
                    serde_json::json!({"contents": [{"uri": "file:///x/y.txt", "text": "b"}]}),
                ),
        );
        let exec = AggregatingExecutor::new(inner);
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "resources/read",
                    serde_json::json!({ "uri": "file:///x/y.txt" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["contents"][0]["uri"], "file:///x/y.txt");
    }

    /// The mirror of the above: a URI the gateway never rewrote is forwarded
    /// byte-for-byte. Tier 2 resolution must not strip anything.
    #[tokio::test]
    async fn index_routed_read_forwards_the_uri_verbatim() {
        let inner = Arc::new(
            CannedExecutor::new()
                .with("a:resources/list", listing(&["file:///x/y.txt"]))
                .with("a", serde_json::json!({"contents": ["body"]})),
        );
        let exec = AggregatingExecutor::new(inner.clone());
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        exec.execute(
            &target,
            &agg_req(
                "resources/read",
                serde_json::json!({ "uri": "file:///x/y.txt" }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(inner.uris_for("a"), vec!["file:///x/y.txt".to_string()]);
    }

    #[tokio::test]
    async fn unlabelled_skill_uri_still_resolves_through_the_index() {
        // A `skill://` URI whose leading segment names no member is not a
        // gateway-namespaced URI — it falls through to the ownership index.
        let inner = CannedExecutor::new()
            .with("a:resources/list", listing(&["skill://refunds/SKILL.md"]))
            .with("a", serde_json::json!({"contents": ["unrewritten"]}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "resources/read",
                    serde_json::json!({ "uri": "skill://refunds/SKILL.md" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["contents"][0], "unrewritten");
    }

    #[tokio::test]
    async fn resources_read_without_uri_is_400() {
        let inner = CannedExecutor::new();
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a")],
        };
        let err = exec
            .execute(&target, &agg_req("resources/read", serde_json::json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    /// One upstream skill entry, as a member would return it.
    fn skills_result(uri: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "skills": [{
                "uri": uri,
                "frontmatter": {"name": name, "description": "d"},
                "resources": [
                    {"uri": uri, "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                    {"uri": format!("{}/examples/email.md",
                        uri.strip_suffix("/SKILL.md").unwrap_or(uri)),
                     "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
                ]
            }]
        })
    }

    /// The collision SEP-2640 names: two members publishing the same skill
    /// URI. Neither may shadow the other.
    #[tokio::test]
    async fn skills_list_namespaces_colliding_uris_per_member() {
        let inner = CannedExecutor::new()
            .with(
                "a:skills/list",
                skills_result("skill://refunds/SKILL.md", "refunds"),
            )
            .with(
                "b:skills/list",
                skills_result("skill://refunds/SKILL.md", "refunds"),
            );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("skills/list", serde_json::json!({})))
            .await
            .unwrap();

        let skills = resp.result["skills"].as_array().expect("skills");
        assert_eq!(skills.len(), 2, "neither is dropped: {}", resp.result);
        let uris: Vec<&str> = skills.iter().map(|s| s["uri"].as_str().unwrap()).collect();
        assert_eq!(
            uris,
            vec!["skill://a/refunds/SKILL.md", "skill://b/refunds/SKILL.md"],
        );

        for skill in skills {
            // The SEP invariant survives rewriting.
            let name = skill["frontmatter"]["name"].as_str().unwrap();
            let final_segment = skill["uri"]
                .as_str()
                .unwrap()
                .strip_suffix("/SKILL.md")
                .and_then(|p| p.rsplit('/').next())
                .unwrap();
            assert_eq!(final_segment, name);
            // Every resource URI moved with the skill...
            let resources = skill["resources"].as_array().unwrap();
            for resource in resources {
                assert!(
                    resource["uri"].as_str().unwrap().starts_with("skill://a/")
                        || resource["uri"].as_str().unwrap().starts_with("skill://b/"),
                    "resource not namespaced: {resource}"
                );
            }
            // ...and digests did not, because they are over content bytes.
            assert_eq!(
                resources[0]["digest"],
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            );
            assert_eq!(
                resources[1]["digest"],
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            );
        }
    }

    #[tokio::test]
    async fn skills_list_skips_unaggregatable_schemes_with_a_note() {
        let inner = CannedExecutor::new()
            .with(
                "a:skills/list",
                serde_json::json!({"skills": [{
                    "uri": "github://owner/repo/skills/x/SKILL.md",
                    "frontmatter": {"name": "x", "description": "d"}
                }]}),
            )
            .with("b:skills/list", skills_result("skill://ok/SKILL.md", "ok"));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("skills/list", serde_json::json!({})))
            .await
            .unwrap();

        let skills = resp.result["skills"].as_array().expect("skills");
        assert_eq!(skills.len(), 1, "the github:// entry is not passed through");
        assert_eq!(skills[0]["uri"], "skill://b/ok/SKILL.md");
        let errors = resp.result["_bitrouterErrors"].as_array().expect("errors");
        assert_eq!(errors[0]["server"], "a");
        assert!(
            errors[0]["error"].as_str().unwrap().contains("skipped"),
            "the skip is reported, not silent: {errors:?}"
        );
    }

    #[tokio::test]
    async fn skills_list_partial_failure_does_not_fail_the_aggregate() {
        let inner = CannedExecutor::new()
            .with("a:skills/list", skills_result("skill://ok/SKILL.md", "ok"))
            .with_err(
                "b:skills/list",
                BitrouterError::Upstream {
                    status: 502,
                    message: "boom".into(),
                },
            );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("a"), member("b")],
        };
        let resp = exec
            .execute(&target, &agg_req("skills/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.result["skills"].as_array().unwrap().len(), 1);
        assert_eq!(resp.result["_bitrouterErrors"][0]["server"], "b");
    }

    #[tokio::test]
    async fn skills_get_strips_the_label_and_renamespaces_the_reply() {
        // The canned executor echoes what it was configured with; the
        // interesting assertions are that the member was chosen by label and
        // the reply came back namespaced.
        let inner = CannedExecutor::new().with(
            "acme:skills/get",
            serde_json::json!({"skill": {
                "uri": "skill://refunds/SKILL.md",
                "frontmatter": {"name": "refunds", "description": "d"},
                "resources": [{"uri": "skill://refunds/SKILL.md", "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]
            }}),
        );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("acme"), member("other")],
        };
        let resp = exec
            .execute(
                &target,
                &agg_req(
                    "skills/get",
                    serde_json::json!({"uri": "skill://acme/refunds/SKILL.md"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["skill"]["uri"], "skill://acme/refunds/SKILL.md");
        assert_eq!(
            resp.result["skill"]["resources"][0]["uri"],
            "skill://acme/refunds/SKILL.md"
        );
        assert_eq!(
            resp.result["skill"]["resources"][0]["digest"],
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[tokio::test]
    async fn skills_get_rejects_a_different_skill_than_the_one_requested() {
        let inner = CannedExecutor::new().with(
            "acme:skills/get",
            serde_json::json!({"skill": {
                "uri": "skill://bar/SKILL.md",
                "frontmatter": {"name": "bar", "description": "d"},
                "resources": [{
                    "uri": "skill://bar/SKILL.md",
                    "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }]
            }}),
        );
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("acme")],
        };

        let err = exec
            .execute(
                &target,
                &agg_req(
                    "skills/get",
                    serde_json::json!({"uri": "skill://acme/foo/SKILL.md"}),
                ),
            )
            .await
            .expect_err("an upstream must not substitute a different skill");
        assert_eq!(err.status(), 502);
    }

    #[tokio::test]
    async fn skills_get_with_an_unknown_label_is_invalid_params() {
        let inner = CannedExecutor::new();
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate {
            members: vec![member("acme")],
        };
        let err = exec
            .execute(
                &target,
                &agg_req(
                    "skills/get",
                    serde_json::json!({"uri": "skill://ghost/refunds/SKILL.md"}),
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn empty_aggregate_answers_skills_list_with_an_empty_list() {
        // The gateway declares the extension optimistically, so this must be
        // an empty list rather than "method not found".
        let exec = AggregatingExecutor::new(Arc::new(CannedExecutor::new()));
        let target = McpTarget::Aggregate { members: vec![] };
        let resp = exec
            .execute(&target, &agg_req("skills/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.result["skills"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn template_prefix_matching_is_literal_only() {
        assert!(matches_template_prefix("db://rows/7", "db://rows/{id}"));
        assert!(!matches_template_prefix("other://rows/7", "db://rows/{id}"));
        // A template that is all expansion matches nothing rather than
        // everything.
        assert!(!matches_template_prefix("anything", "{whole}"));
    }

    #[tokio::test]
    async fn empty_aggregate_returns_empty_list_for_list_methods() {
        let inner = CannedExecutor::new();
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Aggregate { members: vec![] };
        let resp = exec
            .execute(&target, &agg_req("tools/list", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.result["tools"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn direct_target_passes_through_to_inner() {
        let inner = CannedExecutor::new().with("only", serde_json::json!({"pass": true}));
        let exec = AggregatingExecutor::new(Arc::new(inner));
        let target = McpTarget::Direct {
            server_name: "only".into(),
            transport: McpTransport::Stdio {
                command: "/bin/true".into(),
                args: vec![],
                env: Default::default(),
            },
        };
        let resp = exec
            .execute(
                &target,
                &McpRequest::direct(
                    "only",
                    "tools/list",
                    serde_json::json!({}),
                    CallerContext::new("k", "u"),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.result["pass"], true);
    }
}
