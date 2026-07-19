//! `bitrouter` onboarding — a deterministic, scripted wizard (no LLM, no Node,
//! no TUI-manager) that *sequences verbs that already exist* and ends in first
//! value: a launched harness, a running daemon, or a printed paste-in snippet.
//!
//! Two entry points, both landing here:
//! - bare `bitrouter` ([`entry`]) runs the credential [`probe`] and either
//!   launches the wizard (unconfigured) or prints a one-line status + a
//!   `bitrouter launch` hint (configured). It never re-onboards a configured
//!   user and never silently spawns a harness or daemon.
//! - `bitrouter init` ([`run`]) runs the wizard interactively, or — with
//!   `--yes` (or no TTY) — headlessly, emitting the JSON result envelope and
//!   never blocking on a human.
//!
//! **The wizard writes no config.** `Config` is `Deserialize`-only, so the sole
//! durable state onboarding produces is **credentials** (which already persist
//! to the credential store, independent of `bitrouter.yaml`, and are
//! auto-detected by zero-config). The one sanctioned `bitrouter.yaml` write is
//! the canned starter template via [`crate::commands::write_starter_config`],
//! and only on explicit request (`--yes` / `--write-config` / the exit-(c)
//! prompt).

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use bitrouter_cloud_sdk::auth::commands::{LoginInputs, login as cloud_login};
use bitrouter_cloud_sdk::auth::credentials::default_credentials_path;
use bitrouter_providers::oauth::credential_store::CredentialStore;
use clap::ValueEnum;
use serde::Serialize;

use crate::commands::{ProviderLoginOptions, ScaffoldOutcome, login_provider_with_options};
use crate::output::CliReport;
use crate::output::Output;
use crate::output::human::Human;
use crate::spawn::SpawnAgent;

// =====================================================================
// §5 — network-free credential probe
// =====================================================================

/// The BYOK env vars the probe checks, each paired with the provider id it
/// enables. `GEMINI_API_KEY` (Google's own name), **not** `GOOGLE_API_KEY`;
/// `OPENCODE_ZEN_API_KEY` is shared by opencode-zen and opencode-go.
const PROBE_ENV_VARS: &[(&str, &str)] = &[
    ("OPENAI_API_KEY", "openai"),
    ("ANTHROPIC_API_KEY", "anthropic"),
    ("GEMINI_API_KEY", "google"),
    ("OPENROUTER_API_KEY", "openrouter"),
    ("OPENCODE_ZEN_API_KEY", "opencode-zen"),
];

/// Purely-local signals that decide "configured vs not" without any network
/// call, registry load, or `merge_registry_into`. See [`probe`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProbeSignals {
    /// BYOK env var **names** present in the environment (e.g. `OPENAI_API_KEY`).
    pub env_keys: Vec<String>,
    /// Whether the cloud session file (`account-credentials.json`) exists.
    pub cloud_session: bool,
    /// Provider ids with at least one stored credential (subscription session,
    /// pasted key, …) in the local credential store.
    pub subscription_providers: Vec<String>,
}

impl ProbeSignals {
    /// "Configured" = at least one of the three local signals is present.
    pub fn is_configured(&self) -> bool {
        !self.env_keys.is_empty() || self.cloud_session || !self.subscription_providers.is_empty()
    }

    /// The provider ids implied by the detected env keys (e.g. `OPENAI_API_KEY`
    /// → `openai`), used to populate the result envelope.
    fn env_provider_ids(&self) -> Vec<String> {
        self.env_keys
            .iter()
            .filter_map(|var| {
                PROBE_ENV_VARS
                    .iter()
                    .find(|(v, _)| *v == var)
                    .map(|(_, id)| (*id).to_string())
            })
            .collect()
    }

    /// Every provider id onboarding can treat as already-usable: detected env
    /// providers, an active cloud session (`bitrouter`), and store providers.
    fn already_configured(&self) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = self.env_provider_ids().into_iter().collect();
        if self.cloud_session {
            set.insert(bitrouter_cloud_sdk::provider::PROVIDER_ID.to_string());
        }
        for id in &self.subscription_providers {
            set.insert(id.clone());
        }
        set
    }
}

/// Run the network-free credential probe (§5). Reads, in order: the BYOK env
/// vars, the cloud credentials file's existence, and the local credential
/// store's provider list. Performs **no** network I/O and never loads the
/// registry.
pub fn probe() -> ProbeSignals {
    let env_keys = detected_env_keys(|name| std::env::var(name).ok().filter(|v| !v.is_empty()));
    // File-existence check only — never reads/validates the token, never calls
    // the network.
    let cloud_session = default_credentials_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    // A local marker check: the credential store is a single on-disk JSON file;
    // `providers()` lists ids with a stored credential without any refresh or
    // network call. An unresolved/unreadable store is simply "none".
    let subscription_providers = CredentialStore::default_path()
        .map(|store| store.providers().into_iter().map(String::from).collect())
        .unwrap_or_default();
    ProbeSignals {
        env_keys,
        cloud_session,
        subscription_providers,
    }
}

/// The subset of [`PROBE_ENV_VARS`] whose var is present, per the injected
/// lookup. Factored out so the detection can be unit-tested without touching
/// the process environment.
fn detected_env_keys(lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    PROBE_ENV_VARS
        .iter()
        .filter(|(var, _)| lookup(var).is_some())
        .map(|(var, _)| (*var).to_string())
        .collect()
}

