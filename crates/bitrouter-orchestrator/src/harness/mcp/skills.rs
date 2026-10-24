//! Skills use the same harness-owned transports as tools, with an independent
//! explicit source grant. No content reads occur during metadata admission.

use std::collections::BTreeSet;

use base64::Engine;
use bitrouter_sdk::mcp::skills::{
    GetSkillResult, SKILLS_EXTENSION_ID, SKILLS_GET_METHOD, SKILLS_LIST_METHOD, SkillEntry,
};
use rmcp::model::{ClientRequest, CustomRequest, ReadResourceRequestParams, ServerResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{CALL_TIMEOUT, McpConnections};
use crate::harness::skills::mcp::{
    HeldMcpSkill, MAX_CATALOG_BYTES, McpSkillCatalog, McpSkillProblem, SkillAccess, SkillOrigin,
    SkillRead, SkillScope, VerifiedSkillFile,
};

impl McpConnections {
    fn skill_origin(
        &self,
        server: &str,
        access: &SkillAccess<'_>,
        cached: bool,
    ) -> Result<SkillOrigin, String> {
        let grant = access
            .policy
            .mcp_sources
            .get(server)
            .ok_or("MCP server has no explicit native skill source grant")?;
        let configuration = self
            .configured_sources
            .get(server)
            .ok_or("MCP skill source is no longer configured for this execution")?;
        let scope = SkillScope::authorize(grant, access.caller, &configuration.workspace)?;
        if let Some(connection) = self.connections.get(server) {
            let info = connection
                .service
                .peer_info()
                .ok_or("MCP skill source has no connection declaration")?;
            if info.capabilities.resources.is_none()
                || !info
                    .capabilities
                    .extensions
                    .as_ref()
                    .is_some_and(|extensions| extensions.contains_key(SKILLS_EXTENSION_ID))
            {
                return Err(
                    "MCP skill source did not declare Resources and Skills on this connection"
                        .into(),
                );
            }
        } else if !cached {
            return Err(self
                .connection_failures
                .get(server)
                .cloned()
                .unwrap_or_else(|| "MCP skill source is unavailable".into()));
        }
        Ok(SkillOrigin {
            server: server.into(),
            configuration_digest: configuration.configuration_digest.clone(),
            scope,
        })
    }

    async fn skill_request(
        &self,
        server: &str,
        access: &SkillAccess<'_>,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, String> {
        self.skill_origin(server, access, false)?;
        let connection = self
            .connections
            .get(server)
            .ok_or("MCP skill source is unavailable")?;
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("MCP skill metadata request cancelled".into()),
            result = tokio::time::timeout(CALL_TIMEOUT,
                connection.service.peer().send_request(ClientRequest::CustomRequest(CustomRequest::new(method, Some(params))))) =>
                result.map_err(|_| "MCP skill metadata request timed out")?
                    .map_err(|error| format!("MCP {server} {method}: {error}"))?,
        };
        let ServerResult::CustomResult(value) = result else {
            return Err("MCP skill method returned an unexpected result".into());
        };
        validate_envelope(&value.0)?;
        Ok(value.0)
    }

    /// Listings may be partial; this never establishes that an unlisted URI is
    /// absent and never reads SKILL.md or supporting files.
    pub async fn list_skills(
        &self,
        server: &str,
        access: &SkillAccess<'_>,
        cancel: &CancellationToken,
    ) -> Result<McpSkillCatalog, String> {
        let origin = self.skill_origin(server, access, false)?;
        let mut catalog = McpSkillCatalog::default();
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        let mut seen_skills = BTreeSet::new();
        let mut bytes = 0usize;
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        for _ in 0..256 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
            let page = tokio::time::timeout_at(
                deadline,
                self.skill_request(server, access, SKILLS_LIST_METHOD, params, cancel),
            )
            .await
            .map_err(|_| "MCP skills/list exceeded its total deadline")??;
            bytes = bytes.saturating_add(
                serde_json::to_vec(&page)
                    .map_err(|error| error.to_string())?
                    .len(),
            );
            if bytes > MAX_CATALOG_BYTES {
                return Err("MCP skill catalog exceeds the 16 MiB metadata budget".into());
            }
            let entries = page
                .get("skills")
                .and_then(Value::as_array)
                .ok_or("MCP skills/list has no skills array")?;
            for value in entries {
                if catalog.skills.len().saturating_add(catalog.excluded.len()) >= 256 {
                    return Err("MCP skill catalog exceeds 256 entries".into());
                }
                let uri = value.get("uri").and_then(Value::as_str).map(str::to_owned);
                let admitted = serde_json::from_value::<SkillEntry>(value.clone())
                    .map_err(|error| error.to_string())
                    .and_then(|entry| HeldMcpSkill::new(origin.clone(), entry));
                match admitted {
                    Ok(held) => {
                        if !seen_skills.insert(held.entry.uri.clone()) {
                            return Err("MCP skills/list repeated a skill URI".into());
                        }
                        catalog.skills.push(held);
                    }
                    Err(reason) => catalog.excluded.push(McpSkillProblem {
                        server: server.into(),
                        uri,
                        reason,
                    }),
                }
            }
            match page.get("nextCursor") {
                None => return Ok(catalog),
                Some(Value::String(next))
                    if !next.is_empty()
                        && next.len() <= 4096
                        && seen_cursors.insert(next.clone()) =>
                {
                    cursor = Some(next.clone())
                }
                _ => {
                    return Err(
                        "MCP skills/list has a repeated or invalid pagination cursor".into(),
                    );
                }
            }
        }
        Err("MCP skills/list exceeded 256 pages".into())
    }

    /// Direct lookup works before, or independently of, any listing.
    pub async fn get_skill(
        &self,
        server: &str,
        uri: &str,
        access: &SkillAccess<'_>,
        cancel: &CancellationToken,
    ) -> Result<HeldMcpSkill, String> {
        let origin = self.skill_origin(server, access, false)?;
        let result = self
            .skill_request(
                server,
                access,
                SKILLS_GET_METHOD,
                json!({"uri":uri}),
                cancel,
            )
            .await?;
        let entry: GetSkillResult =
            serde_json::from_value(result).map_err(|error| error.to_string())?;
        if entry.skill.uri != uri {
            return Err("MCP skills/get returned a different skill URI".into());
        }
        HeldMcpSkill::new(origin, entry.skill)
    }

    /// A read verifies bytes but does not activate instructions, execute code,
    /// grant tools or approve a nested skill. Callers retain those duties.
    pub async fn read_skill_file(
        &self,
        held: &HeldMcpSkill,
        access: &SkillAccess<'_>,
        read: SkillRead<'_>,
        cancel: &CancellationToken,
    ) -> Result<VerifiedSkillFile, String> {
        if self.skill_origin(&held.origin.server, access, read.cached_bytes.is_some())?
            != held.origin
        {
            return Err("MCP skill source configuration or authorization scope changed".into());
        }
        let file = held.resource(read.reference)?;
        if file.size > read.max_bytes {
            return Err("MCP skill file exceeds the current host/context byte budget".into());
        }
        let bytes = if let Some(bytes) = read.cached_bytes {
            if bytes.len() as u64 != file.size {
                return Err("MCP skill cached raw byte size mismatch".into());
            }
            bytes.to_vec()
        } else {
            let connection = self
                .connections
                .get(&held.origin.server)
                .ok_or("MCP skill source is unavailable")?;
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("MCP skill content request cancelled".into()),
                result = tokio::time::timeout(CALL_TIMEOUT,
                    connection.service.peer().read_resource(ReadResourceRequestParams::new(file.uri.clone()))) =>
                    result.map_err(|_| "MCP skill content request timed out")?
                        .map_err(|error| format!("MCP skill resources/read: {error}"))?,
            };
            let result = serde_json::to_value(result).map_err(|error| error.to_string())?;
            // Typed legacy Resources results can omit cache hints; raw content
            // verification is still mandatory and grants no cache freshness.
            let contents = result
                .get("contents")
                .and_then(Value::as_array)
                .ok_or("MCP resources/read has no contents")?;
            let [content] = contents.as_slice() else {
                return Err("MCP skill resources/read must return one exact file".into());
            };
            if content.get("uri").and_then(Value::as_str) != Some(file.uri.as_str()) {
                return Err("MCP skill resources/read returned another URI".into());
            }
            match (content.get("text"), content.get("blob")) {
                (Some(Value::String(text)), None) if text.len() as u64 == file.size => {
                    text.as_bytes().to_vec()
                }
                (Some(Value::String(_)), None) => {
                    return Err("MCP skill raw byte size mismatch".into());
                }
                (None, Some(Value::String(blob)))
                    if blob.len() as u64 <= file.size.saturating_add(2) / 3 * 4 =>
                {
                    base64::engine::general_purpose::STANDARD
                        .decode(blob)
                        .map_err(|_| "MCP skill resource has invalid base64 encoding")?
                }
                _ => return Err("MCP skill resource requires text or base64 blob encoding".into()),
            }
        };
        if cancel.is_cancelled() {
            return Err("MCP skill read cancelled before use".into());
        }
        held.verify(file, &bytes)?;
        Ok(VerifiedSkillFile {
            uri: file.uri.clone(),
            bytes,
        })
    }
}

fn validate_envelope(value: &Value) -> Result<(), String> {
    if value.get("resultType").and_then(Value::as_str) != Some("complete")
        || value.get("ttlMs").and_then(Value::as_u64).is_none()
        || !matches!(
            value.get("cacheScope").and_then(Value::as_str),
            Some("private" | "public")
        )
    {
        return Err("MCP skill result requires complete/ttlMs/cacheScope".into());
    }
    Ok(())
}
