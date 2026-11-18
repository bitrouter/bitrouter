// Inventory real installer roots and reconcile each executable against its archive.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, lstatSync, readFileSync, readdirSync, readlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [installs, out, target] = process.argv.slice(2);
const baseline = process.env.ROLE === 'm0';
const executable = baseline ? 'bro' : 'bitrouter';
const { SOURCE_SHA: revision } = process.env;
const read = path => JSON.parse(readFileSync(path, 'utf8'));
const run = (cmd, args) => execFileSync(cmd, args, { encoding: 'utf8' }).trim();
async function digest(path) {
  const hash = createHash('sha256');
  for await (const buffer of createReadStream(path)) hash.update(buffer);
  return hash.digest('hex');
}
async function inventory(root) {
  const files = [], seen = new Set();
  let bytes = 0;
  async function visit(relative) {
    for (const name of readdirSync(join(root, relative)).sort()) {
      const path = join(relative, name), full = join(root, path);
      const st = lstatSync(full, { bigint: true });
      if (st.isDirectory()) await visit(path);
      else if (st.isSymbolicLink()) files.push({ path, target: readlinkSync(full) });
      else {
        assert.ok(st.isFile(), full);
        const key = st.dev + '/' + st.ino;
        if (!seen.has(key)) bytes += Number(st.size);
        seen.add(key);
        files.push({ path, bytes: Number(st.size), sha256: await digest(full) });
      }
    }
  }
  await visit('');
  return { install_bytes: bytes, files };
}

const results = {};
for (const role of readdirSync(installs).sort()) {
  const root = join(installs, role);
  const entry = await inventory(root);
  const profile = role.endsWith('-minimum') ? 'gateway' : role.endsWith('-shared') ? 'gateway-shared' : null;
  const packages = role === 'both' ? ['bitrouter', 'bitrouter-bro'] : [role === 'bro' ? 'bitrouter-bro' : 'bitrouter'];
  entry.binaries = {};
  entry.archives = [];
  for (const pkg of packages) {
    const binary = pkg === 'bitrouter-bro' ? 'bro' : executable;
    const basename = profile ? 'bitrouter-' + profile + '-' + target : pkg + '-' + target;
    const archive = basename + (profile ? '.tar.gz' : '.tar.xz');
    const archivePath = join(out, 'archives', archive);
    const checksum = await digest(archivePath);
    assert.deepEqual(readFileSync(archivePath + '.sha256', 'utf8').trim().split(/\s+\*?/), [checksum, archive]);
    const rel = join(profile ? basename : 'cargo/bin', binary);
    const file = entry.files.find(f => f.path === rel);
    assert.ok(file?.sha256, rel);
    // Stream the member to sha256sum so full release binaries do not live in memory.
    const memberHash = run('bash', ['-o', 'pipefail', '-c', 'tar -xOf "$1" "$2" | sha256sum', 'm5',
      archivePath, basename + '/' + binary]).split(/\s/)[0];
    assert.equal(memberHash, file.sha256);
    const info = JSON.parse(run(join(root, rel), ['--json', 'build-info']));
    assert.equal(info.source_revision, revision);
    assert.equal(info.target, target);
    if (!baseline) assert.equal(info.product, pkg);
    if (profile) {
      assert.equal(info.profile, profile);
      const manifest = read(join(root, basename, 'manifest.json'));
      assert.equal(manifest.cargo_profile, 'dist');
      assert.equal(manifest.binary.sha256, file.sha256);
      assert.equal(manifest.binary.path, binary);
      assert.deepEqual(info.features, profile === 'gateway' ? [] : ['db-postgres', 'multi-instance']);
    } else {
      const receipt = read(join(root, 'config', pkg, pkg + '-receipt.json'));
      assert.equal(receipt.source.app_name, pkg);
      assert.deepEqual(receipt.binaries, [binary]);
      if (!baseline) assert.equal(receipt.binary_sha256, file.sha256);
      if (pkg === 'bitrouter') assert.equal(info.profile, 'full');
    }
    entry.binaries[rel] = { ...file, info };
    entry.archives.push({ archive, bytes: Number(lstatSync(archivePath).size), sha256: checksum,
      member: basename + '/' + binary });
  }
  results[role] = entry;
}
assert.deepEqual(Object.keys(results).sort(), baseline ? ['m0-full', 'm0-minimum', 'm0-shared']
  : ['both', 'bro', 'service-default', 'service-minimum', 'service-shared']);
if (baseline) {
  const alias = join(installs, 'm0-full/cargo/bin/bitrouter');
  assert.equal(await digest(alias), results['m0-full'].binaries['cargo/bin/bro'].sha256);
}
writeFileSync(join(out, 'footprints.json'), JSON.stringify({ revision, target, results }, null, 2) + '\n', {flag: 'wx'});
