import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { BASELINE, LINUX_TARGET, ROLES, VARIANTS, hash, verifyBuild } from './synthetic-build-identity.mjs';

const [folder, temp, candidate, runId, attempt, producerRef = process.env.GITHUB_REF] = process.argv.slice(2).map((value, index) => index < 2 ? resolve(value) : value);
const read = async file => JSON.parse(await readFile(file, 'utf8'));
const run = (command, args) => execFileSync(command, args, { encoding: 'utf8', timeout: 10_000 }).trim();
const archiveDigests = {
  'm0-http2': '76fcf5c627d50c796e140ad73c5b445e94219aa09dc02263999dcad3bfaae3cf',
  'm0-minimum': 'a23c7fea4278371bae08a27e96738e5bf44e3c815878a45599eb1e2232b92a2f',
  'service-http2': 'd7bef4f22f0849362755868fe778d95a55964ff8841ff6dc2ae9a8967b00b659',
  'service-minimum': '2255bc41b1ba49822cfc96d6e03a04927fc3735249b5b976cab4702f84cee24a',
};
const peers = {};
assert.match(candidate, /^[0-9a-f]{40}$/);
assert.match(runId, /^\d+$/);
assert.match(attempt, /^[1-9]\d*$/);
for (const [role, source, checkout] of [['m0', BASELINE, 'baseline'], ['service', candidate, 'candidate']]) {
  assert.equal(run('git', ['-C', checkout, 'rev-parse', 'HEAD']), source);
  peers[role] = {};
  for (const variant of Object.keys(VARIANTS)) {
    const name = 'synthetic-linux-input-' + attempt + '-' + role + '-' + variant;
    const directory = join(folder, name);
    const receipt = await read(join(directory, 'build.json'));
    const { binary } = ROLES[role];
    assert.equal(receipt.workflow, 'bitrouter/bitrouter/.github/workflows/split-synthetic-linux.yml@' + producerRef);
    assert.equal(receipt.run_id, runId);
    assert.equal(receipt.run_attempt, attempt);
    const tar = join(directory, 'runtime.tar.gz');
    const digest = await hash(tar);
    assert.equal(digest, archiveDigests[role + '-' + variant], 'previously verified release archive changed');
    assert.equal((await readFile(join(directory, 'runtime.sha256'), 'utf8')).trim(), digest);
    assert.deepEqual(run('/usr/bin/tar', ['-tzf', tar]).split('\n').sort(), [binary, 'build.json'].sort());
    assert.deepEqual(JSON.parse(run('/usr/bin/tar', ['-xOzf', tar, 'build.json'])), receipt);
    const release = await read(join(directory, 'cold-release.json'));
    const args = ['build', '--release', '--bin', binary, '--offline', '--locked', '-j', '2', '-p', 'bitrouter',
      '--target', LINUX_TARGET, '--no-default-features'];
    if (variant === 'http2') args.push('--features', 'http2');
    args.push('--message-format=json-render-diagnostics');
    assert.deepEqual([release.source, release.profile, release.status, release.signal, release.args],
      [source, 'release', 0, null, args]);
    const destination = join(temp, 'executables', role, variant);
    await mkdir(destination, { recursive: true });
    const executable = join(destination, binary);
    await writeFile(executable, execFileSync('/usr/bin/tar', ['-xOzf', tar, binary],
      { timeout: 30_000, maxBuffer: 200 * 1024 * 1024 }), { flag: 'wx', mode: 0o500 });
    await verifyBuild(receipt, executable, role, variant, source, await hash(join(checkout, 'Cargo.lock')), true, LINUX_TARGET);
    peers[role][variant] = { ...receipt, delivery: { run: runId, attempt, artifactName: name, payloadSha256: digest } };
  }
}
for (const variant of Object.keys(VARIANTS)) assert.deepEqual(peers.m0[variant].settings, peers.service[variant].settings);
const builds = join(temp, 'synthetic-builds');
await mkdir(builds);
for (const [role, variants] of Object.entries(peers)) {
  for (const [variant, receipt] of Object.entries(variants)) {
    await writeFile(join(builds, `${role}-${variant}.json`), JSON.stringify(receipt, null, 2) + '\n', { flag: 'wx' });
  }
}
console.log('Verified exact Linux releases, one pinned producer attempt, ELF architecture and matching producer settings.');