// =====================================================================
// Flags (§3.2) — every wizard prompt has a flag equivalent
// =====================================================================

/// The three-way finish exit (§3.2 step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AfterAction {
    /// Launch the harness's native TUI now (`bitrouter launch`).
    Launch,
    /// Start the daemon and print a paste-in snippet for an existing tool.
    Serve,
    /// Do nothing further.
    Exit,
}

impl AfterAction {
    fn as_str(self) -> &'static str {
        match self {
            AfterAction::Launch => "launch",
            AfterAction::Serve => "serve",
            AfterAction::Exit => "exit",
        }
    }
}

/// Every wizard prompt mapped to a flag — consumed by `--yes` and scriptable
/// directly. Built from `Command::Init` in `main.rs`.
#[derive(Debug, Clone)]
pub struct OnboardingFlags {
    /// Starter-config write path (`-c/--config`, default `bitrouter.yaml`).
    pub config: PathBuf,
    /// Run headlessly, emitting the JSON envelope and never blocking.
    pub yes: bool,
    /// Allow overwriting an existing `bitrouter.yaml` when scaffolding.
    pub force: bool,
    /// Clear stored onboarding credentials before running.
    pub reset: bool,
    /// (Step 1) Sign in to BitRouter Cloud via device-flow OAuth.
    pub cloud_login: bool,
    /// (Step 1) Seed the cloud credential from a `brk_` API key (headless).
    pub api_key: Option<String>,
    /// (Step 1) Log in to these upstream providers by id (repeatable).
    pub providers: Vec<String>,
    /// (Step 1) API keys paired by position with `providers` (repeatable).
    pub provider_api_keys: Vec<String>,
    /// (Step 1) Accept the auto-detected credentials without prompting.
    pub use_detected: bool,
    /// (Step 2) Harnesses to drive: `claude` / `codex` (repeatable).
    pub harnesses: Vec<SpawnAgent>,
    /// (Step 2) Never install a missing harness.
    pub no_install: bool,
    /// (Step 3) What to do at the end.
    pub after: Option<AfterAction>,
    /// (Step 3) Model handed to the harness for this session (not persisted).
    pub model: Option<String>,
    /// (Step 3) Write a starter `bitrouter.yaml`.
    pub write_config: bool,
}

impl Default for OnboardingFlags {
    fn default() -> Self {
        Self {
            config: PathBuf::from("bitrouter.yaml"),
            yes: false,
            force: false,
            reset: false,
            cloud_login: false,
            api_key: None,
            providers: Vec::new(),
            provider_api_keys: Vec::new(),
            use_detected: false,
            harnesses: Vec::new(),
            no_install: false,
            after: None,
            model: None,
            write_config: false,
        }
    }
}

// =====================================================================
// Result envelope (§3.3) + configured-status view (§13 Q1)
// =====================================================================

/// Paste-in wiring for `after: serve` (§13 Q2 — all three shapes, labeled).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Snippet {
    /// The daemon base URL the snippets point at.
    pub base_url: String,
    /// `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` export lines.
    pub anthropic: String,
    /// `OPENAI_BASE_URL` + `OPENAI_API_KEY` export lines.
    pub openai: String,
    /// The Codex `-c` provider-override invocation.
    pub codex: String,
}

/// The standard onboarding result envelope, emitted on stdout by the wizard and
/// every `--yes` run.
#[derive(Debug, Clone, Serialize)]
pub struct OnboardingReport {
    /// Always `"onboarding"`.
    pub action: &'static str,
    /// Provider ids now usable (detected, pre-existing, or freshly set up).
    pub providers_configured: Vec<String>,
    /// Providers that would need an interactive human (OAuth device/PKCE, a
    /// claude-code session import) and were reported-and-skipped, not attempted.
    pub providers_skipped_interactive: Vec<String>,
    /// Harnesses confirmed available after step 2.
    pub harnesses_installed: Vec<String>,
    /// `"launch"` | `"serve"` | `"exit"`.
    pub after: String,
    /// The paste-in snippet for `after: serve`; `null` otherwise.
    pub snippet: Option<Snippet>,
}

impl CliReport for OnboardingReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line("onboarding complete")?;
        h.field(
            "providers",
            if self.providers_configured.is_empty() {
                "(none)".to_string()
            } else {
                self.providers_configured.join(", ")
            },
        )?;
        if !self.providers_skipped_interactive.is_empty() {
            h.field("skipped", self.providers_skipped_interactive.join(", "))?;
        }
        h.field(
            "harnesses",
            if self.harnesses_installed.is_empty() {
                "(none)".to_string()
            } else {
                self.harnesses_installed.join(", ")
            },
        )?;
        h.field("after", &self.after)?;
        if let Some(snippet) = &self.snippet {
            h.blank()?;
            h.line(&format!("paste-in wiring ({}):", snippet.base_url))?;
            h.blank()?;
            h.line("  Anthropic / Claude Code:")?;
            for l in snippet.anthropic.lines() {
                h.line(&format!("    {l}"))?;
            }
            h.line("  OpenAI SDK:")?;
            for l in snippet.openai.lines() {
                h.line(&format!("    {l}"))?;
            }
            h.line("  Codex:")?;
            for l in snippet.codex.lines() {
                h.line(&format!("    {l}"))?;
            }
        }
        Ok(())
    }
}

