import {spawn, execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync, openSync, closeSync, copyFileSync, existsSync, mkdirSync} from 'node:fs';
import path from 'node:path';
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter';
const archive='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
mkdirSync(archive,{recursive:true});
const sha=b=>createHash('sha256').update(b).digest('hex');
const snapshot=()=>{
  const names=[...new Set(execFileSync('git',['ls-files','-c','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))].sort();
  return {capturedAt:new Date().toISOString(),head:execFileSync('git',['rev-parse','HEAD'],{cwd}).toString().trim(),files:Object.fromEntries(names.filter(n=>existsSync(path.join(cwd,n))).map(n=>[n,sha(readFileSync(path.join(cwd,n)))]))};
};
for(const name of ['check1','check2','check3','check4','test-build1','test-build2','focused1']) {
  const file=`/tmp/bro-s3-${name}.log`;
  if(existsSync(file))copyFileSync(file,path.join(archive,`managed-acp-${name}.log`));
}
const before=snapshot();
writeFileSync(path.join(archive,'managed-acp-source3.json'),JSON.stringify(before,null,2)+'\n');
writeFileSync(path.join(archive,'managed-acp-source3.patch'),execFileSync('git',['diff','--binary'],{cwd}));
const dirty=[...new Set(execFileSync('git',['ls-files','-m','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))];
execFileSync('tar',['-czf',path.join(archive,'managed-acp-source3-files.tar.gz'),'--',...dirty],{cwd});
copyFileSync('/tmp/bro-s3-focused-runner3.mjs',path.join(archive,'managed-acp-runner3.mjs'));
const args=['nextest','run','-p','bitrouter','-p','bitrouter-orchestrator','--all-features','--lib','--no-fail-fast','-E','test(goal_runtime::tests) | test(supervisor::tests) | test(service::tests::worker) | test(service::tests::generation) | test(thread::executor::tests)'];
const result={source:'managed-acp-source3.json',args,startedAt:new Date().toISOString(),log:'managed-acp-focused3.log',priorRun:'focused2 passed 26 tests on source2; this run adds dispatch and receipt recovery fences'};
const fd=openSync(path.join(archive,result.log),'w');
const child=spawn('cargo',args,{cwd,env:{...process.env,CARGO_PROFILE_DEV_DEBUG:'0',CARGO_PROFILE_TEST_DEBUG:'0',CARGO_INCREMENTAL:'0'},stdio:['ignore',fd,fd]});
process.stdout.write(`Focused S3 run started, cargo PID ${child.pid}\n`);
const outcome=await new Promise(resolve=>{child.once('error',error=>resolve({code:null,error:String(error)}));child.once('close',(code,signal)=>resolve({code,signal}));});
closeSync(fd);
const after=snapshot();
const changed=[...new Set([...Object.keys(before.files),...Object.keys(after.files)])].filter(n=>before.files[n]!==after.files[n]);
Object.assign(result,outcome,{finishedAt:new Date().toISOString(),changed,summary:readFileSync(path.join(archive,result.log),'utf8').split('\n').find(l=>l.includes('Summary ['))??null,passed:outcome.code===0&&changed.length===0});
writeFileSync(path.join(archive,'managed-acp-focused3.json'),JSON.stringify(result,null,2)+'\n');
process.stdout.write(JSON.stringify(result,null,2)+'\n');
process.exitCode=result.passed?0:1;
