# BRO workspace resources and skills

BRO selects a local workspace for each thread. The workspace grant covers
native tools within the selected directory, subject to the thread's tool mode
and approval checks. BRO owns tool effects and their checkpoint history.
The gateway handles model inference and operator-configured server tools.
See [agent runtime ownership](agent-runtime.md) and [native tools](native-tools.md).

## Local Agent Skills

`bro skills list` discovers project skills. Use `--global` for user-level skill
directories or `--root PATH` to select a directory explicitly. Choose one scope
at a time. Resolve same-name matches with a narrower root or exact directory
identity. `show`, `read`, `write` and `init` work on the selected source files.

Pass repeated `--skill NAME` options to `bro task run` or `bro thread create`
to bind project skills to the thread. BRO validates the selected instructions,
records their digests and supplies the frozen text to the model when preparing
a request. Skill content cannot grant permissions or execute tools. Use
`bro skills explain --thread ID --turn ID` to see the revisions used by a
recorded turn. See [local skill commands](../CLI.md#local-skills).

## ACP and gateway MCP

BRO owns native ACP sessions and supervises selected external ACP adapters.
An external adapter's own project resources and permissions follow that
adapter's configuration and its local ACP controller. Server-side MCP tools
and resources follow the gateway operator's configuration and caller access
policy. See [ACP launch](spawn.md) and [gateway MCP](gateway.md#mcp-adaptation).
