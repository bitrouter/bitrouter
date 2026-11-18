//! Native operator checks use the same working-copy publisher as model calls.
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

fn reply() -> String {
    stream(chunk(json!({"content":"Ready."}), Some("stop")) + "data: [DONE]\n\n")
}

#[test]
fn check_publishes_regular_file_changes_without_importing_linked_peer_data() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("project");
    let home = fixture.path().join("home");
    fs::create_dir(&work)?;
    fs::create_dir(work.join("old"))?;
    fs::write(work.join("old/note"), "move me")?;
    fs::write(work.join("script"), "seed")?;
    fs::set_permissions(work.join("script"), fs::Permissions::from_mode(0o640))?;
    let original = fs::metadata(work.join("script"))?;
    let peer = fixture.path().join("peer");
    fs::write(&peer, "peer fixture")?;
    symlink(&peer, work.join("symlink"))?;
    fs::hard_link(&peer, work.join("hardlink"))?;
    let mock = Mock::start(vec![reply()])?;
    let quoted = |p: &Path| -> Result<String> {
        Ok(format!(
            "'{}'",
            p.to_str().context("fixture path")?.replace('\'', "'\\''")
        ))
    };
    let command = format!(
        r#"set -e
if cat {peer} >/dev/null 2>&1; then exit 12; fi
if (printf wrong > {peer}) 2>/dev/null; then exit 13; fi
if cat {original} >/dev/null 2>&1; then exit 14; fi
test ! -e symlink && test ! -e hardlink || exit 10
test -z "$BITROUTER_API_KEY" && test -z "$OPENAI_API_KEY" || exit 11
cat script > copy
printf changed > script
chmod 750 script
mkdir -p new/child
mv old/note new/child/moved
rmdir old
printf evidence"#,
        peer = quoted(&peer)?,
        original = quoted(&work.join("script"))?
    );
    let output = bro(
        &home,
        &work,
        &mock.endpoint,
        &["task", "run", "verify", "--check", &command],
    )?;
    let result = event(&output)?;
    ensure!(output.status.success(), "{result}");
    assert_eq!(result["verification"], "passed");
    assert_eq!(result["verification_evidence"]["stdout"], "evidence");
    assert_eq!(fs::read(work.join("script"))?, b"changed");
    let published = fs::metadata(work.join("script"))?;
    assert_eq!(published.ino(), original.ino());
    assert_eq!(published.mode() & 0o777, 0o750);
    assert_eq!(fs::read(work.join("copy"))?, b"seed");
    assert_eq!(fs::metadata(work.join("copy"))?.mode() & 0o077, 0);
    assert_eq!(fs::read(work.join("new/child/moved"))?, b"move me");
    assert!(!work.join("old").exists());
    assert_eq!(fs::read(&peer)?, b"peer fixture");
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[test]
fn conflict_preserves_exit_evidence_and_cold_resume_never_reexecutes() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("project");
    let home = fixture.path().join("home");
    fs::create_dir(&work)?;
    let peer = fixture.path().join("peer");
    fs::write(&peer, "peer fixture")?;
    fs::hard_link(&peer, work.join("reserved"))?;
    let mock = Mock::start(vec![reply()])?;
    let run = |args: &[&str]| bro(&home, &work, &mock.endpoint, args);
    let result = run(&[
        "task",
        "run",
        "verify",
        "--check",
        "echo once; echo new > first; echo write > reserved",
    ])?;
    assert!(!result.status.success());
    let terminal = event(&result)?;
    assert_eq!(terminal["status"], "recovery_required");
    assert_eq!(terminal["unknown_effect"], true);
    let evidence = &terminal["verification_evidence"];
    assert_eq!(evidence["exit_status"], 0);
    assert_eq!(evidence["stdout"], "once\n");
    assert!(
        evidence["error"]
            .as_str()
            .context("missing conflict evidence")?
            .contains("excluded entry")
    );
    assert_eq!(fs::read(&peer)?, b"peer fixture");
    assert!(!work.join("first").exists());
    // Removing the conflict would let a retry succeed. Resume must return the
    // original evidence without invoking the command a second time.
    fs::remove_file(work.join("reserved"))?;
    let thread = terminal["thread_id"].as_str().context("thread")?;
    let turn = terminal["turn_id"].as_str().context("turn")?;
    assert_eq!(
        event(&run(&["thread", "resume", thread, "--turn", turn])?)?,
        terminal
    );
    assert!(!work.join("first").exists());
    assert!(!work.join("reserved").exists());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[test]
