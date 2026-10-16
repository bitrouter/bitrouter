import {spawn,execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync,openSync,closeSync,copyFileSync,existsSync,mkdirSync} from 'node:fs';
import path from 'node:path';
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter';
const archive='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
mkdirSync(archive,{recursive:true});
const sha=b=>createHash('sha256').update(b).digest('hex');
const snapshot=()=>{
  const names=[...new Set(execFileSync('git',['ls-files','-c','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))].sort();
  return {capturedAt:new Date().toISOString(),head:execFileSync('git',['rev-parse','HEAD'],{cwd}).toString().trim(),files:Object.fromEntries(names.filter(n=>existsSync(path.join(cwd,n))).map(n=>[n,sha(readFileSync(path.join(cwd,n)))]))};
};
const before=snapshot();
writeFileSync(path.join(archive,'native-evidence-source8.json'),JSON.stringify(before,null,2)+'\n');
writeFileSync(path.join(archive,'native-evidence-source8.patch'),execFileSync('git',['diff','--binary'],{cwd}));
const dirty=[...new Set(execFileSync('git',['ls-files','-m','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))];
execFileSync('tar',['-czf',path.join(archive,'native-evidence-source8-files.tar.gz'),'--',...dirty],{cwd});
copyFileSync('/tmp/bro-native-evidence-gates8.mjs',path.join(archive,'native-evidence-runner8.mjs'));
const result={source:'native-evidence-source8.json',startedAt:new Date().toISOString(),scope:'S4 bounded native SDK/material projection; full refactor and native activity gates remain open',runs:[]};
const save=()=>writeFileSync(path.join(archive,'native-evidence-gates8.json'),JSON.stringify(result,null,2)+'\n');
const jobs=[
  ['clippy',['clippy','--workspace','--all-targets','--all-features','--','-D','warnings'],'native-evidence-clippy8.log'],
  ['format',['fmt','--all','--','--check'],'native-evidence-fmt8.log'],
  ['nextest',['nextest','run','-p','bitrouter','--all-features','--lib','--no-fail-fast','-E','test(goal_runtime)'],'native-evidence-goal-runtime8.log'],
  ['rustdoc',['test','--workspace','--all-features','--doc','--jobs=2'],'native-evidence-doctests8.log'],
];
for(const [id,args,log]of jobs){
  const run={id,args,log,startedAt:new Date().toISOString(),status:'running'};result.runs.push(run);save();
  process.stdout.write(`Started ${id}; log ${log}\n`);
  const fd=openSync(path.join(archive,log),'w');
  const child=spawn('cargo',args,{cwd,env:{...process.env,CARGO_PROFILE_DEV_DEBUG:'0',CARGO_PROFILE_TEST_DEBUG:'0',CARGO_INCREMENTAL:'0'},stdio:['ignore',fd,fd]});
  const outcome=await new Promise(resolve=>{child.once('error',error=>resolve({code:null,error:String(error)}));child.once('close',(code,signal)=>resolve({code,signal}));});
  closeSync(fd);Object.assign(run,outcome,{finishedAt:new Date().toISOString(),status:outcome.code===0?'passed':'failed'});save();
  process.stdout.write(`Finished ${id}: ${run.status} (${outcome.code})\n`);
  if(outcome.code!==0)break;
}
const after=snapshot();
const changed=[...new Set([...Object.keys(before.files),...Object.keys(after.files)])].filter(n=>before.files[n]!==after.files[n]);
Object.assign(result,{finishedAt:new Date().toISOString(),changed,passed:result.runs.length===jobs.length&&result.runs.every(r=>r.code===0)&&changed.length===0});
const full=path.join(archive,'native-evidence-goal-runtime8.log');
if(existsSync(full)){
  const log=readFileSync(full,'utf8');
  result.nextestSummary=log.split('\n').find(l=>l.includes('Summary ['))??null;
  result.nextestLeakLines=log.split('\n').filter(l=>/^\s*LEAK\s/.test(l));
}
save();
writeFileSync(path.join(archive,'native-evidence-source-audit8.json'),JSON.stringify({capturedAt:after.capturedAt,source:result.source,checkedFiles:Object.keys(before.files).length,changed},null,2)+'\n');
process.stdout.write(JSON.stringify({passed:result.passed,changed,result:result.nextestSummary??result.runs.at(-1)},null,2)+'\n');
process.exitCode=result.passed?0:1;
