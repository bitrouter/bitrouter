# Engineering guides

These guides describe the current implementation and should change with it.
They do not create independently versioned format specifications. Follow the
[documentation and reference policy](../DEVELOPMENT.md#documentation-policy).

- [Architecture and source layout](architecture.md)
- [Agent runtime ownership and ACP controller boundaries](agent-runtime.md)
- [Project instructions, MCP and skills](harness-resources.md)
- [Native tools](native-tools.md) and [ACP authentication](acp-authentication.md)
- [Recording, checkpoints and learning](evidence-learning.md)
- [Gateway, MCP adaptation and protocol fidelity](gateway.md)
- [Catalog generation and agent conformance](catalogs.md)
- [Observation and export](observability.md)
- [Configuration-state comparison](configuration-state.md)
- [CLI and workflow skills](cli-and-skills.md)
- [Compiled extensions, host integration and request checks](extensions.md)
- [Tenancy OAuth and input-counting boundaries](tenancy-auxiliary.md)
- [Daemon upgrades](local-daemon-upgrade.md),
  [remote administration](remote-administration.md),
  and [ACP launch](spawn.md)

Formats shared with clients and retained files are maintained once in
[specifications](../specs/README.md). Commands remain in the [CLI guide](../CLI.md).
