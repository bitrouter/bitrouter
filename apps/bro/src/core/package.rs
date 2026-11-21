//! Package-manager ownership and commands for an attached gateway.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bitrouter_update::{Product, paired_npm_root};
use serde_json::{Value, json};
use tokio::process::Command;

use super::{SERVICE, display, path_executable};

pub(super) enum Owner {
    Brew {
        executable: PathBuf,
        formula: PathBuf,
    },
    Npm {
        executable: PathBuf,
        package: PathBuf,
    },
}

impl Owner {
    pub(super) fn npm(executable: PathBuf, package: PathBuf) -> Self {
        Self::Npm {
            executable,
            package,
        }
    }

    pub(super) fn discover(executable: &Path) -> Option<Self> {
        if let Some(package) = paired_npm_root(SERVICE, executable) {
            return Some(Self::npm(executable.to_owned(), package.to_owned()));
        }
        let formula = brew_formula(executable)?;
        Some(Self::Brew {
            executable: executable.to_owned(),
            formula: formula.to_owned(),
        })
    }

    pub(super) fn executable(&self) -> &Path {
        match self {
            Self::Brew { executable, .. } | Self::Npm { executable, .. } => executable,
        }
    }

    fn report(&self, status: &str) -> Value {
        json!({
            "status": status, "executable": display(self.executable()),
            "install_method": match self { Self::Brew { .. } => "homebrew", Self::Npm { .. } => "npm" },
            "package": match self { Self::Brew { .. } => SERVICE.package(), Self::Npm { .. } => Product::Bro.package() },
        })
    }

    fn delegated(&self) -> Result<()> {
        let mut report = self.report("delegated");
        match self {
            Self::Brew { .. } => {
                report["upgrade_command"] = json!("brew upgrade bitrouter");
                report["reason"] =
                    json!("Could not identify the Homebrew command that owns this executable.");
            }
            Self::Npm { .. } => {
                report["upgrade_commands"] = json!({
                    "local": "npm update bitrouter-bro",
                    "global": "npm update --global bitrouter-bro",
                });
                report["reason"] = json!(
                    "Run the command in the owning project, or use the global command for a global package."
                );
            }
        }
        crate::emit(&report)
    }

    pub(super) async fn status(&self) -> Result<()> {
        if matches!(self, Self::Brew { .. }) && self.manager().await?.is_none() {
            return self.delegated();
        }
        let mut report = self.report("installed");
        report["current_version"] = json!(installed_version(self.executable()).await?);
        crate::emit(&report)
    }

    pub(super) async fn update(&self, check: bool, yes: bool) -> Result<()> {
        let Some(manager) = self.manager().await? else {
            return self.delegated();
        };
        let executable = manager.executable(self).await?;
        ensure!(
            executable.as_deref() == Some(self.executable()),
            "package ownership changed during discovery"
        );
        let current = installed_version(self.executable()).await?;
        let upgrade = if !check && matches!(&manager, Manager::Brew(_)) {
            // Let normal brew upgrade refresh stale formula metadata.
            Some("the available release".to_owned())
        } else {
            manager.available().await?
        };
        let mut report = self.report("checked");
        report["current_version"] = json!(current);
        if let Some(scope) = manager.scope() {
            report["install_scope"] = json!(scope);
        }
        if check {
            report["update_available"] = json!(upgrade.is_some());
            if let Some(version) = upgrade {
                report["target_package_version"] = json!(version);
            }
        } else if let Some(target) = upgrade {
            if !yes
                && !crate::update::confirm(
                    match self {
                        Self::Brew { .. } => "BitRouter (Homebrew)",
                        Self::Npm { .. } => "BitRouter BRO package (npm)",
                    },
                    &current,
                    &target,
                )?
            {
                report["status"] = json!("aborted");
            } else {
                let still_owned = manager.executable(self).await?;
                ensure!(
                    still_owned.as_deref() == Some(self.executable()),
                    "package ownership changed after checking for updates"
                );
                manager.upgrade().await?;
                let executable = manager
                    .executable(self)
                    .await?
                    .context("package manager did not leave a verified gateway executable")?;
                let version = installed_version(&executable).await?;
                let changed = matches!(self, Self::Npm { .. })
                    || executable != self.executable()
                    || version != current;
                report["status"] = json!(if changed { "updated" } else { "unchanged" });
                report["executable"] = json!(display(&executable));
                report["target_version"] = json!(version);
            }
        } else {
            report["status"] = json!("unchanged");
        }
        crate::emit(&report)
    }

