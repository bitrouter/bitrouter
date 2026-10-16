import fs from 'node:fs';import path from 'node:path';import {execFileSync} from 'node:child_process';
const root='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const patterns=[/sk-or-v1-[a-f0-9]{40,}/gi,/sk-ant-api\d+-[A-Za-z0-9_-]{30,}/g,/\bgh[pousr]_[A-Za-z0-9_]{25,}/g,/-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/g];
let checked=0,archives=0;const hits=[];
function scan(file,label){const b=fs.readFileSync(file);checked++;const s=b.toString('latin1');let count=0;for(const p of patterns){p.lastIndex=0;count += [...s.matchAll(p)].length;}if(count)hits.push({path:label,matches:count});}
const archiveFiles=d=>fs.readdirSync(d,{withFileTypes:true}).flatMap(e=>{const f=path.join(d,e.name);if(e.isDirectory())return archiveFiles(f);if(e.isFile())return[f];throw Error('nonregular archive entry '+f);});
for(const file of archiveFiles(root).sort()){
 const name=path.relative(root,file);
 scan(file,name);
 if(name.endsWith('.tar.gz')){
  archives++;const names=execFileSync('tar',['-tzf',file]).toString().split('\n').filter(Boolean);
  if(names.some(n=>n.startsWith('/')||n.split('/').includes('..')))throw Error('unsafe tar '+name);
  const tmp=fs.mkdtempSync('/tmp/bro-archive-scan-');execFileSync('tar',['-xzf',file,'-C',tmp]);
  const walk=d=>{for(const e of fs.readdirSync(d,{withFileTypes:true})){const f=path.join(d,e.name);if(e.isDirectory())walk(f);else if(e.isFile())scan(f,name+':'+path.relative(tmp,f));else throw Error('nonregular tar entry '+f);}};walk(tmp);fs.rmSync(tmp,{recursive:true});
 }
}
const result={createdAt:new Date().toISOString(),scope:'Credential-shaped string scan of exact local archive files and safely extracted regular tar payloads. Reports names/counts only; no token values. Git bundle verification/provenance is recorded separately. Fake fixture keys do not use these production token prefixes.',checked,archives,hits};
console.log(JSON.stringify(result,null,2));if(hits.length)process.exitCode=1;
