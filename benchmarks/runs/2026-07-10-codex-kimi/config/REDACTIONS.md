# Redactions in the published config

The files under `config/` are the frozen experiment definition for this run —
the Harbor and BitRouter YAML plus the comparable-task set — published for
transparency and reproducibility. A small set of values has been replaced before
publication. Nothing that affects the **routing behavior or the results** was
changed — the model ids, the policy table (tiers, thresholds, cooldowns, explore
schedule), concurrency, timeouts, and task set are all intact. The substitutions
below are either secret hygiene or account/infrastructure identifiers that are
meaningless outside the original environment.

We list them here so the config can be trusted as faithful: where a value was
removed, it reads `REDACTED` or an obvious placeholder rather than a plausible
but false value. We did not swap any value for one that would misrepresent how
the run executed.

| Field / pattern | Published value | What it was |
| --- | --- | --- |
| `providers.openai-codex.api_base` | `REDACTED` | The strong-route provider's access endpoint. Omitted; we are not publishing the strong route's provider access details. It does not affect the score or the token-derived cost. |
| `providers.openai-codex.class` | `REDACTED` | The strong-route provider's access class, for the same reason. |
| Internal host IP | `10.0.0.10` | The benchmark node's private VPC address. |
| `key_name` | `benchmark-key` | The EC2 SSH key-pair name. |
| `ssh_key_path` | `.../benchmark-key.pem` | The SSH private-key path. |
| `ami_id` / `subnet_id` / security-group id | `ami-x…` / `subnet-x…` / `sg-x…` | Account-scoped AWS resource identifiers. |

## Notes on cost

Because the strong-route provider access details are omitted, the cost figures in
`report.md` / `results.json` are reported as **normalized, API-equivalent imputed
cost** — computed from measured token usage at the frozen list per-token prices,
not from a billing statement. This is the reproducible, like-for-like basis for
comparing routes: anyone can recompute it from the token counts in the raw data
at published prices. See [`../data/`](../data/) — specifically each group's
`cloud-usage.jsonl` — for the underlying per-request token usage.

## Not included

Real secrets were never in these files. At runtime the orchestration loaded API
credentials from an untracked `secrets/daemon.env` and from the
`${BITROUTER_API_KEY}` environment variable; neither is committed. The
orchestration scripts themselves are not published here — they are infra-specific
and non-runnable outside the original environment; the recovery/acceptance
protocol they implemented is described in `report.md`.