    async fn manager(&self) -> Result<Option<Manager>> {
        match self {
            Self::Brew { formula, .. } => {
                let adjacent = formula
                    .parent()
                    .and_then(Path::parent)
                    .map(|prefix| prefix.join("bin/brew"));
                let path = adjacent
                    .filter(|path| path_executable(path))
                    .or_else(|| find_command(&["brew"]));
                let Some(path) = path else {
                    return Ok(None);
                };
                let manager = Manager::Brew(path);
                Ok(
                    (manager.executable(self).await?.as_deref() == Some(self.executable()))
                        .then_some(manager),
                )
            }
            Self::Npm { package, .. } => {
                let names: &[_] = if cfg!(windows) {
                    &["npm.exe", "npm.cmd"]
                } else {
                    &["npm"]
                };
                let Some(path) = find_command(names) else {
                    return Ok(None);
                };
                let Some(modules) = package.parent() else {
                    return Ok(None);
                };
                let global = Manager::Npm {
                    command: path.clone(),
                    project: None,
                };
                if global.npm_root().await?.as_deref() == Some(modules) {
                    return Ok(Some(global));
                }
                let Some(project) = npm_project(package) else {
                    return Ok(None);
                };
                let local = Manager::Npm {
                    command: path,
                    project: Some(project.to_owned()),
                };
                Ok((local.npm_root().await?.as_deref() == Some(modules)).then_some(local))
            }
        }
    }
}

enum Manager {
    Brew(PathBuf),
    Npm {
        command: PathBuf,
        project: Option<PathBuf>,
    },
}

impl Manager {
    fn scope(&self) -> Option<&'static str> {
        match self {
            Self::Brew(_) => None,
            Self::Npm {
                project: Some(_), ..
            } => Some("local"),
            Self::Npm { project: None, .. } => Some("global"),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        match self {
            Self::Brew(path) => {
                let mut command = Command::new(path);
                command
                    .args(args)
                    .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
                    .env("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", "1");
                if args.first() != Some(&"upgrade") {
                    command.env("HOMEBREW_NO_AUTO_UPDATE", "1");
                }
                command
            }
            Self::Npm { command, project } => {
                // On Windows Rust invokes a .cmd file through cmd.exe. Every
                // command-line argument here is fixed, including the package.
                let mut command = Command::new(command);
                command.args(args);
                if let Some(project) = project {
                    command
                        .current_dir(project)
                        .args(["--global=false", "--workspaces=false"]);
                } else {
                    command.arg("--global");
                }
                command
            }
        }
    }

    async fn npm_root(&self) -> Result<Option<PathBuf>> {
        let output = run(self.command(&["root"]), Duration::from_secs(15)).await?;
        Ok(output
            .status
            .success()
            .then(|| output_path(&output))
            .flatten())
    }

    async fn executable(&self, owner: &Owner) -> Result<Option<PathBuf>> {
        match owner {
            Owner::Brew { formula, .. } => {
                let output = run(
                    self.command(&["--prefix", SERVICE.package()]),
                    Duration::from_secs(15),
                )
                .await?;
                Ok(output
                    .status
                    .success()
                    .then(|| output_path(&output))
                    .flatten()
                    .and_then(|prefix| {
                        prefix
                            .join("bin")
                            .join(SERVICE.binary())
                            .canonicalize()
                            .ok()
                    })
                    .filter(|exe| {
                        brew_formula(exe) == Some(formula.as_path()) && path_executable(exe)
                    }))
            }
            Owner::Npm {
                executable,
                package,
            } => {
                let root = self.npm_root().await?;
                Ok((root.as_deref() == package.parent()
                    && executable.canonicalize().ok().as_deref() == Some(executable.as_path())
                    && paired_npm_root(SERVICE, executable) == Some(package.as_path())
                    && path_executable(executable))
                .then(|| executable.clone()))
            }
        }
    }

