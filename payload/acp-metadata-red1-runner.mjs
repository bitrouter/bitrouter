import {spawn,execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,openSync,closeSync,copyFileSync,existsSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import path from 'node:path';
const config=JSON.parse(readFileSync(process.argv[2]));
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter',archive='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
if(!/^[a-z0-9-]+$/.test(config.id))throw Error('invalid archive ID');
const target=suffix=>path.join(archive,`${config.id}-${suffix}`);
for(const suffix of ['source.json','source.patch','files.tar.gz','runner.mjs','results.json','audit.json'])if(existsSync(target(suffix)))throw Error('archive ID already used');
const sha=b=>createHash('sha256').update(b).digest('hex');
const snapshot=()=>{const names=[...new Set(execFileSync('git',['ls-files','-c','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))].sort();return {capturedAt:new Date().toISOString(),head:execFileSync('git',['rev-parse','HEAD'],{cwd}).toString().trim(),files:Object.fromEntries(names.filter(n=>existsSync(path.join(cwd,n))).map(n=>[n,sha(readFileSync(path.join(cwd,n)))]))};};
const before=snapshot();writeFileSync(target('source.json'),JSON.stringify(before,null,2)+'\n',{flag:'wx'});
writeFileSync(target('source.patch'),execFileSync('git',['diff','--binary'],{cwd}),{flag:'wx'});
const dirty=[...new Set(execFileSync('git',['ls-files','-m','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))];
execFileSync('tar',dirty.length ? ['-czf',target('files.tar.gz'),'--',...dirty] : ['-czf',target('files.tar.gz'),'-T','/dev/null'],{cwd});
writeFileSync(target('source-archive.json'),JSON.stringify({head:before.head,dirtyFiles:dirty,scope:dirty.length?'Dirty files supplement the exact committed HEAD.':'Empty dirty-file tar; all source is in the exact committed HEAD, which must be retained with its Git bundle.'},null,2)+'\n',{flag:'wx'});
copyFileSync(fileURLToPath(import.meta.url),target('runner.mjs'));writeFileSync(target('config.json'),JSON.stringify(config,null,2)+'\n',{flag:'wx'});
const result={source:path.basename(target('source.json')),scope:config.scope,startedAt:new Date().toISOString(),runs:[]};
const save=()=>writeFileSync(target('results.json'),JSON.stringify(result,null,2)+'\n');
for(const job of config.jobs){
const log=target(`${job.name}.log`),run={name:job.name,args:job.args,log:path.basename(log),startedAt:new Date().toISOString(),status:'running'};result.runs.push(run);save();
const fd=openSync(log,'wx');process.stdout.write(`Started ${job.name}; ${path.basename(log)}\n`);
const child=spawn('cargo',job.args,{cwd,env:{...process.env,CARGO_PROFILE_DEV_DEBUG:'0',CARGO_PROFILE_TEST_DEBUG:'0',CARGO_INCREMENTAL:'0',BRO_GOAL_FIXTURE_KEEP_FAILURES:'1'},stdio:['ignore',fd,fd]});
const status=await new Promise(resolve=>{child.once('error',error=>resolve({code:null,error:String(error)}));child.once('close',(code,signal)=>resolve({code,signal}));});closeSync(fd);
const raw=readFileSync(log,'utf8');Object.assign(run,status,{finishedAt:new Date().toISOString(),status:status.code===0?'passed':'failed',summaries:raw.split('\n').filter(line=>line.includes('Summary [')||line.startsWith('test result:')),leaks:raw.split('\n').filter(line=>/^\s*LEAK\s/.test(line))});save();process.stdout.write(`Finished ${job.name}: ${run.status}\n`);if(status.code!==0)break;
}
const after=snapshot(),changed=[...new Set([...Object.keys(before.files),...Object.keys(after.files)])].filter(n=>before.files[n]!==after.files[n]);
Object.assign(result,{finishedAt:new Date().toISOString(),changed,passed:result.runs.length===config.jobs.length&&result.runs.every(r=>r.code===0)&&!changed.length});save();writeFileSync(target('audit.json'),JSON.stringify({checkedFiles:Object.keys(before.files).length,changed,source:result.source},null,2)+'\n',{flag:'wx'});process.stdout.write(JSON.stringify({passed:result.passed,changed,runs:result.runs.map(r=>({name:r.name,code:r.code,summaries:r.summaries,leaks:r.leaks}))},null,2)+'\n');process.exitCode=result.passed?0:1;
