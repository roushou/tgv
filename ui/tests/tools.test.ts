import test from 'node:test';
import assert from 'node:assert/strict';
import {
  Platforms,
  RowActions,
  Standings,
  Supplies,
  ToolGroups,
  ToolPlan,
  ToolsApi,
  ToolsError,
  type ToolListing,
} from '../src/lib/tools.ts';

/** A listing as the daemon answers it: invented paths and versions. */
const wire = {
  api_version: 1,
  platform: 'linux_x64',
  generation: 3,
  bin: '/data/tools/generations/3/bin',
  packages: [
    {
      id: 'claude',
      origin: 'builtin',
      kind: 'client',
      program: 'claude',
      release: null,
      installed: null,
      system: { path: '/opt/acme/bin/claude', version: '2.1.0', unread: null },
      needed_by: ['claude_code'],
    },
    {
      id: 'node',
      origin: 'builtin',
      kind: 'runtime',
      program: 'node',
      release: '26.13.1',
      installed: {
        version: '24.1.0',
        sha256: 'ab'.repeat(32),
        dir: 'node/24.1.0-abababababababab',
      },
      system: null,
      needed_by: ['pi'],
    },
    {
      id: 'jq',
      origin: 'builtin',
      kind: 'utility',
      program: 'jq',
      release: '1.8.2',
      installed: { version: '1.8.2', sha256: 'cd'.repeat(32), dir: 'jq/1.8.2-cdcdcdcdcdcdcdcd' },
      system: { path: '/usr/bin/jq', version: null, unread: 'jq built by acme' },
      needed_by: [],
    },
    {
      id: 'fd',
      origin: 'builtin',
      kind: 'utility',
      program: 'fd',
      release: '10.5.2',
      installed: null,
      system: null,
      needed_by: [],
    },
    {
      id: 'git',
      origin: 'builtin',
      kind: 'utility',
      program: 'git',
      release: null,
      installed: null,
      system: null,
      needed_by: [],
    },
    {
      id: 'zig',
      origin: 'operator',
      kind: 'compiler',
      program: 'zig',
      release: null,
      installed: null,
      system: null,
      needed_by: [],
    },
    {
      id: 'acme-lint',
      origin: null,
      kind: null,
      program: 'acme-lint',
      release: null,
      installed: {
        version: '0.3.0',
        sha256: 'ef'.repeat(32),
        dir: 'acme-lint/0.3.0-efefefefefefefef',
      },
      system: null,
      needed_by: [],
    },
  ],
  generations: [
    {
      generation: 3,
      state: 'current',
      installs: [
        { package: 'jq', version: '1.8.2' },
        { package: 'node', version: '24.1.0' },
      ],
    },
    { generation: 2, state: 'in_use', installs: [{ package: 'node', version: '24.1.0' }] },
  ],
};

const respond =
  (status: number, body: unknown, seen: string[] = []): typeof fetch =>
  async (input, init) => {
    seen.push(`${init?.method} ${String(input)} ${init?.body ?? ''}`);
    return Response.json(body, { status });
  };

async function listing(): Promise<ToolListing> {
  return new ToolsApi(respond(200, wire)).listing();
}

test('a listing decodes, keeping kinds this UI does not know', async () => {
  const read = await listing();
  assert.equal(read.generation, 3);
  assert.equal(read.platform, 'linux_x64');
  assert.equal(read.packages[5]?.kind, 'compiler');
  assert.equal(read.packages[5]?.origin, 'operator');
  assert.equal(read.packages[6]?.kind, null);
  assert.equal(read.generations[1]?.state, 'in_use');
  await assert.rejects(new ToolsApi(respond(200, { ...wire, api_version: 2 })).listing());
});

test('each package has one standing', async () => {
  const read = await listing();
  assert.deepEqual(
    read.packages.map((p) => Standings.of(p)),
    [
      { kind: 'system' },
      { kind: 'outdated', installed: '24.1.0', release: '26.13.1' },
      { kind: 'installed', version: '1.8.2' },
      { kind: 'available', release: '10.5.2' },
      { kind: 'missing' },
      { kind: 'missing' },
      { kind: 'uncatalogued', installed: '0.3.0' },
    ],
  );
});

test('packages group by kind, runtimes first, unknown kinds and uncatalogued installs last', async () => {
  const groups = ToolGroups.of(await listing());
  assert.deepEqual(
    groups.map((g) => [g.title, g.packages.map((p) => p.id)]),
    [
      ['Runtimes', ['node']],
      ['Utilities', ['jq', 'fd', 'git']],
      ['Clients', ['claude']],
      ['compiler', ['zig']],
      ['Not in the catalog', ['acme-lint']],
    ],
  );
});

