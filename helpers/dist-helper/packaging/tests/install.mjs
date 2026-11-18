// Exercise the actual cargo-dist installers offline, with two tiny Cargo packages
// and two real versioned releases. Pass the pinned dist executable on PATH.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { appendFileSync, copyFileSync, cpSync, existsSync, linkSync, mkdirSync, mkdtempSync, readFileSync, rmSync,
  symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const scripts = fileURLToPath(new URL('../', import.meta.url));
const root = mkdtempSync(join(tmpdir(), 'split-install-'));
const windows = process.platform === 'win32';
const ext = windows ? '.exe' : '';
const names = { bitrouter: 'bitrouter', 'bitrouter-bro': 'bro' };
const versions = ['1.0.0-beta.1', '1.0.0-beta.2'];
const host = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)[1].trim();
const buildEnv = { ...process.env, BITROUTER_BUILD_REVISION: 'fixture-revision' };
const dist = process.env.DIST || 'dist';
const run = (program, args) => execFileSync(program, args, { cwd: root, env: buildEnv, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
function write(path, data) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
}

function buildReleases() {
  run('git', ['init', '-q']);
  run('git', ['remote', 'add', 'origin', 'https://github.com/bitrouter/bitrouter.git']);
  // Keep the production five-target plan for validation and generate real
  // checksums for the one native target used by this offline test.
  const config = readFileSync(join(scripts, '../../../dist-workspace.toml'), 'utf8');
  write(join(root, 'dist-workspace.toml'), config.replace('[dist]', '[dist]\nsource-tarball = false'));
  write(join(root, 'Cargo.toml'), '[workspace]\nmembers = ["bitrouter","bitrouter-bro"]\nresolver = "3"\n[profile.dist]\ninherits = "release"\n');
  for (const version of versions) {
    for (const [pkg, binary] of Object.entries(names)) {
      write(join(root, pkg, 'Cargo.toml'), `[package]
name = "${pkg}"
version = "${version}"
edition = "2024"
repository = "https://github.com/bitrouter/bitrouter"
description = "packaging test fixture"
[[bin]]
name = "${binary}"
path = "src/main.rs"
`);
      const info = { version, target: host, source_revision: buildEnv.BITROUTER_BUILD_REVISION,
        ...(pkg === 'bitrouter' ? { profile: 'full' } : { product: pkg }) };
      write(join(root, pkg, 'src/main.rs'), `fn main() {
  if std::env::args().nth(1).as_deref() == Some("--version") { println!("${binary} ${version}"); }
  else { println!("{}", r#"${JSON.stringify(info)}"#); }
}\n`);
    }
    const plan = join(root, 'plan.json');
    write(plan, run(dist, ['plan', '-o', 'json']));
    run(process.execPath, [join(scripts, 'release-plan.mjs'), plan]);
    for (const pkg of Object.keys(names)) {
      const scoped = join(root, 'scoped-plan.json');
      write(scoped, run(dist, ['plan', '--tag', `${pkg}-v${version}`, '-o', 'json']));
      const result = JSON.parse(run(process.execPath, [join(scripts, 'release-plan.mjs'), scoped]));
      assert.deepEqual(result.products.map(product => product.package), [pkg]);
    }
    const assets = join(root, 'target', 'distrib');
    write(join(assets, 'plan-dist-manifest.json'), readFileSync(plan));
    const args = ['--artifacts=local', '--target', host, '-o', 'json'];
    const local = run(dist, ['build', ...args]);
    const localManifest = join(assets, 'local-dist-manifest.json');
    write(localManifest, local);
    run(process.execPath, [join(scripts, 'verify-archives.mjs'), localManifest]);
    const global = join(root, 'global.json');
    write(global, run(dist, ['build', '--artifacts=global', '-o', 'json']));
    run(process.execPath, [join(scripts, 'guard-installers.mjs'), global]);
    cpSync(assets, join(root, version), { recursive: true });
    rmSync(assets, { recursive: true });
  }
}

function prefix(label) {
  const base = join(root, label);
  return { base, bin: join(base, 'cargo', 'bin'), config: join(base, 'config') };
}
const receipt = (p, pkg) => join(p.config, pkg, pkg + '-receipt.json');
const binary = (p, pkg) => join(p.bin, names[pkg] + ext);
function install(p, pkg, version, refuse) {
  const dir = join(root, version);
  const file = join(dir, pkg + '-installer.' + (windows ? 'ps1' : 'sh'));
  const env = { ...process.env, CARGO_HOME: join(p.base, 'cargo'), XDG_CONFIG_HOME: p.config,
    HOME: p.base, INSTALLER_NO_MODIFY_PATH: '1', INSTALLER_DOWNLOAD_URL: pathToFileURL(dir).href };
  const result = windows
    ? spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', file], { env, encoding: 'utf8' })
    : spawnSync('sh', [file], { env, encoding: 'utf8' });
  const output = result.stdout + result.stderr;
  if (refuse) {
    assert.notEqual(result.status, 0);
    assert.match(output, refuse);
  } else {
    assert.equal(result.status, 0, output);
    const info = JSON.parse(execFileSync(binary(p, pkg)));
    assert.equal(info.version, version);
    const record = JSON.parse(readFileSync(receipt(p, pkg)));
    assert.equal(record.source.app_name, pkg);
    assert.deepEqual(record.binaries, [names[pkg] + ext]);
    assert.match(record.binary_sha256, /^[a-f0-9]{64}$/);
  }
}

function testInstalls() {
  const [old, next] = versions;
  for (const [first, second] of [Object.keys(names), Object.keys(names).reverse()]) {
    const p = prefix(first + '-first');
    install(p, first, old);
    assert.ok(!existsSync(binary(p, second)) && !existsSync(receipt(p, second)));
    install(p, second, old);
    for (const [owner, peer] of [[first, second], [second, first]]) {
      const peerRecord = readFileSync(receipt(p, peer));
      const peerBinary = readFileSync(binary(p, peer));
      for (const version of [next, old]) {
        install(p, owner, version);
        assert.deepEqual(readFileSync(receipt(p, peer)), peerRecord);
        assert.deepEqual(readFileSync(binary(p, peer)), peerBinary);
        assert.equal(JSON.parse(execFileSync(binary(p, peer))).version, old);
      }
    }
    writeFileSync(binary(p, first), 'manual replacement');
    install(p, first, next, /digest mismatch/);
    assert.equal(readFileSync(binary(p, first), 'utf8'), 'manual replacement');
  }
  const p = prefix('legacy');
  const bro = binary(p, 'bitrouter-bro');
  const alias = binary(p, 'bitrouter');
  write(bro, 'legacy combined binary');
  if (windows) linkSync(bro, alias); else symlinkSync(names['bitrouter-bro'], alias);
  const legacy = { source: { app_name: 'bitrouter', name: 'bitrouter', owner: 'bitrouter', release_type: 'github' },
    provider: { source: 'cargo-dist', version: '0.31.0' }, binaries: ['bro' + ext],
    binary_aliases: { ['bro' + ext]: ['bitrouter' + ext] }, version: old,
    install_layout: 'cargo-home', install_prefix: join(p.base, 'cargo') };
  write(receipt(p, 'bitrouter'), JSON.stringify(legacy));
  for (const pkg of Object.keys(names)) {
    install(p, pkg, next, /Verified legacy BitRouter installation/);
    assert.equal(readFileSync(alias, 'utf8'), 'legacy combined binary');
    assert.equal(readFileSync(receipt(p, 'bitrouter'), 'utf8'), JSON.stringify(legacy));
  }
  write(receipt(p, 'bitrouter'), JSON.stringify({ ...legacy, install_prefix: root }));
  install(p, 'bitrouter', next, /[Rr]efusing|Refusing to replace linked/);
  assert.equal(readFileSync(alias, 'utf8'), 'legacy combined binary');

  // A manual archive (including an extracted reduced service) has no receipt.
  const manual = prefix('manual');
  write(binary(manual, 'bitrouter'), 'manual archive');
  install(manual, 'bitrouter', next, /[Rr]efusing|no matching receipt/);
  assert.equal(readFileSync(binary(manual, 'bitrouter'), 'utf8'), 'manual archive');

  // A copied receipt from a different prefix cannot authorize a replacement.
  const stale = prefix('stale-prefix');
  const source = prefix('bitrouter-bro-first');
  mkdirSync(stale.bin, { recursive: true });
  copyFileSync(binary(source, 'bitrouter'), binary(stale, 'bitrouter'));
  write(receipt(stale, 'bitrouter'), readFileSync(receipt(source, 'bitrouter')));
  install(stale, 'bitrouter', next, /digest mismatch/);

  const archive = join(root, next, `bitrouter-${host}.${windows ? 'zip' : 'tar.xz'}`);
  appendFileSync(archive, 'damaged release');
  const receiptBefore = readFileSync(receipt(source, 'bitrouter'));
  install(source, 'bitrouter', next, /checksum mismatch/i);
  assert.deepEqual(readFileSync(receipt(source, 'bitrouter')), receiptBefore);
  assert.equal(JSON.parse(execFileSync(binary(source, 'bitrouter'))).version, old);
}

try {
  buildReleases();
  testInstalls();
  console.log(`Passed release plans, packaged identity, install ownership, upgrade and rollback on ${host}`);
} finally {
  rmSync(root, { recursive: true, force: true });
}
