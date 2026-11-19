import assert from 'node:assert/strict';
import { COUNTS, measuredLegs } from './synthetic-client.mjs';
import { LANES } from './synthetic-trials.mjs';

const RULES = {
  latency: [0.15, 0.30, 1], tail: [0.20, 0.40, 2], rate: [0.10, 0.20, 0.01],
  startup: [0.15, 0.30, 5], rss: [0.08, 0.15, 1024],
};
const quantile = (values, percent) => [...values].sort((a, b) => a - b)[Math.ceil(values.length * percent / 100) - 1];
const median = values => quantile(values, 50);

export function auditTrial(trial, counts) {
  const expectedLegs = measuredLegs(counts);
  assert.equal(trial.rows, expectedLegs);
  assert.equal(trial.warmups, 24);
  assert.equal(trial.receipts, trial.lane === 'tenant' ? 105 : 0);
  assert.deepEqual(trial.observed, {
    started: expectedLegs + 24, completed: expectedLegs + 24 - counts.cancel, cancelled: counts.cancel, failed: 0,
  });
  assert.deepEqual(Object.keys(trial.windows), Object.keys(counts));
  for (const [scenario, count] of Object.entries(counts)) {
    const { rows, elapsed } = trial.windows[scenario];
    const legs = count * (scenario === 'tools' ? 2 : 1);
    assert.ok(Number.isFinite(elapsed) && elapsed > 0);
    assert.equal(rows.length, legs);
    assert.equal(new Set(rows.map(row => row.correlation)).size, legs);
    assert.ok(rows.every(row => row.scenario === scenario && row.cancel === (scenario === 'cancel') &&
      row.version === (trial.group === 'http2-h2c' ? '2.0' : '1.1')));
  }
}

export function summarize(trials, counts = COUNTS) {
  const metrics = {};
  for (const [group, lanes] of Object.entries(LANES)) {
    for (const lane of lanes) {
      const matches = trials.filter(trial => trial.group === group && trial.lane === lane);
      assert.deepEqual(matches.map(trial => trial.repeat), [1, 2, 3, 4, 5]);
      const groups = {};
      const add = (scenario, name, rule, value, legs = 1, pooled) => {
        assert.ok(Number.isFinite(value) && value >= 0);
        const entry = groups[scenario + '/' + name] ??= { rule, values: [], legs, pooled: [] };
        entry.values.push(value);
        if (pooled) entry.pooled.push(...pooled);
      };
      for (const trial of matches) {
        auditTrial(trial, counts);
        if (lane !== 'direct') add('process', 'startup_ms', 'startup', trial.startup);
        add('process', 'idle_kib', 'rss', trial.idleKib);
        add('process', 'sampled_peak_kib', 'rss', trial.peakKib);
        for (const [scenario, count] of Object.entries(counts)) {
          const { rows, elapsed } = trial.windows[scenario];
          const legs = count * (scenario === 'tools' ? 2 : 1);
          const rate = 1000 * legs / elapsed;
          const rateName = scenario === 'cancel' ? 'cancels_per_s' : 'goodput_per_s';
          add(scenario, rateName, 'rate', rate, legs);
          if (scenario === 'tools') add(scenario, 'conversations_per_s', 'rate', rate / 2, count);
          for (const [field, name] of [['elapsed', 'e2e'], ['first', 'first_output']]) {
            if (scenario === 'json' && field === 'first') continue;
            const values = rows.map(row => row[field]);
            assert.ok(values.every(value => Number.isFinite(value) && value >= 0));
            for (const percent of [50, 95, 99]) {
              add(scenario, name + '_p' + percent + '_ms', percent === 50 ? 'latency' : 'tail',
                quantile(values, percent), legs, scenario === 'long' ? values : undefined);
            }
          }
        }
      }
      for (const [key, row] of Object.entries(groups)) {
        assert.equal(row.values.length, 5);
        const trialMedian = median(row.values);
        const mad = median(row.values.map(value => Math.abs(value - trialMedian)));
        const effective = row.pooled.length || row.legs;
        if (row.pooled.length) assert.equal(effective, 100);
        assert.ok(!key.includes('_p99_') || effective >= 100);
        assert.ok(!key.includes('_p95_') || effective >= 20);
        const center = row.pooled.length ? quantile(row.pooled, Number(key.match(/_p(\d+)_/)[1])) : trialMedian;
        const [relative, cap, floor] = RULES[row.rule];
        metrics[[group, lane, key].join('/')] = {
          rule: row.rule, values: row.values, center, trialMedian, mad, effective, legs: row.legs,
          allowance: Math.max(2 * mad, floor, relative * center),
          variable: 2 * mad > Math.max(floor, cap * center),
        };
      }
    }
  }
  assert.equal(trials.length, 40);
  return metrics;
}

export function compare(base, candidate) {
  assert.deepEqual(Object.keys(base), Object.keys(candidate));
  const rows = Object.entries(base).map(([metric, reference]) => {
    const current = candidate[metric];
    assert.equal(current.rule, reference.rule);
    assert.equal(current.effective, reference.effective);
    const delta = current.center - reference.center;
    const [, cap, floor] = RULES[reference.rule];
    const variable = reference.variable || 2 * current.mad > Math.max(floor, cap * reference.center);
    const exceeds = (reference.rule === 'rate' ? -delta : delta) > reference.allowance;
    return { metric, baseline: reference, candidate: current, delta,
      status: variable ? 'variable' : exceeds ? 'outside-budget' : 'within-budget',
      controlDrift: false,
    };
  });
  const controls = new Set(rows.filter(row => row.metric.split('/')[1] === 'direct' &&
    (row.status === 'variable' || Math.abs(row.delta) > row.baseline.allowance)).map(row => {
    const [group, , scenario] = row.metric.split('/');
    return group + '/' + scenario;
  }));
  for (const row of rows) {
    const [group, lane, scenario] = row.metric.split('/');
    const control = group === 'http2-h2c' ? 'http2-http1' : group;
    if (lane !== 'direct') row.controlDrift = controls.has(control + '/' + scenario);
  }
  return rows;
}