fn over_limit_input_refuses_before_launch() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("project");
    let home = fixture.path().join("home");
    fs::create_dir(&work)?;
    fs::File::create(work.join("large"))?.set_len(16 * 1024 * 1024 + 1)?;
    let mock = Mock::start(vec![reply()])?;
    let result = event(&bro(
        &home,
        &work,
        &mock.endpoint,
        &["task", "run", "verify", "--check", "echo ran > marker"],
    )?)?;
    assert_eq!(result["verification"], "unavailable");
    assert_eq!(result["unknown_effect"], false);
    assert!(
        result["verification_evidence"]["error"]
            .as_str()
            .context("missing limit evidence")?
            .contains("byte limit")
    );
    assert!(!work.join("marker").exists());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[test]
fn partial_publication_keeps_private_recovery_copy_and_refuses_replay() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("project");
    let home = fixture.path().join("home");
    fs::create_dir(&work)?;
    let locked = work.join("z-original");
    fs::write(&locked, "original")?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o400))?;
    if fs::OpenOptions::new().write(true).open(&locked).is_ok() {
        return Ok(()); // Superuser bypasses this fixture's write denial.
    }
    let mock = Mock::start(vec![reply()])?;
    let run = |args: &[&str]| bro(&home, &work, &mock.endpoint, args);
    let result = event(&run(&[
        "task",
        "run",
        "verify",
        "--check",
        "printf complete > a-first && chmod u+w z-original && printf desired > z-original",
    ])?)?;
    assert_eq!(result["status"], "recovery_required");
    assert_eq!(result["unknown_effect"], true);
    assert_eq!(result["verification_evidence"]["exit_status"], 0);
    assert_eq!(fs::read(work.join("a-first"))?, b"complete");
    assert_eq!(fs::read(&locked)?, b"original");
    let error = result["verification_evidence"]["error"]
        .as_str()
        .context("publication evidence")?;
    let copy = error
        .split_once("private recovery copy retained at ")
        .and_then(|(_, s)| s.split_once(": "))
        .context("retained copy")?
        .0;
    let copy = Path::new(copy);
    let private = copy.parent().context("private directory")?;
    ensure!(
        private.parent() == Some(Path::new("/private/tmp"))
            && private
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("bro-shell-"))
            && copy.file_name().is_some_and(|n| n == "workspace"),
        "invalid recovery location"
    );
    assert_eq!(fs::metadata(private)?.mode() & 0o777, 0o700);
    assert_eq!(fs::read(copy.join("z-original"))?, b"desired");
    fs::remove_dir_all(private)?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600))?;
    fs::remove_file(work.join("a-first"))?;
    let thread = result["thread_id"].as_str().context("thread")?;
    let turn = result["turn_id"].as_str().context("turn")?;
    assert_eq!(
        event(&run(&["thread", "resume", thread, "--turn", turn])?)?,
        result
    );
    assert!(!work.join("a-first").exists());
    assert_eq!(fs::read(&locked)?, b"original");
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[test]
fn moving_a_file_with_unrepresented_metadata_preserves_the_original() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let work = fixture.path().join("project");
    fs::create_dir(&work)?;
    let old = work.join("original");
    fs::write(&old, "data")?;
    ensure!(
        Command::new("/usr/bin/xattr")
            .args(["-w", "user.bro-fixture", "annotation"])
            .arg(&old)
            .status()?
            .success(),
        "could not install fixture xattr"
    );
    let mock = Mock::start(vec![reply()])?;
    let result = event(&bro(
        &fixture.path().join("home"),
        &work,
        &mock.endpoint,
        &["task", "run", "verify", "--check", "mv original moved"],
    )?)?;
    assert_eq!(result["status"], "recovery_required");
    assert_eq!(result["verification_evidence"]["exit_status"], 0);
    assert!(
        result["verification_evidence"]["error"]
            .as_str()
            .context("metadata refusal")?
            .contains("extended attributes")
    );
    assert_eq!(fs::read(&old)?, b"data");
    let attr = Command::new("/usr/bin/xattr")
        .args(["-p", "user.bro-fixture"])
        .arg(&old)
        .output()?;
    assert!(attr.status.success());
    assert_eq!(attr.stdout, b"annotation\n");
    assert!(!work.join("moved").exists());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}