/// The configured-user status view for bare `bitrouter` (§13 Q1): a compact
/// status plus a `bitrouter launch` hint — never clap help, never the wizard.
#[derive(Debug, Clone, Serialize)]
pub struct OnboardingStatusReport {
    /// Always `"status"`.
    pub action: &'static str,
    /// Always `true` here — this report is only built for a configured user.
    pub configured: bool,
    /// The local credential signals that made the probe report "configured".
    pub signals: Vec<String>,
    /// The one-line next-step hint.
    pub hint: String,
}

impl OnboardingStatusReport {
    fn from_signals(signals: &ProbeSignals) -> Self {
        let mut parts = Vec::new();
        if signals.cloud_session {
            parts.push("cloud session".to_string());
        }
        if !signals.subscription_providers.is_empty() {
            parts.push(format!(
                "providers: {}",
                signals.subscription_providers.join(", ")
            ));
        }
        if !signals.env_keys.is_empty() {
            parts.push(format!("env: {}", signals.env_keys.join(", ")));
        }
        Self {
            action: "status",
            configured: true,
            signals: parts,
            hint: "run `bitrouter launch` to start a coding session".to_string(),
        }
    }
}

impl CliReport for OnboardingStatusReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "bitrouter is configured ({}) — {}",
            if self.signals.is_empty() {
                "credentials present".to_string()
            } else {
                self.signals.join("; ")
            },
            self.hint,
        ))
    }
}

// =====================================================================
// Entry points
// =====================================================================

/// Bare `bitrouter` (no subcommand): probe, then status (configured) or the
/// wizard (unconfigured). Exit code 0 either way; never writes config, never
/// spawns a daemon/harness on its own.
pub async fn entry(output: &Output) -> Result<()> {
    let signals = probe();
    if signals.is_configured() {
        return emit(output, &OnboardingStatusReport::from_signals(&signals));
    }
    if std::io::stdin().is_terminal() {
        run_interactive(OnboardingFlags::default(), &signals, output).await
    } else {
        // No TTY and nothing configured: the interactive wizard can't run.
        // Print the multi-line hint to stderr and emit an inert envelope so
        // the invocation stays machine-observable and exits 0.
        print_hint();
        emit(output, &empty_report())
    }
}

/// `bitrouter init [flags]`. Runs the wizard interactively, or headlessly when
/// `--yes` is set (or no TTY is attached). `--reset` clears credentials first.
pub async fn run(flags: OnboardingFlags, output: &Output) -> Result<()> {
    if flags.reset {
        reset_credentials(flags.yes, std::io::stdin().is_terminal()).await?;
    }
    if flags.yes {
        return run_headless(flags, output).await;
    }
    if std::io::stdin().is_terminal() {
        let signals = probe();
        run_interactive(flags, &signals, output).await
    } else {
        // No TTY and no `--yes`: fall back to the headless runner. Preserve the
        // historical `bitrouter init` behavior (scaffold the starter file) by
        // forcing the config write, and still emit the envelope.
        run_headless(
            OnboardingFlags {
                write_config: true,
                ..flags
            },
            output,
        )
        .await
    }
}

fn empty_report() -> OnboardingReport {
    OnboardingReport {
        action: "onboarding",
        providers_configured: Vec::new(),
        providers_skipped_interactive: Vec::new(),
        harnesses_installed: Vec::new(),
        after: AfterAction::Exit.as_str().to_string(),
        snippet: None,
    }
}

// =====================================================================
// §4 — headless runner
// =====================================================================

async fn run_headless(flags: OnboardingFlags, output: &Output) -> Result<()> {
    let signals = probe();
    // Already-present credentials always count (spec §4: "consume
    // already-present credentials + flag-supplied keys").
    let mut configured = signals.already_configured();
    let mut skipped: Vec<String> = Vec::new();

    // --- Step 1: credentials (flag-driven; interactive OAuth reported-and-skipped) ---
    apply_flag_credentials(&flags, &mut configured, &mut skipped, true).await?;

    // --- Step 2: harness (resolve only; headless never installs — see §13
    // resolution notes: keeps `--yes` non-blocking and network-free) ---
    let mut installed: Vec<String> = Vec::new();
    for agent in &flags.harnesses {
        // `no_install: true` makes `ensure_agent_installed` resolve-or-error
        // without ever prompting or shelling out to an installer.
        match crate::spawn::ensure_agent_installed(*agent, true).await {
            Ok(_) => installed.push(agent.spec().id.to_string()),
            Err(_) => note(&format!(
                "harness '{}' is not installed — skipped (install it, or run \
                 `bitrouter launch -a {}` interactively)",
                agent.spec().id,
                agent.spec().id
            )),
        }
    }

    // --- Config scaffold (the one sanctioned bitrouter.yaml write) ---
    if flags.yes || flags.write_config {
        match crate::commands::write_starter_config(&flags.config, flags.force).await? {
            ScaffoldOutcome::Wrote => note(&format!(
                "wrote starter config to {}",
                flags.config.display()
            )),
            ScaffoldOutcome::Skipped => note(&format!(
                "{} already exists — left untouched (pass --force to overwrite)",
                flags.config.display()
            )),
        }
    }

    // --- Step 3: finish ---
    let after = flags.after.unwrap_or(AfterAction::Exit);
    let mut report = OnboardingReport {
        action: "onboarding",
        providers_configured: configured.into_iter().collect(),
        providers_skipped_interactive: skipped,
        harnesses_installed: installed.clone(),
        after: after.as_str().to_string(),
        snippet: None,
    };

    match after {
        AfterAction::Launch => {
            // Honor launch only when the chosen harness is already present.
            match pick_launch_harness(&flags.harnesses, &installed) {
                Some(agent) => {
                    finish_launch(
                        agent,
                        flags.model.as_deref(),
                        flags.no_install,
                        report,
                        output,
                    )
                    .await
                }
                None => {
                    note("no requested harness is installed — nothing to launch; exiting");
                    report.after = AfterAction::Exit.as_str().to_string();
                    emit(output, &report)
                }
            }
        }
        AfterAction::Serve => finish_serve(report, output).await,
        AfterAction::Exit => emit(output, &report),
    }
}

