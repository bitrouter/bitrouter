//! Product location policy for ordinary provider credentials. AI supplies the
//! explicit-path file implementation; this module selects the existing location.

use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::Result;
use bitrouter_ai::auth::file::snapshot::CredentialStore;

const FILENAME: &str = "oauth-tokens.json";

pub(crate) fn load_default() -> Result<CredentialStore> {
    Ok(CredentialStore::load(default_path()?)?)
}

fn default_path() -> Result<PathBuf> {
    let xdg = std::env::var_os("XDG_DATA_HOME");
    let local = std::env::var_os("LOCALAPPDATA");
    let home = std::env::var_os("HOME");
    path_with(
        xdg.as_deref(),
        local.as_deref(),
        home.as_deref(),
        cfg!(windows),
    )
}

fn path_with(
    xdg: Option<&OsStr>,
    local: Option<&OsStr>,
    home: Option<&OsStr>,
    windows: bool,
) -> Result<PathBuf> {
    let directory = if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        PathBuf::from(xdg).join("bitrouter")
    } else if windows && let Some(local) = local.filter(|value| !value.is_empty()) {
        PathBuf::from(local).join("bitrouter").join("data")
    } else if let Some(home) = home.filter(|value| !value.is_empty()) {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("bitrouter")
    } else {
        anyhow::bail!("could not resolve a data directory for the credential store");
    };
    Ok(directory.join(FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_precedence_and_filename_match_existing_product_policy() -> Result<()> {
        let xdg = OsStr::new("xdg");
        let local = OsStr::new("local");
        let home = OsStr::new("home");
        for windows in [false, true] {
            assert_eq!(
                path_with(Some(xdg), Some(local), Some(home), windows)?,
                PathBuf::from("xdg/bitrouter/oauth-tokens.json")
            );
        }
        assert_eq!(
            path_with(None, Some(local), Some(home), true)?,
            PathBuf::from("local/bitrouter/data/oauth-tokens.json")
        );
        assert_eq!(
            path_with(None, Some(local), Some(home), false)?,
            PathBuf::from("home/.local/share/bitrouter/oauth-tokens.json")
        );
        assert_eq!(
            path_with(Some(OsStr::new("")), Some(OsStr::new("")), Some(home), true)?,
            PathBuf::from("home/.local/share/bitrouter/oauth-tokens.json")
        );
        assert!(path_with(None, None, None, false).is_err());
        Ok(())
    }
}
