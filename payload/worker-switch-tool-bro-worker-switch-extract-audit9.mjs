import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,mkdtempSync,statSync} from 'node:fs';
import {createHash} from 'node:crypto';
import path from 'node:path';
const root='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const source=JSON.parse(readFileSync(path.join(root,'worker-switch-source9.json'),'utf8'));
const bundle=path.join(root,'worker-switch-source9-files.tar.gz');
const files=execFileSync('tar',['-tzf',bundle]).toString().trim().split('\n');
for(const file of files){
  if(path.isAbsolute(file)||file.split('/').some(part=>part==='..')||!source.files[file])throw Error('Invalid source archive member');
}
const directory=mkdtempSync('/tmp/bro-worker-switch-source9-extract-');
execFileSync('tar',['-xzf',bundle,'-C',directory]);
for(const file of files){
  const full=path.join(directory,file);
  if(!statSync(full).isFile())throw Error('Source member is not a regular file');
  const hash=createHash('sha256').update(readFileSync(full)).digest('hex');
  if(hash!==source.files[file])throw Error('Source member checksum differs');
}
const result={capturedAt:new Date().toISOString(),source:'worker-switch-source9.json',parent:source.head,archive:'worker-switch-source9-files.tar.gz',archiveSha256:createHash('sha256').update(readFileSync(bundle)).digest('hex'),verifiedRegularFiles:files.length,extraction:directory,passed:true};
writeFileSync(path.join(root,'worker-switch-source9-extraction-audit.json'),JSON.stringify(result,null,2)+'\n');
console.log(JSON.stringify(result));
