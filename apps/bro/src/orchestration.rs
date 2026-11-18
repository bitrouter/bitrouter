//! Local operator-driven delegation. Queuing commits the turn before a wake can dispatch it.
//! Each child uses the ordinary workspace admission lock and per-thread worker.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use crate::checkpoint::{Home, Thread};
use crate::{ThreadCommand, emit, inspect, thread_command};

#[derive(Subcommand)]
pub(crate) enum AgentCommand {
    /// Create a child with the parent's workspace, gateway and tool permissions.
    Create { parent: Uuid, child: Uuid },
    /// Queue one task. Reusing its request ID returns the original turn.
    Delegate {
        parent: Uuid,
        child: Uuid,
        #[arg(long)]
        request_id: String,
        prompt: String,
    },
    /// Run the named delegated turn until settlement or an approval.
    Wake {
        parent: Uuid,
        child: Uuid,
        turn: Uuid,
    },
    /// Observe a delegated turn until settlement, approval, recovery or timeout.
    Wait {
        parent: Uuid,
        child: Uuid,
        turn: Uuid,
        #[arg(long,default_value_t=30000,value_parser=clap::value_parser!(u64).range(0..=600000))]
        timeout_ms: u64,
    },
    /// Cancel a delegated turn through the same worker fence as thread cancel.
    Cancel {
        parent: Uuid,
        child: Uuid,
        turn: Uuid,
    },
}

fn child_thread(home: &Home, parent: Uuid, child: Uuid) -> Result<Thread> {
    let t = home.load(child)?;
    ensure!(
        t.parent_id == Some(parent),
        "child belongs to a different parent"
    );
    Ok(t)
}

pub(super) async fn run(home: &Home, action: AgentCommand, endpoint: Option<&str>) -> Result<()> {
    match action {
        AgentCommand::Create { parent, child } => {
            ensure!(parent != child, "parent and child must differ");
            let _guard = home.worker(child)?;
            let p = home.load(parent)?;
            crate::for_thread(&p, endpoint)?;
            if home.ids()?.contains(&child) {
                return inspect(&child_thread(home, parent, child)?, None, false);
            }
            let mut t = Thread::new(
                p.workspace,
                p.endpoint,
                p.model,
                p.effort,
                p.max_output_tokens,
                p.tool_mode,
            );
            t.m0_instructions = p.m0_instructions;
            t.thread_id = child;
            t.parent_id = Some(parent);
            home.admit(&t)?;
            emit(&json!({"parent":parent,"child":child}))
        }
        AgentCommand::Delegate {
            parent,
            child,
            request_id,
            prompt,
        } => {
            let (_guard, busy) = home.observe_worker(child)?;
            let mut t = child_thread(home, parent, child)?;
            if let Some(id) = t.accepted_request(&request_id, &prompt)? {
                return inspect(&t, Some(id), busy);
            }
            ensure!(!busy, "another process owns this thread");
            t.start(prompt, Some(request_id))?;
            home.admit(&t)?;
            crate::accepted(&t)
        }
        AgentCommand::Wake {
            parent,
            child,
            turn,
        } => {
            child_thread(home, parent, child)?;
            thread_command(
                home,
                ThreadCommand::Resume {
                    thread_id: child,
                    turn,
                },
                endpoint,
            )
            .await
        }
        AgentCommand::Cancel {
            parent,
            child,
            turn,
        } => {
            child_thread(home, parent, child)?;
            thread_command(
                home,
                ThreadCommand::Cancel {
                    thread_id: child,
                    turn,
                },
                endpoint,
            )
            .await
        }
        AgentCommand::Wait {
            parent,
            child,
            turn,
            timeout_ms,
        } => {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let lock = home.lock_file(child)?;
                let owned = match lock.try_lock() {
                    Ok(()) => false,
                    Err(std::fs::TryLockError::WouldBlock) => true,
                    Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                };
                let t = child_thread(home, parent, child)?;
                let observed = t
                    .turns
                    .iter()
                    .find(|t| t.turn_id == turn)
                    .context("unknown delegated turn")?;
                let status = observed.status(owned);
                if observed.outcome.is_some()
                    || !owned
                        && (observed.pending().is_some()
                            || observed.next() == crate::checkpoint::Next::Blocked)
                    || Instant::now() >= deadline
                {
                    return emit(
                        &json!({"parent":parent,"child":child,"turn_id":turn,"status":status,"outcome":observed.outcome,"pending_input":observed.pending(),"timed_out":observed.outcome.is_none() && status==bitrouter_ai::harness::turn::TurnStatus::Running}),
                    );
                }
                drop(lock);
                tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(40)))
                    .await;
            }
        }
    }
}