/// Apply the flag-supplied credentials shared by both the headless runner and
/// the interactive wizard's step-1 pre-pass. `headless` controls the parts a
/// machine can't do: a bare `--cloud-login` (device flow) and a `--provider`
/// with no key are reported-and-skipped headlessly, but drive the real
/// interactive flows when a human is present.
async fn apply_flag_credentials(
    flags: &OnboardingFlags,
    configured: &mut BTreeSet<String>,
    skipped: &mut Vec<String>,
    headless: bool,
) -> Result<()> {
    let cloud = bitrouter_cloud_sdk::provider::PROVIDER_ID;
    // Cloud: a brk_ key is non-interactive; a bare --cloud-login needs the
    // device flow (headless skips, interactive runs it).
    if let Some(key) = flags.api_key.as_deref() {
        seed_cloud_api_key(key).await?;
        configured.insert(cloud.to_string());
    } else if flags.cloud_login {
        if headless {
            skipped.push(cloud.to_string());
        } else {
            match seed_cloud_interactive().await {
                Ok(()) => {
                    configured.insert(cloud.to_string());
                }
                Err(e) => note(&format!("cloud sign-in did not complete: {e:#}")),
            }
        }
    }

    // Per-provider: a paired key seeds it non-interactively; without one it
    // needs an interactive OAuth/PKCE/session flow (headless skips; interactive
    // runs the provider's login menu).
    for (i, provider) in flags.providers.iter().enumerate() {
        match flags.provider_api_keys.get(i) {
            Some(key) => match seed_provider_api_key(provider, key).await {
                Ok(()) => {
                    configured.insert(provider.clone());
                }
                Err(e) => {
                    note(&format!("provider '{provider}' not configured: {e:#}"));
                    skipped.push(provider.clone());
                }
            },
            None if headless => skipped.push(provider.clone()),
            None => match login_provider_with_options(
                provider,
                bitrouter_providers::oauth::credential_store::DEFAULT_LABEL,
                ProviderLoginOptions::default(),
            )
            .await
            {
                Ok(_) => {
                    configured.insert(provider.clone());
                }
                Err(e) => {
                    note(&format!(
                        "provider '{provider}' login did not complete: {e:#}"
                    ));
                    skipped.push(provider.clone());
                }
            },
        }
    }
    Ok(())
}

/// Seed the cloud credential from a `brk_` key. Calls the cloud SDK's `login`
/// directly (not `cloud::cli::run`) so onboarding emits a single result
/// envelope on stdout — `login` writes only progress to stderr and persists
/// the credential to the store.
async fn seed_cloud_api_key(key: &str) -> Result<()> {
    cloud_login(LoginInputs {
        authorization_server: None,
        client_id: None,
        scope: None,
        api_key: Some(key.to_string()),
    })
    .await
    .map(|_| ())
    .context("seeding the cloud credential from --api-key")
}

/// Seed one upstream provider non-interactively from a pasted key, via the
/// same `providers login --api-key` path.
async fn seed_provider_api_key(provider: &str, key: &str) -> Result<()> {
    login_provider_with_options(
        provider,
        bitrouter_providers::oauth::credential_store::DEFAULT_LABEL,
        ProviderLoginOptions {
            import_existing: false,
            no_browser: true,
            api_key: Some(key.to_string()),
        },
    )
    .await
    .map(|_| ())
}

// =====================================================================
// Interactive wizard
// =====================================================================

