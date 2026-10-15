# PR 981 Goal workflow evidence archive

This archive retains all 890 committed Goal workflow evidence files and the
original implementation ledger from BitRouter PR #981, source commit
`4772d9e09deb9c69f539e97d2b24dda9ca770e0c`. Files retain their original paths and bytes.
It is an archival record of successful, failed and superseded experiments;
presence here does not turn a historical failure into a passing acceptance.

- Original PR: https://github.com/bitrouter/bitrouter/pull/981
- Original source: https://github.com/bitrouter/bitrouter/tree/4772d9e09deb9c69f539e97d2b24dda9ca770e0c
- Archive reference: `codex/pr981-evidence-archive-20261015`
- Payload: 891 files, 64181265 bytes
- Checksum list SHA-256: `b9ee7aee0a4b375f0239f8f692f6c932b9e7f23bf31fb2a0d6c20b993db95c36`

The functional PR keeps the final acceptance summaries and required input
fixtures. Its `docs/evidence/bro-goal-workflow/archive.json` pins this archive
by immutable commit and checksum. Follow that commit rather than relying on a
movable branch name. No runtime or test code is included in this archive tree;
use the original source commit for historical code and configuration.

## Retrieve and verify

From a BitRouter checkout, read `archive.json` to obtain the pinned commit.
Fetch the archive reference, confirm the fetched commit matches that pin,
and extract into a fresh directory outside the functional checkout:

```sh
git fetch origin codex/pr981-evidence-archive-20261015
git rev-parse FETCH_HEAD
archive_dir="$(mktemp -d /tmp/bro-pr981-evidence.XXXXXX)"
git archive FETCH_HEAD | tar -x -C "$archive_dir"
cd "$archive_dir"
shasum -a 256 SHA256SUMS
shasum -a 256 -c SHA256SUMS
```

Compare the checksum-list digest with the value above and the functional PR's
pin. `PROVENANCE.json` records the source commit, scope, file count and bytes.
Each payload file is verified independently by `SHA256SUMS`.

Historical Markdown and JSON retain their original contents, including paths
to private captures that were never published. Those captures remain private;
this archive does not claim to contain them. Relative historical links to code
or specifications outside the payload can be read at the original source
commit linked above. Original source manifests remain historical bindings,
not evidence that a newer checkout was rebuilt or re-evaluated.

This independent archive commit has no parent and is not a merge target for
the functional branch. Retain its branch reference to keep the payload
reachable independently of the feature PR's eventual merge strategy.