test('a plan stages one change per package and previews it in listing order', async () => {
  const read = await listing();
  const [claude, node, jq, fd] = read.packages;
  assert.ok(claude && node && jq && fd);
  let plan = ToolPlan.EMPTY.toggle(fd).toggle(node).toggle(jq).toggle(claude);
  assert.deepEqual(plan.changes(read), [
    { change: 'replace', id: 'node', from: '24.1.0', to: '26.13.1' },
    { change: 'remove', id: 'jq', from: '1.8.2', to: null },
    { change: 'add', id: 'fd', from: null, to: '10.5.2' },
  ]);
  assert.deepEqual(plan.body(), { add: ['fd', 'node'], remove: ['jq'] });
  // An outdated install can be removed instead of updated.
  plan = plan.toggleRemove(node);
  assert.deepEqual(plan.body(), { add: ['fd'], remove: ['jq', 'node'] });
  // An install the catalog no longer names can only be removed.
  const acme = read.packages[6];
  assert.ok(acme);
  assert.deepEqual(ToolPlan.EMPTY.toggle(acme).changes(read), [
    { change: 'remove', id: 'acme-lint', from: '0.3.0', to: null },
  ]);
  // Toggling again unstages.
  plan = plan.toggle(fd).toggle(jq).toggleRemove(node);
  assert.ok(plan.empty);
});

test('a plan keeps only what still applies after the listing changed', async () => {
  const read = await listing();
  const [, node, , fd] = read.packages;
  assert.ok(node && fd);
  const plan = ToolPlan.EMPTY.toggle(fd).toggle(node);
  const after: ToolListing = {
    ...read,
    packages: read.packages.map((p) =>
      p.id === 'fd'
        ? { ...p, installed: { version: p.release ?? ('' as never), sha256: '', dir: '' } }
        : p,
    ),
  };
  assert.deepEqual(plan.within(after).body(), { add: ['node'], remove: [] });
});

test('changes and collections post their bodies; refusals read as sentences', async () => {
  const seen: string[] = [];
  const api = new ToolsApi(
    respond(
      200,
      {
        api_version: 1,
        outcome: 'activated',
        generation: 4,
        changes: [{ change: 'add', package: 'fd', from: null, to: '10.5.2' }],
        failures: [],
      },
      seen,
    ),
  );
  const changed = await api.change(new ToolPlan(['fd' as never]));
  assert.equal(changed.outcome, 'activated');
  await api.rollBack(2 as never);
  assert.deepEqual(seen, [
    'POST /control/tools/change {"add":["fd"],"remove":[]}',
    'POST /control/tools/rollback {"generation":2}',
  ]);

  const refused = new ToolsApi(
    respond(422, { error: 'tools_refused', message: 'fd is not installed by Ren' }),
  );
  await assert.rejects(refused.change(new ToolPlan([], ['fd' as never])), (error: unknown) => {
    assert.ok(error instanceof ToolsError);
    assert.equal(error.message, 'fd is not installed by Ren.');
    return true;
  });
  assert.equal(
    ToolsError.describe(404, 'runs_off', null),
    'Set REN_CONTROL_TOKEN_FILE on the UI server to manage tools here.',
  );
  assert.equal(ToolsError.describe(418, null, null), 'The request was refused (418).');
});

test('where runs get each package, and the header tally', async () => {
  const read = await listing();
  const [claude, node, jq, fd, git, , acme] = read.packages;
  assert.ok(claude && node && jq && fd && git && acme);
  assert.deepEqual(Supplies.of(claude), {
    kind: 'system',
    system: { path: '/opt/acme/bin/claude', version: '2.1.0', unread: null },
  });
  assert.deepEqual(Supplies.of(node), { kind: 'managed', version: '24.1.0', shadows: null });
  assert.deepEqual(Supplies.of(jq), {
    kind: 'managed',
    version: '1.8.2',
    shadows: { path: '/usr/bin/jq', version: null, unread: 'jq built by acme' },
  });
  assert.deepEqual(Supplies.of(fd), { kind: 'none' });
  assert.deepEqual(
    [claude, node, fd, git].map((p) => Supplies.tone(p)),
    ['system', 'managed', 'none', 'none'],
  );
  assert.equal(Supplies.tone({ ...fd, needed_by: ['pi'] }), 'missing');
  assert.deepEqual(Supplies.tally(read), { managed: 3, system: 1, missing: 0, updates: 1 });
  assert.equal(Platforms.label('linux_x64'), 'Linux x64');
});

test('each row stages one change, named with the version it brings', async () => {
  const read = await listing();
  assert.deepEqual(
    read.packages.map((p) => RowActions.of(p)),
    [
      null,
      { kind: 'update', version: '26.13.1' },
      { kind: 'remove' },
      { kind: 'install', version: '10.5.2' },
      null,
      null,
      { kind: 'remove' },
    ],
  );
});
