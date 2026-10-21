//! Scope-owned installation aliases and immutable version receipts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::bundle::{self, Bundle, Metadata};
use super::source::{self, Prepared, Source};

const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Project,
    Global,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    pub kind: Kind,
    pub root: PathBuf,
    pub library: PathBuf,
    pub principal: String,
}

fn future_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if absolute.exists() {
        return Ok(absolute.canonicalize()?);
    }
    let parent = absolute.parent().context("path has no parent")?;
    Ok(future_path(parent)?.join(absolute.file_name().context("path has no filename")?))
}

fn identity() -> Result<String> {
    #[cfg(unix)]
    {
        Ok(format!("uid:{}", rustix::process::geteuid().as_raw()))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "user:{}",
            std::env::var("USERNAME").context("local user identity unavailable")?
        ))
    }
}

fn global_roots() -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    if let Ok(home) = super::root::home_dir() {
        roots.extend([
            home.join(".agents"),
            home.join(".codex"),
            home.join(".claude"),
        ]);
    }
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|home| !home.is_empty()) {
        roots.push(std::path::absolute(PathBuf::from(home))?);
    }
    Ok(roots)
}

impl Scope {
    pub async fn resolve(global: bool, workspace: Option<&Path>) -> Result<Self> {
        ensure!(
            !global || workspace.is_none(),
            "--global conflicts with --workspace"
        );
        let cwd = std::env::current_dir()?.canonicalize()?;
        let (kind, root) = if global {
            (Kind::Global, future_path(&crate::paths::runtime_home()?)?)
        } else {
            (
                Kind::Project,
                match workspace {
                    Some(path) => path.canonicalize().context("workspace must exist")?,
                    None => source::project_root(&cwd).await?,
                },
            )
        };
        ensure!(global || root.is_dir(), "workspace must be a directory");
        Self::from_root(kind, root)
    }

    pub(super) fn native_global(root: &Path) -> Result<Self> {
        Self::from_root(Kind::Global, future_path(root)?)
    }