async fn run_interactive(
    flags: OnboardingFlags,
    signals: &ProbeSignals,
    output: &Output,
) -> Result<()> {
    eprintln!();
    eprintln!("Welcome to BitRouter — let's get you to first value.");
    eprintln!();

    // --- Step 1: credentials ---
    let mut configured = signals.already_configured();
    let mut skipped: Vec<String> = Vec::new();
    // Apply any flag-supplied credentials first (so `bitrouter init --api-key …`
    // / `--provider …` seed before we prompt), then either accept the detected
    // set (`--use-detected`) or open the interactive credential menu.
    apply_flag_credentials(&flags, &mut configured, &mut skipped, false).await?;
    let seeded_from_flags =
        flags.api_key.is_some() || flags.cloud_login || !flags.providers.is_empty();
    if flags.use_detected && signals.is_configured() {
        eprintln!("Step 1/3 — Credentials");
        note("using the detected credential(s)");
    } else if !seeded_from_flags {
        interactive_credentials(signals, &mut configured, &mut skipped).await?;
    }

    // --- Step 2: harness ---
    let installed = interactive_harness(&flags).await?;

    // --- Step 3: finish ---
    let after = interactive_after(&flags, &installed)?;

    let mut report = OnboardingReport {
        action: "onboarding",
        providers_configured: configured.into_iter().collect(),
        providers_skipped_interactive: skipped,
        harnesses_installed: installed.clone(),
        after: after.as_str().to_string(),
        snippet: None,
    };

    match after {
        AfterAction::Launch => match pick_launch_harness(&flags.harnesses, &installed)
            .or_else(|| installed.first().and_then(|id| agent_by_id(id)))
        {
            Some(agent) => {
                finish_launch(
                    agent,
                    flags.model.as_deref(),
                    flags.no_install,
                    report,
                    output,
                )
                .await
            }
            None => {
                note("no harness available to launch; exiting");
                report.after = AfterAction::Exit.as_str().to_string();
                emit(output, &report)
            }
        },
        AfterAction::Serve => finish_serve(report, output).await,
        AfterAction::Exit => {
            // Optional starter-config write (the one safe config write).
            if flags.write_config
                || prompt_yes_no("Write a starter bitrouter.yaml to edit later?", false)
            {
                match crate::commands::write_starter_config(&flags.config, flags.force).await? {
                    ScaffoldOutcome::Wrote => note(&format!(
                        "wrote starter config to {}",
                        flags.config.display()
                    )),
                    ScaffoldOutcome::Skipped => note(&format!(
                        "{} already exists — left untouched (pass --force to overwrite)",
                        flags.config.display()
                    )),
                }
            }
            emit(output, &report)
        }
    }
}

async fn interactive_credentials(
    signals: &ProbeSignals,
    configured: &mut BTreeSet<String>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    eprintln!("Step 1/3 — Credentials");
    if signals.is_configured() {
        eprintln!(
            "  Detected: {}",
            OnboardingStatusReport::from_signals(signals)
                .signals
                .join("; ")
        );
    }
    loop {
        eprintln!();
        eprintln!("  How would you like to authenticate?");
        eprintln!("    1) Sign in to BitRouter Cloud — one account, every model [default]");
        eprintln!("    2) Log in to a specific provider (claude-code / openai-codex / …)");
        if signals.is_configured() {
            eprintln!("    3) Use the detected credential(s) and continue");
        }
        eprintln!("    0) Skip for now");
        match prompt_line("  Choose [1]: ")?.as_str() {
            "" | "1" => match seed_cloud_interactive().await {
                Ok(()) => {
                    configured.insert(bitrouter_cloud_sdk::provider::PROVIDER_ID.to_string());
                }
                Err(e) => note(&format!("cloud sign-in did not complete: {e:#}")),
            },
            "2" => {
                let id = prompt_line("  Provider id: ")?;
                if id.is_empty() {
                    note("no provider id entered — skipping");
                } else {
                    match login_provider_with_options(
                        &id,
                        bitrouter_providers::oauth::credential_store::DEFAULT_LABEL,
                        ProviderLoginOptions::default(),
                    )
                    .await
                    {
                        Ok(_) => {
                            configured.insert(id);
                        }
                        Err(e) => {
                            note(&format!("provider '{id}' login did not complete: {e:#}"));
                            skipped.push(id);
                        }
                    }
                }
            }
            "3" if signals.is_configured() => break,
            "0" => break,
            other => {
                note(&format!("'{other}' is not a choice"));
                continue;
            }
        }
        if !prompt_yes_no("  Add another provider?", false) {
            break;
        }
    }
    Ok(())
}

/// The cloud device-flow sign-in. Calls the cloud SDK's `login` directly (not
/// `cloud::cli::run`) so onboarding emits a single result envelope on stdout;
/// `login` drives the device-flow prompts on stderr and persists the token.
async fn seed_cloud_interactive() -> Result<()> {
    cloud_login(LoginInputs {
        authorization_server: None,
        client_id: None,
        scope: None,
        api_key: None,
    })
    .await
    .map(|_| ())
    .context("BitRouter Cloud sign-in")
}

async fn interactive_harness(flags: &OnboardingFlags) -> Result<Vec<String>> {
    eprintln!();
    eprintln!("Step 2/3 — Harness");
    // Honor flag-provided harnesses non-interactively; otherwise ask.
    let chosen: Vec<SpawnAgent> = if !flags.harnesses.is_empty() {
        flags.harnesses.clone()
    } else {
        let answer = prompt_line("  Which coding agent do you drive? [claude/codex/skip]: ")?;
        match answer.to_ascii_lowercase().as_str() {
            "" | "claude" => vec![SpawnAgent::Claude],
            "codex" => vec![SpawnAgent::Codex],
            "skip" | "none" => Vec::new(),
            other => {
                note(&format!("'{other}' is not a known harness — skipping"));
                Vec::new()
            }
        }
    };

    let mut installed = Vec::new();
    for agent in chosen {
        // `ensure_agent_installed` offers the native installer when missing (a
        // TTY + not --no-install) and re-resolves the freshly-installed path,
        // so the launch exit can't dead-end on the PATH-after-install caveat.
        match crate::spawn::ensure_agent_installed(agent, flags.no_install).await {
            Ok(_) => installed.push(agent.spec().id.to_string()),
            Err(e) => note(&format!("{}: {e:#}", agent.spec().id)),
        }
    }
    Ok(installed)
}

