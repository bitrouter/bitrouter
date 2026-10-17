import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import cp from 'node:child_process';
const root = '/Users/kelsen/.codex/archives/bro-refactor/2026-10-16-review-fixes';
const base = 'c0b75b141b3041a1cd9c1284aa16acc27caaab79';
const prefix = 'review-fixes/2026-10-16';
const index = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'bro-review-evidence-')), 'index');
const env = { ...process.env, GIT_INDEX_FILE: index };
function git(args, input) {
  const r = cp.spawnSync('git', args, { env, input, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  if (r.status !== 0) throw new Error(r.stderr || r.stdout);
  return r.stdout.trimEnd();
}
function files(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).sort((a,b)=>a.name.localeCompare(b.name)).flatMap(e=>{
    const p=path.join(dir,e.name);return e.isDirectory()?files(p):[p];
  });
}
function sha(bytes) { return crypto.createHash('sha256').update(bytes).digest('hex'); }
function add(name, bytes, mode='100644') {
  const blob = git(['hash-object','-w','--stdin'], bytes);
  git(['update-index','--add','--cacheinfo',`${mode},${blob},${name}`]);
  return sha(bytes);
}
const payload = files(root).filter(p=>path.relative(root,p)!=='SHA256SUMS');
const sums=payload.map(p=>`${sha(fs.readFileSync(p))}  ${path.relative(root,p)}`).join('\n')+'\n';
fs.writeFileSync(path.join(root,'SHA256SUMS'),sums);
git(['read-tree',base]);
const manifest = new Map(git(['show',`${base}:SHA256SUMS`]).split('\n').filter(Boolean).map(l=>[l.slice(66),l.slice(0,64)]));
for(const p of [...payload,path.join(root,'SHA256SUMS')]) {
  const name=`${prefix}/${path.relative(root,p)}`,bytes=fs.readFileSync(p);
  manifest.set(name,add(name,bytes,(fs.statSync(p).mode&0o111)?'100755':'100644'));
}
const readme=Buffer.from('# PR #981 BRO refactor evidence\n\nLatest review fixes: [results, logs, source bindings and retained Rust fixture files](review-fixes/2026-10-16/README.md). Previous archives remain pinned to their original commits.\n\n'+git(['show',`${base}:README.md`]).replace(/^# PR #981 BRO refactor evidence\n\n/,'' )+'\n');
manifest.set('README.md',add('README.md',readme));
const globalSums=Buffer.from([...manifest].sort(([a],[b])=>a.localeCompare(b)).map(([n,h])=>`${h}  ${n}`).join('\n')+'\n');
add('SHA256SUMS',globalSums);
const tree=git(['write-tree']);
const commit=git(['commit-tree',tree,'-p',base,'-F','-'],'docs: archive bro review fixes and validation\n');
const changed=git(['diff-tree','--no-commit-id','--name-only','-r',base,commit]).split('\n');
if(changed.some(p=>!['README.md','SHA256SUMS'].includes(p)&&!p.startsWith(prefix+'/')))throw Error('Unexpected existing archive mutation');
const receipt={commit,tree,parent:base,prefix,files:payload.length+1,manifestSha256:sha(globalSums),sourceCommit:git(['rev-parse','HEAD']),createdAt:new Date().toISOString()};
fs.writeFileSync('/tmp/bro-pr981-review-fixes-archive-receipt.json',JSON.stringify(receipt,null,2)+'\n');
console.log(JSON.stringify(receipt));
