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
writeFileSync(path.join(archive,'native-activity-source6.json'),JSON.stringify(before,null,2)+'\n');
writeFileSync(path.join(archive,'native-activity-source6.patch'),execFileSync('git',['diff','--binary'],{cwd}));
const dirty=[...new Set(execFileSync('git',['ls-files','-m','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))];
execFileSync('tar',['-czf',path.join(archive,'native-activity-source12-files.tar.gz'),'--',...dirty],{cwd});
copyFileSync('/tmp/bro-native-activity-gates6.mjs',path.join(archive,'native-activity-runner6.mjs'));
const result={source:'native-activity-source6.json',startedAt:new Date().toISOString(),scope:'Actual native content receipt, monotonic overlap, isolated conflicting writes, checked join and immutable outcome replay; other refactor gates remain open',runs:[]};
const save=()=>writeFileSync(path.join(archive,'native-activity-gates6.json'),JSON.stringify(result,null,2)+'\n');
const jobs=[['orchestrator',['nextest','run','-p','bitrouter-orchestrator','--all-features','--lib','--no-fail-fast','-E','test(agent::tests::native)'],'native-activity-orchestrator6.log']];
for(const [id,args,log]of jobs){
  const run={id,args,log,startedAt:new Date().toISOString(),status:'running'};result.runs.push(run);save();
  process.stdout.write(`Started ${id}; log ${log}\n`);
  const fd=openSync(path.join(archive,log),'w');
  const child=spawn('cargo',args,{cwd,env:{...process.env,CARGO_PROFILE_DEV_DEBUG:'0',CARGO_PROFILE_TEST_DEBUG:'0',CARGO_INCREMENTAL:'0',BRO_GOAL_FIXTURE_KEEP_FAILURES:'1'},stdio:['ignore',fd,fd]});
  const outcome=await new Promise(resolve=>{child.once('error',error=>resolve({code:null,error:String(error)}));child.once('close',(code,signal)=>resolve({code,signal}));});
  closeSync(fd);Object.assign(run,outcome,{finishedAt:new Date().toISOString(),status:outcome.code===0?'passed':'failed'});save();
  process.stdout.write(`Finished ${id}: ${run.status} (${outcome.code})\n`);
  if(outcome.code!==0)break;
}
const after=snapshot();
const changed=[...new Set([...Object.keys(before.files),...Object.keys(after.files)])].filter(n=>before.files[n]!==after.files[n]);
Object.assign(result,{finishedAt:new Date().toISOString(),changed,passed:result.runs.length===jobs.length&&result.runs.every(r=>r.code===0)&&changed.length===0});
const full=path.join(archive,'native-activity-goal-runtime6.log');
if(existsSync(full)){
  const log=readFileSync(full,'utf8');
  result.nextestSummary=log.split('\n').find(l=>l.includes('Summary ['))??null;
  result.nextestLeakLines=log.split('\n').filter(l=>/^\s*LEAK\s/.test(l));
}
save();
writeFileSync(path.join(archive,'native-activity-source-audit6.json'),JSON.stringify({capturedAt:after.capturedAt,source:result.source,checkedFiles:Object.keys(before.files).length,changed},null,2)+'\n');
process.stdout.write(JSON.stringify({passed:result.passed,changed,result:result.nextestSummary??result.runs.at(-1)},null,2)+'\n');
process.exitCode=result.passed?0:1;
