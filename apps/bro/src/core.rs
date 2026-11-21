//! Manage the local gateway executable independently of gateway sessions.

mod package;
#[cfg(windows)]
mod windows;

use std::{
    env,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use bitrouter_update::{Product, channel, install::Installation};
use clap::{Args, Subcommand};
use serde_json::json;

const SERVICE: Product = Product::Service;

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Report the local gateway installation without contacting it.
    Status {
        /// Select an absolute gateway bin directory.
        #[arg(long, value_name = "DIR")]
        prefix: Option<PathBuf>,
    },
    /// Install a verified full gateway into an explicit user bin directory.
    Install {
        /// Absolute directory in which to place bitrouter.
        #[arg(long, value_name = "DIR")]
        prefix: PathBuf,
        #[command(flatten)]
        release: ReleaseOptions,
    },
    /// Update the local gateway through its verified installation owner.
    #[command(
        long_about = "Update the local gateway through its verified installation owner. Native installs support release tags and the stable channel. Homebrew uses the installed formula. npm follows the owning project's compatible version range or the global package channel."
    )]
    Update {
        /// Select an absolute gateway bin directory.
        #[arg(long, value_name = "DIR")]
        prefix: Option<PathBuf>,
        #[command(flatten)]
        release: ReleaseOptions,
        /// Check without installing. Homebrew uses its cached formula metadata.
        #[arg(long)]
        check: bool,
        /// Skip confirmation.
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Args)]
pub(crate) struct ReleaseOptions {
    /// Select an exact release tag, including for rollback.
    #[arg(long, conflicts_with = "stable")]
    tag: Option<String>,
    /// Select stable releases only.
    #[arg(long)]
    stable: bool,
}

impl ReleaseOptions {
    async fn resolve(&self) -> Result<channel::Target> {
        channel::resolve(
            SERVICE,
            env!("BRO_BUILD_TARGET"),
            self.tag.as_deref(),
            self.stable,
        )
        .await
    }
}

enum LocalCore {
    Owned(Installation),
    Package(package::Owner),
    External {
        executable: PathBuf,
        method: &'static str,
    },
    Missing,
}

impl LocalCore {
    fn find(prefix: Option<&Path>) -> Result<Self> {
        if let Some(prefix) = prefix {
            ensure!(prefix.is_absolute(), "install prefix must be absolute");
        }
        let bro = env::current_exe()?.canonicalize()?;
        if let Some((executable, package)) = npm_core(&bro)
            && prefix
                .is_none_or(|prefix| prefix.canonicalize().ok().as_deref() == executable.parent())
        {
            return Ok(Self::Package(package::Owner::npm(executable, package)));
        }
        if let Some(install) = Installation::discover(SERVICE, prefix)? {
            return Ok(Self::Owned(install));
        }
        let name = SERVICE.binary();
        let found = if let Some(prefix) = prefix {
            let path = prefix.join(name);
            match path.symlink_metadata() {
                Ok(_) => Some(path),
                Err(err) if err.kind() == ErrorKind::NotFound => None,
                Err(err) => return Err(err.into()),
            }
        } else {
            env::var_os("PATH").and_then(|path| {
                env::split_paths(&path)
                    .filter(|dir| dir.is_absolute())
                    .map(|dir| dir.join(&name))
                    .find(|path| path_executable(path))
            })
        };
        let Some(executable) = found else {
            return Ok(Self::Missing);
        };
        let canonical = executable
            .canonicalize()
            .ok()
            .filter(|path| path_executable(path));
        if let Some(owner) = canonical.as_deref().and_then(package::Owner::discover) {
            return Ok(Self::Package(owner));
        }
        let method = canonical.as_deref().map_or("unknown", |path| {
            bitrouter_update::install_method(path, bitrouter_update::cargo_home().as_deref())
        });
        Ok(Self::External { executable, method })
    }

    async fn report(self) -> Result<()> {
        match self {
            Self::Owned(install) => crate::emit(&json!({
                "status": "installed", "install_method": "cargo-dist",
                "executable": display(install.executable()), "current_version": install.version()
            })),
            Self::Package(owner) => owner.status().await,
            Self::External { executable, method } => crate::emit(&json!({
                "status": if method == "unknown" { "unmanaged" } else { "delegated" },
                "executable": display(&executable), "install_method": method,
                "upgrade_command": bitrouter_update::manual_command(SERVICE, method),
            })),
            Self::Missing => crate::emit(&json!({
                "status": "missing",
                "install_command": "bro core install --prefix <absolute-bin-dir>"
            })),
        }
    }
}