    // The manager's package version can differ from the service's native version.
    async fn available(&self) -> Result<Option<String>> {
        match self {
            Self::Brew(_) => {
                let output = run(
                    self.command(&["outdated", "--json=v2", "--formula", SERVICE.package()]),
                    Duration::from_secs(60),
                )
                .await?;
                let data = serde_json::from_slice::<Value>(&output.stdout);
                let formulae = data
                    .as_ref()
                    .ok()
                    .and_then(|data| data["formulae"].as_array());
                // Homebrew exits with 1 when a named formula is outdated.
                if output.status.code() != Some(1)
                    || !formulae.is_some_and(|values| !values.is_empty())
                {
                    succeeded("brew outdated", &output)?;
                }
                let formulae = formulae.context("decode brew outdated report")?;
                let Some(formula) = formulae.first() else {
                    return Ok(None);
                };
                ensure!(
                    formulae.len() == 1
                        && formula["name"]
                            .as_str()
                            .and_then(|name| name.rsplit('/').next())
                            == Some(SERVICE.package()),
                    "unexpected formula in brew outdated report"
                );
                if formula["pinned"] == true {
                    return Ok(None);
                }
                Ok(Some(
                    formula["current_version"]
                        .as_str()
                        .context("brew did not report the target formula version")?
                        .to_owned(),
                ))
            }
            Self::Npm { .. } => {
                let package = Product::Bro.package();
                let output = run(
                    self.command(&["outdated", "--json", package]),
                    Duration::from_secs(60),
                )
                .await?;
                let data = serde_json::from_slice::<Value>(&output.stdout);
                // npm exits with 1 when it reports an outdated package.
                if output.status.code() != Some(1)
                    || !data.as_ref().is_ok_and(|data| data[package].is_object())
                {
                    succeeded("npm outdated", &output)?;
                }
                let data = data.context("decode npm outdated report")?;
                ensure!(data.is_object(), "invalid npm outdated report");
                let entry = &data[package];
                if entry.is_null() {
                    return Ok(None);
                }
                let current = entry["current"]
                    .as_str()
                    .context("npm did not report an installed package version")?;
                let wanted = entry["wanted"]
                    .as_str()
                    .context("npm did not report a compatible package version")?;
                Ok((current != wanted).then(|| wanted.to_owned()))
            }
        }
    }

    async fn upgrade(&self) -> Result<()> {
        let args: &[_] = match self {
            Self::Brew(_) => &["upgrade", "--formula", SERVICE.package()],
            Self::Npm { .. } => &["update", Product::Bro.package()],
        };
        let output = run(self.command(args), Duration::from_secs(600)).await?;
        succeeded("package-manager upgrade", &output)
    }
}

fn brew_formula(executable: &Path) -> Option<&Path> {
    let bin = executable.parent()?;
    let formula = bin.parent()?.parent()?;
    (executable.file_name()? == SERVICE.binary().as_str()
        && bin.file_name()? == "bin"
        && formula.file_name()? == SERVICE.package()
        && formula.parent()?.file_name()? == "Cellar")
        .then_some(formula)
}

fn npm_project(package: &Path) -> Option<&Path> {
    let modules = package.parent()?;
    if modules.file_name()? != "node_modules" {
        return None;
    }
    let root = modules.parent()?;
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.join("package.json")).ok()?).ok()?;
    ["dependencies", "devDependencies", "optionalDependencies"]
        .iter()
        .any(|field| manifest[*field][Product::Bro.package()].is_string())
        .then_some(root)
}

fn find_command(names: &[&str]) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path_executable(path))
}

fn output_path(output: &Output) -> Option<PathBuf> {
    let path = Path::new(
        std::str::from_utf8(&output.stdout)
            .ok()?
            .trim_end_matches(['\r', '\n']),
    );
    path.is_absolute()
        .then(|| path.canonicalize().ok())
        .flatten()
}

async fn installed_version(executable: &Path) -> Result<String> {
    let mut command = Command::new(executable);
    command.args(["--json", "build-info"]);
    let output = run(command, Duration::from_secs(10)).await?;
    succeeded("gateway build-info", &output)?;
    let info: Value =
        serde_json::from_slice(&output.stdout).context("decode gateway build-info")?;
    ensure!(
        info["product"] == SERVICE.package()
            && info["profile"] == "full"
            && info["target"] == env!("BRO_BUILD_TARGET"),
        "package-managed executable does not identify as the full native BitRouter gateway"
    );
    Ok(info["version"]
        .as_str()
        .filter(|version| !version.is_empty())
        .context("missing gateway version")?
        .to_owned())
}

async fn run(mut command: Command, timeout: Duration) -> Result<Output> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    tokio::time::timeout(timeout, command.kill_on_drop(true).output())
        .await
        .with_context(|| format!("{program} timed out"))?
        .with_context(|| format!("run {program}"))
}

fn succeeded(operation: &str, output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "{operation} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
