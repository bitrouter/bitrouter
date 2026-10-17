import fs from 'node:fs';
import cp from 'node:child_process';
import {promisify} from 'node:util';
const execFile=promisify(cp.execFile);
const root='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16-review-fixes';
const head=JSON.parse(fs.readFileSync(root+'/RESULTS.json','utf8')).sourceCommit;
const dest=root+'/hosted-ci';fs.mkdirSync(dest+'/jobs',{recursive:true});
async function gh(args,raw=false){return (await execFile('gh',args,{maxBuffer:64*1024*1024,encoding:raw?null:'utf8'})).stdout;}
const pr=JSON.parse(await gh(['pr','view','981','--json','headRefOid,state,statusCheckRollup']));
if(pr.headRefOid!==head)throw Error('PR head changed');
const allowed=new Set(['SUCCESS','NEUTRAL','SKIPPED']);
if(pr.statusCheckRollup.length===0||pr.statusCheckRollup.some(x=>!allowed.has(x.conclusion||x.state)))throw Error('Checks are not all terminal and allowed');
fs.writeFileSync(dest+'/checks.json',JSON.stringify(pr,null,2)+'\n');
const runs=JSON.parse(await gh(['run','list','--repo','bitrouter/bitrouter','--commit',head,'--limit','50','--json','databaseId,status,conclusion,name,headSha,url']));
if(runs.some(x=>x.status!=='completed'))throw Error('Workflow still running');
fs.writeFileSync(dest+'/runs.json',JSON.stringify(runs,null,2)+'\n');
const jobs=[];
for(const run of runs){const data=JSON.parse(await gh(['api',`repos/bitrouter/bitrouter/actions/runs/${run.databaseId}/jobs?per_page=100`]));if(data.total_count>100)throw Error('Job pagination required');fs.writeFileSync(dest+`/run-${run.databaseId}.json`,JSON.stringify(data,null,2)+'\n');for(const job of data.jobs)jobs.push({...job,workflowRun:run.databaseId});}
const logs=[];let cursor=0;
await Promise.all(Array.from({length:3},async()=>{while(cursor<jobs.length){const job=jobs[cursor++];if(!job.steps?.length||job.conclusion==='skipped'){logs.push({job:job.id,status:'skipped-no-log',conclusion:job.conclusion});continue;}try{const text=await gh(['api',`repos/bitrouter/bitrouter/actions/jobs/${job.id}/logs`],true);const file=`jobs/${job.id}.log`;fs.writeFileSync(dest+'/'+file,text);logs.push({job:job.id,path:file,bytes:Buffer.byteLength(text),conclusion:job.conclusion});}catch(e){logs.push({job:job.id,status:'log-unavailable',error:String(e.stderr||e.message)});}}}));
const result={head,recordedAt:new Date().toISOString(),checks:pr.statusCheckRollup.length,allChecksTerminalAllowed:true,runs:runs.length,jobs:jobs.length,rawLogs:logs.filter(x=>x.path).length,logs};
fs.writeFileSync(dest+'/INDEX.json',JSON.stringify(result,null,2)+'\n');
if(logs.some(x=>x.status==='log-unavailable'))throw Error('A non-skipped job log was unavailable; inspect INDEX.json');
console.log(JSON.stringify({checks:result.checks,runs:result.runs,jobs:result.jobs,rawLogs:result.rawLogs}));
