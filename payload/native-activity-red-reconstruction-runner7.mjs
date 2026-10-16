import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,mkdtempSync,existsSync,mkdirSync,copyFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import path from 'node:path';
const cwd='/Users/kelsen/.codex/worktrees/d91b/bitrouter',archive='/Users/kelsen/.codex/archives/bro-refactor/2026-10-16';
const source=JSON.parse(readFileSync(path.join(archive,'native-activity-source1.json')));
const root=mkdtempSync('/tmp/bro-native-red1-reconstruct-'),tree=path.join(root,'tree');mkdirSync(tree);
execFileSync('git',['archive','--format=tar',`--output=${root}/parent.tar`,source.head],{cwd});
execFileSync('tar',['-xf',path.join(root,'parent.tar'),'-C',tree]);
execFileSync('git',['apply',path.join(archive,'native-activity-source1.patch')],{cwd:tree});
const sha=b=>createHash('sha256').update(b).digest('hex');
for(const [name,expected]of Object.entries(source.files)){
 const file=path.join(tree,name);
 if(!existsSync(file)){mkdirSync(path.dirname(file),{recursive:true});writeFileSync(file,execFileSync('git',['show',`${source.head}:${name}`],{cwd,maxBuffer:64*1024*1024}));}
 if(sha(readFileSync(file))!==expected)throw Error(`red source mismatch: ${name}`);
}
const changes=execFileSync('git',['apply','--numstat',path.join(archive,'native-activity-source1.patch')],{cwd:tree}).toString().trim().split('\n').filter(Boolean).map(line=>line.split('\t').at(-1));
const name='native-activity-red1-reconstructed-files.tar.gz';execFileSync('tar',['-czf',path.join(archive,name),'--',...changes],{cwd:tree});
const extracted=path.join(root,'check');mkdirSync(extracted);execFileSync('tar',['-xzf',path.join(archive,name),'-C',extracted]);
if(changes.some(file=>sha(readFileSync(path.join(extracted,file)))!==source.files[file]))throw Error('reconstructed tar differs');
writeFileSync(path.join(archive,'native-activity-red1-reconstruction7.json'),JSON.stringify({capturedAt:new Date().toISOString(),base:source.head,method:'Parent Git archive plus original saved source1 patch; all original manifest file hashes verified',checkedFiles:Object.keys(source.files).length,changedFiles:changes,archive:name,sha256:sha(readFileSync(path.join(archive,name))),extracted,passed:true,originalGzipContainerOverwritten:true,scope:'Exact source-file bytes reconstructed; original gzip container and transient runtime state are not recovered.'},null,2)+'\n');
copyFileSync('/tmp/bro-native-activity-red-reconstruct7.mjs',path.join(archive,'native-activity-red-reconstruction-runner7.mjs'));
console.log(JSON.stringify({verifiedFiles:Object.keys(source.files).length,changedFiles:changes,passed:true}));
