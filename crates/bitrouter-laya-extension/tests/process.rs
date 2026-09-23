#[cfg(unix)]
#[test]
fn local_provider_process_contract() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("python3")
        .arg("-B")
        .arg("-m")
        .arg("unittest")
        .arg("discover")
        .arg("-s")
        .arg("local_provider/tests")
        .arg("-p")
        .arg("test_*.py")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()?;
    assert!(status.success(), "local Laya process contract failed");
    Ok(())
}
