---
title: Codex subscription
description: Route your ChatGPT plan through BitRouter via the Codex backend — OAuth, no OpenAI API key.
sourceHash: ebdaa5b67dcae3c46e127d28467be57ed8f2624e8058c61fe9bc748695cde34d
---

Have a ChatGPT Plus or Pro plan? Use it as a model source through the **Codex** backend. `bitrouter providers login openai-codex` first reuses an existing local Codex CLI session when one is present, otherwise it can run the same OAuth flow OpenAI's Codex CLI uses, stores the refreshing token, and attaches it to requests on the `openai-codex` provider — so your ChatGPT subscription covers the tokens, with no `OPENAI_API_KEY`.

<Callout type="warn">
**`openai-codex` is not `openai`.** This is a separate provider. It reaches ChatGPT's Codex backend (`chatgpt.com/backend-api/codex`), speaks the **Responses API only**, and is authenticated solely by your subscription's OAuth — it does *not* share endpoints or credentials with the standard `openai` (API-key) provider. For pay-per-token access to the public OpenAI API, use the `openai` provider with a key instead.
</Callout>

## Log in

```bash
bitrouter providers login openai-codex
```

By default, the menu offers **"Import an existing session from the vendor CLI"** first. BitRouter reads the credential the Codex CLI already stored in `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) first, then the macOS Keychain, and adopts it with no fresh browser sign-in. If no local Codex session exists, choose the browser subscription flow; it opens OpenAI's authorize page (PKCE, on a pinned loopback port), then stores the credential under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The token auto-refreshes — log in once.

For scripts and E2E checks, skip the menu and require the existing local Codex session:

```bash
bitrouter providers login openai-codex --import-existing --no-browser
```

This command fails instead of opening a browser when no local Codex credential is available.

To remove the stored credential:

```bash
bitrouter providers logout openai-codex
```

<Callout type="info">
**Already signed in to Codex?** Press enter on the default import option. The file credential wins over Keychain so BitRouter follows the same local Codex home you are already using.
</Callout>

<Callout type="info">
Provider credentials are separate from BitRouter Cloud credentials. `bitrouter cloud login` signs the CLI into your BitRouter account; `bitrouter providers login openai-codex` stores a ChatGPT/Codex subscription credential for the upstream `openai-codex` provider.
</Callout>

### Multiple accounts

Credentials are keyed by `(provider, label)`. Use `--label` to keep more than one account:

```bash
bitrouter providers login openai-codex --label work
```

## Route to it

No `bitrouter.yaml` block is required — `openai-codex` is a registry-backed local-login provider, and the stored credential is found at request time. Address a model your plan exposes by its registry id:

```bash
bitrouter route openai-codex:gpt-5-codex
```

Then [start BitRouter and send a request](/docs/integrations/models#start-bitrouter-and-send-a-request). Use the provider-qualified id `openai-codex:<model>` to pin the request to your subscription; see [Models](/docs/concepts/models) for the exact ids the Codex backend serves.

## Learn more

- [Codex](/docs/integrations/codex) — point the Codex *CLI* at BitRouter (a harness), distinct from using your plan as a model source here.
- [Model fallback](/docs/features/model-fallback) — fail over from your subscription to another source on overload.