    fn from_root(kind: Kind, root: PathBuf) -> Result<Self> {
        let library = if kind == Kind::Global {
            root.join("skills")
        } else {
            root.join(".bitrouter/skills")
        };
        let scope = Self {
            kind,
            root,
            library,
            principal: identity()?,
        };
        // A managed version tree must never be placed in another harness's scan roots.
        for root in global_roots()? {
            let skills = future_path(&root.join("skills"))?;
            ensure!(
                !scope.library.starts_with(&skills),
                "BitRouter library overlaps an unmanaged harness skills root; select a separate BITROUTER_HOME/workspace"
            );
        }
        Ok(scope)
    }
    pub fn excluded(&self) -> PathBuf {
        if self.kind == Kind::Project {
            self.root.join(".bitrouter")
        } else {
            self.library.clone()
        }
    }
    fn compatibility_roots(&self) -> Result<Vec<PathBuf>> {
        if self.kind == Kind::Project {
            return Ok(vec![self.root.clone()]);
        }
        global_roots()
    }
    pub fn index(&self) -> PathBuf {
        self.library.join("installed.json")
    }
    pub(super) fn check_paths(&self) -> Result<()> {
        let relative = self.library.strip_prefix(&self.root)?;
        let mut path = self.root.clone();
        if path.exists() {
            ensure!(
                std::fs::symlink_metadata(&path)?.is_dir(),
                "scope root must remain a directory"
            );
            ensure!(
                path.canonicalize()? == path,
                "scope root changed through a symlink"
            );
        }
        for component in relative.components() {
            path.push(component);
            match std::fs::symlink_metadata(&path) {
                Ok(m) => ensure!(
                    m.is_dir() && !m.is_symlink(),
                    "managed library components must be real directories"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        for child in [self.index(), self.library.join("versions")] {
            match std::fs::symlink_metadata(&child) {
                Ok(m) => {
                    ensure!(!m.is_symlink(), "managed metadata must not be a symlink");
                    if child == self.index() {
                        ensure!(m.is_file(), "installed.json must be a regular file");
                    } else {
                        ensure!(m.is_dir(), "versions must be a directory");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Version {
    pub bundle: Metadata,
    pub source: Source,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Installation {
    pub id: String,
    pub name: String,
    pub description: String,
    pub current: Version,
    pub versions: Vec<Version>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    revision: u64,
    scope: Scope,
    aliases: BTreeMap<String, String>,
    installations: BTreeMap<String, Installation>,
}

impl Manifest {
    fn empty(scope: Scope) -> Self {
        Self {
            format: 1,
            revision: 0,
            scope,
            aliases: BTreeMap::new(),
            installations: BTreeMap::new(),
        }
    }
    fn validate(&self, scope: &Scope) -> Result<()> {
        ensure!(
            self.format == 1 && self.scope == *scope,
            "installation index belongs to a different scope/principal or format"
        );
        for (id, installation) in &self.installations {
            ensure!(
                id == &installation.id
                    && id.starts_with("skill_")
                    && uuid::Uuid::parse_str(&id[6..]).is_ok(),
                "invalid installation identity"
            );
            super::validate_skill_name(&installation.name)?;
            ensure!(
                !installation.versions.is_empty()
                    && installation.versions.contains(&installation.current),
                "invalid version receipt history"
            );
            for version in &installation.versions {
                version.bundle.validate()?;
                version.source.validate()?;
            }
        }
        for (name, id) in &self.aliases {
            let installed = self.installations.get(id).context("alias has no receipt")?;
            ensure!(
                name == &installed.name,
                "alias name differs from its receipt"
            );
        }
        Ok(())
    }
}

pub(super) struct Store {
    pub scope: Scope,
    raw: Option<String>,
    manifest: Manifest,
}

impl Store {
    pub(super) fn revision(&self) -> u64 {
        self.manifest.revision
    }

    pub(super) fn active_installations(&self) -> impl Iterator<Item = &Installation> {
        self.manifest
            .aliases
            .values()
            .filter_map(|id| self.manifest.installations.get(id))
    }

    pub(super) fn retained_version(&self, id: &str, version: &Version) -> Result<&Version> {
        self.manifest
            .installations
            .get(id)
            .and_then(|installation| {
                installation
                    .versions
                    .iter()
                    .find(|retained| *retained == version)
            })
            .context("original installation/version receipt is unavailable; refusing replacement")
    }

    pub fn load(scope: Scope) -> Result<Self> {
        scope.check_paths()?;
        let raw = if scope.index().exists() {
            Some(String::from_utf8(bundle::read_regular(
                &scope.library,
                &scope.index(),
                MAX_INDEX_BYTES as u64,
            )?)?)
        } else {
            None
        };
        let manifest = match &raw {
            Some(raw) => serde_json::from_str::<Manifest>(raw)
                .context("invalid installed.json; refusing to replace it")?,
            None => Manifest::empty(scope.clone()),
        };
        manifest.validate(&scope)?;
        Ok(Self {
            scope,
            raw,
            manifest,
        })
    }
    fn verify(
        &self,
        version: &Version,
    ) -> Result<bitrouter_orchestrator::harness::skills::format::SkillFrontmatter> {
        let path = self
            .scope
            .library
            .join("versions")
            .join(&version.bundle.digest);
        ensure!(
            bundle::inspect(&path, None)? == version.bundle,
            "installed bundle was modified; refusing to overwrite or claim intact retention"
        );
        let text = bitrouter_orchestrator::harness::skills::read_skill(&path.join("SKILL.md"))?;
        let fm = bitrouter_orchestrator::harness::skills::format::parse_frontmatter(&text)
            .map_err(|_| anyhow::anyhow!("installed SKILL.md is invalid"))?;
        fm.validate()?;
        Ok(fm)
    }
    fn active(&self, name: &str) -> Result<Installation> {
        let id = self
            .manifest
            .aliases
            .get(name)
            .or_else(|| {
                self.manifest
                    .installations
                    .get_key_value(name)
                    .filter(|(_, r)| self.manifest.aliases.get(&r.name) == Some(&r.id))
                    .map(|(id, _)| id)
            })
            .context(
                "no managed installation in this scope; unmanaged content is never modified",
            )?;
        self.manifest
            .installations
            .get(id)
            .cloned()
            .context("installation receipt missing")
    }
    fn verify_installation(&self, installed: &Installation) -> Result<()> {
        let current = self.verify(&installed.current)?;
        ensure!(
            current.name == installed.name && current.description == installed.description,
            "receipt identity differs from its immutable bundle"
        );
        let mut seen = BTreeSet::new();
        seen.insert(&installed.current.bundle.digest);
        for version in &installed.versions {
            if seen.insert(&version.bundle.digest) {
                let fm = self.verify(version)?;
                ensure!(
                    fm.name == installed.name,
                    "receipt name differs from its immutable bundle"
                );
            }
        }
        Ok(())
    }
    pub async fn install(
        self,
        source: &str,
        selected: Option<&str>,
        reference: Option<String>,
    ) -> Result<Report> {
        self.install_with_interaction(source, selected, reference, crate::prompt::terminal())
            .await
    }

    pub(crate) async fn install_with_interaction(
        mut self,
        source: &str,
        selected: Option<&str>,
        reference: Option<String>,
        interactive: bool,
    ) -> Result<Report> {
        let prepared = crate::prompt::interruptible(source::prepare(
            source,
            selected,
            reference,
            &self.scope.excluded(),
            interactive,
        ))
        .await?;
        ensure!(
            !self.manifest.aliases.contains_key(&prepared.bundle.name),
            "managed name already exists; use skills update"
        );
        let id = format!("skill_{}", uuid::Uuid::new_v4());
        let version = Version {
            bundle: prepared.bundle.metadata.clone(),
            source: prepared.source.clone(),
        };
        let installed = Installation {
            id: id.clone(),
            name: prepared.bundle.name.clone(),
            description: prepared.bundle.description.clone(),
            current: version.clone(),
            versions: vec![version],
        };
        self.manifest
            .aliases
            .insert(installed.name.clone(), id.clone());
        self.manifest.installations.insert(id.clone(), installed);
        self.commit(Some(&prepared.bundle))?;
        self.report(Some(&id), "install", "installed")
    }
    pub async fn update(mut self, name: &str, reference: Option<String>) -> Result<Report> {
        let mut installed = self.active(name)?;
        self.verify_installation(&installed)?;
        let prepared = crate::prompt::interruptible(source::refresh(
            &installed.current.source,
            reference,
            &self.scope.excluded(),
        ))
        .await?;
        let Some(Prepared { bundle, source }) = prepared else {
            return self.report(Some(&installed.id), "update", "unchanged_pinned");
        };
        ensure!(
            bundle.name == installed.name,
            "updated source changed its name; uninstall/install explicitly instead of renaming another alias"
        );
        let version = Version {
            bundle: bundle.metadata.clone(),
            source,
        };
        if version == installed.current {
            return self.report(Some(&installed.id), "update", "unchanged");
        }
        installed.description = bundle.description.clone();
        installed.current = version.clone();
        installed.versions.push(version);
        let id = installed.id.clone();
        self.manifest.installations.insert(id.clone(), installed);
        self.commit(Some(&bundle))?;
        self.report(Some(&id), "update", "installed")
    }
    pub fn uninstall(mut self, name: &str) -> Result<Report> {
        let installed = self.active(name)?;
        self.verify_installation(&installed)?;
        self.manifest.aliases.remove(&installed.name);
        self.commit(None)?;
        self.report(Some(&installed.id), "uninstall", "uninstalled_retained")
    }
    fn commit(&mut self, bundle: Option<&Bundle>) -> Result<()> {
        self.scope.check_paths()?;
        if !self.scope.library.exists() {
            crate::paths::ensure_home_directory(&self.scope.library)?;
        }
        let _lock = crate::policy_lock::acquire_publication_lock(&self.scope.index())?;
        self.scope.check_paths()?;
        let current = if self.scope.index().exists() {
            Some(std::fs::read_to_string(self.scope.index())?)
        } else {
            None
        };
        ensure!(
            current == self.raw,
            "installation index changed during preparation; retry explicitly"
        );
        if let Some(raw) = &self.raw {
            let previous: Manifest = serde_json::from_str(raw)?;
            for installed in previous.installations.values() {
                let changed = self.manifest.installations.get(&installed.id) != Some(installed)
                    || (previous.aliases.get(&installed.name) == Some(&installed.id)
                        && self.manifest.aliases.get(&installed.name) != Some(&installed.id));
                if changed {
                    self.verify_installation(installed)?;
                }
            }
        }
        if let Some(bundle) = bundle {
            let versions = self.scope.library.join("versions");
            if !versions.exists() {
                crate::paths::ensure_home_directory(&versions)?;
            }
            bundle::publish(bundle, &versions)?;
        }
        self.manifest.revision = self
            .manifest
            .revision
            .checked_add(1)
            .context("catalog revision exhausted")?;
        self.manifest.validate(&self.scope)?;
        let updated = serde_json::to_string_pretty(&self.manifest)?;
        ensure!(
            updated.len() <= MAX_INDEX_BYTES,
            "installation receipts exceed 16 MiB; previous aliases remain unchanged"
        );
        match &self.raw {Some(raw)=>crate::policy_lock::write_text_atomic_unlocked(&self.scope.index(),raw,&updated),None=>crate::policy_lock::write_new_file_atomic(&self.scope.index(),updated.as_bytes())}.context("publishing skill receipts/aliases; after a directory-sync failure inspect skills list before retrying because publication may have completed")?;
        self.raw = Some(updated);
        Ok(())
    }
    pub fn report(&self, selected: Option<&str>, action: &str, state: &str) -> Result<Report> {
        let mut rows = Vec::new();
        for installed in self.manifest.installations.values() {
            if selected
                .is_some_and(|selected| selected != installed.name && selected != installed.id)
            {
                continue;
            }
            let active = self.manifest.aliases.get(&installed.name) == Some(&installed.id);
            let problem = self
                .verify_installation(installed)
                .err()
                .map(|_| "installed bundle is missing, modified, or invalid".to_string());
            rows.push(Row {
                skill: crate::actions::skills::SkillRow {
                    name: installed.name.clone(),
                    description: installed.description.clone(),
                    dir: self
                        .scope
                        .library
                        .join("versions")
                        .join(&installed.current.bundle.digest)
                        .display()
                        .to_string(),
                    skill_md: self
                        .scope
                        .library
                        .join("versions")
                        .join(&installed.current.bundle.digest)
                        .join("SKILL.md")
                        .display()
                        .to_string(),
                    valid: problem.is_none(),
                    problem,
                },
                ownership: "managed",
                scope: self.scope.clone(),
                installation_id: Some(installed.id.clone()),
                installed: active,
                state: if active { "installed" } else { "retained" },
                version: Some(self.version_info(&installed.current)),
                versions: installed
                    .versions
                    .iter()
                    .map(|v| self.version_info(v))
                    .collect(),
            });
        }
        if action == "list" || action == "show" {
            let mut seen = BTreeSet::new();
            for root in self.scope.compatibility_roots()? {
                for found in
                    bitrouter_orchestrator::harness::skills::format::discover_all_skills(&root)
                {
                    if !seen.insert(found.skill_md.clone())
                        || selected.is_some_and(|name| name != found.name())
                    {
                        continue;
                    }
                    let problem = found.problem();
                    rows.push(Row {
                        skill: crate::actions::skills::SkillRow {
                            name: found.name(),
                            description: found.description().into(),
                            dir: found.dir.display().to_string(),
                            skill_md: found.skill_md.display().to_string(),
                            valid: problem.is_none(),
                            problem,
                        },
                        ownership: "unmanaged",
                        scope: self.scope.clone(),
                        installation_id: None,
                        installed: false,
                        state: "discovered",
                        version: None,
                        versions: Vec::new(),
                    });
                }
            }
        }
        rows.sort_by(|a, b| {
            (&a.skill.name, a.ownership, &a.installation_id, &a.skill.dir).cmp(&(
                &b.skill.name,
                b.ownership,
                &b.installation_id,
                &b.skill.dir,
            ))
        });
        ensure!(
            selected.is_none() || !rows.is_empty(),
            "skill is not present in the selected scope"
        );
        Ok(Report {
            scope: self.scope.clone(),
            catalog_revision: self.manifest.revision,
            action: action.into(),
            state: state.into(),
            retained_versions: true,
            activation: "not_requested",
            skills: rows,
        })
    }
    fn version_info(&self, version: &Version) -> VersionInfo {
        VersionInfo {
            digest: version.bundle.digest.clone(),
            directory: self
                .scope
                .library
                .join("versions")
                .join(&version.bundle.digest),
            source: version.source.clone(),
            files: version.bundle.files.len(),
            bytes: version.bundle.bytes,
        }
    }
}

#[derive(Serialize)]
struct VersionInfo {
    digest: String,
    directory: PathBuf,
    source: Source,
    files: usize,
    bytes: u64,
}
#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    skill: crate::actions::skills::SkillRow,
    ownership: &'static str,
    scope: Scope,
    installation_id: Option<String>,
    installed: bool,
    state: &'static str,
    version: Option<VersionInfo>,
    versions: Vec<VersionInfo>,
}
#[derive(Serialize)]
pub(super) struct Report {
    scope: Scope,
    catalog_revision: u64,
    action: String,
    state: String,
    retained_versions: bool,
    activation: &'static str,
    skills: Vec<Row>,
}
impl crate::output::CliReport for Report {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "Scope: {:?} {}",
            self.scope.kind,
            self.scope.root.display()
        ))?;
        h.line(&format!(
            "Library: {} (revision {})",
            self.scope.library.display(),
            self.catalog_revision
        ))?;
        h.line(&format!(
            "{}: {}; activation not requested",
            self.action, self.state
        ))?;
        for row in &self.skills {
            h.line(&serde_json::to_string(row).map_err(std::io::Error::other)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_initial_installer_cannot_replace_a_new_scope_manifest() -> Result<()> {
        let home = tempfile::tempdir()?;
        let root = home.path().canonicalize()?;
        let source = root.join("alpha");
        std::fs::create_dir(&source)?;
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: alpha\ndescription: first\n---\n",
        )?;
        let scope = Scope {
            kind: Kind::Project,
            library: root.join(".bitrouter/skills"),
            root,
            principal: identity()?,
        };
        let first = Store::load(scope.clone())?;
        let stale = Store::load(scope.clone())?;
        let source = source.to_str().context("source path")?;
        first.install(source, None, None).await?;
        let committed = std::fs::read(scope.index())?;
        let error = stale
            .install(source, None, None)
            .await
            .err()
            .context("stale write unexpectedly succeeded")?;
        ensure!(error.to_string().contains("index changed"));
        ensure!(std::fs::read(scope.index())? == committed);
        let reopened = Store::load(scope)?;
        ensure!(
            reopened.manifest.revision == 1
                && reopened.manifest.aliases.len() == 1
                && reopened.manifest.installations.len() == 1
        );
        Ok(())
    }
}