fn interactive_after(flags: &OnboardingFlags, installed: &[String]) -> Result<AfterAction> {
    if let Some(after) = flags.after {
        return Ok(after);
    }
    eprintln!();
    eprintln!("Step 3/3 — Finish");
    let can_launch = !installed.is_empty();
    if can_launch {
        eprintln!("    1) Launch now [default]");
    }
    eprintln!("    2) Start the daemon and print a paste-in snippet");
    eprintln!("    3) Exit");
    let default_choice = if can_launch { "1" } else { "2" };
    let choice = prompt_line(&format!("  Choose [{default_choice}]: "))?;
    let choice = if choice.is_empty() {
        default_choice.to_string()
    } else {
        choice
    };
    Ok(match choice.as_str() {
        "1" if can_launch => AfterAction::Launch,
        "2" => AfterAction::Serve,
        _ => AfterAction::Exit,
    })
}

// =====================================================================
// Finish exits (a) launch / (b) serve+snippet
// =====================================================================

/// Exit (a): emit the envelope, then hand the terminal to the harness. The
/// launch diverges (it exits the process with the child's status), so the
/// envelope must be emitted first.
async fn finish_launch(
    agent: SpawnAgent,
    model: Option<&str>,
    no_install: bool,
    report: OnboardingReport,
    output: &Output,
) -> Result<()> {
    let source = crate::paths::resolve_config(None)?;
    let cfg = crate::paths::load_config(&source).await?;
    emit(output, &report)?;
    let agent_args = match model {
        Some(m) if !m.is_empty() => vec!["--model".to_string(), m.to_string()],
        _ => Vec::new(),
    };
    let opts = crate::spawn::SpawnOptions {
        agent,
        agent_args,
        base_url: None,
        no_install,
        no_start: false,
        check: false,
    };
    crate::spawn::run(&source, &cfg, opts).await
}

/// Exit (b): start the local daemon (best-effort, reusing the launch path's
/// auto-start), build the paste-in snippet, and emit the envelope with it.
async fn finish_serve(mut report: OnboardingReport, output: &Output) -> Result<()> {
    let source = crate::paths::resolve_config(None)?;
    let cfg = crate::paths::load_config(&source).await?;
    crate::spawn::ensure_local_daemon(&source, &cfg, false).await;
    report.snippet = Some(build_snippet(&cfg.server.listen));
    emit(output, &report)
}

/// Build the three labeled paste-in shapes (§13 Q2). Templates the bearer by
/// auth mode: a real exported `BITROUTER_API_KEY` (`brk_`) when present, else
/// the local `skip_auth` placeholder. The Codex form is taken from the shared
/// harness catalog so it stays in lockstep with `bitrouter launch`.
fn build_snippet(listen: &str) -> Snippet {
    let base_url = crate::spawn::derive_base_url(listen);
    let token = crate::spawn::nonempty_env(crate::harness::BITROUTER_API_KEY_ENV)
        .unwrap_or_else(|| crate::harness::PLACEHOLDER_API_KEY.to_string());
    let v1 = {
        let trimmed = base_url.trim_end_matches('/');
        if trimmed.ends_with("/v1") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/v1")
        }
    };
    let anthropic =
        format!("export ANTHROPIC_BASE_URL={base_url}\nexport ANTHROPIC_AUTH_TOKEN={token}");
    let openai = format!("export OPENAI_BASE_URL={v1}\nexport OPENAI_API_KEY={token}");
    let codex = match crate::harness::by_id("codex-acp") {
        Some(h) => {
            let overlay = h.routing_overlay(&base_url, &token, None);
            let mut parts = vec!["codex".to_string()];
            parts.extend(overlay.args);
            parts.join(" ")
        }
        None => String::new(),
    };
    Snippet {
        base_url,
        anthropic,
        openai,
        codex,
    }
}

// =====================================================================
// --reset (§13 Q3)
// =====================================================================

/// Clears the cloud session (always) and — after a confirm, unless `--yes` —
/// the stored provider credentials.
async fn reset_credentials(assume_yes: bool, interactive: bool) -> Result<()> {
    let cloud_path = default_credentials_path().ok();
    let mut store = CredentialStore::default_path().ok();

    let provider_ids: Vec<String> = store
        .as_ref()
        .map(|s| s.providers().into_iter().map(String::from).collect())
        .unwrap_or_default();

    // Cloud session is cleared unconditionally; provider credentials only after
    // an explicit confirm (or under --yes).
    let remove_providers = if provider_ids.is_empty() {
        false
    } else if assume_yes {
        true
    } else if interactive {
        prompt_yes_no(
            &format!(
                "  --reset: also remove {} stored provider credential(s) ({})?",
                provider_ids.len(),
                provider_ids.join(", ")
            ),
            false,
        )
    } else {
        false
    };

    let outcome = reset_with(cloud_path.as_deref(), store.as_mut(), remove_providers)?;
    if outcome.cloud_cleared {
        note("cleared the cloud session");
    }
    if outcome.providers_removed > 0 {
        note(&format!(
            "removed {} provider credential(s)",
            outcome.providers_removed
        ));
    } else if !provider_ids.is_empty() && !remove_providers {
        note("kept provider credentials");
    }
    Ok(())
}

/// Outcome of [`reset_with`].
#[derive(Debug, Default, PartialEq, Eq)]
struct ResetOutcome {
    cloud_cleared: bool,
    providers_removed: usize,
}

