import assert from 'node:assert/strict';
import { setTimeout as delay } from 'node:timers/promises';

const permissions = [
  'resource.read', 'resource.draft', 'execution.submit', 'credential.use',
  'credential.manage', 'policy.activate', 'receipt.read',
];

export async function createTenant(control, operator, upstream) {
  const tenant = 'synthetic';
  const principal = 'fixture-principal';
  const binding = 'fixture-binding';
  let installation;
  async function request(path, { method = 'GET', token = operator, payload } = {}) {
    const response = await fetch(control + '/control/v1/' + path, {
      method, redirect: 'error', signal: AbortSignal.timeout(5000),
      headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json',
        ...(installation ? { 'bitrouter-installation-id': installation } : {}),
      },
      body: payload === undefined ? undefined : JSON.stringify(payload),
    });
    assert.ok(response.ok, `${method} ${path.split('?')[0]} returned ${response.status}`);
    return response.json();
  }
  const initial = await request('tenancy/capabilities');
  assert.ok(initial.dispatch_enabled && initial.strict_receipts);
  installation = initial.installation_id;
  const prefix = 'tenants/' + tenant;
  const mutate = (method, path, operation_id, attributes, token) => request(path, {
    method, token, payload: { installation_id: installation, operation_id, ...attributes },
  });
  await mutate('PUT', prefix, 'make-tenant', { expected_revision: null, state: 'active' });
  await mutate('PUT', 'principals/' + principal, 'make-principal', {
    expected_revision: null, issuer: 'synthetic-fixture', subject: 'request-driver',
  });
  const expires_at = new Date(Date.now() + 3600_000).toISOString().replace(/\.\d+Z$/, 'Z');
  const scope = { kind: 'tenant' };
  const bound = await mutate('PUT', prefix + '/bindings/' + binding, 'bind-principal', {
    expected_revision: null, principal_id: principal, role: 'tenant_admin', scope, expires_at,
    expected_management_owner: null,
  });
  const issued = await mutate('POST', prefix + '/credentials', 'issue-fixture-key', {
    expected_authority_revision: bound.authority_revision, principal_id: principal, binding_id: binding,
    scope, expires_at, permissions, rotate_credential_id: null,
  });
  assert.ok(issued.secret);
  const token = issued.secret;
  let authority;
  async function resource(kind, name, definition) {
    const key = { owner_kind: 'tenant', owner_id: tenant, resource_kind: kind, resource_id: name };
    const result = await mutate('POST', prefix + '/resources', 'resource-' + name,
      { format: 1, key, definition, expected_revision: null }, token);
    authority = result.authority_revision;
    return { ...key, revision_id: result.resource_revision };
  }
  const credential = await resource('credential', 'provider-key', {
    kind: 'credential', secret: 'synthetic-loopback-key',
  });
  const routes = [];
  for (const model of ['short', 'paced', 'tools']) {
    routes.push(await resource('route', model, {
      kind: 'route', selector: 'fixture:' + model, policy: null,
      routing: { providers: { fixture: {
        api_base: upstream + '/v1', api_protocol: [{ '*': 'chat_completions' }], models: [{ id: model }],
      } } },
      credentials: [{ provider: 'fixture', account: 'default', credential }],
    }));
  }
  const draft = await mutate('POST', prefix + '/configurations', 'draft-fixture', {
    expected_authority_revision: authority,
    document: { routes, policy: null, generation_defaults: { max_output_tokens: 4096, temperature: 0 },
      learning: { mode: 'off', max_evaluations: 0 },
      limits: { active_executions: 8, queued_work: 0, learning_jobs: 0 },
    },
  }, token);
  const operation = await request('tenancy/operations/draft-fixture', { token });
  const active = await mutate('POST', prefix + '/activation', 'activate-fixture', {
    expected_authority_revision: operation.authority_revision,
    expected_active_revision: null, revision_id: draft.revision_id,
  }, token);
  assert.equal(active.status, 'applied');
  const cap = await request('tenancy/capabilities', { token });
  const headers = {
    authorization: 'Bearer ' + token, 'bitrouter-installation-id': installation, 'bitrouter-instance-id': cap.instance_id,
    'bitrouter-capability-revision': String(cap.capability_revision), 'bitrouter-execution-revision': draft.revision_id,
    'bitrouter-tenancy-version': '1', 'bitrouter-receipt-mode': 'strict',
  };
  return { headers, async check(rows, samplesPerLeg) {
    if (samplesPerLeg) {
      const byLeg = Map.groupBy(rows, row => row.leg);
      rows = [...byLeg.values()].flatMap(group => {
        assert.ok(group.length >= samplesPerLeg);
        return Array.from({ length: samplesPerLeg }, (_, index) => group[
          Math.floor((index + 0.5) * group.length / samplesPerLeg)]);
      });
    }
    const examples = new Set();
    let checked = 0;
    for (const row of rows) {
      const key = row.scenario + '/' + row.leg;
      if (!samplesPerLeg && !row.cancel && examples.has(key)) continue;
      examples.add(key);
      const path = prefix + '/executions';
      let page;
      for (let attempt = 0; attempt < (row.cancel ? 12 : 1); attempt++) {
        page = await request(path + '?correlation_id=' + row.correlation + '&limit=2', { token });
        if (page.execution_ids.length) break;
        if (row.cancel) await delay(100);
      }
      assert.equal(page.execution_ids.length, 1, 'receipt cardinality');
      if (row.execution) assert.equal(page.execution_ids[0], row.execution);
      assert.equal(page.multiple_executions, false);
      const receipt = await request(path + '/' + page.execution_ids[0], { token });
      assert.equal(receipt.tenant_id, tenant);
      assert.equal(receipt.principal_id, principal);
      assert.equal(receipt.correlation_id, row.correlation);
      assert.equal(receipt.execution_revision, draft.revision_id);
      assert.equal(receipt.attempts.length, 1);
      if (row.cancel) {
        assert.equal(receipt.state, 'unknown');
        assert.equal(receipt.attempts[0].outcome, 'unknown');
        assert.equal(receipt.usage.complete, false);
      } else {
        assert.equal(receipt.outcome, 'completed');
        assert.equal(receipt.attempts[0].outcome, 'completed');
        if (samplesPerLeg) {
          assert.ok(row.execution);
          for (const usage of [receipt.usage, receipt.attempts[0].usage]) {
            assert.deepEqual([usage.input_tokens, usage.output_tokens, usage.provenance, usage.complete],
              [3, 5, 'provider', false]);
          }
        }
      }
      checked++;
    }
    return checked;
  } };
}
