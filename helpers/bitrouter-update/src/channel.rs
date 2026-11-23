//! Resolve a release once, then use only assets belonging to that release.

use std::{env, fs::File, io::Write, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use axotag::{Package, ReleaseType, Version};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;

use crate::Product;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    prerelease: bool,
    draft: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Result<Url> {
        self.asset_if_present(name)?
            .with_context(|| format!("release {} has no {name}", self.tag_name))
    }

    fn asset_if_present(&self, name: &str) -> Result<Option<Url>> {
        let mut matches = self.assets.iter().filter(|a| a.name == name);
        let asset = matches.next();
        ensure!(matches.next().is_none(), "ambiguous release asset {name}");
        asset
            .map(|asset| {
                Url::parse(&asset.browser_download_url).context("invalid release asset URL")
            })
            .transpose()
    }
}

#[derive(Deserialize, PartialEq, Eq)]
enum ManifestSchema {
    #[serde(rename = "bitrouter.distribution.v1")]
    V1,
}

#[derive(Deserialize)]
struct Plan {
    schema: Option<ManifestSchema>,
    tag: String,
    source_revision: String,
    products: Vec<PackagePlan>,
}

#[derive(Deserialize)]
struct PackagePlan {
    package: String,
    version: String,
    executable: String,
    distributions: Vec<Distribution>,
}

#[derive(Deserialize)]
struct Distribution {
    target: String,
    profile: Option<String>,
    binary: String,
    archive: String,
    checksum: Option<String>,
    sha256: Option<String>,
}

enum ArchiveDigest {
    Inline(String),
    Sidecar(Url),
}

fn validate_sha256(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "invalid archive SHA-256"
    );
    Ok(())
}

/// The resolved native archive and checksum from one product release.
pub struct Target {
    client: Client,
    pub version: Version,
    product: Product,
    revision: String,
    distribution: Distribution,
    archive: Url,
    digest: ArchiveDigest,
}

fn api_url(product: Product) -> Result<Url> {
    let prefix = product.package().replace('-', "_").to_ascii_uppercase();
    let ghe = env::var(format!("{prefix}_INSTALLER_GHE_BASE_URL")).ok();
    let github = env::var(format!("{prefix}_INSTALLER_GITHUB_BASE_URL")).ok();
    match (ghe, github) {
        (Some(_), Some(_)) => bail!("set only one GitHub release host for {prefix}"),
        (Some(url), None) => {
            Ok(Url::parse(&url)?.join("/api/v3/repos/bitrouter/bitrouter/releases")?)
        }
        (None, Some(url)) => {
            let mut url = Url::parse(&url)?;
            let domain = url.domain().context("GitHub base URL requires a domain")?;
            url.set_host(Some(&format!("api.{domain}")))?;
            Ok(url.join("/repos/bitrouter/bitrouter/releases")?)
        }
        _ => Ok(Url::parse(
            "https://api.github.com/repos/bitrouter/bitrouter/releases",
        )?),
    }
}

fn tag_version(product: Product, tag: &str) -> Option<Version> {
    let packages = [Product::Service, Product::Bro].map(|p| Package {
        name: p.package().into(),
        version: None,
    });
    match axotag::parse_tag(&packages, tag).ok()?.release {
        ReleaseType::Version(v) => Some(v),
        ReleaseType::Package { idx, version } if packages.get(idx)?.name == product.package() => {
            Some(version)
        }
        _ => None,
    }
}