pub(crate) async fn run(action: Action) -> Result<()> {
    match action {
        Action::Status { prefix } => LocalCore::find(prefix.as_deref())?.report().await,
        Action::Install { prefix, release } => install(&prefix, release).await,
        Action::Update {
            prefix,
            release,
            check,
            yes,
        } => {
            let core = LocalCore::find(prefix.as_deref())?;
            if !matches!(&core, LocalCore::Owned(_)) {
                ensure!(
                    release.tag.is_none() && !release.stable,
                    "--tag and --stable are supported only for native cargo-dist installations"
                );
            }
            match core {
                LocalCore::Owned(install) => update(install, release, check, yes).await,
                LocalCore::Package(owner) => owner.update(check, yes).await,
                other => other.report().await,
            }
        }
    }
}

async fn install(prefix: &Path, options: ReleaseOptions) -> Result<()> {
    match LocalCore::find(Some(prefix))? {
        LocalCore::External { executable, .. } => anyhow::bail!(
            "refusing to overwrite an unowned executable at {}",
            executable.display()
        ),
        LocalCore::Package(owner) => anyhow::bail!(
            "refusing to overwrite a package-managed executable at {}",
            owner.executable().display()
        ),
        _ => {}
    }
    require_user_directory(prefix)?;
    let release = options.resolve().await?;
    let version = release.version.to_string();
    let candidate = release.stage().await?;
    require_user_directory(prefix)?;
    let created = Installation::create(SERVICE, prefix, candidate)
        .context("install gateway; use bro core update to change an installed release")?;
    crate::emit(&json!({
        "status": if created { "installed" } else { "unchanged" },
        "install_method": "cargo-dist", "current_version": version,
        "executable": display(&prefix.canonicalize()?.join(SERVICE.binary()))
    }))
}

async fn update(
    install: Installation,
    options: ReleaseOptions,
    check: bool,
    yes: bool,
) -> Result<()> {
    let current = install.version().to_owned();
    let release = options.resolve().await?;
    let version = release.version.to_string();
    let available =
        options.tag.is_some() || release.version.cmp_precedence(&current.parse()?).is_gt();
    let mut report = json!({
        "current_version": current, "install_method": "cargo-dist",
        "executable": display(install.executable()),
    });
    if check {
        report["status"] = json!("checked");
        report["target_version"] = json!(version);
        report["update_available"] = json!(available);
    } else if !available {
        verify_install_directory(&install)?;
        install.finish_interrupted()?;
        report["status"] = json!("unchanged");
    } else if !yes && !crate::update::confirm("BitRouter", &current, &version)? {
        report["status"] = json!("aborted");
    } else {
        verify_install_directory(&install)?;
        let candidate = release.stage().await?;
        verify_install_directory(&install)?;
        install.apply(candidate)?;
        report["status"] = json!("updated");
        report["target_version"] = json!(version);
    }
    crate::emit(&report)
}

fn verify_install_directory(install: &Installation) -> Result<()> {
    require_user_directory(
        install
            .executable()
            .parent()
            .context("missing executable directory")?,
    )
}

fn require_user_directory(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match ancestor.metadata() {
            Ok(meta) => {
                ensure!(
                    meta.is_dir(),
                    "expected a directory at {}",
                    ancestor.display()
                );
                #[cfg(unix)]
                let owned = {
                    use std::os::unix::fs::MetadataExt;
                    meta.uid() == rustix::process::geteuid().as_raw()
                };
                #[cfg(windows)]
                let owned = windows::owned_by_user(&ancestor.canonicalize()?)
                    .with_context(|| format!("read owner of {}", ancestor.display()))?;
                ensure!(
                    owned,
                    "install directory must belong to the current user: {}",
                    ancestor.display()
                );
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("no accessible ancestor for {}", path.display())
}

fn path_executable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    meta.is_file()
}

fn npm_core(bro: &Path) -> Option<(PathBuf, PathBuf)> {
    let package = bitrouter_update::paired_npm_root(Product::Bro, bro)?;
    let executable = package
        .join("core/node_modules/.bin_real")
        .join(SERVICE.binary());
    if !path_executable(&executable)
        || executable.canonicalize().ok()? != executable
        || same_file::is_same_file(bro, &executable).ok()?
    {
        return None;
    }
    Some((executable, package.to_owned()))
}

fn display(path: &Path) -> String {
    path.display().to_string()
}
