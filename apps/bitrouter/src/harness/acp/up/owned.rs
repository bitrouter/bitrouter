//! Strict child custody for an authenticated host; ordinary launch is unchanged.
#[cfg(unix)]
pub(crate) mod ingress;
#[cfg(all(test, unix))]
mod tests;

use super::*;
#[cfg(unix)]
use futures::FutureExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

pub(crate) struct Control {
    pub(crate) binding: Arc<crate::tenancy::capture::Binding>,
    pub(crate) stop: CancellationToken,
    started: AtomicBool,
    #[cfg(all(test, unix))]
    pub(crate) ingress: Arc<ingress::Statistics>,
}
impl Control {
    pub(crate) fn new(
        binding: Arc<crate::tenancy::capture::Binding>,
        stop: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            stop,
            started: AtomicBool::new(false),
            #[cfg(all(test, unix))]
            ingress: Arc::default(),
        })
    }
    pub(crate) fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }
}

impl AgentProcess {
    pub(crate) fn owned(mut self, control: Arc<Control>) -> Self {
        self.owned = Some(control);
        self
    }

    pub(super) async fn connect_owned(
        self,
        client: impl ConnectTo<Agent>,
        control: Arc<Control>,
    ) -> Result<(), agent_client_protocol::Error> {
        if control.stop.is_cancelled() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("owned process opening stopped"));
        }
        #[cfg(not(unix))]
        {
            let _unstarted = (self, client, control);
            Err(agent_client_protocol::Error::internal_error()
                .data("owned process groups require Unix"))
        }
        #[cfg(unix)]
        {
            let mut command = agent_command(
                &self.command,
                &self.args,
                &self.env,
                &self.strip_inherited_env,
            );
            // The trusted embedding supplies the complete environment, including
            // PATH/HOME if needed. No developer/provider credential is inherited.
            command.env_clear().envs(&self.env);
            let child = control
                .binding
                .with_start(|| {
                    if control.stop.is_cancelled() {
                        return Err(std::io::Error::other("owned process opening stopped"));
                    }
                    command.spawn()
                })
                .map_err(|_| {
                    agent_client_protocol::Error::internal_error()
                        .data("owned process authority rejected")
                })?
                .map_err(|_| {
                    agent_client_protocol::Error::internal_error()
                        .data("owned process spawn failed")
                })?;
            control.started.store(true, Ordering::Release);
            let mut child = child;
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            // Owned stderr is drained without tracing arbitrary child credentials.
            let stderr = child.stderr.take().map(|mut stderr| {
                tokio::spawn(
                    async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await },
                )
            });
            let (kill, killed) = oneshot::channel();
            let (dead, mut death) = oneshot::channel();
            let (done, complete) = oneshot::channel();
            own_child(
                child,
                Reaper {
                    killed,
                    dead,
                    done,
                    reaped: self.reaped,
                    stderr,
                    control: control.clone(),
                },
            );
            let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
                drop(kill);
                return Err(agent_client_protocol::Error::internal_error()
                    .data("owned process pipes unavailable"));
            };
            let stdout = ingress::Ingress::new(stdout, control.clone());
            let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
            let protocol = ConnectTo::<Client>::connect_to(transport, client);
            tokio::pin!(protocol);
            let result = tokio::select! {
                biased;
                () = control.stop.cancelled() => Err(agent_client_protocol::Error::internal_error().data("owned process stopped")),
                _ = &mut death => Err(agent_client_protocol::Error::internal_error().data("owned process exited")),
                result = &mut protocol => result,
            };
            drop(kill);
            // Never time out then abandon a child. The owning host supervises
            // this same work and quarantines while it continues awaiting proof.
            complete.await.map_err(|_| {
                agent_client_protocol::Error::internal_error()
                    .data("owned process cleanup proof lost")
            })?;
            result
        }
    }
}

