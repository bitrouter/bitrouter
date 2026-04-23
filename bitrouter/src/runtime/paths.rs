use std::path::{Path, PathBuf};

/// All resolved paths for a bitrouter instance.
///
/// The canonical derivation is from a **home directory**:
///   `<home>/bitrouter.yaml`, `<home>/.env`, `<home>/run/`, `<home>/logs/`
///
/// Individual paths can be overridden via CLI flags.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub home_dir: PathBuf,
    pub config_file: PathBuf,
    pub env_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_dir: PathBuf,
    /// OAuth token store file (`<home>/tokens.json`).
    pub token_store_file: PathBuf,
    /// On-disk cache root (`<home>/cache/`).  Used for the ACP registry
    /// snapshot and other short-lived artifacts safe to discard.
    pub cache_dir: PathBuf,
    /// Installed ACP agent root (`<home>/agents/`).  Per-agent install dirs
    /// are flat subdirectories keyed by agent id.
    pub agents_dir: PathBuf,
    /// ACP agent install-state ledger (`<home>/agents/state.json`).
    pub agent_state_file: PathBuf,
}

impl RuntimePaths {
    /// Derive all paths from a resolved home directory.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let agents_dir = home.join("agents");
        Self {
            config_file: home.join("bitrouter.yaml"),
            env_file: home.join(".env"),
            runtime_dir: home.join("run"),
            log_dir: home.join("logs"),
            token_store_file: home.join("tokens.json"),
            cache_dir: home.join("cache"),
            agent_state_file: agents_dir.join("state.json"),
            agents_dir,
            home_dir: home,
        }
    }

    /// Return the flat install directory for the given agent id.
    pub fn agent_install_dir(&self, agent_id: &str) -> PathBuf {
        self.agents_dir.join(agent_id)
    }
}

/// Overrides that can be applied to individual paths after resolution.
#[derive(Debug, Clone, Default)]
pub struct PathOverrides {
    pub config_file: Option<PathBuf>,
    pub env_file: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
}

impl PathOverrides {
    pub fn apply(self, mut paths: RuntimePaths) -> RuntimePaths {
        if let Some(v) = self.config_file {
            paths.config_file = v;
        }
        if let Some(v) = self.env_file {
            paths.env_file = v;
        }
        if let Some(v) = self.runtime_dir {
            paths.runtime_dir = v;
        }
        if let Some(v) = self.log_dir {
            paths.log_dir = v;
        }
        paths
    }
}

/// Resolve the bitrouter home directory.
///
/// Priority:
/// 1. Explicit `--home-dir` override
/// 2. CWD, if `./bitrouter.yaml` exists
/// 3. `$BITROUTER_HOME` environment variable
/// 4. `~/.bitrouter` (scaffolded if missing)
pub fn resolve_home(explicit: Option<&Path>) -> RuntimePaths {
    if let Some(dir) = explicit {
        let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        return RuntimePaths::from_home(abs);
    }

    // CWD check
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("bitrouter.yaml").exists() {
        return RuntimePaths::from_home(cwd);
    }

    // $BITROUTER_HOME
    if let Ok(home_env) = std::env::var("BITROUTER_HOME") {
        let p = PathBuf::from(home_env);
        if p.is_dir() {
            return RuntimePaths::from_home(p);
        }
    }

    // ~/.bitrouter (scaffold if needed)
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let default_home = home.join(".bitrouter");
    if !default_home.exists() {
        let _ = scaffold_home(&default_home);
    }
    RuntimePaths::from_home(default_home)
}

/// Create the default home directory with placeholder files.
fn scaffold_home(home: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    std::fs::create_dir_all(home.join("run"))?;
    std::fs::create_dir_all(home.join("logs"))?;
    std::fs::create_dir_all(home.join("cache"))?;
    std::fs::create_dir_all(home.join("agents"))?;

    let config_path = home.join("bitrouter.yaml");
    if !config_path.exists() {
        std::fs::write(&config_path, include_str!("../../templates/minimal.yaml"))?;
    }

    let env_path = home.join(".env");
    if !env_path.exists() {
        std::fs::write(
            &env_path,
            "\
# BitRouter environment variables
# Place your API keys here. This file is ignored by git.
#
# OPENAI_API_KEY=sk-...
# ANTHROPIC_API_KEY=sk-ant-...
# GOOGLE_API_KEY=...
",
        )?;
    }

    let gitignore_path = home.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(
            &gitignore_path,
            "\
# Secrets and credentials
.env
.keys/
tokens.json

# Runtime state
bitrouter.db
logs/
run/
",
        )?;
    }

    let readme_path = home.join("README.md");
    if !readme_path.exists() {
        std::fs::write(&readme_path, include_str!("../../templates/home_readme.md"))?;
    }

    Ok(())
}
