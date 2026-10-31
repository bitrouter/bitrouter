//! Instance-scoped lifecycle for clients with independent custom providers.
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{Options, Prepared, Protocol, Target, files};
use crate::client::local_gateway;
use crate::output::{CliReport, Output, human::Human};

#[derive(Serialize)]
struct Report {
    target: Target,
    instance: String,
    state: &'static str,
    endpoint: String,
    model: String,
    display_name: String,
    protocol: Protocol,
    files: Vec<PathBuf>,
    surfaces: Vec<String>,
    notes: Vec<String>,
    configuration_verified: bool,
    gateway_catalog_verified: bool,
    inference_verified: bool,
}

impl CliReport for Report {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "{}: {} — {}",
            self.target.name(),
            self.display_name,
            self.state
        ))?;
        h.line(&format!("  Instance: {}", self.instance))?;
        for path in &self.files {
            h.line(&format!("  {}", path.display()))?;
        }
        for note in &self.notes {
            h.note(note)?;
        }
        Ok(())
    }
}

fn report(target: Target, receipt: &files::Receipt, state: &'static str) -> Result<Report> {
    let native = receipt
        .native
        .as_ref()
        .context("missing native connection metadata")?;
    Ok(Report {
        target,
        instance: native.instance.clone(),
        state,
        endpoint: receipt.endpoint.clone(),
        model: receipt.model.clone(),
        display_name: receipt.display_name().into(),
        protocol: native.protocol,
        files: receipt.files.iter().map(|f| f.path.clone()).collect(),
        surfaces: native.surfaces.clone(),
        notes: native.notes.clone(),
        configuration_verified: false,
        gateway_catalog_verified: false,
        inference_verified: false,
    })
}