/// The testable core of [`reset_credentials`]: delete the cloud credentials
/// file if present, and (when asked) remove every provider credential from the
/// store. Purely local — no network.
fn reset_with(
    cloud_path: Option<&std::path::Path>,
    store: Option<&mut CredentialStore>,
    remove_providers: bool,
) -> Result<ResetOutcome> {
    let mut outcome = ResetOutcome::default();
    if let Some(path) = cloud_path
        && path.exists()
    {
        std::fs::remove_file(path)
            .with_context(|| format!("removing cloud credentials {}", path.display()))?;
        outcome.cloud_cleared = true;
    }
    if remove_providers && let Some(store) = store {
        let ids: Vec<String> = store.providers().into_iter().map(String::from).collect();
        for id in ids {
            outcome.providers_removed += store
                .remove_all_for(&id)
                .with_context(|| format!("removing credentials for {id}"))?;
        }
    }
    Ok(outcome)
}

// =====================================================================
// Small helpers
// =====================================================================

/// Map a harness id back to its [`SpawnAgent`].
fn agent_by_id(id: &str) -> Option<SpawnAgent> {
    SpawnAgent::value_variants()
        .iter()
        .copied()
        .find(|a| a.spec().id == id)
}

/// The first requested harness that is actually installed (§3.2: "picks
/// claude|codex when both are installed").
fn pick_launch_harness(requested: &[SpawnAgent], installed: &[String]) -> Option<SpawnAgent> {
    requested
        .iter()
        .copied()
        .find(|a| installed.iter().any(|id| id == a.spec().id))
}

/// A dim, indented note to stderr (diagnostics never touch stdout).
fn note(msg: &str) {
    let p = crate::style::Palette::for_stderr();
    eprintln!("  {dim}{msg}{reset}", dim = p.dim, reset = p.reset);
}

/// Emit a report to stdout, mapping `Output::emit`'s `io::Result` into the
/// `anyhow::Result` this module threads.
fn emit(output: &Output, report: &dyn CliReport) -> Result<()> {
    Output::emit(output, report).map_err(anyhow::Error::from)
}

/// Prompt on stderr, read one trimmed line from stdin. EOF yields an empty
/// string (so non-interactive contexts fall through to defaults, never hang).
fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let n = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading input from stdin")?;
    if n == 0 {
        return Ok(String::new());
    }
    Ok(line.trim().to_string())
}

/// A `[y/N]` (or `[Y/n]`) confirm. EOF / blank returns `default_yes`.
fn prompt_yes_no(prompt: &str, default_yes: bool) -> bool {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    match prompt_line(&format!("{prompt} {suffix}: ")) {
        Ok(answer) => match answer.to_ascii_lowercase().as_str() {
            "" => default_yes,
            "y" | "yes" => true,
            _ => false,
        },
        Err(_) => default_yes,
    }
}

