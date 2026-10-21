//! Real CLI lifecycle, isolated scopes and local Git object fixtures.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::process::Command;

#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;

fn fixture_binary() -> Result<PathBuf> {
    static BINARY: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| example_binary::build("bitrouter", "skills_fixture"))
        .clone()
        .map_err(anyhow::Error::msg)
}

struct Fixture {
    home: tempfile::TempDir,
    project: PathBuf,
    source: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let home = tempfile::tempdir()?;
        let project = home.path().join("project");
        let source = home.path().join("source");
        std::fs::create_dir_all(&project)?;
        std::fs::create_dir_all(&source)?;
        std::fs::create_dir_all(home.path().join("user"))?;
        Ok(Self {
            home,
            project,
            source,
        })
    }
    fn skill(&self, name: &str) -> Result<PathBuf> {
        let source = self.source.join(name);
        std::fs::create_dir_all(source.join("references"))?;
        std::fs::create_dir_all(source.join("scripts"))?;
        std::fs::create_dir_all(source.join("assets/empty"))?;
        std::fs::write(
            source.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Fixture instructions\nmetadata:\n  arbitrary: preserved\n---\n\nRead references/guide.md.\n"
            ),
        )?;
        std::fs::write(source.join("references/guide.md"), "version one\n")?;
        std::fs::write(source.join("assets/data.bin"), [0, 255, 1, 128])?;
        Ok(source)
    }
    fn cli(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .current_dir(&self.project)
            .env("HOME", self.home.path().join("user"))
            .env("USERPROFILE", self.home.path().join("user"))
            .env("BITROUTER_HOME", self.home.path().join("bro"))
            .env("CODEX_HOME", self.home.path().join("codex"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_CACHE_HOME", self.home.path().join("cache"))
            .env(
                "GIT_CONFIG_GLOBAL",
                self.home.path().join("empty-gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "BRO_SKILLS_SCRIPT_MARKER",
                self.home.path().join("executed"),
            );
        command
            .arg("skills")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
    async fn run(&self, args: &[&str], success: bool) -> Result<Value> {
        run(self.cli(args), success).await
    }
    fn index(&self) -> PathBuf {
        self.project.join(".bitrouter/skills/installed.json")
    }
}

#[tokio::test]
async fn unicode_skill_names_install_show_and_uninstall_through_the_actual_cli() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.skill("审阅-٢")?;
    let installed = fixture.run(&["install", text(&source)?], true).await?;
    let row = managed(&installed)?;
    ensure!(row["name"] == "审阅-٢");
    let shown = fixture.run(&["show", "审阅-٢"], true).await?;
    ensure!(managed(&shown)?["name"] == "审阅-٢");
    let removed = fixture.run(&["uninstall", "审阅-٢"], true).await?;
    ensure!(removed.to_string().contains("审阅-٢"));
    ensure!(
        version_dir(row)?.is_dir(),
        "uninstall retains original version"
    );
    Ok(())
}

async fn run(mut command: Command, success: bool) -> Result<Value> {
    let output = tokio::time::timeout(Duration::from_secs(90), command.output()).await??;
    ensure!(
        output.status.success() == success,
        "unexpected CLI status: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(!output.stdout.contains(&0x1b), "ANSI in JSON");
    serde_json::from_slice(&output.stdout).context("CLI JSON")
}
fn text(path: &Path) -> Result<&str> {
    path.to_str().context("UTF-8 path")
}
fn managed(report: &Value) -> Result<&Value> {
    report["skills"]
        .as_array()
        .context("skills")?
        .iter()
        .find(|row| row["ownership"] == "managed" && row["installed"] == true)
        .context("managed installation")
}
fn version_dir(row: &Value) -> Result<PathBuf> {
    Ok(PathBuf::from(
        row["version"]["directory"]
            .as_str()
            .context("version path")?,
    ))
}
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()?;
    ensure!(
        output.status.success(),
        "Git fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
fn commit(repo: &Path, message: &str) -> Result<String> {
    git(repo, &["add", "."])?;
    git(repo, &["commit", "-m", message])?;
    git(repo, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn local_complete_snapshot_updates_and_retains_versions_after_uninstall_and_restart()
-> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.skill("alpha")?;
    let binary = fixture_binary()?;
    std::fs::copy(binary, source.join("scripts/install"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            source.join("scripts/install"),
            std::fs::Permissions::from_mode(0o755),
        )?;
    }
    let installed = fixture.run(&["install", text(&source)?], true).await?;
    let row = managed(&installed)?;
    let original = row.clone();
    let old_dir = version_dir(row)?;
    ensure!(installed["state"] == "installed" && installed["activation"] == "not_requested");
    ensure!(
        old_dir.join("assets/empty").is_dir()
            && std::fs::read(old_dir.join("assets/data.bin"))? == [0, 255, 1, 128]
    );
    ensure!(
        std::fs::read(old_dir.join("scripts/install"))?
            == std::fs::read(source.join("scripts/install"))?
    );
    ensure!(!fixture.home.path().join("executed").exists());
    std::fs::write(source.join("references/guide.md"), "version two\n")?;
    ensure!(std::fs::read_to_string(old_dir.join("references/guide.md"))? == "version one\n");
    let updated = fixture.run(&["update", "alpha"], true).await?;
    let new = managed(&updated)?;
    let new_dir = version_dir(new)?;
    ensure!(
        original["installation_id"] == new["installation_id"]
            && original["version"]["digest"] != new["version"]["digest"]
    );
    ensure!(new["versions"].as_array().is_some_and(|v| v.len() == 2));
    ensure!(std::fs::read_to_string(new_dir.join("references/guide.md"))? == "version two\n");
    let before = std::fs::read(fixture.index())?;
    std::fs::write(source.join("SKILL.md"), "invalid\n")?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == before);
    let removed = fixture.run(&["uninstall", "alpha"], true).await?;
    ensure!(removed["state"] == "uninstalled_retained");
    ensure!(
        removed["skills"][0]["installed"] == false
            && old_dir.join("SKILL.md").is_file()
            && new_dir.join("SKILL.md").is_file()
    );
    let reopened = fixture.run(&["show", "alpha"], true).await?;
    ensure!(
        reopened["skills"][0]["state"] == "retained"
            && reopened["skills"][0]["valid"] == true
            && reopened["skills"][0]["versions"]
                .as_array()
                .is_some_and(|v| v.len() == 2)
    );
    let index: Value = serde_json::from_slice(&std::fs::read(fixture.index())?)?;
    ensure!(index["aliases"] == json!({}));
    ensure!(std::fs::read_to_string(old_dir.join("references/guide.md"))? == "version one\n");
    Ok(())
}

#[tokio::test]
async fn same_names_across_scopes_and_unmanaged_content_keep_independent_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.skill("alpha")?;
    let unmanaged = fixture.project.join(".agents/skills/alpha");
    std::fs::create_dir_all(&unmanaged)?;
    std::fs::copy(source.join("SKILL.md"), unmanaged.join("SKILL.md"))?;
    let original = std::fs::read(unmanaged.join("SKILL.md"))?;
    let listing = fixture.run(&["list"], true).await?;
    ensure!(
        listing["skills"][0]["ownership"] == "unmanaged"
            && !fixture.project.join(".bitrouter").exists()
    );
    let project = fixture.run(&["install", text(&source)?], true).await?;
    let global = fixture
        .run(&["install", text(&source)?, "--global"], true)
        .await?;
    ensure!(managed(&project)?["installation_id"] != managed(&global)?["installation_id"]);
    ensure!(
        project["scope"]["kind"] == "project"
            && global["scope"]["kind"] == "global"
            && project["scope"]["library"] != global["scope"]["library"]
    );
    ensure!(
        fixture.run(&["show", "alpha"], true).await?["skills"]
            .as_array()
            .is_some_and(|rows| rows.len() == 2)
    );
    let before = std::fs::read(fixture.index())?;
    fixture.run(&["install", text(&source)?], false).await?;
    ensure!(std::fs::read(fixture.index())? == before);
    fixture.run(&["uninstall", "alpha"], true).await?;
    ensure!(managed(&fixture.run(&["show", "alpha", "--global"], true).await?).is_ok());
    ensure!(std::fs::read(unmanaged.join("SKILL.md"))? == original);
    fixture.run(&["uninstall", "alpha"], false).await?;
    ensure!(std::fs::read(unmanaged.join("SKILL.md"))? == original);
    let mut conflict = fixture.cli(&["list", "--global", "--workspace", text(&fixture.project)?]);
    ensure!(!conflict.output().await?.status.success());
    Ok(())
}

#[tokio::test]
async fn invalid_ambiguous_and_oversized_sources_publish_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let alpha = fixture.skill("alpha")?;
    fixture.skill("beta")?;
    fixture
        .run(&["install", text(&fixture.source)?], false)
        .await?;
    ensure!(!fixture.index().exists());
    fixture
        .run(
            &["install", text(&fixture.source)?, "--skill", "missing"],
            false,
        )
        .await?;
    std::fs::write(
        alpha.join("SKILL.md"),
        "---\nname: alpha\n---\nmissing description\n",
    )?;
    fixture.run(&["install", text(&alpha)?], false).await?;
    ensure!(!fixture.index().exists());
    fixture.skill("alpha")?;
    let file = std::fs::File::create(alpha.join("assets/large"))?;
    file.set_len(32 * 1024 * 1024)?;
    fixture.run(&["install", text(&alpha)?], false).await?;
    std::fs::remove_file(alpha.join("assets/large"))?;
    let file = std::fs::File::create(alpha.join("SKILL.md"))?;
    file.set_len(256 * 1024 + 1)?;
    fixture.run(&["install", text(&alpha)?], false).await?;
    ensure!(!fixture.index().exists());
    fixture.skill("alpha")?;
    for i in 0..1024 {
        std::fs::write(alpha.join("assets").join(format!("file-{i}")), "")?;
    }
    fixture.run(&["install", text(&alpha)?], false).await?;
    ensure!(!fixture.index().exists());
    ensure!(!fixture.project.join(".bitrouter").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_and_modified_installed_content_are_conflicts_not_overwrites() -> Result<()>
{
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new()?;
    let source = fixture.skill("alpha")?;
    let outside = fixture.home.path().join("outside");
    std::fs::write(&outside, "do not read/copy through a link")?;
    symlink(&outside, source.join("references/escape"))?;
    fixture.run(&["install", text(&source)?], false).await?;
    ensure!(!fixture.index().exists());
    std::fs::remove_file(source.join("references/escape"))?;
    let fifo = source.join("assets/fifo");
    let made = std::process::Command::new("mkfifo").arg(&fifo).output()?;
    ensure!(
        made.status.success(),
        "creating FIFO fixture: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    fixture.run(&["install", text(&source)?], false).await?;
    ensure!(!fixture.index().exists());
    std::fs::remove_file(fifo)?;
    let installed = fixture.run(&["install", text(&source)?], true).await?;
    let dir = version_dir(managed(&installed)?)?;
    let file = dir.join("references/guide.md");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(&file, "local modification")?;
    let original = std::fs::read(fixture.index())?;
    fixture.run(&["update", "alpha"], false).await?;
    fixture.run(&["uninstall", "alpha"], false).await?;
    ensure!(
        std::fs::read(fixture.index())? == original
            && std::fs::read_to_string(&file)? == "local modification"
    );
    let shown = fixture.run(&["show", "alpha"], true).await?;
    ensure!(shown["skills"][0]["valid"] == false && shown["skills"][0]["problem"].is_string());
    std::fs::remove_file(&file)?;
    symlink(&outside, &file)?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read_to_string(&outside)? == "do not read/copy through a link");
    Ok(())
}

#[tokio::test]
async fn git_branch_tracks_exact_commits_tags_and_commits_stay_pinned() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = fixture.source.join("repo");
    std::fs::create_dir_all(&repo)?;
    git(&repo, &["init", "-b", "main"])?;
    let skill = repo.join("skills/alpha");
    std::fs::create_dir_all(skill.join("references"))?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: alpha\ndescription: Git fixture\n---\nbody\n",
    )?;
    std::fs::write(skill.join("references/guide.md"), "one")?;
    let first = commit(&repo, "one")?;
    git(&repo, &["tag", "v1"])?;
    let source = format!("git+{}", repo.display());
    let installed = fixture
        .run(&["install", &source, "--skill", "alpha"], true)
        .await?;
    ensure!(managed(&installed)?["version"]["source"]["commit"] == first);
    let unchanged = fixture.run(&["update", "alpha"], true).await?;
    ensure!(
        unchanged["state"] == "unchanged"
            && unchanged["catalog_revision"] == installed["catalog_revision"]
    );
    std::fs::write(skill.join("references/guide.md"), "two")?;
    let second = commit(&repo, "two")?;
    git(&repo, &["tag", "v2"])?;
    let updated = fixture.run(&["update", "alpha"], true).await?;
    ensure!(
        managed(&updated)?["version"]["source"]["commit"] == second
            && managed(&updated)?["version"]["source"]["requested_ref"].is_null()
    );
    let pinned = fixture
        .run(&["update", "alpha", "--ref", "v1"], true)
        .await?;
    let pinned_dir = version_dir(managed(&pinned)?)?;
    ensure!(
        managed(&pinned)?["version"]["source"]["commit"] == first
            && managed(&pinned)?["version"]["source"]["tracking"] == false
    );
    git(&repo, &["tag", "-f", "v1"])?;
    let held = fixture.run(&["update", "alpha"], true).await?;
    ensure!(
        held["state"] == "unchanged_pinned"
            && managed(&held)?["version"]["source"]["commit"] == first
            && std::fs::read_to_string(pinned_dir.join("references/guide.md"))? == "one"
    );
    let explicit = fixture
        .run(&["update", "alpha", "--ref", "v2"], true)
        .await?;
    ensure!(managed(&explicit)?["version"]["source"]["commit"] == second);
    let commit_pin = fixture
        .run(&["update", "alpha", "--ref", &first], true)
        .await?;
    ensure!(
        managed(&commit_pin)?["version"]["source"]["commit"] == first
            && managed(&commit_pin)?["version"]["source"]["tracking"] == false
    );
    let before = std::fs::read(fixture.index())?;
    fixture
        .run(&["update", "alpha", "--ref", "missing"], false)
        .await?;
    ensure!(std::fs::read(fixture.index())? == before);
    std::fs::rename(&repo, fixture.source.join("repo-gone"))?;
    fixture.run(&["update", "alpha"], true).await?;
    ensure!(std::fs::read(fixture.index())? == before);
    Ok(())
}

#[tokio::test]
async fn git_root_bundles_multiple_selection_and_fetch_failure_keep_aliases() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = fixture.skill("alpha")?;
    git(&repo, &["init", "-b", "main"])?;
    commit(&repo, "root")?;
    let source = format!("git+{}", repo.display());
    let installed = fixture.run(&["install", &source], true).await?;
    ensure!(managed(&installed)?["version"]["source"]["skill_path"] == ".");
    let before = std::fs::read(fixture.index())?;
    std::fs::rename(&repo, fixture.source.join("gone"))?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == before);
    fixture
        .run(&["install", "git+ext::never-execute"], false)
        .await?;
    let collection = fixture.source.join("collection");
    std::fs::create_dir_all(&collection)?;
    git(&collection, &["init", "-b", "main"])?;
    for name in ["beta", "gamma"] {
        let directory = collection.join("skills").join(name);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: fixture\n---\nbody\n"),
        )?;
    }
    commit(&collection, "multiple")?;
    let source = format!("git+{}", collection.display());
    fixture.run(&["install", &source], false).await?;
    let chosen = fixture
        .run(&["install", &source, "--skill", "./skills/beta"], true)
        .await?;
    ensure!(managed(&chosen)?["name"] == "beta");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn git_hooks_filters_and_bundle_scripts_are_data_only() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new()?;
    let repo = fixture.skill("alpha")?;
    let executable = fixture_binary()?;
    std::fs::copy(&executable, repo.join("scripts/install"))?;
    std::fs::set_permissions(
        repo.join("scripts/install"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::write(repo.join(".gitattributes"), "*.md filter=attack\n")?;
    git(&repo, &["init", "-b", "main"])?;
    commit(&repo, "inert payload")?;
    let hooks = fixture.home.path().join("hooks");
    std::fs::create_dir_all(&hooks)?;
    for name in ["post-checkout", "post-merge", "post-rewrite"] {
        std::fs::copy(&executable, hooks.join(name))?;
        std::fs::set_permissions(hooks.join(name), std::fs::Permissions::from_mode(0o755))?;
    }
    let poison = fixture.home.path().join("gitconfig");
    std::fs::write(
        &poison,
        format!(
            "[core]\n hooksPath = {}\n[filter \"attack\"]\n smudge = {} attack\n required = true\n",
            hooks.display(),
            executable.display()
        ),
    )?;
    let source = format!("git+{}", repo.display());
    let mut command = fixture.cli(&["install", &source]);
    command.env("GIT_CONFIG_GLOBAL", poison);
    let installed = run(command, true).await?;
    ensure!(!fixture.home.path().join("executed").exists());
    let target = version_dir(managed(&installed)?)?;
    ensure!(
        std::fs::read(target.join("scripts/install"))? == std::fs::read(&executable)?
            && std::fs::read(target.join(".gitattributes"))?
                == std::fs::read(repo.join(".gitattributes"))?
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn simultaneous_git_updates_compare_and_swap_one_manifest() -> Result<()> {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new()?;
    let repo = fixture.skill("alpha")?;
    git(&repo, &["init", "-b", "main"])?;
    commit(&repo, "one")?;
    let source = format!("git+{}", repo.display());
    let installed = fixture.run(&["install", &source], true).await?;
    let old = version_dir(managed(&installed)?)?;
    std::fs::write(repo.join("references/guide.md"), "new")?;
    let commit = commit(&repo, "two")?;
    let executable = fixture_binary()?;
    let bin = fixture.home.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    symlink(executable, bin.join("git"))?;
    let real_git = std::process::Command::new("/usr/bin/which")
        .arg("git")
        .output()?;
    ensure!(real_git.status.success());
    let real_git = String::from_utf8(real_git.stdout)?.trim().to_owned();
    let gate = fixture.home.path().join("gate");
    std::fs::create_dir_all(&gate)?;
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))?;
    let mut children = Vec::new();
    for _ in 0..2 {
        let mut command = fixture.cli(&["update", "alpha"]);
        command
            .env("PATH", &path)
            .env("BRO_SKILLS_GIT_GATE", &gate)
            .env("BRO_SKILLS_REAL_GIT", &real_git);
        children.push(command.spawn()?);
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if std::fs::read_dir(&gate)?.count() >= 2 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    ensure!(std::fs::read_to_string(old.join("references/guide.md"))? == "version one\n");
    std::fs::write(gate.join("release"), "")?;
    let mut passed = 0;
    for child in children {
        let output =
            tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
        if output.status.success() {
            passed += 1;
        } else {
            ensure!(
                String::from_utf8_lossy(&output.stdout).contains("index changed"),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
    ensure!(passed == 1, "simultaneous snapshots both committed");
    let shown = fixture.run(&["show", "alpha"], true).await?;
    ensure!(
        managed(&shown)?["version"]["source"]["commit"] == commit && shown["catalog_revision"] == 2
    );
    ensure!(std::fs::read_to_string(old.join("references/guide.md"))? == "version one\n");
    Ok(())
}

#[tokio::test]
async fn project_root_workspace_override_foreign_index_and_init_remain_explicit() -> Result<()> {
    let fixture = Fixture::new()?;
    git(&fixture.project, &["init", "-b", "main"])?;
    let source = fixture.skill("alpha")?;
    let sub = fixture.project.join("deep/path");
    std::fs::create_dir_all(&sub)?;
    let mut command = fixture.cli(&["install", text(&source)?]);
    command.current_dir(&sub);
    let installed = run(command, true).await?;
    ensure!(installed["scope"]["root"] == text(&fixture.project.canonicalize()?)?);
    let other = fixture.home.path().join("other");
    std::fs::create_dir_all(&other)?;
    let explicit = fixture
        .run(
            &["install", text(&source)?, "--workspace", text(&other)?],
            true,
        )
        .await?;
    ensure!(explicit["scope"]["root"] == text(&other.canonicalize()?)?);
    let old = std::fs::read(fixture.index())?;
    std::fs::copy(
        other.join(".bitrouter/skills/installed.json"),
        fixture.index(),
    )?;
    let foreign = std::fs::read(fixture.index())?;
    fixture.run(&["uninstall", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == foreign);
    std::fs::write(fixture.index(), old)?;
    let initialized = fixture.run(&["init", "new-source"], true).await?;
    ensure!(
        initialized["created"] == true && fixture.project.join("new-source/SKILL.md").is_file()
    );
    ensure!(fixture.run(&["list"], true).await?["catalog_revision"] == 1);
    let mut human = fixture.cli(&["show", "alpha", "--human"]);
    let output = human.output().await?;
    ensure!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("Library:")
    );
    for verb in ["list", "show", "init", "install", "update", "uninstall"] {
        let output = fixture.cli(&[verb, "--help"]).output().await?;
        ensure!(output.status.success() && !output.stdout.contains(&0x1b));
    }
    Ok(())
}

#[tokio::test]
async fn failed_manifest_commit_keeps_alias_and_reinstall_retains_distinct_receipts() -> Result<()>
{
    let fixture = Fixture::new()?;
    let source = fixture.skill("alpha")?;
    let installed = fixture.run(&["install", text(&source)?], true).await?;
    let original = managed(&installed)?.clone();
    let mut index: Value = serde_json::from_slice(&std::fs::read(fixture.index())?)?;
    index["revision"] = json!(u64::MAX);
    let exhausted = serde_json::to_vec_pretty(&index)?;
    std::fs::write(fixture.index(), &exhausted)?;
    std::fs::write(source.join("assets/data.bin"), [7, 8, 9])?;
    let failed = fixture.run(&["update", "alpha"], false).await?;
    ensure!(failed.to_string().contains("revision exhausted"));
    ensure!(std::fs::read(fixture.index())? == exhausted);
    let old = fixture.run(&["show", "alpha"], true).await?;
    ensure!(managed(&old)?["version"] == original["version"]);
    ensure!(
        managed(&old)?["versions"]
            .as_array()
            .is_some_and(|v| v.len() == 1)
    );
    let versions = fixture.project.join(".bitrouter/skills/versions");
    ensure!(
        std::fs::read_dir(&versions)?
            .collect::<std::io::Result<Vec<_>>>()?
            .len()
            == 2
    );
    index["revision"] = json!(1);
    std::fs::write(fixture.index(), serde_json::to_vec_pretty(&index)?)?;
    let updated = fixture.run(&["update", "alpha"], true).await?;
    let current = managed(&updated)?.clone();
    ensure!(current["version"]["digest"] != original["version"]["digest"]);
    fixture.run(&["uninstall", "alpha"], true).await?;
    let reinstalled = fixture.run(&["install", text(&source)?], true).await?;
    let fresh = managed(&reinstalled)?;
    ensure!(fresh["installation_id"] != original["installation_id"]);
    ensure!(fresh["version"] == current["version"]);
    let old_id = original["installation_id"].as_str().context("old ID")?;
    let retained = fixture.run(&["show", old_id], true).await?;
    ensure!(retained["skills"][0]["state"] == "retained");
    ensure!(
        retained["skills"][0]["versions"]
            .as_array()
            .is_some_and(|v| v.len() == 2)
    );
    ensure!(
        fixture.run(&["list"], true).await?["skills"]
            .as_array()
            .is_some_and(|v| v.len() == 2)
    );
    Ok(())
}

#[tokio::test]
async fn global_unmanaged_roots_and_invalid_receipts_remain_untouched() -> Result<()> {
    let fixture = Fixture::new()?;
    let roots = [
        fixture.home.path().join("user/.agents/skills"),
        fixture.home.path().join("user/.codex/skills"),
        fixture.home.path().join("user/.claude/skills"),
        fixture.home.path().join("codex/skills"),
    ];
    let mut originals = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let name = format!("unmanaged-{i}");
        let path = root.join(&name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().context("skill parent")?)?;
        let bytes = format!("---\nname: {name}\ndescription: unmanaged\n---\n");
        std::fs::write(&path, &bytes)?;
        originals.push((path, bytes));
    }
    let report = fixture.run(&["list", "--global"], true).await?;
    let rows = report["skills"].as_array().context("rows")?;
    ensure!(
        rows.len() == 4
            && rows
                .iter()
                .all(|r| r["ownership"] == "unmanaged" && r["installation_id"].is_null())
    );
    ensure!(!fixture.home.path().join("bro").exists());
    let project_only = fixture.project.join("skills/project-only");
    std::fs::create_dir_all(&project_only)?;
    std::fs::write(
        project_only.join("SKILL.md"),
        "---\nname: project-only\ndescription: project scope only\n---\n",
    )?;
    let mut empty_codex = fixture.cli(&["list", "--global"]);
    empty_codex.env("CODEX_HOME", "");
    let scoped = run(empty_codex, true).await?;
    ensure!(scoped["skills"].as_array().is_some_and(
        |rows| rows.len() == 3 && rows.iter().all(|row| row["name"] != "project-only")
    ));
    fixture
        .run(&["uninstall", "unmanaged-0", "--global"], false)
        .await?;
    for (path, bytes) in originals {
        ensure!(std::fs::read(path)? == bytes.as_bytes());
    }
    let source = fixture.skill("alpha")?;
    let installed = fixture.run(&["install", text(&source)?], true).await?;
    let original = std::fs::read(fixture.index())?;
    let id = managed(&installed)?["installation_id"]
        .as_str()
        .context("ID")?;
    for problem in ["inventory", "path", "scope"] {
        let mut value: Value = serde_json::from_slice(&original)?;
        match problem {
            "inventory" => {
                value["installations"][id]["versions"][0]["bundle"]["files"][0]["digest"] =
                    json!("0".repeat(64))
            }
            "path" => {
                value["installations"][id]["versions"][0]["source"]["skill_path"] =
                    json!("../escape");
            }
            _ => value["scope"]["principal"] = json!("foreign-principal"),
        }
        if problem != "scope" {
            value["installations"][id]["current"] =
                value["installations"][id]["versions"][0].clone();
        }
        let invalid = serde_json::to_vec_pretty(&value)?;
        std::fs::write(fixture.index(), &invalid)?;
        fixture.run(&["uninstall", "alpha"], false).await?;
        ensure!(std::fs::read(fixture.index())? == invalid);
    }
    std::fs::write(fixture.index(), &original)?;
    let mut overlap = fixture.cli(&["install", text(&source)?, "--global"]);
    overlap.env("BITROUTER_HOME", fixture.home.path().join("user/.agents"));
    run(overlap, false).await?;
    ensure!(std::fs::read(fixture.index())? == original);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn git_symlinks_submodules_and_quota_failures_keep_the_old_alias() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.skill("alpha")?;
    git(&fixture.source, &["init", "-b", "main"])?;
    let first = commit(&fixture.source, "first")?;
    let remote = format!("git+{}", text(&fixture.source)?);
    fixture
        .run(&["install", &remote, "--skill", "alpha"], true)
        .await?;
    let original = std::fs::read(fixture.index())?;
    std::os::unix::fs::symlink("/outside", source.join("assets/escape"))?;
    commit(&fixture.source, "symlink")?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == original);
    std::fs::remove_file(source.join("assets/escape"))?;
    git(&fixture.source, &["add", "."])?;
    git(
        &fixture.source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},alpha/submodule"),
        ],
    )?;
    git(&fixture.source, &["commit", "-m", "gitlink"])?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == original);
    git(
        &fixture.source,
        &["update-index", "--force-remove", "alpha/submodule"],
    )?;
    let file = std::fs::File::create(source.join("assets/large"))?;
    file.set_len(32 * 1024 * 1024)?;
    commit(&fixture.source, "oversized")?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == original);
    std::fs::remove_file(source.join("assets/large"))?;
    for i in 0..1024 {
        std::fs::write(source.join("assets").join(format!("f-{i}")), "")?;
    }
    commit(&fixture.source, "too many files")?;
    fixture.run(&["update", "alpha"], false).await?;
    ensure!(std::fs::read(fixture.index())? == original);
    Ok(())
}
