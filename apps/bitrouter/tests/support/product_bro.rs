//! Launch the product under the filesystem grant used by this process test.
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub(super) struct Bro<'a> {
    pub binary: &'a Path,
    pub home: &'a Path,
    pub endpoint: &'a str,
    pub credential: &'a str,
    #[cfg(target_os = "linux")]
    pub gateway_files: [&'a Path; 2],
}

impl Bro<'_> {
    #[cfg(not(target_os = "linux"))]
    fn command(&self) -> Command {
        Command::new(self.binary)
    }

    #[cfg(target_os = "linux")]
    fn command(&self) -> Command {
        let mut cmd = Command::new("/usr/bin/bwrap");
        // Retain loopback to the real gateway. The new PID namespace has its
        // own procfs, and the only writable host mount is the client fixture.
        cmd.args([
            "--unshare-all",
            "--share-net",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
        ]);
        for dir in [
            "/usr/bin",
            "/bin",
            "/lib",
            "/lib64",
            "/usr/lib",
            "/usr/lib64",
        ] {
            if Path::new(dir).is_dir() {
                cmd.args(["--ro-bind", dir, dir]);
            }
        }
        // The SDK initializes the system TLS verifier even for a loopback client.
        let roots = "/etc/ssl/certs/ca-certificates.crt";
        cmd.args([
            "--ro-bind",
            roots,
            roots,
            "--setenv",
            "SSL_CERT_FILE",
            roots,
        ]);
        cmd.args([
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind",
        ])
        .arg(self.home)
        .arg(self.home)
        .arg("--ro-bind")
        .arg(self.binary)
        .arg("/bro")
        .args(["--remount-ro", "/", "--chdir"])
        .arg(self.home)
        .args([
            "--",
            "/bin/sh",
            "-ec",
            r#"
                for file in "$1" "$2"; do
                    if (exec 3<"$file") 2>/dev/null; then
                        echo "BRO can open the gateway file: $file" >&2
                        exit 1
                    fi
                done
                shift 2
                exec "$@"
            "#,
            "bro-file-access",
        ])
        .args(self.gateway_files)
        .arg("/bro");
        cmd
    }

    pub(super) async fn run(&self, args: &[&str], input: Option<&str>) -> Result<Value> {
        let mut child = self
            .command()
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("BITROUTER_API_KEY", self.credential)
            .env("HOME", self.home)
            .env("BRO_HOME", self.home.join("bro"))
            .current_dir(self.home)
            .args(["--endpoint", self.endpoint])
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting the BRO product fixture")?;
        if let Some(text) = input {
            child
                .stdin
                .take()
                .context("missing piped stdin")?
                .write_all(text.as_bytes())
                .await?;
        }
        let output =
            tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
        ensure!(
            !output.stdout.contains(&0x1b),
            "redirected BRO emitted terminal controls"
        );
        ensure!(
            output.status.success(),
            "BRO {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let line = std::str::from_utf8(&output.stdout)?
            .lines()
            .last()
            .context("empty BRO output")?;
        Ok(serde_json::from_str(line)?)
    }
}
