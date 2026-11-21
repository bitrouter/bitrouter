// Exercise the actual cargo-dist installers offline, with two tiny Cargo packages
// and two real versioned releases. Pass the pinned dist executable on PATH.
import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { appendFileSync, copyFileSync, cpSync, existsSync, linkSync, mkdirSync, readFileSync, renameSync, rmSync,
  symlinkSync, writeFileSync } from 'node:fs';
import { installEnv, tempRoot } from './environment.mjs';
import { dirname, join } from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath, pathToFileURL } from 'node:url';

const scripts = fileURLToPath(new URL('../', import.meta.url));
const root = tempRoot('split-install-');
const windows = process.platform === 'win32';
const ext = windows ? '.exe' : '';
const names = { bitrouter: 'bitrouter', 'bitrouter-bro': 'bro' };
const versions = ['1.0.0-beta.1', '1.0.0-beta.2'];
const host = execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)[1].trim();
const buildEnv = { ...process.env, CARGO_TARGET_DIR: join(root, 'target'), BITROUTER_BUILD_REVISION: 'fixture-revision' };
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
        product: pkg, ...(pkg === 'bitrouter' ? { profile: 'full' } : {}) };
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
function installer(p, pkg, version, interrupt) {
  const dir = join(root, version);
  let file = join(dir, pkg + '-installer.' + (windows ? 'ps1' : 'sh'));
  if (interrupt) {
    const script = readFileSync(file, 'utf8');
    const marker = interrupt === 'before' || interrupt === 'hold'
      ? (windows ? '  $dest_dir = New-Item -Force -ItemType Directory -Path $dest_dir' : '    say "installing to $_install_dir"')
      : (windows ? '  # Write the install receipt' : '    # Install the install receipt');
    assert.equal(script.split(marker).length, 2);
    file = join(dir, 'interrupt-' + pkg + (windows ? '.ps1' : '.sh'));
    let hook = windows ? '[Diagnostics.Process]::GetCurrentProcess().Kill()' : 'kill -KILL "$$"';
    if (interrupt === 'hold') {
      hook = windows ? `[IO.File]::WriteAllText((Join-Path $env:USERPROFILE 'ready'), '')
        while (!(Test-Path -LiteralPath (Join-Path $env:USERPROFILE 'resume'))) { Start-Sleep -Milliseconds 50 }`
        : 'touch "$HOME/ready"; while [ ! -f "$HOME/resume" ]; do sleep 0.1; done';
    }
    write(file, script.replace(marker, () => hook + '\n' + marker));
  }
  const env = { ...installEnv(p.base, join(p.base, 'cargo'), p.config),
    INSTALLER_DOWNLOAD_URL: pathToFileURL(dir).href };
  return windows ? ['powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', file], { env, encoding: 'utf8' }]
    : ['sh', [file], { env, encoding: 'utf8' }];
}
function install(p, pkg, version, refuse, interrupt) {
  const result = spawnSync(...installer(p, pkg, version, interrupt));
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
    assert.equal(record.binary_sha256, createHash('sha256').update(readFileSync(binary(p, pkg))).digest('hex'));
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
  const p = prefix('linked-products');
  for (const pkg of Object.keys(names)) install(p, pkg, old);
  const bro = binary(p, 'bitrouter-bro');
  const service = binary(p, 'bitrouter');
  const records = Object.fromEntries(Object.keys(names).map(pkg => [pkg, readFileSync(receipt(p, pkg))]));
  const original = readFileSync(service);
  rmSync(bro);
  if (windows) linkSync(service, bro); else symlinkSync(names.bitrouter, bro);
  for (const pkg of Object.keys(names)) {
    install(p, pkg, next, /[Rr]efusing.*linked/);
    assert.deepEqual(readFileSync(service), original);
    assert.deepEqual(readFileSync(bro), original);
    for (const peer of Object.keys(names)) assert.deepEqual(readFileSync(receipt(p, peer)), records[peer]);
  }

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

  for (const pkg of Object.keys(names)) {
    const recover = prefix('recovery-' + pkg);
    const lock = join(dirname(receipt(recover, pkg)), '.install-lock');
    const journal = join(lock, 'receipt');
    const binLock = join(recover.bin, '.' + pkg + '-install-lock');
    // Kill the generated installer itself, including its cleanup handler.
    install(recover, pkg, old, /.*/, 'after');
    assert.ok(!existsSync(receipt(recover, pkg)));
    assert.equal(JSON.parse(readFileSync(journal)).version, old);
    install(recover, pkg, old);
    install(recover, pkg, next, /.*/, 'before');
    assert.equal(JSON.parse(execFileSync(binary(recover, pkg))).version, old);
    if (windows) {
      // The native updater relocates the running EXE. Exercise its missing-image
      // window with the same retained recovery copy the updater would leave.
      renameSync(binary(recover, pkg), join(lock, 'previous.exe'));
    }
    install(recover, pkg, next);
    install(recover, pkg, old, /.*/, 'after');
    assert.equal(JSON.parse(readFileSync(receipt(recover, pkg))).version, next);
    const valid = readFileSync(journal);
    const original = readFileSync(binary(recover, pkg));
    writeFileSync(journal, JSON.stringify({ ...JSON.parse(valid), install_prefix: root }));
    install(recover, pkg, next, /prefix mismatch|prefix or binary digest mismatch/i);
    assert.deepEqual(readFileSync(binary(recover, pkg)), original);
    assert.equal(JSON.parse(readFileSync(journal)).install_prefix, root);
    writeFileSync(journal, valid);
    install(recover, pkg, old);
    assert.ok(!existsSync(lock) && !existsSync(binLock));
    mkdirSync(binLock); // Unmarked locks could belong to an active older writer.
    install(recover, pkg, next, /unrecognized installer lock/i);
    assert.ok(existsSync(binLock));
    assert.deepEqual(readFileSync(binary(recover, pkg)), original);
  }

  const archive = join(root, next, `bitrouter-${host}.${windows ? 'zip' : 'tar.xz'}`);
  appendFileSync(archive, 'damaged release');
  const receiptBefore = readFileSync(receipt(source, 'bitrouter'));
  install(source, 'bitrouter', next, /checksum mismatch/i);
  assert.deepEqual(readFileSync(receipt(source, 'bitrouter')), receiptBefore);
  assert.equal(JSON.parse(execFileSync(binary(source, 'bitrouter'))).version, old);
}

async function testContention() {
  const p = prefix('held-lock'), other = { ...prefix('other-prefix'), config: p.config };
  const owner = 'bitrouter-bro';
  const holder = spawn(...installer(p, owner, versions[0], 'hold'));
  let diagnostic = '';
  holder.stdout.on('data', data => { diagnostic += data; });
  holder.stderr.on('data', data => { diagnostic += data; });
  const finished = new Promise((resolve, reject) => {
    holder.on('error', reject);
    holder.on('close', resolve);
  });
  try {
    const deadline = Date.now() + 60_000;
    while (!existsSync(join(p.base, 'ready'))) {
      assert.ok(holder.exitCode === null && Date.now() < deadline, 'installer did not take locks: ' + diagnostic);
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    for (const dest of [p, other]) {
      install(dest, owner, versions[1], /active installer/i);
      assert.ok(!existsSync(binary(dest, owner)));
    }
  } finally {
    write(join(p.base, 'resume'), '');
    assert.equal(await finished, 0, diagnostic);
  }
}

try {
  buildReleases();
  await testContention();
  testInstalls();
  console.log(`Passed release plans, packaged identity, install ownership, upgrade, rollback, process termination and lock contention on ${host}`);
} finally {
  rmSync(root, { recursive: true, force: true });
}
