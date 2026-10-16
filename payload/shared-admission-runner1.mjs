import {spawn, execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync, openSync, closeSync, copyFileSync, existsSync, mkdirSync} from 'node:fs';
import path from 'node:path';

const cwd = '/Users/kelsen/.codex/worktrees/d91b/bitrouter';
const archive = '/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
mkdirSync(archive, {recursive:true});
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const snapshot = () => {
  const names = [...new Set(execFileSync('git', ['ls-files','-c','-o','--exclude-standard','-z'], {cwd}).toString().split('\0').filter(Boolean))].sort();
  const files = {};
  for (const name of names) if (existsSync(path.join(cwd, name))) files[name] = sha(readFileSync(path.join(cwd, name)));
  return {capturedAt:new Date().toISOString(), head:execFileSync('git',['rev-parse','HEAD'],{cwd}).toString().trim(), files};
};
const source = snapshot();
const sourceName = 'shared-admission-source1.json';
writeFileSync(path.join(archive,sourceName),JSON.stringify(source,null,2)+'\n');
for (const n of [1,2,3]) {
  const name = `bro-shared-admission-focused${n}.log`;
  copyFileSync(path.join('/tmp',name), path.join(archive,`shared-admission-focused${n}.log`));
}
const result = {source:sourceName,startedAt:new Date().toISOString(), purpose:'Shared bounded generation/native admission waiting; does not qualify pending S3-S6', runs:[]};
const save = () => writeFileSync(path.join(archive,'shared-admission-gates1.json'),JSON.stringify(result,null,2)+'\n');
const jobs = [
  ['nextest',['nextest','run','--workspace','--all-features','--no-fail-fast'],'shared-admission-full1.log'],
  ['clippy',['clippy','--workspace','--all-targets','--all-features','--','-D','warnings'],'shared-admission-clippy1.log'],
  ['format',['fmt','--all','--','--check'],'shared-admission-fmt1.log'],
  ['rustdoc',['test','--workspace','--all-features','--doc','--jobs=2'],'shared-admission-doctests1.log'],
];
for (const [id,args,log] of jobs) {
  const run = {id,args,log,startedAt:new Date().toISOString(),status:'running'};
  result.runs.push(run);save();
  process.stdout.write(`Started ${id}; output retained in ${log}\n`);
  const fd = openSync(path.join(archive,log),'w');
  const child = spawn('cargo',args,{cwd,env:{...process.env,CARGO_PROFILE_DEV_DEBUG:'0',CARGO_PROFILE_TEST_DEBUG:'0',CARGO_INCREMENTAL:'0'},stdio:['ignore',fd,fd]});
  const outcome = await new Promise(resolve => {
    child.once('error', error => resolve({code:null,signal:null,error:String(error)}));
    child.once('close',(code,signal)=>resolve({code,signal}));
  });
  closeSync(fd);
  Object.assign(run,outcome,{finishedAt:new Date().toISOString(),status:outcome.code===0?'passed':'failed'});
  save();
  process.stdout.write(`Finished ${id}: ${run.status} (${outcome.code})\n`);
}
const after = snapshot();
const changed = [...new Set([...Object.keys(source.files),...Object.keys(after.files)])].filter(name=>source.files[name]!==after.files[name]);
const audit = {source:sourceName,capturedAt:after.capturedAt,checkedFiles:Object.keys(source.files).length,changed,executableOrBuildSourceChanged:changed.some(name=>/\.(rs|toml|lock|jsonl)$/.test(name))};
writeFileSync(path.join(archive,'shared-admission-source-audit1.json'),JSON.stringify(audit,null,2)+'\n');
result.finishedAt=new Date().toISOString();
result.passed=result.runs.every(run=>run.status==='passed')&&!audit.executableOrBuildSourceChanged;
save();
const files = [sourceName,'shared-admission-gates1.json','shared-admission-source-audit1.json',...jobs.map(job=>job[2]),...[1,2,3].map(n=>`shared-admission-focused${n}.log`)];
writeFileSync(path.join(archive,'shared-admission-verification-summary.json'),JSON.stringify({capturedAt:new Date().toISOString(),source:sourceName,passed:result.passed,open:['S3 managed ACP driver and shared authority','S4-S6 acceptance','Token-quote waiting and persistent UI waiting phase','Earlier intermittent PTY and route-startup causes'],sha256:Object.fromEntries(files.map(name=>[name,sha(readFileSync(path.join(archive,name)))]))},null,2)+'\n');
process.stdout.write(`All gates recorded; passed=${result.passed}, source changes=${changed.length}\n`);
process.exitCode=result.passed?0:1;
