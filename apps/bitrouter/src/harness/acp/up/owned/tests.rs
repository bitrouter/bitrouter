use super::wait_for_group_exit;
use futures::FutureExt;
use rustix::io::Errno;
use std::{cell::Cell, time::Duration};

#[tokio::test(start_paused = true)]
async fn group_cleanup_requires_absence_after_permission_and_live_probes() -> Result<(), Errno> {
    let mut probes = 0;
    wait_for_group_exit(|| {
        probes += 1;
        match probes {
            1 | 3 => Err(Errno::PERM),
            2 => Ok(()),
            _ => Err(Errno::SRCH),
        }
    })
    .await?;
    assert_eq!(probes, 4);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn persistent_permission_or_live_group_never_sends_cleanup_ack() {
    for result in [Err(Errno::PERM), Ok(())] {
        let probes = Cell::new(0);
        let (ack, mut receive) = tokio::sync::oneshot::channel();
        let work = async {
            wait_for_group_exit(|| {
                probes.set(probes.get() + 1);
                result
            })
            .await?;
            let _sent = ack.send(());
            Ok::<_, Errno>(())
        };
        tokio::pin!(work);
        assert!(work.as_mut().now_or_never().is_none());
        // Poll this SAME future on both sides of the host's 30-second cleanup
        // deadline. Advancing test time is not an OS process-exit observation.
        for duration in [Duration::from_secs(1), Duration::from_secs(30)] {
            tokio::time::advance(duration).await;
            assert!(work.as_mut().now_or_never().is_none());
            assert_eq!(
                receive.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            );
        }
        assert!(probes.get() >= 3);
    }
}

#[tokio::test]
async fn other_group_probe_errors_remain_errors() {
    assert_eq!(wait_for_group_exit(|| Err(Errno::IO)).await, Err(Errno::IO));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod process;
