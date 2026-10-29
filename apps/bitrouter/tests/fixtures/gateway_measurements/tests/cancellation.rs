//! Catchable interruption must reap the fixture's gateway and remove private state.

#[cfg(unix)]
mod unix {
    use anyhow::{Context, Result, ensure};
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct Fixture {
        child: Child,
        gateway: Option<(u32, PathBuf)>,
    }

    fn owned_gateway_alive(pid: u32, config: &std::path::Path) -> Result<bool> {
        let output = Command::new("/bin/ps")
            .args(["-o", "args=", "-p"])
            .arg(pid.to_string())
            .output()?;
        Ok(output.status.success()
            && std::str::from_utf8(&output.stdout)?
                .split_whitespace()
                .any(|argument| argument == config.as_os_str()))
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                // Let the fixture's own guard clean up even if reading its
                // ownership record failed. SIGKILL would bypass that guard.
                match Command::new("/bin/kill")
                    .arg("-TERM")
                    .arg(self.child.id().to_string())
                    .status()
                {
                    Ok(status) if status.success() => {}
                    result => eprintln!("fixture cleanup termination failed: {result:?}"),
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                let exited = loop {
                    match self.child.try_wait() {
                        Ok(Some(_)) => break true,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Ok(None) => break false,
                        Err(error) => {
                            eprintln!("fixture cleanup wait failed: {error}");
                            break false;
                        }
                    }
                };
                if !exited {
                    if let Err(error) = self.child.kill() {
                        eprintln!("fixture cleanup kill failed: {error}");
                    }
                    if let Err(error) = self.child.wait() {
                        eprintln!("fixture cleanup reap failed: {error}");
                    }
                }
            }
            // A failed cleanup assertion must not leave the test's process alive.
            // Recheck its unique private config before targeting a recorded PID.
            if let Some((pid, config)) = &self.gateway
                && matches!(owned_gateway_alive(*pid, config), Ok(true))
            {
                match Command::new("/bin/kill")
                    .arg("-KILL")
                    .arg(pid.to_string())
                    .status()
                {
                    Ok(status) if status.success() => {}
                    result => eprintln!("owned gateway fallback cleanup failed: {result:?}"),
                }
            }
        }
    }

    #[test]
    #[ignore = "requires BITROUTER_MEASUREMENT_TEST_BINARY pointing to an immutable gateway"]
    fn interrupts_clean_up_owned_gateway() -> Result<()> {
        let binary = fs::canonicalize(
            std::env::var_os("BITROUTER_MEASUREMENT_TEST_BINARY")
                .context("gateway artifact missing")?,
        )?;
        for signal in ["-INT", "-TERM"] {
            let directory = tempfile::tempdir()?;
            let log_path = directory.path().join("fixture.log");
            let log = File::create(&log_path)?;
            let child = Command::new(env!("CARGO_BIN_EXE_bitrouter-gateway-measurements"))
                .args([&binary, &binary, &binary])
                .arg(directory.path().join("output.json"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()?;
            let mut fixture = Fixture {
                child,
                gateway: None,
            };
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let logs = fs::read_to_string(&log_path)?;
                if let Some(record) = logs
                    .split_inclusive('\n')
                    .filter(|line| line.ends_with('\n'))
                    .find_map(|line| line.trim_end_matches('\n').strip_prefix("owned_gateway "))
                {
                    let (pid, config) =
                        record.split_once(' ').context("invalid ownership record")?;
                    fixture.gateway = Some((pid.parse()?, PathBuf::from(config)));
                    break;
                }
                ensure!(
                    fixture.child.try_wait()?.is_none(),
                    "fixture exited before ownership record: {logs}"
                );
                ensure!(Instant::now() < deadline, "ownership record deadline");
                std::thread::sleep(Duration::from_millis(10));
            }
            let (pid, config) = fixture
                .gateway
                .as_ref()
                .context("ownership record missing")?;
            ensure!(
                owned_gateway_alive(*pid, config)?,
                "gateway was not running before interruption"
            );
            ensure!(
                Command::new("/bin/kill")
                    .arg(signal)
                    .arg(fixture.child.id().to_string())
                    .status()?
                    .success()
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = fixture.child.try_wait()? {
                    ensure!(!status.success(), "interruption reported success");
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "fixture did not exit after interruption"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            ensure!(
                !owned_gateway_alive(*pid, config)?,
                "gateway survived fixture interruption"
            );
            ensure!(
                !Command::new("/bin/ps")
                    .args(["-o", "pid=", "-p"])
                    .arg(pid.to_string())
                    .output()?
                    .status
                    .success(),
                "gateway PID still present after fixture exit"
            );
            ensure!(
                !config
                    .parent()
                    .context("private config parent missing")?
                    .exists(),
                "private gateway files survived interruption"
            );
            ensure!(
                !directory.path().join("output.json").exists(),
                "interrupted run wrote completed evidence"
            );
            let logs = fs::read_to_string(log_path)?;
            ensure!(
                logs.contains(if signal == "-INT" {
                    "measurement interrupted"
                } else {
                    "measurement terminated"
                }),
                "fixture did not handle its signal: {logs}"
            );
        }
        Ok(())
    }
}
