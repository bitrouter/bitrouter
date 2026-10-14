# PR preparation and main integration

The user explicitly requested a PR targeting main on 2026-10-14. This
authorizes committing, pushing and opening this PR. The implementation Goal
remains unfinished, so the PR is a draft; no merge or release is authorized.

Main advanced from 9b9a1762 to 9956a427 with the native BRO ACP server and SDK
ACP migration. The feature commit was rebased onto that main commit. Conflict
resolution preserves Goals for bare bro/code, native BRO ACP for bare acp serve,
and transparent external acp serve with an agent argument. New Goal imports
use orchestrator ACP and SDK config::agent. Skill/plugin metadata and the
development guide preserve both entrypoint contracts. The handoff build
fingerprint now includes the resource/recovery implementation modules.

Historical raw diagnostic/test outputs remain in the evidence directory. For
Git whitespace hygiene, 35 log/text files had trailing whitespace and extra
blank EOF lines normalized. Original byte-for-byte copies were preserved in a
private local temporary directory before normalization. Test outcomes and
source SHA manifests were not changed. The published logs are normalized
copies, and are not claimed to be their original byte streams.

The previous 4,564-pass result applies to the pre-rebase source. Integrated
source verification and hosted CI are recorded separately. Real harness,
provider, typed-backend/input-quality and remaining G/A/F gates are incomplete.
