// Project the cargo-dist plan and the reusable gateway build matrix into the
// inventory shipped with this tag. Package-scoped tags own only their assets.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const plan = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const targets = ['aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu',
  'x86_64-unknown-linux-musl', 'x86_64-pc-windows-msvc'];
const tag = plan.announcement_tag;
const scoped = tag.match(/(?:^|\/)(bitrouter(?:-bro)?)[-/]v?\d/);
const expected = scoped ? [scoped[1]] : ['bitrouter', 'bitrouter-bro'];
assert.deepEqual(plan.releases.map(r => r.app_name).sort(), expected);
const products = plan.releases.map(({ app_name: pkg, app_version: version, artifacts }) => {
  const bin = pkg === 'bitrouter' ? 'bitrouter' : 'bro';
  const installers = ['-installer.sh', '-installer.ps1', '.rb', '-npm-package.tar.gz'].map(suffix => pkg + suffix);
  for (const name of installers) assert.ok(artifacts.includes(name), `missing ${name}`);
  const distributions = targets.flatMap(target => {
    const windows = target.includes('windows');
    const binary = bin + (windows ? '.exe' : '');
    const archive = `${pkg}-${target}.${windows ? 'zip' : 'tar.xz'}`;
    const checksum = archive + '.sha256';
    assert.ok(artifacts.includes(archive) && artifacts.includes(checksum), `missing ${archive} or checksum`);
    const declared = plan.artifacts[archive];
    assert.equal(declared.checksum, checksum);
    assert.deepEqual(declared.target_triples, [target]);
    assert.deepEqual(declared.assets.filter(a => a.kind === 'executable').map(a => a.path), [binary]);
    const primary = { target, binary, archive, checksum };
    const variants = [primary];
    if (pkg === 'bitrouter') {
      primary.profile = 'full';
      for (const profile of ['gateway', 'gateway-shared']) {
        const archive = `bitrouter-${profile}-${target}.${windows ? 'zip' : 'tar.gz'}`;
        variants.push({ profile, target, binary, archive, checksum: archive + '.sha256' });
      }
    }
    return variants;
  });
  return { package: pkg, version, executable: bin, installers, distributions };
});
console.log(JSON.stringify({ tag, source_revision: process.env.BITROUTER_BUILD_REVISION ?? null, products }, null, 2));
