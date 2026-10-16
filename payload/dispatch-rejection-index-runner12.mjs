import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,mkdtempSync,readdirSync,statSync,existsSync,copyFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import path from 'node:path';
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter';
const archive='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const prefix='dispatch-rejection';
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const read=name=>JSON.parse(readFileSync(path.join(archive,name)));
const final=read(`${prefix}-final11-source.json`), results=read(`${prefix}-final11-results.json`);
if(!results.passed||results.changed.length)throw Error('final assertions/checks failed or source drifted');
const observedLeaks=results.runs.flatMap(run=>run.leaks.map(line=>({run:run.name,line})));
for(const name of [`${prefix}-applicability12.json`,`${prefix}-manifest12.json`,'DISPATCH_REJECTION_INDEX.md'])if(existsSync(path.join(archive,name)))throw Error('archive index already exists');
const audits=[];
for(const id of ['dispatch-rejection-red4','dispatch-rejection-green6','dispatch-rejection-completion-red8','dispatch-rejection-completion-red8b','dispatch-rejection-green9','dispatch-rejection-input-red10','dispatch-rejection-final11']){
  const source=read(`${id}-source.json`), directory=mkdtempSync(`/tmp/${id}-audit-`);
  execFileSync('tar',['-xzf',`${archive}/${id}-files.tar.gz`,'-C',directory]);
  const walk=d=>readdirSync(d,{withFileTypes:true}).flatMap(entry=>entry.isDirectory()?walk(path.join(d,entry.name)):[path.join(d,entry.name)]);
  const names=walk(directory).map(file=>path.relative(directory,file));
  if(names.some(name=>sha(readFileSync(path.join(directory,name)))!==source.files[name]))throw Error(`source tar does not match ${id}`);
  audits.push({source:`${id}-source.json`,directory,verifiedFiles:names.length,passed:true});
}
const names=[...new Set(execFileSync('git',['ls-files','-c','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))].sort();
const files=Object.fromEntries(names.filter(name=>existsSync(path.join(cwd,name))).map(name=>[name,sha(readFileSync(path.join(cwd,name)))]));
const changed=[...new Set([...Object.keys(final.files),...Object.keys(files)])].filter(name=>final.files[name]!==files[name]);
const documentation=new Set(['docs/BRO_ORCHESTRATOR_REFACTOR_IMPLEMENTATION.md','docs/DEVELOPMENT.md','docs/CLI.md','skills/bitrouter/SKILL.md','.claude-plugin/plugin.json','.codex-plugin/plugin.json','.agents/plugins/marketplace.json']);
if(changed.some(name=>!documentation.has(name)))throw Error('unexpected executable or fixture drift after final gates');
for(const name of ['.claude-plugin/plugin.json','.codex-plugin/plugin.json','.agents/plugins/marketplace.json']){
  const original=execFileSync('git',['show',`${final.head}:${name}`],{cwd,encoding:'utf8'}), current=readFileSync(path.join(cwd,name),'utf8');
  if(current!==original.replaceAll('Goal IPC v6','Goal IPC v7'))throw Error('plugin manifest changed beyond its protocol description');
  JSON.parse(current);
}
const baseline=read(`${prefix}-green6-source.json`);
const fullBaselineCodeDelta=[...new Set([...Object.keys(baseline.files),...Object.keys(final.files)])].filter(name=>baseline.files[name]!==final.files[name]);
const probe=read(`${prefix}-process-probe7-source.json`);
const probeMatchesBaseline=Object.keys(baseline.files).length===Object.keys(probe.files).length&&Object.keys(baseline.files).every(name=>baseline.files[name]===probe.files[name]);
if(!probeMatchesBaseline)throw Error('process probe drifted from the full-run source');
writeFileSync(`${archive}/${prefix}-final12.patch`,execFileSync('git',['diff','--binary'],{cwd}),{flag:'wx'});
const dirty=execFileSync('git',['ls-files','-m','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean);
execFileSync('tar',['-czf',`${archive}/${prefix}-final12-files.tar.gz`,'--',...dirty],{cwd});
const finalDirectory=mkdtempSync('/tmp/dispatch-rejection-final12-audit-');
execFileSync('tar',['-xzf',`${archive}/${prefix}-final12-files.tar.gz`,'-C',finalDirectory]);
for(const name of dirty)if(sha(readFileSync(path.join(finalDirectory,name)))!==files[name])throw Error('final archive source mismatch');
audits.push({source:`${prefix}-applicability12.json`,directory:finalDirectory,verifiedFiles:dirty.length,passed:true});
writeFileSync(`${archive}/${prefix}-applicability12.json`,JSON.stringify({capturedAt:new Date().toISOString(),parent:final.head,finalGates:results.source,observedLeaks,fullyCleanRuntimeQualification:observedLeaks.length===0,audits,fullBaselineCodeDelta,probeMatchesBaseline,changedSinceFinalGates:changed,onlyDocumentationAndProtocolDescriptionsChanged:true,files},null,2)+'\n',{flag:'wx'});
copyFileSync('/tmp/bro-dispatch-index12.mjs',`${archive}/${prefix}-index-runner12.mjs`);
writeFileSync(`${archive}/DISPATCH_REJECTION_INDEX.md`,`# Original worker admission and completion checkpoint

Parent ${final.head}. The overall refactor remains active and PR #981 is unpushed. The later local checkpoint JSON and Git bundle bind the committed source to this index; they do not publish or qualify hosted CI.

A saved dispatch intent is retained. A managed attempt with an uncertain missing link first revokes its source. Shared admission rechecks authorization under the existing admission lock; original Thread/Turn key reconciliation uses that same lock. Existing accepted Turns are linked and remain subject to stop/effect evidence. Only absence after revocation permits a durable not_admitted verdict and a bounded new preparation. Legacy supervisor starts keep their reconciliation path. Selected-MCP and saved-input integrity refusals are covered; corrupted input is never executed or silently repaired. No new store, graph engine, admission keys for a replay, permission grants or cross-store transaction were introduced.

Completion reconciles prior not-admitted attempts without erasing their intent. Unknown attempts still block completion. Goal IPC is 7, application writer fence is 000032; runtime format 9, native 17 and HTTP 2 are unchanged. The old migration-list startup refusal is tested; already running old writers still require the final quiescent cutover.

## Evidence and limits

- Red4 reproduces the permanently blocked no-Turn attempt. Compile5 preserves a Rust moved-value compile diagnostic and tracked patch only; it is not a complete source snapshot or passing test.
- Green6: 5 shared admission cases, 11 interruption/compatibility cases, strict workspace Clippy/fmt, and 4,744 full-workspace tests passed with 24 skipped and 8 slow. The 11-case selection had one output-handle LEAK; the later full run had none. This full run predates the completion rule and saved-input handler fixes; fullBaselineCodeDelta records its exact difference from final11.
- Process probe7 repeats the affected native cold-admission case 20 times on exactly green6 source. All 20 pass, 23 owned processes are observed and none remain observed. Sampling is diagnostic, may miss short children, changes no limits/timeouts and sends no signals. It does not explain or fix the earlier LEAK.
- Completion red8/8b locate the old no-admission history blocking current verified handoffs. Green9 passes 94 Goal domain cases and 81 Goal runtime cases, strict checks and 6 workspace doctests with 1 ignored.
- Input red10 fails because an integrity refusal bypasses persisted uncertainty. Final11 covers that boundary: 94 Goal domain and 81 Goal runtime assertions, 5 shared admission cases, 1 writer compatibility case, strict workspace Clippy/fmt and 6 workspace doctests with 1 ignored all pass without source drift. Two output-handle LEAK reports remain: model_gate_reports_owner_transfer_instead_of_an_opaque_unavailability in the Goal runtime run, and native_worker_wait_never_treats_a_foreign_os_workspace_lock_as_known_contention in the shared admission run. The first case uses the database and model-control gate without launching an executor; the causes remain unconfirmed. This is not a fully clean runtime qualification. No test, leak timeout, assertion or product limit was relaxed to remove them. Exact reports remain in final11-results.json and applicability12.json.
- All seven gate-source tars and the final documentation/source tar were independently extracted and every captured file hash verified. After final11 only the declared documentation and three protocol-description strings changed. Original failures and the LEAK report remain unchanged.
- Manual fixture temporary directories and in-memory databases were not captured in these runs. Cargo-built Rust protocol/process reproduction sources are retained in Git and the verified source tars. These archives are not process images, private ACP state or a machine-wide effects snapshot.

Cold native/confirmed ACP continuation, budget-wait projection, live provider/harness qualification and the final complete-refactor source/CI/archive publication remain separate gates. Nothing here marks S0-S6 or A01-A22 globally complete. See dispatch-rejection-manifest12.json for raw log/source hashes and dispatch-rejection-checkpoint12.json for the local committed bundle. Nextest output-handle semantics: https://www.nexte.st/docs/features/leaky-tests/ .
`,{flag:'wx'});
const archived=readdirSync(archive).filter(name=>name.startsWith(`${prefix}-`)||name==='DISPATCH_REJECTION_INDEX.md').filter(name=>name!==`${prefix}-manifest12.json`).sort();
writeFileSync(`${archive}/${prefix}-manifest12.json`,JSON.stringify({parent:final.head,files:archived.map(name=>({file:name,bytes:statSync(`${archive}/${name}`).size,sha256:sha(readFileSync(`${archive}/${name}`))}))},null,2)+'\n',{flag:'wx'});
console.log(JSON.stringify({audits,fullBaselineCodeDelta,changedSinceFinalGates:changed,archivedFiles:archived.length},null,2));
