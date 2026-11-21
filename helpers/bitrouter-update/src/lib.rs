//! Ownership and release checks used by both standalone product commands.
//! The only bytes installed are the staged executable that passed build-info.

pub mod channel;
pub mod install;

use std::path::{Path, PathBuf};

/// The two packages distributed by the release plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Service,
    Bro,
}

impl Product {
    /// Cargo-dist application and receipt namespace.
    pub fn package(self) -> &'static str {
        match self {
            Self::Service => "bitrouter",
            Self::Bro => "bitrouter-bro",
        }
    }

    /// Platform-independent command name.
    pub fn command(self) -> &'static str {
        match self {
            Self::Service => "bitrouter",
            Self::Bro => "bro",
        }
    }

    /// Native executable filename for the current platform.
    pub fn binary(self) -> String {
        format!("{}{}", self.command(), std::env::consts::EXE_SUFFIX)
    }
}

/// Identify package managers by the canonical executable location. Cargo-dist
/// also installs into Cargo home, so a matching receipt takes precedence there.
pub fn install_method(exe: &Path, cargo_home: Option<&Path>) -> &'static str {
    let parent = exe.parent();
    if parent
        .and_then(Path::file_name)
        .is_some_and(|n| n == ".bin_real")
        && parent
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|n| n == "node_modules")
    {
        "npm"
    } else if exe
        .ancestors()
        .any(|p| p.file_name().is_some_and(|n| n == "Cellar"))
    {
        "homebrew"
    } else if cargo_home.is_some_and(|home| {
        let bin = home.join("bin");
        parent == Some(bin.as_path()) || bin.canonicalize().ok().as_deref() == parent
    }) {
        "cargo"
    } else {
        "unknown"
    }
}

/// Resolve the bin directory convention used by cargo install and cargo-dist.
pub fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cargo")))
}

/// Find the combined npm package containing a canonical native executable.
///
/// Requires the packaged layout and the enclosing `bitrouter-bro` manifest.
pub fn paired_npm_root(product: Product, exe: &Path) -> Option<&Path> {
    let native_root = exe.parent()?.parent()?.parent()?;
    let package = match product {
        Product::Bro => native_root,
        Product::Service if native_root.file_name()? == "core" => native_root.parent()?,
        Product::Service => return None,
    };
    if exe
        != native_root
            .join("node_modules")
            .join(".bin_real")
            .join(product.binary())
    {
        return None;
    }
    let manifest = std::fs::read(package.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest).ok()?;
    (manifest["name"] == Product::Bro.package()).then_some(package)
}

/// Product-specific package-manager instructions for an unmanaged executable.
pub fn manual_command(product: Product, method: &str) -> String {
    let pkg = product.package();
    match method {
        "cargo" => format!("cargo install {pkg} --bin {} --force", product.command()),
        "npm" => format!("npm i -g {pkg}@latest"),
        "homebrew" => format!("brew upgrade {pkg}"),
        _ => format!("Use the {pkg} installer from a split release matching this platform."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_the_combined_npm_layout() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let package = root.path().canonicalize()?.join("bitrouter-bro");
        let bro_dir = package.join("node_modules/.bin_real");
        let core_dir = package.join("core/node_modules/.bin_real");
        std::fs::create_dir_all(&bro_dir)?;
        std::fs::create_dir_all(&core_dir)?;
        std::fs::write(package.join("package.json"), r#"{"name":"bitrouter-bro"}"#)?;
        assert_eq!(
            paired_npm_root(Product::Bro, &bro_dir.join(Product::Bro.binary())),
            Some(package.as_path())
        );
        let core = core_dir.join(Product::Service.binary());
        assert_eq!(
            paired_npm_root(Product::Service, &core),
            Some(package.as_path())
        );
        assert!(
            paired_npm_root(Product::Service, &bro_dir.join(Product::Service.binary())).is_none()
        );
        std::fs::write(package.join("package.json"), r#"{"name":"bitrouter"}"#)?;
        assert!(paired_npm_root(Product::Service, &core).is_none());
        Ok(())
    }
}
