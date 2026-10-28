//! Capacity facts derived from the actual Core all-pending cleanup projection.
//! These values never authorize work or stand in for a result/receipt.
use super::*;
use crate::storage::checkpoint::serialized_bytes;
use bitrouter_ai::core::protocol::OwnershipGrant;

pub(crate) struct Projection {
    pub checkpoint: u64,
    pub artifacts: u64,
    pub tools: BTreeMap<String, Tool>,
}
pub(crate) struct Tool {
    pub command: u64,
    pub result: u64,
}

pub(super) fn project(
    state: &SessionSnapshot,
    prepared: &SessionSnapshot,
    payload: &CheckpointPayload,
    host: &Limits,
    grant: &OwnershipGrant,
) -> Result<Projection, CoreError> {
    let checkpoint = capacity::projected_bytes(state, payload, host, grant)?;
    let artifacts = artifact_storage::projected_bytes(state, prepared, host)?;
    let mut tools = BTreeMap::new();
    for turn in state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
    {
        for call in &turn.invocations {
            let limits =
                tool_payloads::limits(call, host, state.run.as_ref(), turn.input.limits.as_ref())?;
            let tool = Tool {
                command: serialized_bytes(&call.dispatch)?,
                result: limits.payload_bytes,
            };
            if tools
                .insert(call.dispatch.invocation_id.clone(), tool)
                .is_some()
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "duplicate native tool reservation",
                ));
            }
        }
    }
    Ok(Projection {
        checkpoint,
        artifacts,
        tools,
    })
}