async fn select(
    client: &Client,
    product: Product,
    tag: Option<&str>,
    stable: bool,
) -> Result<(Version, Release)> {
    let mut api = api_url(product)?;
    if let Some(tag) = tag {
        let version = tag_version(product, tag).context("tag is not a release for this product")?;
        api.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid GitHub URL"))?
            .extend(["tags", tag]);
        let release: Release = client
            .get(api)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            !release.draft && release.tag_name == tag,
            "release tag mismatch or draft release"
        );
        return Ok((version, release));
    }
    let installer = format!(
        "{}-installer.{}",
        product.package(),
        if cfg!(windows) { "ps1" } else { "sh" }
    );
    let eligible = |release: &Release| {
        let version = tag_version(product, &release.tag_name)?;
        (!release.draft
            && (!stable || (!release.prerelease && version.pre.is_empty()))
            && release.assets.iter().any(|a| a.name == installer))
        .then_some(version)
    };
    // Respect the publisher's designated stable release when it belongs to
    // this product. A workspace may designate a release of the other product.
    if stable {
        let response = client.get(format!("{api}/latest")).send().await?;
        if response.status() != reqwest::StatusCode::NOT_FOUND {
            let release: Release = response.error_for_status()?.json().await?;
            if let Some(version) = eligible(&release) {
                return Ok((version, release));
            }
        }
    }
    let mut latest = None;
    for page in 1u32.. {
        let releases: Vec<Release> = client
            .get(api.clone())
            .query(&[("per_page", 100), ("page", page)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let last = releases.len() < 100;
        for release in releases {
            let Some(version) = eligible(&release) else {
                continue;
            };
            if latest
                .as_ref()
                .is_none_or(|(v, _)| version.cmp_precedence(v).is_gt())
            {
                latest = Some((version, release));
            }
        }
        if last {
            break;
        }
    }
    latest.context("no published release found for this product and channel")
}

/// Resolve the product, tag, source revision and native distribution before staging.
///
/// # Errors
/// Refuses missing or inconsistent inventory, including pre-split releases.
pub async fn resolve(
    product: Product,
    target: &str,
    tag: Option<&str>,
    stable: bool,
) -> Result<Target> {
    let client = Client::builder()
        .user_agent(product.package())
        .timeout(Duration::from_secs(180))
        .build()?;
    let (version, release) = select(&client, product, tag, stable).await?;
    let (inventory, schema) = match release.asset_if_present("distribution-manifest.json")? {
        Some(url) => (url, Some(ManifestSchema::V1)),
        None => (release.asset("distribution-plan.json")?, None),
    };
    let plan: Plan = client
        .get(inventory)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(plan.schema == schema, "release manifest schema mismatch");
    ensure!(
        plan.tag == release.tag_name && !plan.source_revision.trim().is_empty(),
        "release plan tag or revision mismatch"
    );
    let mut entries = plan
        .products
        .into_iter()
        .filter(|p| p.package == product.package());
    let pkg = entries
        .next()
        .context("release plan is missing the requested product")?;
    ensure!(
        entries.next().is_none()
            && pkg.version == version.to_string()
            && pkg.executable == product.command(),
        "release plan product identity mismatch"
    );
    let profile = (product == Product::Service).then_some("full");
    let mut variants = pkg
        .distributions
        .into_iter()
        .filter(|d| d.target == target && d.profile.as_deref() == profile);
    let distribution = variants
        .next()
        .context("release has no matching native distribution")?;
    let suffix = if cfg!(windows) { "zip" } else { "tar.xz" };
    let name = format!("{}-{target}.{suffix}", product.package());
    ensure!(
        variants.next().is_none()
            && distribution.binary == product.binary()
            && distribution.archive == name,
        "release distribution identity mismatch"
    );
    let digest = match (&schema, &distribution.sha256, &distribution.checksum) {
        (Some(ManifestSchema::V1), Some(digest), None) => {
            validate_sha256(digest)?;
            ArchiveDigest::Inline(digest.clone())
        }
        (None, None, Some(name)) if *name == format!("{}.sha256", distribution.archive) => {
            ArchiveDigest::Sidecar(release.asset(name)?)
        }
        _ => bail!("release distribution has an invalid checksum declaration"),
    };
    Ok(Target {
        client,
        version,
        product,
        revision: plan.source_revision,
        archive: release.asset(&name)?,
        digest,
        distribution,
    })
}

/// An isolated, verified executable. The temporary directory owns its lifetime.
pub struct Candidate {
    dir: tempfile::TempDir,
    binary: String,
    pub identity: Value,
    pub(crate) product: Product,
    pub(crate) version: String,
}

impl Candidate {
    pub(crate) fn path(&self) -> std::path::PathBuf {
        self.dir.path().join(&self.binary)
    }
}

impl Target {
    /// Download and hash the archive, then probe only its declared native member.
    ///
    /// # Errors
    /// Refuses a corrupt archive or an executable with a different built-in identity.
    pub async fn stage(self) -> Result<Candidate> {
        let dir = tempfile::tempdir()?;
        let digest = match self.digest {
            ArchiveDigest::Inline(digest) => digest,
            ArchiveDigest::Sidecar(url) => {
                let sidecar = self
                    .client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                let mut parts = sidecar.split_whitespace();
                let digest = parts.next().context("empty checksum")?;
                ensure!(
                    parts.next().map(|s| s.trim_start_matches('*'))
                        == Some(self.distribution.archive.as_str())
                        && parts.next().is_none(),
                    "checksum does not name the selected archive"
                );
                validate_sha256(digest)?;
                digest.to_owned()
            }
        };
        let archive = dir.path().join(&self.distribution.archive);
        let mut response = self
            .client
            .get(self.archive)
            .send()
            .await?
            .error_for_status()?;
        let mut file = File::create(&archive)?;
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk)?;
        }
        drop(file);
        ensure!(
            crate::install::sha256(&archive)? == digest,
            "archive checksum mismatch"
        );
        let binary = self.distribution.binary;
        let member = match self.distribution.archive.strip_suffix(".tar.xz") {
            Some(root) => format!("{root}/{binary}"),
            None => binary.clone(),
        };
        let executable = dir.path().join(&binary);
        // Stream one member to a file we create. Archive paths, links and peer
        // executables can never be materialized in the installation prefix.
        let status = std::process::Command::new("tar")
            .arg("-xOf")
            .arg(&archive)
            .arg(member)
            .stdout(File::create_new(&executable)?)
            .status()
            .context("extract native archive member with tar")?;
        ensure!(status.success(), "archive lacks its declared executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        }
        let info: Value =
            serde_json::from_slice(&probe(&executable, &["--json", "build-info"]).await?)?;
        let version = self.version.to_string();
        ensure!(
            info["product"] == self.product.package()
                && info["version"] == version
                && info["target"] == self.distribution.target
                && info["source_revision"] == self.revision,
            "staged build-info product, version, platform or source revision mismatch"
        );
        if self.product == Product::Service {
            ensure!(info["profile"] == "full", "staged service profile mismatch");
        }
        let banner = String::from_utf8(probe(&executable, &["--version"]).await?)?;
        ensure!(
            banner.trim() == format!("{} {version}", self.product.command()),
            "staged executable version banner mismatch"
        );
        Ok(Candidate {
            dir,
            binary,
            identity: info,
            product: self.product,
            version,
        })
    }
}

async fn probe(candidate: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(candidate)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("staged executable probe timed out")??;
    ensure!(output.status.success(), "staged executable probe failed");
    Ok(output.stdout)
}
