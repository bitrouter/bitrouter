import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';

export function cpuCounters(text) {
  const fields = text.split('\n', 1)[0].trim().split(/\s+/);
  assert.equal(fields.shift(), 'cpu');
  assert.ok(fields.length >= 8);
  return fields.slice(0, 8).map(field => {
    assert.match(field, /^\d+$/);
    return BigInt(field);
  });
}

export function cpuInterval(before, after) {
  const ticks = after.map((end, index) => end - before[index]);
  const total = ticks.reduce((sum, value) => sum + value, 0n);
  if (total <= 0n || ticks.some(value => value < 0n)) return null;
  return { idle: 100 * Number(ticks[3]) / Number(total), steal: 100 * Number(ticks[7]) / Number(total) };
}

export async function snapshot() {
  return cpuCounters(await readFile('/proc/stat', 'utf8'));
}

const metric = values => {
  const ordered = values.sort((left, right) => left - right);
  return ordered.length ? { min: ordered[0], median: ordered[Math.floor(ordered.length / 2)], max: ordered.at(-1) } : null;
};

export function aggregateCpu(samples) {
  const valid = samples.filter(Boolean);
  return { samples: samples.length, invalid: samples.length - valid.length,
    idlePercent: metric(valid.map(row => row.idle)), stealPercent: metric(valid.map(row => row.steal)) };
}

export async function linuxReadiness(deadline) {
  const samples = [];
  let streak = 0;
  while (performance.now() < deadline) {
    const before = await snapshot();
    await delay(1000);
    const interval = cpuInterval(before, await snapshot());
    samples.push(interval);
    streak = interval && interval.idle > 80 ? streak + 1 : 0;
    if (streak === 3) break;
    await delay(10_000);
  }
  return { ...aggregateCpu(samples), above80: samples.filter(row => row && row.idle > 80).length, passed: streak === 3 };
}
