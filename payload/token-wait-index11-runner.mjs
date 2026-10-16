import {readFileSync,writeFileSync,readdirSync,existsSync,mkdtempSync,lstatSync,copyFileSync} from 'node:fs';
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import path from 'node:path';
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter',root='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const sha=b=>createHash('sha256').update(b).digest('hex');
const read=n=>JSON.parse(readFileSync(path.join(root,n),'utf8'));
const write=(n,o)=>writeFileSync(path.join(root,n),typeof o==='string'?o:JSON.stringify(o,null,2)+'\n',{flag:'wx'});
const ids=['red1','green2','final3','final4','diag5','final6','final7','red8','green9','green10'].map(s=>'token-wait-'+s);
const extracts=[];
for(const id of ids){
  const source=read(id+'-source.json'),result=read(id+'-results.json');
  if(!result.finishedAt||result.runs.some(r=>r.status==='running'))throw Error('run not terminal: '+id);
  const tar=path.join(root,id+'-files.tar.gz');
  const names=execFileSync('tar',['-tzf',tar]).toString().trim().split('\n');
  for(const n of names)if(n.startsWith('/')||n.split('/').includes('..'))throw Error('unsafe tar path');
  const dest=mkdtempSync('/tmp/'+id+'-source-');
  execFileSync('tar',['-xzf',tar,'-C',dest]);
  for(const n of names)if(sha(readFileSync(path.join(dest,n)))!==source.files[n])throw Error('source hash mismatch '+id+' '+n);
  extracts.push({id,extracted:dest,files:names.length,tarSha256:sha(readFileSync(tar)),passed:result.passed,leaks:result.runs.flatMap(r=>r.leaks||[])});
}
const tested=read('token-wait-green10-source.json');
const componentSource=read('token-wait-green9-source.json');
const componentDelta=[...new Set([...Object.keys(componentSource.files),...Object.keys(tested.files)])].filter(n=>componentSource.files[n]!==tested.files[n]);
if(componentDelta.some(n=>!['apps/bitrouter/tests/goal_tui_pty.rs','docs/BRO_ORCHESTRATOR_REFACTOR_IMPLEMENTATION.md'].includes(n)))throw Error('component production source changed: '+componentDelta);
const names=[...new Set(execFileSync('git',['ls-files','-c','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean))];
const delta=[...new Set([...names,...Object.keys(tested.files)])].filter(n=>!existsSync(path.join(cwd,n))||sha(readFileSync(path.join(cwd,n)))!==tested.files[n]);
if(delta.some(n=>n!=='docs/BRO_ORCHESTRATOR_REFACTOR_IMPLEMENTATION.md'))throw Error('unexpected post-test change '+delta);
const dirty=execFileSync('git',['ls-files','-m','-o','--exclude-standard','-z'],{cwd}).toString().split('\0').filter(Boolean);
write('token-wait-final11.patch',execFileSync('git',['diff','--binary'],{cwd}).toString());
const tar=path.join(root,'token-wait-final11-files.tar.gz');if(existsSync(tar))throw Error('final tar exists');
execFileSync('tar',['-czf',tar,'--',...dirty],{cwd});
const restored=mkdtempSync('/tmp/token-wait-final11-');execFileSync('tar',['-xzf',tar,'-C',restored]);
for(const n of dirty)if(sha(readFileSync(path.join(cwd,n)))!==sha(readFileSync(path.join(restored,n))))throw Error('final tar mismatch '+n);
write('token-wait-applicability11.json',{head:execFileSync('git',['rev-parse','HEAD'],{cwd}).toString().trim(),extracts,componentSource:'token-wait-green9-source.json',componentDelta,postTestDelta:delta,finalExtracted:restored,finalFiles:Object.fromEntries(dirty.map(n=>[n,sha(readFileSync(path.join(cwd,n)))])),finalTarSha256:sha(readFileSync(tar))});
copyFileSync('/tmp/bro-token-wait-index11.mjs',path.join(root,'token-wait-index11-runner.mjs'));
const files=[];
function visit(base){for(const name of readdirSync(path.join(root,base))){const relative=path.join(base,name),full=path.join(root,relative),stat=lstatSync(full);if(stat.isDirectory())visit(relative);else if(stat.isFile())files.push(relative);else throw Error('non-regular archive member '+relative);}}
for(const name of readdirSync(root).sort())if((name.startsWith('token-wait-')&&name!=='token-wait-manifest11.json')||name==='NATIVE_QUOTE_WAIT_INDEX.md'){const stat=lstatSync(path.join(root,name));if(stat.isDirectory())visit(name);else if(stat.isFile())files.push(name);else throw Error('non-regular archive member '+name);}
write('token-wait-manifest11.json',{createdAt:new Date().toISOString(),files:Object.fromEntries(files.sort().map(n=>{const b=readFileSync(path.join(root,n));return[n,{bytes:b.length,sha256:sha(b)}];}))});
process.stdout.write(JSON.stringify({sourceTars:extracts.length,postTestDelta:delta,finalSourceFiles:dirty.length,manifestFiles:files.length})+'\n');
