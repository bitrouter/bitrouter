//! Update the local harness before opening a session store or gateway connection.

use std::io::{self, Write};

use anyhow::Result;
use bitrouter_update::{Product, channel, install::Installation};
use clap::Args;
use serde_json::json;

#[derive(Args)]
pub(crate) struct Options {
    /// Check a release without installing it.
    #[arg(long)]
    check: bool,
    /// Select an exact release tag, including for rollback.
    #[arg(long)]
    tag: Option<String>,
    /// Select stable releases only.
    #[arg(long)]
    stable: bool,
    /// Skip confirmation.
    #[arg(short, long)]
    yes: bool,
}

pub(crate) async fn run(opts: Options) -> Result<()> {
    let product = Product::Bro;
    let current = env!("CARGO_PKG_VERSION");
    let exe = std::env::current_exe()?.canonicalize()?;
    let method = bitrouter_update::install_method(&exe, bitrouter_update::cargo_home().as_deref());
    let install = match method {
        "homebrew" | "npm" => None,
        _ => Installation::load(product, &exe, current)?,
    };
    let Some(install) = install else {
        return crate::emit(
            &json!({"status":"delegated","current_version":current,"install_method":method,
            "upgrade_command":bitrouter_update::manual_command(product, method)}),
        );
    };
    let release = channel::resolve(
        product,
        env!("BRO_BUILD_TARGET"),
        opts.tag.as_deref(),
        opts.stable,
    )
    .await?;
    let version = release.version.to_string();
    let available = opts.tag.is_some() || release.version.cmp_precedence(&current.parse()?).is_gt();
    if opts.check {
        return crate::emit(
            &json!({"status":"checked","current_version":current,"update_available":available,"target_version":version}),
        );
    }
    if !available {
        install.finish_interrupted()?;
        return crate::emit(&json!({"status":"unchanged","current_version":current}));
    }
    if !opts.yes && !confirm("BRO", current, &version)? {
        return crate::emit(&json!({"status":"aborted","current_version":current}));
    }
    let candidate = release.stage().await?;
    install.apply(candidate)?;
    crate::emit(&json!({"status":"updated","current_version":current,"target_version":version}))
}

pub(crate) fn confirm(product: &str, current: &str, version: &str) -> Result<bool> {
    eprint!("Update {product} from {current} to {version}? [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
