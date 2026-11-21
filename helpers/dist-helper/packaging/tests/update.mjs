// Run against two native builds of both CLIs. Each directory must contain bro
// and bitrouter from one version. DIST may point to the pinned dist executable.
// node helpers/dist-helper/packaging/tests/update.mjs <old-bin-dir> <new-bin-dir>
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFile, execFileSync, spawn } from 'node:child_process';
import { createReadStream, createWriteStream, existsSync, linkSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync,
  rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { installEnv, tempRoot } from './environment.mjs';
import { basename, dirname, join, resolve } from 'node:path';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { promisify } from 'node:util';

const exec = promisify(execFile);
const archiveMode = process.argv[2] === '--archives';
const [before, after] = process.argv.slice(archiveMode ? 3 : 2).map(p => resolve(p));
assert.ok(before && after, 'supply two versioned native bin directories');
const scripts = fileURLToPath(new URL('../', import.meta.url));
const root = tempRoot('product update ');
const windows = process.platform === 'win32';
const ext = windows ? '.exe' : '';
const names = { bitrouter: 'bitrouter', 'bitrouter-bro': 'bro' };
const config = join(root, 'config');
const cargo = join(root, 'cargo');
const bin = pkg => join(cargo, 'bin', names[pkg] + ext);
const receipt = pkg => join(config, pkg, pkg + '-receipt.json');
const digest = path => createHash('sha256').update(readFileSync(path)).digest('hex');
const safeEnv = { ...installEnv(root, cargo, config), BITROUTER_HOME: join(root, 'service state'),
  BRO_HOME: join(root, 'harness state'), BITROUTER_NO_UPDATE_CHECK: '1' };
const releases = [];
const files = new Map();
const requests = [];
const server = createServer(async (req, res) => {
  const path = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  requests.push(path);
  const api = '/api/v3/repos/bitrouter/bitrouter/releases';
  let data;
  if (path === api) data = releases;
  else if (path === api + '/latest') data = releases.find(r => !r.prerelease && !r.draft);
  else if (path.startsWith(api + '/tags/')) data = releases.find(r => r.tag_name === path.slice((api + '/tags/').length));
  if (data) { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(data)); }
  else if (files.has(path)) await pipeline(createReadStream(files.get(path)), res);
  else { res.statusCode = 404; res.end(); }
});
let base;
function write(path, content) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, content);
}
function release(tag, assets, prerelease) {
  const r = { tag_name: tag, draft: false, prerelease, assets: assets.map(file => {
    const name = basename(file);
    const path = '/' + tag + '/' + name;
    files.set(path, file);
    return { name, browser_download_url: base + path };
  }) };
  releases.unshift(r);
  return r;
}
async function pack(archive, source, name) {
  const temp = mkdtempSync(join(root, 'archive '));
  const member = windows ? name : join(basename(archive, '.tar.xz'), name);
  const dest = join(temp, member);
  mkdirSync(dirname(dest), { recursive: true });
  try {
    try { linkSync(source, dest); } catch (error) { if (error.code !== 'EXDEV') throw error; copyFileSync(source, dest); }
    if (windows) await exec('tar', ['-caf', archive, member], { cwd: temp, env: safeEnv });
    else {
      const tar = spawn('tar', ['cf', '-', member], { cwd: temp });
      const xz = spawn('xz', ['-0c']);
      const finished = child => new Promise((ok, fail) => {
        child.on('error', fail);
        child.on('close', code => code === 0 ? ok() : fail(Error('archive tool exited ' + code)));
      });
      await Promise.all([finished(tar), finished(xz), pipeline(tar.stdout, xz.stdin), pipeline(xz.stdout, createWriteStream(archive))]);
    }
    write(archive + '.sha256', digest(archive) + '  ' + basename(archive) + '\n');
  } finally { rmSync(temp, { recursive: true, force: true }); }
}
async function prepare(dir) {
  const native = {};
  for (const [pkg, name] of Object.entries(names)) {
    const path = join(dir, name + ext);
    const info = JSON.parse(execFileSync(path, ['--json', 'build-info'], { env: safeEnv }));
    assert.equal(info.product, pkg);
    assert.ok(info.source_revision);
    native[pkg] = info;
  }
  const { version, target, source_revision } = native.bitrouter;
  assert.equal(native['bitrouter-bro'].version, version);
  assert.equal(native['bitrouter-bro'].source_revision, source_revision);
  const tag = 'v' + version;
  const work = join(root, tag);
  const dist = process.env.DIST || 'dist';
  const build = (cmd, args) => execFileSync(cmd, args, { cwd: work, encoding: 'utf8',
    env: { ...process.env, CARGO_TARGET_DIR: join(work, 'target'), BITROUTER_BUILD_REVISION: source_revision } });
  write(join(work, 'Cargo.toml'), '[workspace]\nmembers = ["bitrouter","bitrouter-bro"]\nresolver="3"\n[profile.dist]\ninherits="release"\n');
  write(join(work, 'dist-workspace.toml'), readFileSync(join(scripts, '../../../dist-workspace.toml'), 'utf8')
    .replace('[dist]', '[dist]\nsource-tarball = false'));
  build('git', ['init', '-q']);
  build('git', ['remote', 'add', 'origin', 'https://github.com/bitrouter/bitrouter.git']);
  for (const [pkg, name] of Object.entries(names)) {
    write(join(work, pkg, 'Cargo.toml'), `[package]\nname="${pkg}"\nversion="${version}"\nedition="2024"\nrepository="https://github.com/bitrouter/bitrouter"\n[[bin]]\nname="${name}"\npath="src/main.rs"\n`);
    write(join(work, pkg, 'src/main.rs'), 'fn main() {}');
  }
  write(join(work, 'plan.json'), build(dist, ['plan', '--tag', tag, '-o', 'json']));
  const output = join(work, 'target/distrib');
  const inventory = join(output, 'distribution-plan.json');
  write(inventory, build(process.execPath, [join(scripts, 'release-plan.mjs'), 'plan.json']));
  write(join(work, 'global.json'), build(dist, ['build', '--artifacts=global', '--tag', tag, '-o', 'json']));
  build(process.execPath, [join(scripts, 'guard-installers.mjs'), 'global.json']);
  const assets = [inventory];
  for (const product of JSON.parse(readFileSync(inventory)).products) {
    const dist = product.distributions.find(d => d.target === target && (!d.profile || d.profile === 'full'));
    const archive = join(output, dist.archive);
    const name = names[product.package] + ext;
    await pack(archive, join(dir, name), name);
    assets.push(archive, archive + '.sha256', join(output, product.package + '-installer.' + (windows ? 'ps1' : 'sh')));
  }
  const entry = release(tag, assets, version.includes('-'));
  return { entry, tag, output, version, inventory, binaries: dir };
}
// Keep the native release archives intact. Copy only the files the test
// mutates. The immutable originals also supply the independently expected bytes.
function fromArchives(dir) {
  const plan = JSON.parse(readFileSync(join(dir, 'distribution-plan.json')));
  const target = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)[1].trim();
  const output = join(root, plan.tag), binaries = join(output, 'bin');
  const inventory = join(output, 'distribution-plan.json');
  mkdirSync(binaries, { recursive: true });
  copyFileSync(join(dir, 'distribution-plan.json'), inventory);
  const assets = [inventory];
  const version = plan.products[0].version;
  assert.deepEqual(plan.products.map(p => p.package).sort(), Object.keys(names).sort());
  for (const product of plan.products) {
    const dist = product.distributions.find(d => d.target === target && (!d.profile || d.profile === 'full'));
    assert.equal(product.version, version);
    assert.ok(dist, `missing native ${product.package} archive for ${target}`);
    const installer = product.package + '-installer.' + (windows ? 'ps1' : 'sh');
    for (const file of [dist.archive, dist.checksum, installer]) {
      const dest = join(output, file);
      copyFileSync(join(dir, file), dest);
      assets.push(dest);
    }
    const extracted = mkdtempSync(join(root, 'extracted-'));
    execFileSync('tar', ['xf', join(output, dist.archive), '-C', extracted], { env: safeEnv });
    const member = windows ? join(extracted, dist.binary) : join(extracted, basename(dist.archive, '.tar.xz'), dist.binary);
    copyFileSync(member, join(binaries, dist.binary));
    const info = JSON.parse(execFileSync(member, ['--json', 'build-info'], { env: safeEnv }));
    assert.equal(info.product, product.package);
    assert.equal(info.version, version);
    assert.equal(info.target, target);
    assert.equal(info.source_revision, plan.source_revision);
    rmSync(extracted, { recursive: true });
    console.log(`Verified ${dist.archive} contains ${dist.binary} ${version}`);
  }
  return { tag: plan.tag, version, inventory, output, binaries, entry: release(plan.tag, assets, version.includes('-')) };
}
function legacyRelease() {
  const version = '0.9.0', tag = 'v' + version;
  const work = join(root, 'M0');
  const run = (cmd, args) => execFileSync(cmd, args, { cwd: work, encoding: 'utf8', env: { ...process.env, CARGO_TARGET_DIR: join(work, 'target') } });
  write(join(work, 'Cargo.toml'), '[workspace]\nmembers=["bitrouter"]\nresolver="3"\n[profile.dist]\ninherits="release"\n');
  write(join(work, 'bitrouter/Cargo.toml'), `[package]\nname="bitrouter"\nversion="${version}"\nedition="2024"\nrepository="https://github.com/bitrouter/bitrouter"\n[[bin]]\nname="bro"\npath="src/main.rs"\n`);
  write(join(work, 'bitrouter/src/main.rs'), 'fn main() {}');
  const cfg = readFileSync(join(scripts, '../../../dist-workspace.toml'), 'utf8').replace('[dist]', '[dist]\nsource-tarball = false');
  write(join(work, 'dist-workspace.toml'), cfg + '\n[dist.bin-aliases]\nbro=["bitrouter"]\n');
  run('git', ['init', '-q']);
  run('git', ['remote', 'add', 'origin', 'https://github.com/bitrouter/bitrouter.git']);
  const manifest = JSON.parse(run(process.env.DIST || 'dist', ['build', '--artifacts=global', '--tag', tag, '-o', 'json']));
  assert.equal(manifest.releases[0].app_name, 'bitrouter');
  const files = manifest.upload_files.filter(f => f.endsWith('-installer.sh') || f.endsWith('-installer.ps1'));
  assert.ok(files.length);
  return release(tag, files.map(f => resolve(work, f)), false);
}
async function update(pkg, flags, refusal) {
  const env = { ...safeEnv, BITROUTER_INSTALLER_GHE_BASE_URL: base, BITROUTER_BRO_INSTALLER_GHE_BASE_URL: base };
  const command = exec(bin(pkg), ['--json', 'update', ...flags], { env, cwd: root });
  if (refusal) {
    await assert.rejects(command, e => (assert.match(e.stdout + e.stderr, refusal), true));
  } else return JSON.parse((await command).stdout);
}
function state(pkg) { return [digest(bin(pkg)), digest(receipt(pkg))]; }
try {
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  base = 'http://127.0.0.1:' + server.address().port;
  write(join(safeEnv.BITROUTER_HOME, 'bitrouter.yaml'), '{}\n');
  const prepareRelease = archiveMode ? fromArchives : prepare;
  const old = await prepareRelease(before), next = await prepareRelease(after);
  for (const pkg of Object.keys(names)) {
    const script = join(old.output, pkg + '-installer.' + (windows ? 'ps1' : 'sh'));
    const args = windows ? ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script] : [script];
    execFileSync(windows ? 'powershell.exe' : 'sh', args, { cwd: root,
      env: { ...safeEnv, INSTALLER_DOWNLOAD_URL: pathToFileURL(old.output).href } });
  }
  for (const [pkg, peer] of [Object.keys(names), Object.keys(names).reverse()]) {
    const peerBefore = state(peer), original = state(pkg);
    assert.equal((await update(pkg, ['--check'])).target_version, next.version);
    assert.deepEqual(state(pkg), original);
    assert.equal((await update(pkg, ['-y'])).target_version, next.version);
    assert.equal(digest(bin(pkg)), digest(join(next.binaries, names[pkg] + ext)));
    assert.equal(JSON.parse(readFileSync(receipt(pkg))).version, next.version);
    assert.deepEqual(state(peer), peerBefore);
    assert.equal((await update(pkg, ['--tag', old.tag, '-y'])).target_version, old.version);
    assert.equal(digest(bin(pkg)), original[0]);
    assert.equal(JSON.parse(readFileSync(receipt(pkg))).binary_sha256, original[0]);
    assert.deepEqual(state(peer), peerBefore);
    assert.equal(JSON.parse(execFileSync(bin(peer), ['--json', 'build-info'], { env: safeEnv })).product, peer);
    console.log(`Verified ${names[pkg]} update ${old.version} -> ${next.version} -> ${old.version} and unchanged ${names[peer]}`);
    // A process stopped after the executable swap has the old published
    // receipt and the matching candidate still in the installer journal.
    const oldReceipt = readFileSync(receipt(pkg));
    const upgraded = await update(pkg, ['--tag', next.tag, '-y']);
    assert.equal(upgraded.target_version, next.version);
    const newReceipt = readFileSync(receipt(pkg));
    const lock = join(dirname(receipt(pkg)), '.install-lock'), journal = join(lock, 'receipt');
    const binLock = join(dirname(bin(pkg)), '.' + pkg + '-install-lock');
    mkdirSync(join(lock, 'lease-v1'), { recursive: true });
    mkdirSync(join(binLock, 'lease-v1'), { recursive: true });
    write(receipt(pkg), oldReceipt);
    write(journal, newReceipt);
    await update(pkg, ['--check']);
    assert.deepEqual(readFileSync(receipt(pkg)), oldReceipt);
    assert.deepEqual(readFileSync(journal), newReceipt);
    const wrongPrefix = JSON.stringify({ ...JSON.parse(newReceipt), install_prefix: root });
    write(journal, wrongPrefix);
    await update(pkg, ['-y'], /prefix/i);
    assert.deepEqual(readFileSync(receipt(pkg)), oldReceipt);
    assert.equal(readFileSync(journal, 'utf8'), wrongPrefix);
    write(journal, newReceipt);
    assert.equal((await update(pkg, ['-y'])).status, 'unchanged');
    assert.deepEqual(readFileSync(receipt(pkg)), newReceipt);
    assert.ok(!existsSync(binLock) && !existsSync(lock));
    assert.deepEqual(state(peer), peerBefore);
    await update(pkg, ['--tag', old.tag, '-y']);
    const owned = state(pkg);
    const record = readFileSync(receipt(pkg));
    for (const bad of [
      { ...JSON.parse(record), binaries: ['bro' + ext], binary_aliases: { ['bro' + ext]: ['bitrouter' + ext] } },
      { ...JSON.parse(record), install_prefix: root },
      { ...JSON.parse(record), binary_sha256: '0'.repeat(64) },
      { ...JSON.parse(record), source: JSON.parse(readFileSync(receipt(peer))).source },
    ]) {
      write(receipt(pkg), JSON.stringify(bad));
      const count = requests.length;
      await update(pkg, ['--tag', next.tag, '-y'], /receipt|digest mismatch/i);
      assert.equal(requests.length, count);
      assert.equal(digest(bin(pkg)), owned[0]);
    }
    write(receipt(pkg), record);
    const flat = { ...JSON.parse(record), install_layout: 'flat', install_prefix: dirname(bin(pkg)) };
    write(receipt(pkg), JSON.stringify(flat));
    assert.equal((await update(pkg, ['--check'])).target_version, next.version);
    write(receipt(pkg), record);
    // A valid inventory that points at the wrong native bytes must fail before replacement.
    const info = JSON.parse(readFileSync(next.inventory));
    const dist = info.products.find(p => p.package === pkg).distributions.find(d => d.target === JSON.parse(execFileSync(bin(pkg), ['--json','build-info'], {env:safeEnv})).target && (!d.profile || d.profile === 'full'));
    const sidecar = join(next.output, dist.checksum);
    const saved = readFileSync(sidecar);
    write(sidecar, '0'.repeat(64) + '  ' + dist.archive + '\n');
    await update(pkg, ['--tag', next.tag, '-y'], /checksum mismatch/i);
    write(sidecar, saved);
    // Exercise the actual executable identity check with a valid archive and checksum.
    const alternate = join(root, 'wrong ' + pkg, dist.archive);
    mkdirSync(dirname(alternate));
    await pack(alternate, join(next.binaries, names[peer] + ext), dist.binary);
    const archiveURL = '/' + next.tag + '/' + dist.archive;
    const goodArchive = files.get(archiveURL), goodChecksum = files.get(archiveURL + '.sha256');
    files.set(archiveURL, alternate);
    files.set(archiveURL + '.sha256', alternate + '.sha256');
    await update(pkg, ['--tag', next.tag, '-y'], /staged build-info/i);
    files.set(archiveURL, goodArchive);
    files.set(archiveURL + '.sha256', goodChecksum);
    const originalPlan = readFileSync(next.inventory);
    info.source_revision += '-mismatch';
    write(next.inventory, JSON.stringify(info));
    await update(pkg, ['-y'], /staged build-info/i);
    write(next.inventory, originalPlan);
    assert.deepEqual(state(pkg), owned);
    assert.deepEqual(state(peer), peerBefore);
  }
  // Package-scoped publications carry only the assets for the named product.
  for (const [pkg, peer] of [Object.keys(names), Object.keys(names).reverse()]) {
    const tag = pkg + '-' + next.tag;
    const plan = JSON.parse(readFileSync(next.inventory));
    plan.tag = tag;
    plan.products = plan.products.filter(p => p.package === pkg);
    const inventory = join(root, tag, 'distribution-plan.json');
    write(inventory, JSON.stringify(plan));
    const namesForProduct = new Set(plan.products[0].installers.concat(
      plan.products[0].distributions.flatMap(d => [d.archive, d.checksum])));
    release(tag, [inventory, ...next.entry.assets.filter(a => namesForProduct.has(a.name))
      .map(a => join(next.output, a.name))], true);
    assert.equal((await update(pkg, ['--tag', tag, '--check'])).target_version, next.version);
    await update(peer, ['--tag', tag, '-y'], /not a release for this product/);
  }
  const m0 = legacyRelease();
  for (const pkg of Object.keys(names)) {
    const unchanged = state(pkg);
    await update(pkg, ['--tag', m0.tag_name, '-y'], /distribution-plan.json/);
    assert.deepEqual(state(pkg), unchanged);
  }
  // A stable legacy release can win latest selection for the service. Reject
  // it before any legacy installer or executable is requested.
  const serviceBefore = state('bitrouter');
  await update('bitrouter', ['--stable', '-y'], /distribution-plan.json/);
  assert.deepEqual(state('bitrouter'), serviceBefore);
  assert.ok(!requests.some(path => path.startsWith('/' + m0.tag_name + '/')));
  assert.ok(!existsSync(safeEnv.BRO_HOME));
  console.log('Passed both CLI upgrades, pinned rollbacks, peer isolation and release/receipt refusal checks.');
} finally {
  server.close();
  rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
