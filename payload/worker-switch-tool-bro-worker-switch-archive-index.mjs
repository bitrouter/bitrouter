import {readFileSync,writeFileSync,copyFileSync,existsSync} from 'node:fs';
import {createHash} from 'node:crypto';
import path from 'node:path';
const root='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const sha=file=>createHash('sha256').update(readFileSync(path.join(root,file))).digest('hex');
const trials=Array.from({length:8},(_,i)=>{
  const file=`worker-switch-focused${i+1}.json`;
  const result=JSON.parse(readFileSync(path.join(root,file),'utf8'));
  return {file,sha256:sha(file),...result};
});
const duplicates=[];
for(const tail of ['check1.log','check2.log','check3.log','check4.log','test-build1.log','test-build2.log']){
  const file=`worker-switch-${tail}`, original=`managed-acp-${tail}`;
  duplicates.push({file,original,sha256:sha(file),sameBytes:sha(file)===sha(original),applicability:'Earlier managed-ACP foundation diagnostics accidentally copied by runner1; not worker-switch verification.'});
}
writeFileSync(path.join(root,'worker-switch-trial-index.json'),JSON.stringify({capturedAt:new Date().toISOString(),trials,duplicates},null,2)+'\n');
const initial='/tmp/bro-worker-switch-check1.log';
if(existsSync(initial)&&!existsSync(path.join(root,'worker-switch-initial-check1.log')))copyFileSync(initial,path.join(root,'worker-switch-initial-check1.log'));
const index=`# Worker replacement and requested-worker provenance evidence

Local PR #981 checkpoint based on 9d0460696b39b12b8585334b401224fbafde6494.
This is partial S3 work. Complete refactor, live-provider/harness qualification,
hosted CI and final publication remain separate gates.

Each source manifest records all repository file hashes and its parent HEAD.
The matching binary patch and source-files tar retain the dirty source,
including untracked Rust modules. Runners record exact commands and unchanged
normal stack/time bounds. Logs include compile failures and failed assertions.

| Source | Observed result |
| --- | --- |
| 1 | Compile rejected: fixture helper attempted to serialize Config. No tests ran. |
| 2 | Compile rejected: new fixture input arguments were reversed. No tests ran. |
| 3 | 113 passed, three failed: legacy archive key, domain revision, hot archive length assertion. |
| 4 | 115 passed, one domain revision failure; actual ACP-to-ACP and ACP-to-native result transfer passed. |
| 5 | 115 passed, same domain revision failure; no source changes during run. |
| 6 | Isolated domain test failed at stopped-effect reconciliation; phase diagnostics retained. |
| 7 | Correct control revision exposed missing replacement workspace registration. |
| 8 | Corrected domain test passed; all source hashes stable. |
| 9 | Full required checks: consult worker-switch-gates9.json and source-audit9.json. Running/failed is not a passing gate. |

The domain fixture now uses its actual saved control source and registers the
replacement workspace before testing resource admission. Runtime version,
effect, workspace and retry guards were not weakened. Missing/foreign transfer
material is rejected, while preparation-only retries retain original results.

The actual ACP replacement peer checks inherited LEFT bytes before session
startup. The native replacement reads inherited LEFT with the native read tool
before producing a result. Changed old-worker provenance cannot relabel these
original run/session bindings. Unknown old tool effects block replacement;
a worker edit never resumes an explicit pause. Independent right work is retained.
These are deterministic Rust/HTTP/protocol fixtures, not real-model quality proof.

## Artifact applicability and retained fixtures

worker-switch-trial-index.json binds trial metadata and checksums. Runner1
accidentally copied earlier managed-ACP check1..4 and test-build1/2 logs under
worker-switch names. They are retained and explicitly mapped to their byte-
identical managed-ACP originals; they do not verify this worker-switch batch.
worker-switch-initial-check1.log is the actual early worker-switch check, which
predates later handoff implementation and is not final-source qualification.

Necessary Cargo-built fixture source and regression tests remain in the feature
PR. Their exact intermediate bytes are also in the source tars/patches; the
Git checkpoint/bundle identifies the unchanged fixture package and dependencies.
Normal tests clean their temporary runtime directories and in-memory databases;
no claim is made that those transient runtime states were captured for these
trials. Original failed logs and all source snapshots remain available.
Earlier managed-ACP stack-abort crash fixtures have their own independent
archive and checksum audit; this batch does not rewrite or relabel them.

Raw evidence currently resides in this local archive. Preserve it in a separate,
sanitized, commit-pinned archive for final publication. The functional PR should
retain final acceptance/reproduction material, not the raw campaign snapshots.
`;
writeFileSync(path.join(root,'WORKER_SWITCH_INDEX.md'),index);
console.log(JSON.stringify({index:'WORKER_SWITCH_INDEX.md',trials:trials.length,duplicates:duplicates.length,duplicatesVerified:duplicates.every(x=>x.sameBytes)}));
