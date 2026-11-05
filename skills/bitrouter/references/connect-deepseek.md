# DeepSeek Harness native model connection

`bro connect deepseek-harness` adds an independent **BitRouter** provider with
the native model label `bitrouter/automatos`. Existing providers and the current
default model stay in place. Select the new model in the Harness picker to use
it; a picker selection also becomes that profile's default for new sessions.

```sh
bro connect deepseek-harness --dry-run
bro connect deepseek-harness --yes
bro connect deepseek-harness --instance /path/to/harness-home \
  --profile web --profile headless --base-url https://gateway.example --yes
bro connect deepseek-harness --instance /path/to/harness-home --status
bro connect deepseek-harness --instance /path/to/harness-home --check
bro connect deepseek-harness --instance /path/to/harness-home \
  --profile web --profile desktop --profile headless --update --yes
bro connect deepseek-harness --instance /path/to/harness-home --remove --yes
```

`--instance` is the actual Harness home, resolved from `DSH_HOME` or `~/.dsh`
when omitted. It is independent of Electron's device preference directory.
`--profile NAME` is repeatable. Explicit profiles must already have a valid
`profiles/NAME/package.json` with a base-backed `dsh.profile.bundles` manifest.
Without profile flags, new connections select initialized, supported profiles
under that home. No uninitialized profile is silently created. Status reports
the exact recorded instance and profile scope; Web success does not establish
Desktop or headless success. An update without profile flags keeps the saved
scope. To add a newly initialized surface, name the full desired profile set
with `--update`.

The built-in `llm-pi-ai` adapter supports `--protocol chat-completions`
(default), `responses`, and `messages`. It writes these as
`openai-completions`, `openai-responses`, and `anthropic-messages`. One provider
uses one protocol. Repeat `--model` to register multiple exact IDs in each selected profile.
`--display-name` applies to the first entry and additional labels use wire IDs.
Updates replace the full ordered list when model flags are provided and preserve
it otherwise. Select models for tasks/profiles in Harness itself; the connector
does not manage workflow bindings.

The gateway request model is exact; `--display-name` changes
only its native label. Text input is declared; image and reasoning capabilities
are not inferred from a model ID.
OpenAI protocols use the gateway root plus `/v1`; Anthropic Messages uses the
gateway root because its SDK adds `/v1/messages` itself. Reverse-proxy prefixes
are retained.

Configuration lives in `profiles/NAME/cordis.patch.yml`. Credentials use the
version-1 `refs` mapping in `.credentials.yaml`, with only
`BITROUTER_CONNECT_API_KEY` referenced from the provider configuration. The
connector enables the built-in `credentials` row because shipped headless
profiles disable the stored credential service. It leaves authorization and
tool permission settings alone. The native Models page keeps stored credentials
write-only. Profile adapter settings apply on the next request; restart a
headless process after changing its startup profile.

Desktop owns `profiles/desktop` and its runtime. Open the native app first to
initialize that profile. The connector edits its configuration directly and
never asks the npm CLI to boot or manage the reserved Desktop profile. A
Desktop-installed command and an npm-installed command do not share executable
or plugin installation state merely because they share a Harness home.

Cordis overrides replace an entry's entire configuration. The connector retains
every provider and field in an existing top-level `llm-pi-ai` override. An
unknown custom bundle without a complete explicit override is refused, since
its inherited provider configuration cannot be safely guessed. Home-level
`llm-pi-ai` and credential overrides are refused because they take precedence;
move those settings into the intended profiles first. Executable YAML inside a
row that needs editing, nested patches for owned entries, unsupported credential
layouts, and redirected credential stores also require explicit configuration.
Unrelated block YAML rows, including their JavaScript tags, remain verbatim.

Removal deletes only the owned provider and credential reference, covering
the complete recorded profile scope: omit `--profile`, or provide all saved
profiles. A partial profile list is refused rather than removing other profiles.
It preserves later providers and unrelated application preferences, and restores a saved
default only while that default still names BitRouter. Edited owned settings
are refused with the receipt retained. Interrupted removal can be retried.
If a later manual home patch makes BitRouter the global default, removal is
refused until that home-level default selects another provider.
Before deleting the credential, removal scans the home patch and every remaining
profile patch; unreadable or symlinked paths fail safely. If another profile or
provider still references the credential, the key stays. That remaining key is
then unmanaged and prevents a new connection from overwriting it: migrate the
remaining references, then remove only `refs.BITROUTER_CONNECT_API_KEY` from
`.credentials.yaml` when it is no longer used, before reconnecting.

`--check` validates the saved connection and gateway model catalog; it does not
run inference or a tool. Verify the native picker, streaming and a complete tool
roundtrip against the intended gateway before treating inference as working.
