# Connect Hermes Agent

`bro connect hermes` adds a private `providers.bitrouter` entry to the selected
Hermes `config.yaml`. It keeps existing providers, the main model, auxiliary
models and other settings. It does not install or launch Hermes.

```sh
bro connect hermes
bro connect hermes --instance /path/to/hermes-home
bro connect hermes --instance /path/to/hermes-root --profile work
bro connect hermes --status
bro connect hermes --check
bro connect hermes --update
bro connect hermes --remove
```

`--instance` means an actual Hermes home. Otherwise `HERMES_HOME` selects that
home. Without either, discovery follows the platform default and Hermes'
`active_profile` file. The default is `~/.hermes` on macOS/Linux and
`%LOCALAPPDATA%/hermes` on Windows. Only the selected/current home is changed;
there is no automatic fan-out to every profile. Repeat `--profile` to select
initialized named profiles explicitly. Names use Hermes' lowercase canonical
form; `default` means the root profile. The connection receipt records the
selected scope, and update/removal must cover that scope.

For an existing gateway, `--base-url` is the gateway root; use
`BITROUTER_API_KEY` for scripted setup. The provider receives `<root>/v1`,
`transport: chat_completions`, `discover_models: false`, and all exact
connected models. Chat Completions is the supported connector protocol. Responses
and Messages are rejected until separately verified with native Hermes.

In Hermes, reopen the model picker or restart the selected profile/backend, then
choose **BitRouter** and the connected model. The default wire ID is
`bitrouter/automatos`. Repeat `--model` to register additional IDs under the same
provider; its `default_model` is the first ID. `--update` replaces the list when
model flags are supplied and preserves it otherwise. Select chat, cron, and
auxiliary routes in Hermes itself; connection setup leaves those settings alone. Hermes' picker uses exact ID strings and has no separate
model display label; `--display-name automatos` therefore cannot rename the row.
Its chat chip can shorten the ID to `automatos` natively. The connector never
changes the gateway model to obtain a shorter label and does not produce a
`bitrouter/bitrouter/automatos` selector.

The same provider catalog serves the native CLI, TUI, official Desktop and Web
Dashboard when they use the same backend/profile. Desktop can connect to another
machine: configure that backend's Hermes home to affect its catalog. A local
connector does not change a remote backend. Restart a long-running backend when
its in-memory catalog is stale. The Web Dashboard's Chat tab embeds Hermes'
terminal interface; it is distinct from the Desktop workspace served in a
browser.

The API key is persisted in the private `config.yaml`, so the connection does
not depend on the shell that launches Hermes. Hermes expands `${...}` in YAML;
the connector rejects tokens containing that form rather than silently changing
them. It does not use Hermes' global API-key environment variables or replace an
existing BitRouter provider. Native user-created backups or corrupt-config
recovery snapshots are outside the connector receipt; removal does not claim to
erase historical copies.

Removal deletes only the owned provider. If any connected BitRouter model is
still the main selection, it restores the previous main route while retaining
unrelated native preferences. A later choice of another model/provider remains.
Hermes Desktop stores a manual composer selection in its own local preferences.
A removed provider disappears from the picker, but a pinned Automatos label can
remain; reopen the picker and choose another available model. The connector
does not edit Electron storage or historical sessions.

Edits to the owned provider cause removal to fail closed for review. Native
auxiliary overrides and other providers remain unchanged. No model context,
price, vision or reasoning capability is asserted by the connector; Hermes may
apply its own defaults to unknown models.

Native schema and behavior references:

- [Configuring Models](https://hermes-agent.nousresearch.com/docs/user-guide/configuring-models)
- [Profiles](https://hermes-agent.nousresearch.com/docs/user-guide/profiles)
- [Hermes Desktop](https://hermes-agent.nousresearch.com/docs/user-guide/desktop)
- [Provider normalization](https://github.com/NousResearch/hermes-agent/blob/main/hermes_cli/config.py)
- [Model picker and switching](https://github.com/NousResearch/hermes-agent/blob/main/hermes_cli/model_switch.py)
