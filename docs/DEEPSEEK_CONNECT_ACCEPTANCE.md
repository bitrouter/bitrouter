# DeepSeek Harness connector acceptance

Date: 2026-10-31. All configuration, credentials, sessions, and file effects
belong to isolated acceptance directories. No real user Harness profile was
changed. The upstream is a deterministic HTTP fixture, not a live model.

## Native configuration contract

The connector uses the shipped `llm-pi-ai` custom API provider; no executable
provider plugin is needed. It adds a distinct `bitrouter` route whose model ID
and native label default to `bitrouter/automatos`. Existing providers, default
selection, and tool permission policy are preserved when connecting.

The official source inspected was
`deepseek-ai/deepseek-harness@ce7073d51254c26cd63b22a60835b8f948deb447`
(the repository's `master` branch at inspection). Relevant owning sources:

- [Model-provider guide](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/docs/user/guide/providers.md): custom APIs, three protocol names, profile patch precedence, write-only credential UI.
- [CLI reference](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/apps/cli/README.md): initialized profile manifests, layer ordering, reserved Desktop profile.
- [Desktop paths](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/apps/desktop/src/paths.ts): `DSH_HOME/profiles/desktop`, independent of Electron preferences.
- [Base composition](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/packages/bundle/base/cordis.patch.yml): dormant built-in adapter and default credential store.
- [Headless composition](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/packages/bundle/headless/cordis.patch.yml): stored credential service is disabled; the connector must enable that row for persistent authentication.
- [Credential store](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/packages/credentials/credentials-local/src/index.ts): version-1 document, `refs` mapping, owner-only files and dynamic reads.
- [Adapter schema](https://github.com/deepseek-ai/deepseek-harness/blob/ce7073d51254c26cd63b22a60835b8f948deb447/packages/llm/llm-pi-ai/src/config.ts): model `name`, provider `displayName`, API and credential-reference fields.

Home-level adapter/credential overrides, nested owned-entry patches, unsafe
credential redirection, and custom bundles with unknown inherited provider
configuration are refused instead of replacing unrelated settings. One receipt
owns one Harness home and its recorded profiles. Explicit selection does not
create a new Harness instance or silently initialize another surface.

## Runtime matrix

| Surface | Native release | Evidence | Scope |
| --- | --- | --- | --- |
| Web | npm `@deepseek-ai/dsh` 0.2.1-rc.5 | Real model picker shows group `BitRouter`, item `bitrouter/automatos`; selecting it and submitting a prompt completes through BitRouter | Native Web accepted against fixture upstream |
| Headless CLI | Same npm release | Streamed two-step turn; native `write` creates the expected file; subsequent upstream request contains the actual tool result; process exits 0 with final text | Native headless accepted against fixture upstream |
| Desktop | Installed macOS 0.2.1-rc.2 | Native picker retains DeepSeek models beside BitRouter; streamed prompt and actual `write` tool complete through BitRouter | Accepted in the isolated launch harness described below |

The npm release's actual headless help accepts `--json` and `--session-id`;
newer flags found on `master` must not be assumed to exist in that release.
The successful run used no permission-changing CLI flags.

The runtime lives under `/workspace/acceptance/deepseek` on the shared acceptance
VM. The native app talks to BitRouter at `127.0.0.1:50421`; BitRouter routes
`bitrouter/automatos` to the fixture's `fixture-target` on port 50420. The Web
listener is 50422. Credentials are dummy values confined to these fixtures.
The file written by Harness is `workspace/created-by-dsh.txt` with contents
`DeepSeek Harness native write reached BitRouter.` plus a newline.

Observed headless completion is
`DeepSeek Harness file tool round trip accepted.` The upstream capture records
streaming, exact target model, tool declarations, authentication success, and a
real `Created file` tool-result envelope. Native Web completion is
`DeepSeek Harness connected through BitRouter.` Native menu and completed-chat
screenshots are retained as acceptance artifacts.

The installed macOS Desktop carrier was paused with its Node inspector before
application startup. The acceptance launcher redirected Electron `userData`,
`sessionData`, logs and crash files into scratch directories, and suppressed
only OS `dsh://` handler registration. It then resumed the unchanged packaged
application and its original resources. `DSH_HOME` named a separate scratch
home. This establishes native Desktop model, stream and tool behavior under an
isolated launch harness; it does not qualify unmodified packaged startup.
Authentication, permission checks and tool execution were unchanged. A dummy
DeepSeek environment key provided ordinary first-run onboarding; every accepted
inference selected BitRouter and used the local fixture instead.

The Desktop file-tool session selected the disposable scratch workspace through
the native directory chooser and kept **Workspace Write**. The UI reported
`Edited created-by-dsh.txt`, one turn, two steps, and the expected final text.
The actual file and subsequent upstream tool-result envelope were checked.
The earlier text-only session referenced a pre-existing empty default workspace
but wrote no files there. A second isolated launch retained the checked BitRouter picker entry and the
completed tool session. The app quit normally. Module restoration removed the
BitRouter route, selected default and owned credential reference while retaining
Desktop sessions and its application-generated browser-session credential.

At the initial native runtime pass, the exact connector module generated the
profile changes through the small adapter acceptance binary and requests ran
through the baseline BitRouter gateway. The rebuilt `bro` subsequently passed
actual connect, offline status, check with no credential in the launching
environment, repeat setup, and protocol updates. A fresh native headless tool
turn also passed against that actual connector-generated configuration.

Native headless tool turns passed with Chat Completions, Responses, and
Anthropic Messages. The Messages run exposed an endpoint distinction: its SDK
adds `/v1/messages`, so its configured `baseURL` must be the gateway root;
OpenAI protocols use the root plus `/v1`. A focused regression covers the root
and reverse-proxy prefix. The final rebuilt `bro --update` generated that corrected Messages
configuration. Both headless and the real Web UI then passed streamed two-step
turns with native file writes through BitRouter.

Final actual CLI lifecycle evidence includes `configured`, `catalog_verified`
with no launching credential environment variable, reordered/duplicate profile
repeat setup, `updated`, `restored`, and `not_configured`. A later home-level
BitRouter default correctly refused removal with its receipt retained. After
removing that fixture override, removal restored both original `[]` patches,
removed the owned key, and kept the session directory. Connection/check reports
continued to state `inference_verified: false`; native inference acceptance is
the separate controlled fixture evidence above.

## Automated lifecycle coverage

`apps/bitrouter/tests/connect_deepseek.rs` exercises detected Web/Desktop/headless
profiles, explicit profiles and instance selection, dry run, Responses protocol
serialization, provider/default preservation, picker-generated default changes,
later application preferences, copied credential references in a newly created
profile, unknown/missing profiles, home precedence, edited owned settings,
interrupted restore, and unrelated JavaScript YAML rows.

The lightweight actual-module harness passed four module tests, including raw
Unicode/JavaScript row preservation, conditional default restoration, the
Messages endpoint convention, and refusal to remove a provider while a later
home-level default still selects it. The compiled cloud CLI passed all five DeepSeek integration tests, within the
shared 19-test CLI run. Root verification owns the separate full-suite and
all-features clippy results.

Limits: fixture responses establish transport, native stream handling, and
tool execution. They do not establish a real model's behavior, image support,
reasoning support, real model quality, or unmodified packaged Desktop startup.
The connector's catalog check continues to report
`inference_verified: false`.

## Acceptance artifacts

The local task outputs retain `deepseek-cli-lifecycle.json`,
`deepseek-native-upstream.json`, the three
`deepseek-headless-*-stream.ndjson` files, Web picker/completion images,
Desktop picker/completion/tool/restart images,
`deepseek-desktop-isolation.json`, `deepseek-desktop-upstream.json`, and
`deepseek-desktop-restore.json`. They contain fixture data and sanitized
connection reports; credentials and browser-session secret records are omitted.
