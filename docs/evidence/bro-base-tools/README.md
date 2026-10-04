# BRO base tool evidence

This directory separates readable acceptance summaries from complete committed
exports. The [tool acceptance record](../../BRO_BASE_TOOLS_ACCEPTANCE.md) explains
what each run establishes. Historical provider and CI results are attributed to
their original source; none establish every later PR #945 revision.

## Read first

| Cohort | Readable evidence | Raw exports |
| --- | --- | --- |
| Original six-tool runs, 2026-10-01 | [Manifest](manifest.json); `coding`, `readonly`, `natural`, `before-natural` summaries/prompts | [Original archive](archives/original-2026-10-01.tar.gz) |
| Original platform CI | [CI summary](ci.json), including the preceding retry result | Original archive: full `ci.json` and `windows-ci-first.json` |
| Thread/Turn tool integration, 2026-10-04 | [Source manifest](integration.json); [coding](integrated-coding/summary.json) and [read-only](integrated-readonly/summary.json) | [Integration archive](archives/integration-2026-10-04.tar.gz) |

Prompts and scenario `summary.json` files remain at their original paths and are
unchanged. Each archive contains original paths relative to this directory.
[archive-index.json](archive-index.json) records archive and per-member byte sizes
and SHA-256 hashes, plus the immutable source snapshot. Original manifest/CI
bytes are archived alongside the raw outputs; their readable copies are condensed.

Archive exports include execution/model records, event streams, fixture outputs,
cleanup records and CI detail. Checkpoints contain cumulative context by design;
they are execution evidence, not another maintained source of runtime requirements.
The archives are byte-for-byte copies of files committed at `65555b12`. Original
local `/tmp` databases and any uncommitted full streams were never in this tree;
they are not recovered or represented as downloadable artifacts here.

## Verify offline

Run from the repository root; this reads archives without extracting files:

```sh
python3 - <<'PY'
import hashlib
import io
import json
import tarfile
from pathlib import Path

root = Path('docs/evidence/bro-base-tools')
index = json.loads((root / 'archive-index.json').read_text())
for entry in index['archives']:
    data = (root / entry['path']).read_bytes()
    assert len(data) == entry['bytes']
    assert hashlib.sha256(data).hexdigest() == entry['sha256']
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        members = archive.getmembers()
        assert all(member.isfile() for member in members)
        assert sorted(member.name for member in members) == sorted(
            item['path'] for item in entry['files'])
        for item in entry['files']:
            stream = archive.extractfile(item['path'])
            assert stream is not None
            content = stream.read()
            assert len(content) == item['bytes']
            assert hashlib.sha256(content).hexdigest() == item['sha256']
    print(entry['path'], 'verified', len(entry['files']), 'files')
PY
```

Inspect a particular original record without unpacking the whole archive:

```sh
tar -xOf docs/evidence/bro-base-tools/archives/integration-2026-10-04.tar.gz \
  integrated-coding/execution-evidence.json
```

Archive construction uses sorted USTAR members, mode 0644, zero timestamps and
owner IDs, followed by gzip with mtime 0. Member hashes, rather than compressor
version-dependent archive bytes, establish the preserved content. Every member
can also be compared with `git show <source_snapshot>:docs/evidence/bro-base-tools/<member>`.
The index fixes archive bytes for the checked-in copies.

## Reproduce and add evidence

[The existing probe](../../../scripts/probe-bro-base-tools.py) generates a fresh
workspace/database, runs real inference, verifies the code or read-only file
hashes, records results and stops its daemon. The [reproduction instructions](../../BRO_BASE_TOOLS_ACCEPTANCE.md#reproduce)
require a configured gateway and a fresh short output directory. Paid provider
runs are separate from offline archive verification and deterministic Rust tests.

For a new accepted run, keep its source/environment, prompt, binary identity,
result summary and unresolved failures readable. Archive committed raw exports
with per-file hashes; do not append repeated full successful streams to Markdown.
Keep failures, comparisons and older source results identifiable. Never replace
an archived result with a rerun or claim a transient path is durable storage.