#[cfg(unix)]
struct Reaper {
    killed: oneshot::Receiver<()>,
    dead: oneshot::Sender<()>,
    done: oneshot::Sender<()>,
    reaped: Option<oneshot::Sender<()>>,
    stderr: Option<tokio::task::JoinHandle<std::io::Result<u64>>>,
    control: Arc<Control>,
}

#[cfg(unix)]
fn own_child(mut child: tokio::process::Child, reaper: Reaper) {
    let Reaper {
        killed,
        dead,
        done,
        reaped,
        mut stderr,
        control,
    } = reaper;
    tokio::spawn(async move {
        // The Child remains outside catch_unwind, including when wait consumes
        // an error. A signal result alone cannot send a successful proof.
        #[cfg(test)]
        let mut phase = "pid";
        let result = std::panic::AssertUnwindSafe(async {
            let raw = i32::try_from(
                child
                    .id()
                    .ok_or_else(|| anyhow::anyhow!("owned child ID absent"))?,
            )?;
            let pid = rustix::process::Pid::from_raw(raw)
                .ok_or_else(|| anyhow::anyhow!("owned child ID invalid"))?;
            #[cfg(test)]
            {
                phase = "initial_wait";
            }
            let waited =
                tokio::select! { result = child.wait() => Some(result), _ = killed => None };
            #[cfg(test)]
            {
                phase = "cleanup_gate";
            }
            // Hold the actual Child inside the caught work, before any cleanup
            // acknowledgement. The shared host gate has no production effect.
            #[cfg(test)]
            if let Some(gate) = control
                .binding
                .service()
                .take_host_gate(crate::tenancy::hosts::HostBoundary::Cleanup)
                .map_err(|_| anyhow::anyhow!("capture cleanup test gate unavailable"))?
            {
                gate.reached().await;
            }
            // killpg's success acknowledges a signal, not completion. Confirm
            // the group no longer exists as well as reaping the direct child.
            // https://pubs.opengroup.org/onlinepubs/9799919799/functions/kill.html
            // https://docs.rs/rustix/latest/rustix/process/fn.test_kill_process_group.html
            #[cfg(test)]
            {
                phase = "killpg";
            }
            match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
                // Darwin can find a group but filter out all zombie members,
                // returning EPERM. Defer this result to actual reap + absence;
                // it is neither a successful signal nor cleanup evidence.
                // https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sig.c#L1709-L1718
                Ok(()) | Err(rustix::io::Errno::SRCH | rustix::io::Errno::PERM) => {}
                Err(error) => return Err(error.into()),
            }
            match waited {
                Some(result) => {
                    #[cfg(test)]
                    {
                        phase = "initial_wait_result";
                    }
                    result?;
                }
                None => {
                    #[cfg(test)]
                    {
                        phase = "child_wait";
                    }
                    child.wait().await?;
                }
            }
            #[cfg(test)]
            {
                phase = "group_absence";
            }
            wait_for_group_exit(|| rustix::process::test_kill_process_group(pid)).await?;
            if let Some(stderr) = &mut stderr {
                #[cfg(test)]
                {
                    phase = "stderr_join_or_copy";
                }
                stderr.await??;
            }
            Ok::<_, anyhow::Error>(())
        })
        .catch_unwind()
        .await;
        if !matches!(result, Ok(Ok(()))) {
            #[cfg(test)]
            control.ingress.record_reaper_failure(phase, &result);
            control.binding.service().host_cleanup_unknown();
            std::future::pending::<()>().await;
        }
        let _sent = dead.send(());
        let _sent = done.send(());
        if let Some(reaped) = reaped {
            let _sent = reaped.send(());
        }
        drop(child);
    });
}

#[cfg(unix)]
async fn wait_for_group_exit(
    mut probe: impl FnMut() -> Result<(), rustix::io::Errno>,
) -> Result<(), rustix::io::Errno> {
    loop {
        match probe() {
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            // EPERM (including Darwin zombie-only groups) still means absence
            // is unproven. The existing host cleanup deadline quarantines while
            // this same reaper retains its Child and stderr task. Never ACK here.
            Err(rustix::io::Errno::PERM) | Ok(()) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}
