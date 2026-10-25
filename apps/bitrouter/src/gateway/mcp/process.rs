//! Joinable stdio ownership, retained even when an initialization future is
//! cancelled. rmcp logs close errors without returning them through cancel().

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

struct OwnedChild {
    process: Box<dyn ChildWrapper>,
    #[cfg(unix)]
    group: rustix::process::Pid,
}

#[derive(Clone)]
pub(super) struct ProcessOwner {
    child: Arc<Mutex<Option<OwnedChild>>>,
    closed: Arc<AtomicBool>,
    #[cfg(all(test, feature = "core", feature = "harness"))]
    cleanup_gate: Arc<std::sync::Mutex<Option<Arc<crate::tenancy::hosts::HostGate>>>>,
}

impl ProcessOwner {
    pub(super) fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            closed: Arc::new(AtomicBool::new(true)),
            #[cfg(all(test, feature = "core", feature = "harness"))]
            cleanup_gate: Default::default(),
        }
    }

    #[cfg(all(test, feature = "core", feature = "harness"))]
    pub(super) fn hold_cleanup(
        &self,
        gate: Arc<crate::tenancy::hosts::HostGate>,
    ) -> std::io::Result<()> {
        *self
            .cleanup_gate
            .lock()
            .map_err(|_| std::io::Error::other("MCP cleanup test gate poisoned"))? = Some(gate);
        Ok(())
    }

    #[cfg(all(test, feature = "core", feature = "harness"))]
    pub(super) async fn retained_pid(&self) -> Option<u32> {
        self.child
            .lock()
            .await
            .as_ref()
            .and_then(|child| child.process.id())
    }

    pub(super) async fn stop(&self) -> std::io::Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut slot = self.child.lock().await;
            let Some(child) = slot.as_mut() else {
                return Ok(());
            };
            #[cfg(all(test, feature = "core", feature = "harness"))]
            {
                let gate = self
                    .cleanup_gate
                    .lock()
                    .map_err(|_| std::io::Error::other("MCP cleanup test gate poisoned"))?
                    .clone();
                if let Some(gate) = gate {
                    gate.reached().await;
                }
            }
            // Stop the group/job, including descendants, then await exit.
            // Keeping the child in its owner also allows cancellation of wait.
            if child.process.id().is_some() {
                let _ = child.process.start_kill();
            }
            child.process.wait().await?;
            // A repeated kill may fail after the transport has already killed
            // the process. Confirm that the owned scope is gone instead.
            #[cfg(unix)]
            wait_for_group_exit(|| rustix::process::test_kill_process_group(child.group)).await?;
            self.closed.store(true, Ordering::SeqCst);
            *slot = None;
            Ok(())
        })
        .await
        .map_err(|_| std::io::Error::other("MCP process cleanup timed out"))?
    }

    fn signal(&self) {
        if !self.closed.load(Ordering::SeqCst)
            && let Ok(mut slot) = self.child.try_lock()
            && let Some(child) = slot.as_mut()
            && child.process.id().is_some()
        {
            let _ = child.process.start_kill();
        }
    }
}

#[cfg(unix)]
async fn wait_for_group_exit(
    mut probe: impl FnMut() -> Result<(), rustix::io::Errno>,
) -> std::io::Result<()> {
    loop {
        match probe() {
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            // Darwin can return EPERM for a group containing only zombies.
            // Wait for reaping within stop()'s existing deadline. EPERM never
            // proves cleanup: persistent permission failures still time out.
            // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c
            Err(rustix::io::Errno::PERM) | Ok(()) => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) struct StdioTransport {
    io: AsyncRwTransport<RoleClient, tokio::process::ChildStdout, tokio::process::ChildStdin>,
    owner: ProcessOwner,
}

impl StdioTransport {
    pub(super) fn spawn(
        mut command: tokio::process::Command,
        owner: ProcessOwner,
    ) -> std::io::Result<Self> {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut wrapped = CommandWrap::from(command);
        #[cfg(unix)]
        wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrapped.wrap(process_wrap::tokio::JobObject);
        wrapped.wrap(KillOnDrop);
        let child = wrapped.spawn()?;
        let mut slot = owner
            .child
            .try_lock()
            .map_err(|_| std::io::Error::other("MCP process owner is busy"))?;
        #[cfg(unix)]
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| std::io::Error::other("MCP process group unavailable"))?;
        *slot = Some(OwnedChild {
            process: child,
            #[cfg(unix)]
            group,
        });
        owner.closed.store(false, Ordering::SeqCst);
        let child = slot
            .as_mut()
            .ok_or_else(|| std::io::Error::other("MCP child unavailable"))?;
        let stdout = child
            .process
            .inner_mut()
            .stdout()
            .take()
            .ok_or_else(|| std::io::Error::other("MCP stdout unavailable"))?;
        let stdin = child
            .process
            .inner_mut()
            .stdin()
            .take()
            .ok_or_else(|| std::io::Error::other("MCP stdin unavailable"))?;
        drop(slot);
        Ok(Self {
            io: AsyncRwTransport::new(stdout, stdin),
            owner,
        })
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.owner.signal();
    }
}

impl Transport<RoleClient> for StdioTransport {
    type Error = std::io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.io.send(item)
    }
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.io.receive()
    }
    async fn close(&mut self) -> Result<(), Self::Error> {
        let io = self.io.close().await;
        self.owner.stop().await?;
        io
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::wait_for_group_exit;
    use rustix::io::Errno;
    use std::time::Duration;

    #[tokio::test]
    async fn cleanup_waits_through_permission_probe_until_group_disappears() -> std::io::Result<()>
    {
        let mut probes = 0;
        wait_for_group_exit(|| {
            probes += 1;
            match probes {
                1 => Err(Errno::PERM),
                2 => Ok(()),
                _ => Err(Errno::SRCH),
            }
        })
        .await?;
        assert_eq!(probes, 3);
        Ok(())
    }

    #[tokio::test]
    async fn permission_denial_never_confirms_cleanup() {
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                wait_for_group_exit(|| Err(Errno::PERM)),
            )
            .await
            .is_err()
        );
    }
}
