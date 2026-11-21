//! Verify that checkpoint storage does not adopt a gateway home.
#[path = "support/binary.rs"]
mod binary;
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::{ffi::OsString, fs, path::Path, process::Command};

fn entries(path: &Path) -> Result<Option<Vec<OsString>>> {
    let dir = match fs::read_dir(path) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut names = dir
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(Some(names))
}

fn reject(home: &Path) -> Result<()> {
    let before = entries(home)?;
    let id = "2ad653eb-9a51-4999-b59f-3c4b1d65174c";
    let mut commands = vec![vec!["thread", "create"], vec!["thread", "show", id]];
    if before.is_some() {
        commands.push(vec!["skills", "explain", "--thread", id, "--turn", id]);
    }
    for args in commands {
        let output = Command::new(binary::bro())
            .env("BRO_HOME", home)
            .args(&args)
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        ensure!(
            !output.status.success() && stderr.contains("gateway home"),
            "BRO {args:?} should reject {} as a gateway home: {stderr}",
            home.display()
        );
        assert_eq!(entries(home)?, before);
    }
    assert!(!home.join("threads").exists());
    Ok(())
}

#[test]
fn ordinary_gateway_home_is_rejected_before_any_bro_writes() -> Result<()> {
    for (name, bytes) in [
        ("bitrouter.db", b"fixture".as_slice()),
        (".bitrouter-home-role", b"bitrouter\n".as_slice()),
    ] {
        let home = tempfile::tempdir()?;
        let file = home.path().join(name);
        fs::write(&file, bytes)?;
        reject(home.path())?;
        assert_eq!(fs::read(file)?, bytes);
    }
    Ok(())
}

#[test]
fn nested_gateway_home_is_rejected_before_any_bro_writes() -> Result<()> {
    for (name, bytes) in [
        ("bitrouter.db", b"fixture".as_slice()),
        (".bitrouter-home-role", b"bitrouter\n".as_slice()),
    ] {
        let fixture = tempfile::tempdir()?;
        let gateway = fixture.path().join("gateway");
        let existing = gateway.join("existing/nested");
        fs::create_dir_all(&existing)?;
        let file = gateway.join(name);
        fs::write(&file, bytes)?;

        let missing = existing.join("new/bro");
        reject(&missing)?;
        reject(&existing)?;
        #[cfg(unix)]
        {
            let alias = fixture.path().join("alias");
            std::os::unix::fs::symlink(&existing, &alias)?;
            reject(&alias.join("new/bro"))?;
        }
        assert!(fs::read_dir(&existing)?.next().is_none());
        assert_eq!(fs::read(file)?, bytes);
    }
    Ok(())
}

#[test]
fn gateway_owned_threads_directory_is_rejected_before_any_bro_writes() -> Result<()> {
    let home = tempfile::tempdir()?;
    let threads = home.path().join("threads");
    fs::create_dir(&threads)?;
    let marker = threads.join(".bitrouter-home-role");
    let sentinel = threads.join("sentinel");
    fs::write(&marker, b"bitrouter\n")?;
    fs::write(&sentinel, b"untouched")?;
    let home_before = entries(home.path())?;
    let threads_before = entries(&threads)?;

    let output = Command::new(binary::bro())
        .env("BRO_HOME", home.path())
        .args(["thread", "show", "2ad653eb-9a51-4999-b59f-3c4b1d65174c"])
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    ensure!(
        !output.status.success() && stderr.contains("gateway home"),
        "BRO should refuse the gateway-owned threads directory: {stderr}"
    );
    assert_eq!(entries(home.path())?, home_before);
    assert_eq!(entries(&threads)?, threads_before);
    assert_eq!(fs::read(marker)?, b"bitrouter\n");
    assert_eq!(fs::read(sentinel)?, b"untouched");
    Ok(())
}

#[cfg(any(unix, windows))]
#[test]
fn linked_threads_directory_is_rejected_before_any_bro_writes() -> Result<()> {
    for claimed in [false, true] {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path().join("bro");
        let gateway = fixture.path().join("gateway");
        let threads = home.join("threads");
        let sentinel = gateway.join("sentinel");
        let gateway_marker = gateway.join(".bitrouter-home-role");
        fs::create_dir(&home)?;
        fs::create_dir(&gateway)?;
        fs::write(&sentinel, b"untouched")?;
        fs::write(&gateway_marker, b"bitrouter\n")?;
        let marker = home.join(".bitrouter-home-role");
        if claimed {
            fs::write(&marker, b"bro\n")?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&gateway, &threads)?;
        #[cfg(windows)]
        {
            let output = Command::new("cmd.exe")
                .current_dir(fixture.path())
                .args(["/D", "/C", "mklink", "/J", "bro\\threads"])
                .arg(&gateway)
                .output()?;
            ensure!(
                output.status.success(),
                "creating threads junction: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(threads.canonicalize()?, gateway.canonicalize()?);
        let home_before = entries(&home)?;
        let gateway_before = entries(&gateway)?;
        let output = Command::new(binary::bro())
            .env("BRO_HOME", &home)
            .args(["thread", "show", "2ad653eb-9a51-4999-b59f-3c4b1d65174c"])
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        ensure!(
            !output.status.success() && stderr.contains("BRO threads directory"),
            "BRO should refuse the linked threads directory: {stderr}"
        );
        assert_eq!(entries(&home)?, home_before);
        assert_eq!(entries(&gateway)?, gateway_before);
        assert_eq!(threads.canonicalize()?, gateway.canonicalize()?);
        assert_eq!(fs::read(sentinel)?, b"untouched");
        assert_eq!(fs::read(gateway_marker)?, b"bitrouter\n");
        if claimed {
            assert_eq!(fs::read(marker)?, b"bro\n");
        }
    }
    Ok(())
}

#[cfg(unix)]
#[path = "home_owner/gateway.rs"]
mod gateway;

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires the separately built gateway in BITROUTER_TEST_GATEWAY_BINARY"]
async fn gateway_refuses_bro_checkpoint_homes() -> Result<()> {
    let binary = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY").context("gateway executable")?;
    let fixture = tempfile::tempdir()?;
    gateway::ordinary_and_unmarked(&binary, fixture.path()).await
}
