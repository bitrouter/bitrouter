import {spawn, execFileSync} from 'node:child_process';
import {readFileSync, writeFileSync, openSync, closeSync, appendFileSync, copyFileSync, existsSync} from 'node:fs';
import {createHash} from 'node:crypto';
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter';
const archive='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const id='dispatch-rejection-process-probe7';
const file=suffix=>`${archive}/${id}-${suffix}`;
const sha=b=>createHash('sha256').update(b).digest('hex');
const snapshot=()=>Object.fromEntries([...new Set(execFileSync('git',['ls-files','-c','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))].sort().filter(n=>existsSync(`${cwd}/${n}`)).map(n=>[n,sha(readFileSync(`${cwd}/${n}`))]));
if(existsSync(file('source.json')))throw Error('archive ID already exists');
const before=snapshot();
writeFileSync(file('source.json'),JSON.stringify({head:execFileSync('git',['rev-parse','HEAD'],{cwd}).toString().trim(),files:before},null,2)+'\n',{flag:'wx'});
copyFileSync('/tmp/bro-dispatch-process-probe7.mjs',file('runner.mjs'));
const args=['nextest','run','-p','bitrouter','--all-features','--lib','--no-fail-fast','--stress-count','20','-E','test(native_dispatch_before_turn_acceptance_uses_original_input_and_keys_after_reopen)'];
const fd=openSync(file('nextest.log'),'wx');
const child=spawn('cargo',args,{cwd,env:{...process.env,CARGO_PROFILE_DEV_DEBUG:'0',CARGO_PROFILE_TEST_DEBUG:'0',CARGO_INCREMENTAL:'0',BRO_GOAL_FIXTURE_KEEP_FAILURES:'1'},stdio:['ignore',fd,fd]});
writeFileSync(file('processes.jsonl'),'',{flag:'wx'});
const emit=value=>appendFileSync(file('processes.jsonl'),JSON.stringify({time:new Date().toISOString(),...value})+'\n');
const known=new Map(), present=new Map();
let sampling=false, observedLeakCount=0;
function sample(){
  if(sampling)return; sampling=true;
  try {
    const rows=execFileSync('/bin/ps',['-axo','pid=,ppid=,args='],{encoding:'utf8',maxBuffer:4*1024*1024}).split('\n').flatMap(line=>{
      const match=line.match(/^\s*(\d+)\s+(\d+)\s+(.+)$/);
      return match?[{pid:Number(match[1]),parent:Number(match[2]),command:match[3]}]:[];
    });
    const family=new Set([child.pid,...present.keys()]);
    let changed=true;
    while(changed){changed=false;for(const row of rows)if(family.has(row.parent)&&!family.has(row.pid)){family.add(row.pid);changed=true;}}
    const current=new Map();
    for(const row of rows){
      if(!family.has(row.pid))continue;
      const test=row.command.match(/goal_runtime::[a-zA-Z0-9_:]+/)?.[0]??present.get(row.pid)?.test??present.get(row.parent)?.test??null;
      const value={...row,test};
      if(!present.has(row.pid)||present.get(row.pid).parent!==row.parent)emit({event:'observed',...value});
      known.set(row.pid,value); current.set(row.pid,value);
    }
    for(const [pid,row]of present)if(!current.has(pid))emit({event:'gone',...row});
    present.clear(); for(const [pid,row]of current)present.set(pid,row);
    const leaks=readFileSync(file('nextest.log'),'utf8').split('\n').filter(line=>/^\s*LEAK\s/.test(line));
    if(leaks.length>observedLeakCount){
      emit({event:'leak_reported',lines:leaks.slice(observedLeakCount),ownedProcesses:[...present.values()]});
      for(const row of present.values())if(row.test){
        try {emit({event:'owned_output_descriptors',pid:row.pid,output:execFileSync('/usr/sbin/lsof',['-nP','-a','-p',String(row.pid),'-d','1,2'],{encoding:'utf8',timeout:2000,maxBuffer:65536})});}
        catch(error){emit({event:'descriptor_unavailable',pid:row.pid,detail:String(error)});}
      }
      observedLeakCount=leaks.length;
    }
  } catch(error){emit({event:'monitor_error',detail:String(error)});}
  finally {sampling=false;}
}
sample(); const timer=setInterval(sample,40);
const outcome=await new Promise(resolve=>{child.once('error',error=>resolve({code:null,error:String(error)}));child.once('close',(code,signal)=>resolve({code,signal}));});
clearInterval(timer); sample(); closeSync(fd);
const after=snapshot(), changed=[...new Set([...Object.keys(before),...Object.keys(after)])].filter(n=>before[n]!==after[n]);
const raw=readFileSync(file('nextest.log'),'utf8');
const result={...outcome,args,changed,leaks:raw.split('\n').filter(line=>/^\s*LEAK\s/.test(line)),summaries:raw.split('\n').filter(line=>line.includes('Summary [')),processesSeen:known.size,stillObserved:[...present.values()],scope:'Read-only sampling of this owned Cargo/nextest tree only; no signals, no source or product/test limit changes. Instrumentation is diagnostic and can miss brief children. A passing repetition does not explain the earlier leak.'};
writeFileSync(file('result.json'),JSON.stringify(result,null,2)+'\n',{flag:'wx'});
console.log(JSON.stringify(result,null,2));
process.exitCode=outcome.code===0&&!changed.length?0:1;