fn normalized(path: PathBuf) -> Result<PathBuf> {
    let path = super::absolute(path)?;
    files::check_path(&path)?;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Do not collapse a symlink/.. pair into a different instance.
                ensure!(
                    !std::fs::symlink_metadata(&result).is_ok_and(|m| m.file_type().is_symlink()),
                    "symlinked configuration instance is unsupported"
                );
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

fn instance(target: Target, explicit: Option<&Path>) -> Result<PathBuf> {
    normalized(match target {
        Target::OpenCode => super::opencode::instance(explicit)?,
        Target::Pi => super::pi::instance(explicit)?,
        Target::Deepseek => super::deepseek::instance(explicit)?,
        _ => anyhow::bail!("unsupported native provider client"),
    })
}

fn prepare(
    target: Target,
    endpoint: &str,
    token: &str,
    model_and_label: (&str, &str),
    instance: Option<&Path>,
    profiles: &[String],
    protocol: Protocol,
) -> Result<files::Receipt> {
    let (model, label) = model_and_label;
    let Prepared {
        files,
        instance,
        surfaces,
        notes,
    } = match target {
        Target::OpenCode => {
            super::opencode::prepare(endpoint, token, model, label, instance, protocol)?
        }
        Target::Pi => super::pi::prepare(endpoint, token, model, label, instance, protocol)?,
        Target::Deepseek => {
            super::deepseek::prepare(endpoint, token, model, label, instance, profiles, protocol)?
        }
        _ => anyhow::bail!("unsupported native provider client"),
    };
    let mut receipt = files::Receipt::new(endpoint.into(), model.into(), files)
        .with_display_name(Some(label.into()));
    receipt.native = Some(files::Native {
        instance,
        protocol,
        profiles: profiles.into(),
        surfaces,
        notes,
    });
    Ok(receipt)
}

async fn credential(interactive: bool) -> Result<String> {
    if let Some(token) = local_gateway::nonempty_env(local_gateway::BITROUTER_API_KEY_ENV) {
        return Ok(token);
    }
    if interactive && crate::prompt::terminal() {
        let token = crate::prompt::secret("Gateway API key").await?;
        ensure!(
            !token.trim().is_empty(),
            "gateway credential cannot be empty"
        );
        return Ok(token);
    }
    anyhow::bail!("set BITROUTER_API_KEY to this gateway's credential")
}

pub(super) async fn run(target: Target, mut options: Options, output: &Output) -> Result<()> {
    options.profiles.sort();
    options.profiles.dedup();
    ensure!(
        options.codex_catalog.is_none(),
        "--codex-catalog requires codex"
    );
    ensure!(
        matches!(target, Target::Deepseek) || options.profiles.is_empty(),
        "--profile requires deepseek-harness"
    );
    let instance = instance(target, options.instance.as_deref())?;
    let instance_text = instance
        .to_str()
        .context("configuration instance must be UTF-8")?;
    let digest = hex::encode(Sha256::digest(instance_text.as_bytes()));
    let state = super::absolute(crate::paths::runtime_home()?)?.join("connections");
    let receipt_path = state.join(format!("{}-{digest}.json", target.name()));
    let read_only = options.status || options.check || options.dry_run;
    let _lock = if read_only {
        None
    } else {
        Some(files::lock(&receipt_path.with_extension("lock"))?)
    };
    if receipt_path.with_extension("update.json").exists() {
        ensure!(
            !read_only,
            "interrupted update; rerun connect for this instance to recover it"
        );
        files::Receipt::recover_update(&receipt_path)?;
    }
    let previous = files::Receipt::load(&receipt_path)?;
    if options.status && previous.is_none() {
        let mut receipt = files::Receipt::new(String::new(), String::new(), vec![]);
        receipt.native = Some(files::Native {
            instance: instance_text.into(),
            protocol: Protocol::default(),
            profiles: vec![],
            surfaces: vec![],
            notes: vec![],
        });
        return output
            .emit(&report(target, &receipt, "not_configured")?)
            .map_err(Into::into);
    }
    if let Some(receipt) = &previous {
        let metadata = receipt
            .native
            .as_ref()
            .context("missing native connection metadata")?;
        if options.remove {
            ensure!(
                options.profiles.is_empty() || options.profiles == metadata.profiles,
                "removal applies to the complete saved profile scope; omit --profile or specify every saved profile"
            );
            let mut result = report(target, receipt, "restored")?;
            preview(&result);
            if super::approved(
                &options,
                "Remove this BitRouter provider and restore owned settings?",
            )
            .await?
            {
                receipt.restore(&receipt_path)?;
            } else {
                result.state = "cancelled";
            }
            return output.emit(&result).map_err(Into::into);
        }
        if !options.update {
            ensure!(
                options.model.as_ref().is_none_or(|v| v == &receipt.model)
                    && options
                        .display_name
                        .as_deref()
                        .is_none_or(|v| v == receipt.display_name())
                    && options
                        .base_url
                        .as_ref()
                        .is_none_or(|v| super::endpoint(v).ok().as_ref() == Some(&receipt.endpoint))
                    && options.protocol.is_none_or(|v| v == metadata.protocol)
                    && (options.profiles.is_empty() || options.profiles == metadata.profiles),
                "connection differs; use --update to replace it"
            );
            let installed = receipt.installed()?;
            let mut result = report(
                target,
                receipt,
                if installed {
                    "already_configured"
                } else {
                    "configuration_changed"
                },
            )?;
            result.configuration_verified = installed;
            if !options.status {
                ensure!(
                    installed,
                    "connection files changed; use --remove to recover before reconnecting"
                );
            }
            if options.check {
                super::verify_gateway(&receipt.endpoint, &receipt.stored_token()?, &receipt.model)
                    .await?;
                result.gateway_catalog_verified = true;
                result.state = "catalog_verified";
            }
            return output.emit(&result).map_err(Into::into);
        }
        ensure!(
            receipt.installed()?,
            "connection files changed; resolve changes before --update"
        );
        if options.model.is_none() {
            options.model = Some(receipt.model.clone());
        }
        if options.display_name.is_none() {
            options.display_name = Some(receipt.display_name().into());
        }
        if options.base_url.is_none() && options.config.is_none() {
            options.base_url = Some(receipt.endpoint.clone());
        }
        if options.protocol.is_none() {
            options.protocol = Some(metadata.protocol);
        }
        if options.profiles.is_empty() {
            options.profiles = metadata.profiles.clone();
        }
    } else {
        ensure!(
            !options.remove && !options.check && !options.update,
            "no saved connection for this instance"
        );
    }
    let model = options.model.as_deref().unwrap_or(super::DEFAULT_MODEL);
    let label = options.display_name.as_deref().unwrap_or(model);
    super::validate_model(model)?;
    super::validate_display_name(label)?;
    let protocol = options.protocol.unwrap_or_default();
    let profiles = if matches!(target, Target::Deepseek) {
        super::deepseek::profiles(&instance, &options.profiles)?
    } else {
        vec![]
    };
    if previous.is_none()
        && options.base_url.is_none()
        && options.config.is_none()
        && !options.yes
        && !options.dry_run
        && crate::prompt::terminal()
        && crate::prompt::select(
            "Gateway",
            "Choose the gateway for this client instance.",
            vec![
                crate::prompt::Item::new(
                    "Local BitRouter",
                    "Use the current configuration and start its local gateway if needed",
                ),
                crate::prompt::Item::new("Existing gateway", "Enter a gateway URL and API key"),
            ],
            0,
        )
        .await?
            == 1
    {
        options.base_url = Some(crate::prompt::line("Gateway URL").await?);
    }
    let local = if options.base_url.is_none() {
        let source = crate::paths::resolve_config(options.config.as_deref())?;
        let config = crate::paths::load_config(&source).await?;
        Some((source, config))
    } else {
        None
    };
    let endpoint = match (&options.base_url, &local) {
        (Some(url), _) => super::endpoint(url)?,
        (_, Some((_, config))) => {
            super::endpoint(&local_gateway::derive_base_url(&config.server.listen))?
        }
        _ => anyhow::bail!("no gateway selected"),
    };
    let token = match local_gateway::nonempty_env(local_gateway::BITROUTER_API_KEY_ENV) {
        Some(token) => token,
        None if local.as_ref().is_some_and(|(_, c)| c.server.skip_auth)
            && super::loopback(&endpoint)? =>
        {
            local_gateway::PLACEHOLDER_API_KEY.into()
        }
        None if previous
            .as_ref()
            .is_some_and(|receipt| receipt.endpoint == endpoint) =>
        {
            previous
                .as_ref()
                .context("missing previous connection")?
                .stored_token()?
        }
        None => credential(!options.yes).await?,
    };
    let build = || {
        prepare(
            target,
            &endpoint,
            &token,
            (model, label),
            options.instance.as_ref().map(|_| instance.as_path()),
            &profiles,
            protocol,
        )
    };
    // Fresh setup has a complete file preview. Replacement is planned against the
    // previous receipt; its recoverable transaction prepares the restored baseline.
    let prepared = if previous.is_none() {
        Some(build()?)
    } else {
        None
    };
    let mut result = if let Some(receipt) = &prepared {
        report(
            target,
            receipt,
            if options.dry_run {
                "preview"
            } else {
                "configured"
            },
        )?
    } else {
        let receipt = previous.as_ref().context("missing connection to update")?;
        let mut result = report(target, receipt, "updated")?;
        result.endpoint = endpoint.clone();
        result.model = model.into();
        result.display_name = label.into();
        result.protocol = protocol;
        result.notes = vec![
            format!("Replace the saved provider: display {label}, request model {model}, protocol {protocol:?}."),
            "Updating restores the earlier native startup default; select BitRouter again afterward if desired. Unrelated settings are retained.".into(),
            "The previous connection remains recoverable if the replacement fails or is interrupted.".into(),
        ];
        if matches!(target, Target::Deepseek) {
            result.notes.push(format!(
                "Selected Harness profiles: {}",
                profiles.join(", ")
            ));
            result.files.extend(profiles.iter().map(|profile| {
                instance
                    .join("profiles")
                    .join(profile)
                    .join("cordis.patch.yml")
            }));
            result.files.sort();
            result.files.dedup();
        }
        result
    };
    preview(&result);
    if options.dry_run {
        return output.emit(&result).map_err(Into::into);
    }
    if !super::approved(&options, "Connect this client instance to BitRouter?").await? {
        result.state = "cancelled";
        return output.emit(&result).map_err(Into::into);
    }
    if let Some((source, config)) = &local
        && local_gateway::listen_is_local(&config.server.listen)
    {
        local_gateway::ensure_local_daemon(source, config, false).await;
    }
    super::verify_gateway(&endpoint, &token, model).await?;
    let receipt = if let Some(previous) = previous {
        previous.replace(&receipt_path, build)?
    } else {
        let receipt = prepared.context("missing prepared connection")?;
        receipt.apply(&receipt_path)?;
        receipt
    };
    result = report(target, &receipt, result.state)?;
    result.configuration_verified = receipt.installed()?;
    result.gateway_catalog_verified = true;
    output.emit(&result).map_err(Into::into)
}

fn preview(report: &Report) {
    eprintln!(
        "\n{} — {} via {}",
        report.target.name(),
        report.model,
        report.endpoint
    );
    eprintln!("  Instance: {}", report.instance);
    for path in &report.files {
        eprintln!("  {}", path.display());
    }
    for note in &report.notes {
        eprintln!("  {note}");
    }
}
