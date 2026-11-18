//! Build-time artifact metadata. Shared with focused integration tests only.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use sha2::{Digest, Sha256};

pub struct FeatureSelection {
    pub features: Vec<String>,
    /// Rust variant name used when generating the typed contract expression.
    pub profile: &'static str,
    /// Stable wire/archive spelling, which can differ from the Rust variant.
    pub wire_profile: &'static str,
}

impl FeatureSelection {
    /// Hash only the takeover protocols compiled into this executable.
    pub fn handoff_sources(&self) -> impl Iterator<Item = &'static str> + '_ {
        const SOURCES: &[(&str, Option<&str>)] = &[
            ("src/daemon.rs", None),
            ("src/daemon_handoff.rs", None),
            ("src/client/sessions/mod.rs", Some("harness")),
            ("src/client/sessions/wire.rs", Some("harness")),
            ("src/harness/supervisor.rs", Some("harness")),
            ("src/core/evolution/scheduler.rs", Some("core")),
            ("src/main.rs", None),
            ("src/upgrade.rs", None),
            ("src/upgrade_preflight.rs", None),
        ];
        SOURCES.iter().filter_map(|&(path, feature)| {
            feature
                .is_none_or(|required| self.features.iter().any(|name| name == required))
                .then_some(path)
        })
    }

    pub fn from_manifest(
        manifest: &str,
        enabled: impl Fn(&str) -> bool,
    ) -> Result<Self, Box<dyn Error>> {
        let manifest: toml::Value = toml::from_str(manifest)?;
        let table = manifest
            .get("features")
            .and_then(toml::Value::as_table)
            .ok_or("application manifest has no feature table")?;
        let mut graph = BTreeMap::<String, Vec<String>>::new();
        for (name, value) in table {
            let edges = value
                .as_array()
                .ok_or("manifest feature must be an array")?
                .iter()
                .map(|edge| {
                    edge.as_str()
                        .map(str::to_owned)
                        .ok_or("manifest feature edge must be a string")
                })
                .collect::<Result<Vec<_>, _>>()?;
            graph.insert(name.clone(), edges);
        }

        // Cargo's optional dependencies create implicit features unless a
        // `dep:` reference suppresses them. Published normalized manifests may
        // make those features explicit; both forms must yield the same identity.
        // https://doc.rust-lang.org/cargo/reference/features.html#optional-dependencies
        let suppressed = graph
            .values()
            .flatten()
            .filter_map(|edge| edge.strip_prefix("dep:").map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let mut optional_dependencies = BTreeSet::new();
        let mut dependency_tables = vec![manifest.get("dependencies")];
        if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
            dependency_tables.extend(targets.values().map(|target| target.get("dependencies")));
        }
        for dependencies in dependency_tables.into_iter().flatten() {
            if let Some(dependencies) = dependencies.as_table() {
                for (name, dependency) in dependencies {
                    if dependency.get("optional").and_then(toml::Value::as_bool) == Some(true) {
                        optional_dependencies.insert(name.clone());
                        if !suppressed.contains(name) {
                            graph.entry(name.clone()).or_default();
                        }
                    }
                }
            }
        }

        // Cargo supplies effective features with '-' changed to '_' and names
        // uppercased. Map back through the manifest to retain Cargo spellings.
        // https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts
        let mut environment_names = BTreeSet::new();
        let mut selected = Vec::new();
        for name in graph.keys() {
            let variable = format!("CARGO_FEATURE_{}", name.replace('-', "_").to_uppercase());
            if !environment_names.insert(variable.clone()) {
                return Err("manifest features collide in Cargo's build environment".into());
            }
            if enabled(&variable) {
                selected.push(name.as_str());
            }
        }
        let features = normalize(&graph, &optional_dependencies, selected)?;
        let defaults = normalize(
            &graph,
            &optional_dependencies,
            graph
                .get_key_value("default")
                .map(|(name, _)| name.as_str()),
        )?;
        let (profile, wire_profile) = if features.is_empty() {
            ("Gateway", "gateway")
        } else if features == defaults {
            ("Full", "full")
        } else if features == ["db-postgres", "multi-instance"] {
            // The named distribution is intentionally exact. Extra capability
            // selections remain custom, even when they include shared mode.
            ("GatewayShared", "gateway-shared")
        } else {
            ("Custom", "custom")
        };
        Ok(Self {
            features,
            profile,
            wire_profile,
        })
    }
}

fn normalize<'a>(
    graph: &'a BTreeMap<String, Vec<String>>,
    optional_dependencies: &BTreeSet<String>,
    roots: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut pending = roots.into_iter().collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name) {
            continue;
        }
        let edges = graph.get(name).ok_or("unknown local manifest feature")?;
        for edge in edges {
            // Dependency features are not application capability names. A
            // strong dep/feature edge also enables that dep's implicit feature.
            // https://doc.rust-lang.org/cargo/reference/features.html#dependency-features
            // https://doc.rust-lang.org/stable/nightly-rustc/src/cargo/resolver/features.rs.html
            if edge.starts_with("dep:") {
                continue;
            }
            if let Some((dependency, _)) = edge.split_once('/') {
                if optional_dependencies.contains(dependency) && graph.contains_key(dependency) {
                    pending.push(dependency);
                }
            } else {
                pending.push(edge);
            }
        }
    }
    Ok(visited
        .into_iter()
        .filter(|name| !matches!(*name, "default" | "otel"))
        .map(str::to_owned)
        .collect())
}

pub struct BuildMetadata {
    pub version: String,
    pub source_revision: Option<String>,
    pub target: String,
    pub selection: FeatureSelection,
}

impl BuildMetadata {
    /// Package the same identity when a cross-compiled executable cannot run on
    /// the build host. The application contract test checks this representation
    /// against the compiled typed expression, including optional field presence.
    pub fn artifact_json(&self, handoff_build_id: &str) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "source_revision": self.source_revision,
            "target": self.target,
            "features": self.selection.features,
            "profile": self.selection.wire_profile,
            "handoff_build_id": handoff_build_id,
        })
    }

    /// Extend the existing migration/handoff source hash, never replace it.
    pub fn extend_handoff_hash(&self, hash: &mut Sha256) -> Result<(), Box<dyn Error>> {
        // JSON supplies unambiguous field/array boundaries and distinguishes
        // an unknown revision from every known revision. Paths never enter it.
        hash.update(serde_json::to_vec(&(
            "bitrouter-artifact-v1",
            &self.version,
            &self.target,
            &self.selection.features,
            self.selection.profile,
            &self.source_revision,
        ))?);
        Ok(())
    }

    pub fn render(&self, handoff_build_id: &str) -> String {
        let revision = match &self.source_revision {
            Some(revision) => format!("Some({revision:?}.to_owned())"),
            None => "None".to_owned(),
        };
        let features = self
            .selection
            .features
            .iter()
            .map(|feature| format!("{feature:?}.to_owned()"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "// @generated by build.rs; no runtime metadata discovery.\n\
             ArtifactIdentity {{\n\
                 version: {version:?}.to_owned(),\n\
                 source_revision: {revision},\n\
                 target: {target:?}.to_owned(),\n\
                 features: vec![{features}],\n\
                 profile: DistributionProfile::{profile},\n\
                 handoff_build_id: {handoff_build_id:?}.to_owned(),\n\
             }}\n",
            version = self.version,
            target = self.target,
            profile = self.selection.profile,
        )
    }
}
