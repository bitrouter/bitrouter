//! End-to-end smoke test for the routing shim.
//!
//! Sets up a temp dir with a fake "real" claude that prints whether
//! `ANTHROPIC_BASE_URL` is set, installs a shim pointing at it, and
//! invokes the shim twice:
//!
//! 1. Against an unreachable port -> shim must fall through, fake claude
//!    prints "direct".
//! 2. Against `bitrouter serve` (if started by the caller on the configured
//!    port) -> shim must set the env var, fake claude prints "routed".
//!
//! Run with:
//!   cargo run --example shim_smoke -p bitrouter-acp
//! Set `BITROUTER_LISTEN=127.0.0.1:8787` to override the live-route probe port.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use bitrouter_acp::shim::{Platform, ShimEnv, install_shim, render_shim, shim_path_for};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let shim_dir = tmp.path().join("shim_dir");
    std::fs::create_dir_all(&shim_dir)?;

    // Fake "real claude" — prints whether ANTHROPIC_BASE_URL was set.
    let real = tmp.path().join("real-claude.sh");
    std::fs::write(
        &real,
        "#!/usr/bin/env bash\n\
         if [ -n \"$ANTHROPIC_BASE_URL\" ]; then \
             echo \"routed via $ANTHROPIC_BASE_URL\"; \
         else \
             echo direct; \
         fi\n",
    )?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))?;
    }

    let listen: SocketAddr = std::env::var("BITROUTER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
        .parse()?;
    let env = ShimEnv {
        var: "ANTHROPIC_BASE_URL".to_owned(),
        value: format!("http://{listen}/v1"),
    };

    let shim_path = shim_path_for(Platform::Unix, &shim_dir, "claude");

    println!("== Rendering shim body (preview) ==");
    println!("{}", render_shim(Platform::Unix, &real, listen, &env));

    println!("== Installing shim at {} ==", shim_path.display());
    let action = install_shim(Platform::Unix, &shim_path, &real, listen, &env)?;
    println!("install action: {action:?}");

    println!();
    println!("== Probe 1: BitRouter UNREACHABLE port (1) ==");
    run_against_dead_port(&real)?;

    println!();
    println!("== Probe 2: BitRouter on {listen} ==");
    let out = Command::new(&shim_path).output()?;
    print!("stdout: {}", String::from_utf8_lossy(&out.stdout));
    if !out.stderr.is_empty() {
        eprintln!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    }

    println!();
    println!("(Run `bitrouter serve` in another shell to see Probe 2 flip to 'routed'.)");
    Ok(())
}

/// Install a second shim pointing at port 1 (nothing listens there) and
/// invoke it. Demonstrates the fall-open behaviour.
fn run_against_dead_port(real: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let dead: SocketAddr = "127.0.0.1:1".parse()?;
    let env = ShimEnv {
        var: "ANTHROPIC_BASE_URL".to_owned(),
        value: "http://dead/v1".to_owned(),
    };
    let shim_path = shim_path_for(Platform::Unix, tmp.path(), "claude");
    install_shim(Platform::Unix, &shim_path, real, dead, &env)?;
    let out = Command::new(&shim_path).output()?;
    print!("stdout: {}", String::from_utf8_lossy(&out.stdout));
    Ok(())
}
