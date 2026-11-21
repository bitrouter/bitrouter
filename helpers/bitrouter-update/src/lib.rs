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

    fn binary(self) -> String {
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
