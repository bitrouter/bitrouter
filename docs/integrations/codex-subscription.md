---
title: Codex subscription
description: Route your ChatGPT plan through BitRouter via the Codex backend — OAuth, no OpenAI API key.
sourceHash: ebdaa5b67dcae3c46e127d28467be57ed8f2624e8058c61fe9bc748695cde34d
---

Have a ChatGPT Plus or Pro plan? Use it as a model source through the **Codex** backend. `bitrouter providers login openai-codex` runs the same OAuth flow OpenAI's Codex CLI uses, stores the refreshing token, and attaches it to requests on the `openai-codex` provider — so your ChatGPT subscription covers the tokens, with no `OPENAI_API_KEY`.

<Callout type="warn">
**`openai-codex` is not `openai`.** This is a separate provider. It reaches ChatGPT's Codex backend (`chatgpt.com/backend-api/codex`), speaks the **Responses API only**, and is authenticated solely by your subscription's OAuth — it does *not* share endpoints or credentials with the standard `openai` (API-key) provider. For pay-per-token access to the public OpenAI API, use the `openai` provider with a key instead.
</Callout>

## Log in

```bash
bitrouter providers login openai-codex
```

This opens OpenAI's authorize page in your browser (PKCE, on a pinned loopback port), and on approval stores the credential under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The token auto-refreshes — log in once. To remove it:

```bash
bitrouter providers logout openai-codex
```

<Callout type="info">
**Already signed in to Codex?** The login menu offers **"Import an existing session from the vendor CLI"** — BitRouter adopts the credential the Codex CLI already stored (macOS Keychain or `$CODEX_HOME/auth.json`), with no fresh browser sign-in. The imported token refreshes like any other.
</Callout>

### Multiple accounts

Credentials are keyed by `(provider, label)`. Use `--label` to keep more than one account:

```bash
bitrouter providers login openai-codex --label work
```

## Route to it

No `bitrouter.yaml` block is required — `openai-codex` is built in, and the stored credential is found at request time. Address a model your plan exposes by its registry id:

```bash
bitrouter route openai-codex:gpt-5-codex
```

Then [start BitRouter and send a request](/docs/integrations/models#start-bitrouter-and-send-a-request). Use the provider-qualified id `openai-codex:<model>` to pin the request to your subscription; see [Models](/docs/concepts/models) for the exact ids the Codex backend serves.

## Learn more

- [Codex](/docs/integrations/codex) — point the Codex *CLI* at BitRouter (a harness), distinct from using your plan as a model source here.
- [Model fallback](/docs/features/model-fallback) — fail over from your subscription to another source on overload.