/// The multi-line onboarding hint (unconfigured, non-TTY). Mirrors the recovery
/// chain of the former `print_onboarding_hint`.
fn print_hint() {
    let p = crate::style::Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} no credentials detected yet. Get started with one of:",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!();
    eprintln!("  bitrouter init                 # guided setup wizard (interactive)");
    eprintln!("  bitrouter cloud login          # one BitRouter Cloud account, every model");
    eprintln!("  bitrouter providers login claude-code   # a subscription you already pay for");
    eprintln!("  export OPENAI_API_KEY=…        # or any BYOK provider key, then re-run");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_env_keys_reports_present_vars_only() {
        // Only OPENAI + GEMINI "present" in the injected lookup.
        let present = |name: &str| {
            matches!(name, "OPENAI_API_KEY" | "GEMINI_API_KEY").then(|| "x".to_string())
        };
        let keys = detected_env_keys(present);
        assert_eq!(keys, vec!["OPENAI_API_KEY", "GEMINI_API_KEY"]);
    }

    #[test]
    fn detected_env_keys_ignores_empty_values() {
        // The probe's real lookup filters empties; here nothing is present.
        let keys = detected_env_keys(|_| None);
        assert!(keys.is_empty());
    }

    #[test]
    fn probe_uses_gemini_not_google_key() {
        // GOOGLE_API_KEY must NOT be a probe signal (Google's own SDKs use
        // GEMINI_API_KEY); guard against a regression to the wrong var.
        assert!(PROBE_ENV_VARS.iter().any(|(v, _)| *v == "GEMINI_API_KEY"));
        assert!(!PROBE_ENV_VARS.iter().any(|(v, _)| *v == "GOOGLE_API_KEY"));
    }

    #[test]
    fn is_configured_is_false_when_no_signal_present() {
        assert!(!ProbeSignals::default().is_configured());
    }

    #[test]
    fn is_configured_true_for_each_signal_independently() {
        // env key alone
        assert!(
            ProbeSignals {
                env_keys: vec!["OPENAI_API_KEY".to_string()],
                ..Default::default()
            }
            .is_configured()
        );
        // cloud session file alone
        assert!(
            ProbeSignals {
                cloud_session: true,
                ..Default::default()
            }
            .is_configured()
        );
        // a stored subscription/provider credential alone
        assert!(
            ProbeSignals {
                subscription_providers: vec!["claude-code".to_string()],
                ..Default::default()
            }
            .is_configured()
        );
    }

    #[test]
    fn already_configured_maps_env_cloud_and_store() {
        let signals = ProbeSignals {
            env_keys: vec!["OPENAI_API_KEY".to_string(), "GEMINI_API_KEY".to_string()],
            cloud_session: true,
            subscription_providers: vec!["claude-code".to_string()],
        };
        let set = signals.already_configured();
        assert!(set.contains("openai"));
        assert!(set.contains("google")); // GEMINI_API_KEY → google
        assert!(set.contains("bitrouter")); // cloud session
        assert!(set.contains("claude-code"));
    }

    #[test]
    fn envelope_shape_includes_skipped_interactive() {
        let report = OnboardingReport {
            action: "onboarding",
            providers_configured: vec!["bitrouter".to_string(), "openai".to_string()],
            providers_skipped_interactive: vec!["github-copilot".to_string()],
            harnesses_installed: vec!["claude".to_string()],
            after: "launch".to_string(),
            snippet: None,
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["action"], "onboarding");
        assert_eq!(
            v["providers_configured"],
            serde_json::json!(["bitrouter", "openai"])
        );
        assert_eq!(
            v["providers_skipped_interactive"],
            serde_json::json!(["github-copilot"])
        );
        assert_eq!(v["harnesses_installed"], serde_json::json!(["claude"]));
        assert_eq!(v["after"], "launch");
        // `snippet` is present-but-null when there is nothing to paste.
        assert!(v.get("snippet").is_some());
        assert!(v["snippet"].is_null());
    }

    #[test]
    fn snippet_uses_placeholder_when_no_bitrouter_key() {
        // With no BITROUTER_API_KEY exported, the local `skip_auth` placeholder
        // is templated into all three shapes.
        // (This test process does not set BITROUTER_API_KEY.)
        if crate::spawn::nonempty_env(crate::harness::BITROUTER_API_KEY_ENV).is_some() {
            return; // environment is non-hermetic; skip rather than false-fail
        }
        let snippet = build_snippet("127.0.0.1:4356");
        assert_eq!(snippet.base_url, "http://127.0.0.1:4356");
        assert!(
            snippet
                .anthropic
                .contains("ANTHROPIC_BASE_URL=http://127.0.0.1:4356")
        );
        assert!(snippet.anthropic.contains(&format!(
            "ANTHROPIC_AUTH_TOKEN={}",
            crate::harness::PLACEHOLDER_API_KEY
        )));
        // OpenAI shape carries the /v1 suffix.
        assert!(
            snippet
                .openai
                .contains("OPENAI_BASE_URL=http://127.0.0.1:4356/v1")
        );
        // Codex shape is the `-c` provider override, in lockstep with launch.
        assert!(snippet.codex.starts_with("codex "));
        assert!(snippet.codex.contains("model_provider=\"bitrouter\""));
        assert!(snippet.codex.contains("http://127.0.0.1:4356/v1"));
    }

    #[test]
    fn pick_launch_harness_prefers_first_installed() {
        let requested = vec![SpawnAgent::Codex, SpawnAgent::Claude];
        // Only claude installed → codex requested first but skipped.
        let picked = pick_launch_harness(&requested, &["claude".to_string()]);
        assert_eq!(picked, Some(SpawnAgent::Claude));
        // Neither installed → None (launch downgrades to exit).
        assert_eq!(pick_launch_harness(&requested, &[]), None);
    }

    #[test]
    fn agent_by_id_round_trips() {
        assert_eq!(agent_by_id("claude"), Some(SpawnAgent::Claude));
        assert_eq!(agent_by_id("codex"), Some(SpawnAgent::Codex));
        assert_eq!(agent_by_id("nope"), None);
    }

    fn tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-onboarding-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reset_clears_cloud_session_and_optionally_providers() {
        let dir = tmp_dir("reset");
        let cloud = dir.join("account-credentials.json");
        std::fs::write(&cloud, "{}").unwrap();
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json")).unwrap();
        store
            .set(
                "claude-code",
                "default",
                bitrouter_providers::oauth::credential_store::Credential::ClaudeCodeCli,
            )
            .unwrap();
        store
            .set(
                "openai",
                "default",
                bitrouter_providers::oauth::credential_store::Credential::api_key("sk-x"),
            )
            .unwrap();

        // remove_providers = false: cloud cleared, provider creds kept (Q3).
        let outcome = reset_with(Some(&cloud), Some(&mut store), false).unwrap();
        assert!(outcome.cloud_cleared);
        assert_eq!(outcome.providers_removed, 0);
        assert!(!cloud.exists());
        assert_eq!(store.providers().len(), 2, "provider creds must survive");

        // remove_providers = true: every provider credential is dropped.
        let outcome = reset_with(None, Some(&mut store), true).unwrap();
        assert!(!outcome.cloud_cleared); // no cloud file this call
        assert_eq!(outcome.providers_removed, 2);
        assert!(store.providers().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_is_a_noop_when_nothing_stored() {
        let dir = tmp_dir("reset-empty");
        let missing_cloud = dir.join("account-credentials.json");
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json")).unwrap();
        let outcome = reset_with(Some(&missing_cloud), Some(&mut store), true).unwrap();
        assert_eq!(outcome, ResetOutcome::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_report_lists_signals_and_hint() {
        let signals = ProbeSignals {
            env_keys: vec!["OPENAI_API_KEY".to_string()],
            cloud_session: true,
            subscription_providers: vec![],
        };
        let report = OnboardingStatusReport::from_signals(&signals);
        assert!(report.configured);
        assert!(report.hint.contains("bitrouter launch"));
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["action"], "status");
        assert_eq!(v["configured"], true);
    }
}
