# OpenClaw native connection

Use `bro connect openclaw` to add BitRouter to the model picker of one OpenClaw gateway instance. This adapter targets the configuration schema validated with OpenClaw **2026.10.9**. Upgrade older OpenClaw installations before connecting; legacy `agents.list` configurations are refused.

BitRouter is the provider. Automatos is its router model. The selector is **`bitrouter/automatos`**, with **`automatos`** as a native alias. OpenClaw's Control UI currently title-cases the custom provider group as **Bitrouter** and shows **automatos** beneath it. The gateway request retains the exact model ID **`bitrouter/automatos`**.

```sh
export BITROUTER_API_KEY='<gateway credential>'
bro connect openclaw --base-url http://127.0.0.1:4356 --dry-run
bro connect openclaw --base-url http://127.0.0.1:4356 --yes
```

The default model is `bitrouter/automatos`; `--model` changes the exact gateway wire ID. `--display-name` changes the first entry's catalog label. Repeat `--model` to add
selectors to the same provider. Additional native refs are `bitrouter/<wire-ID>`;
only the first entry receives the existing `automatos` alias. The additional
wire ID `automatos` is refused because it conflicts with that alias. Every ID
must pass existing agent model policies; permission for the primary alias does
not authorize other models. Configure agent/session/job selections in OpenClaw
itself. Updates replace the full list when model flags are supplied, and keep
the list otherwise. Saved job bindings are not rewritten on update/removal. This connector accepts only `--protocol chat-completions`; Responses and Messages are not claimed.

## Select the gateway instance

The default instance follows `OPENCLAW_CONFIG_PATH`, then `OPENCLAW_STATE_DIR/openclaw.json`, then `OPENCLAW_HOME`/`HOME` plus `.openclaw/openclaw.json`. `--instance` explicitly declares the state directory, or a config file inside it:

```sh
bro connect openclaw --instance /path/to/openclaw-state --yes
OPENCLAW_STATE_DIR=/path/to/openclaw-state openclaw gateway run
```

A nonstandard config filename additionally requires the same `OPENCLAW_CONFIG_PATH` when starting native OpenClaw. A config-path override alone does **not** relocate native agent state. The connector refuses known config/state directory mismatches so that it cannot miss generated credentials in another state root.

For a native named profile, select its actual state directory, then run OpenClaw with that same native profile or state environment. BitRouter's `--profile` option does not select OpenClaw profiles or agent IDs.

The connection is instance-wide. It covers the configured agents and existing agent catalogs in that state directory, plus configured custom `agentDir` locations. It retains their primary/fallback defaults and existing providers. Existing per-agent model policies are checked; they are not expanded.

## Native CLI, TUI, Web, and Desktop

Start or reload the selected OpenClaw gateway yourself after connecting. In TUI use:

```text
/model bitrouter/automatos
```

A one-turn CLI override is:

```sh
openclaw agent --agent main --model bitrouter/automatos --message 'Hello'
```

In Control UI or a macOS Desktop companion connected to that gateway, open the model picker, expand **Bitrouter**, and select **automatos**. These clients consume their gateway's provider configuration. A remote gateway must be configured on its own host; changing local files does not configure that server.

The CLI, gateway-connected TUI, Web Control UI, and macOS Desktop companion have been exercised with native streaming and a real file-read tool result. This is protocol evidence using a deterministic upstream; it does not qualify a hosted model, every OpenClaw platform, or other API protocols. The displayed context/token limits and zero/default cost values come from OpenClaw defaults, not verified Automatos capabilities or pricing.

## Persistent credentials and ownership

The gateway credential is stored in the private `bitrouter-connect-credential.json` beside the selected config. A dedicated `secrets.providers.bitrouter-connect` file provider and an API-key SecretRef refer to it. This keeps the literal credential out of OpenClaw's config backups and generated `models.json` catalogs. `auth: "api-key"` gives this explicit provider credential precedence over ambient profile/environment defaults. A newly started native process does not need `BITROUTER_API_KEY`.

The connector records the config, its owned secret file, and existing/default agent catalogs in a private BitRouter receipt. Existing secret providers, credentials, and other settings are retained. A pre-existing dedicated secret file/provider, BitRouter provider, or conflicting Automatos alias is refused. Referenced paths must be regular files; symlinked agent directories are refused.

The config retains native JSON5 comments and unrelated formatting during connector edits. Configurations using `$include` are refused because this adapter cannot establish ownership across included sources. Consolidate the chosen provider/agent configuration before connecting.

## Policies and removal

Nonempty `agents.defaults.modelPolicy.allow` and effective per-agent `modelPolicy.allow` must permit the exact native model ref, its configured alias, or a matching segment wildcard. For the default route, the full native ref is `bitrouter/bitrouter/automatos`; the alias makes `bitrouter/automatos` usable without changing the wire ID. The connector names a blocking policy and refuses the operation. It does not append a model or provider wildcard to that policy.

On fresh legacy configurations, adding model metadata could otherwise cause OpenClaw to infer a new allowlist. The connector adds an explicit empty `modelPolicy` only to preserve the existing unrestricted behavior; removal retains it if later unrelated model metadata still needs that protection. Existing legacy model allowlists that exclude BitRouter must be migrated explicitly first.

```sh
bro connect openclaw --status
bro connect openclaw --check
bro connect openclaw --update --display-name automatos --yes
bro connect openclaw --remove --yes
```

Use the same `--instance` for a nondefault connection. `--status` checks owned configuration; `--check` also verifies the exact model in the gateway catalog with the stored credential. Neither command claims inference verification.

Before removal, stop or disconnect the selected running native gateway yourself so it cannot retain an in-memory credential. Removal restores owned defaults that still select the connected model, removes its provider/alias/SecretRef source and private credential, and removes recorded catalog replicas while preserving unrelated later edits. OpenClaw's native alias-key canonicalization is recognized only when the owned metadata still matches. Existing sessions selecting BitRouter must choose another model afterward.

Native config backups can retain stale SecretRefs, but removal revokes their only owned credential file. If another live provider now references that private secret source, an owned setting was edited, or a newly created agent has an unrecorded BitRouter catalog, removal fails closed and names the conflict. Reconcile that explicit reference or replica before retrying.

Sources: [OpenClaw model selection](https://docs.openclaw.ai/concepts/models), [custom providers](https://docs.openclaw.ai/gateway/config-tools/custom-providers), [file SecretRefs](https://docs.openclaw.ai/gateway/secrets/secretref-contract).
