import test from 'node:test';
import assert from 'node:assert/strict';
import { RunsAccessConfig, RunsProxy, type ControlHost } from '../src/lib/server/control.ts';

const run = '0190f3a0-0000-7000-8000-0000000000aa';
const on = RunsAccessConfig.access('/data/control.token', []);

function host(answer: (url: URL, init: RequestInit) => Response): ControlHost & { seen: number } {
  const seen = { seen: 0 };
  return Object.assign(seen, {
    fetch: (async (url: URL | string | Request, init?: RequestInit) => {
      seen.seen++;
      return answer(new URL(String(url)), init ?? {});
    }) as typeof fetch,
    readToken: async (path: string) => {
      assert.equal(path, '/data/control.token');
      return 'secret-token\n';
    },
  });
}

const never = () => {
  throw new Error('must not reach the daemon');
};

function post(path: string, body: unknown, extra: Record<string, string> = {}): Request {
  return new Request(`http://ui.test${path}`, {
    method: 'POST',
    headers: {
      origin: 'http://ui.test',
      'sec-fetch-site': 'same-origin',
      'content-type': 'application/json',
      ...extra,
    },
    body: JSON.stringify(body),
  });
}

test('access is off without a token file, and logins need one', () => {
  assert.deepEqual(RunsAccessConfig.access(undefined, []), { kind: 'off' });
  assert.throws(() => RunsAccessConfig.access(undefined, ['a@example.com']), /needs/);
  assert.throws(() => RunsAccessConfig.tokenFile('data/control.token'), /absolute/);
  assert.equal(RunsAccessConfig.tokenFile(''), undefined);
  assert.deepEqual(RunsAccessConfig.logins(' a@example.com , b@example.com,'), [
    'a@example.com',
    'b@example.com',
  ]);
  assert.throws(() => RunsAccessConfig.logins('a b'), /visible/);
});

test('commands reach the control API with the token from the file, never browser credentials', async () => {
  const daemon = host((url, init) => {
    assert.equal(String(url), `http://daemon.test/control/v1/sessions/${run}/prompt`);
    assert.equal(init.method, 'POST');
    assert.deepEqual(init.headers, {
      authorization: 'Bearer secret-token',
      accept: 'application/json',
      'content-type': 'application/json',
    });
    assert.equal(init.body, '{"text":"fix it"}');
    assert.equal(init.redirect, 'manual');
    return Response.json({ api_version: 1 }, { status: 202, headers: { 'set-cookie': 'x' } });
  });
  const proxy = new RunsProxy(on, 'http://daemon.test', daemon);
  const response = await proxy.respond(
    post(
      `/control/sessions/${run}/prompt`,
      { text: 'fix it' },
      { cookie: 'ui=1', authorization: 'Bearer browser' },
    ),
  );
  assert.equal(response.status, 202);
  assert.equal(response.headers.get('set-cookie'), null);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.equal(daemon.seen, 1);
});

test('starting, listing, projects, models, answering, history edits, and the feed map to their control routes', async () => {
  const seen: string[] = [];
  const daemon = host((url, init) => {
    seen.push(`${init.method} ${url.pathname}`);
    return url.pathname.endsWith('/events')
      ? new Response('event: run\ndata: {}\n\n', {
          headers: { 'content-type': 'text/event-stream' },
        })
      : Response.json({ api_version: 1 });
  });
  const proxy = new RunsProxy(on, 'http://daemon.test', daemon);
  await proxy.respond(post('/control/sessions', { harness: 'codex', dir: '/work/acme' }));
  await proxy.respond(
    new Request('http://ui.test/control/runs', { headers: { 'sec-fetch-site': 'same-origin' } }),
  );
  await proxy.respond(
    new Request('http://ui.test/control/runs/projects', {
      headers: { 'sec-fetch-site': 'same-origin' },
    }),
  );
  await proxy.respond(
    new Request('http://ui.test/control/runs/clients', {
      headers: { 'sec-fetch-site': 'same-origin' },
    }),
  );
  await proxy.respond(
    new Request('http://ui.test/control/runs/clients/pi/models', {
      headers: { 'sec-fetch-site': 'same-origin' },
    }),
  );
  await proxy.respond(
    post('/control/runs/clients/claude_code/sign-in', { method: 'anthropic_console' }),
  );
  await proxy.respond(post('/control/runs/clients/claude_code/sign-in/code', { code: 'abc' }));
  await proxy.respond(post('/control/runs/clients/claude_code/sign-in/check', {}));
  await proxy.respond(post('/control/runs/clients/claude_code/sign-in/cancel', {}));
  await proxy.respond(post('/control/runs/clients/codex/sign-in/wait', {}));
  await proxy.respond(post(`/control/sessions/${run}/approvals/ap-1`, { decision: 'accept' }));
  await proxy.respond(post(`/control/sessions/${run}/stop`, {}));
  await proxy.respond(post(`/control/sessions/${run}/unqueue`, {}));
  await proxy.respond(post(`/control/sessions/${run}/settings`, { effort: 'high' }));
  await proxy.respond(post(`/control/sessions/${run}/delete`, {}));
  await proxy.respond(post(`/control/history/sessions/${run}/archive`, {}));
  await proxy.respond(post(`/control/history/runs/${run}/delete`, {}));
  await proxy.respond(
    new Request('http://ui.test/control/tools', { headers: { 'sec-fetch-site': 'same-origin' } }),
  );
  await proxy.respond(post('/control/tools/change', { add: ['jq'] }));
  await proxy.respond(post('/control/tools/rollback', { generation: 1 }));
  await proxy.respond(post('/control/tools/gc', {}));
  const feed = await proxy.respond(new Request('http://ui.test/control/runs/events'));
  assert.equal(await feed.text(), ': connected\n\nevent: run\ndata: {}\n\n');
  assert.deepEqual(seen, [
    'POST /control/v1/sessions',
    'GET /control/v1/runs',
    'GET /control/v1/runs/projects',
    'GET /control/v1/runs/clients',
    'GET /control/v1/runs/clients/pi/models',
    'POST /control/v1/runs/clients/claude_code/sign-in',
    'POST /control/v1/runs/clients/claude_code/sign-in/code',
    'POST /control/v1/runs/clients/claude_code/sign-in/check',
    'POST /control/v1/runs/clients/claude_code/sign-in/cancel',
    'POST /control/v1/runs/clients/codex/sign-in/wait',
    `POST /control/v1/sessions/${run}/approvals/ap-1`,
    `POST /control/v1/sessions/${run}/stop`,
    `POST /control/v1/sessions/${run}/unqueue`,
    `POST /control/v1/sessions/${run}/settings`,
    `POST /control/v1/sessions/${run}/delete`,
    `POST /control/v1/history/sessions/${run}/archive`,
    `POST /control/v1/history/runs/${run}/delete`,
    'GET /control/v1/tools',
    'POST /control/v1/tools/change',
    'POST /control/v1/tools/rollback',
    'POST /control/v1/tools/gc',
    'GET /control/v1/runs/events',
  ]);
});

test('sessions map to their control routes, and a recording block is not proxied', async () => {
  const seen: string[] = [];
  const daemon = host((url, init) => {
    seen.push(`${init.method} ${url.pathname}${url.search}`);
    return Response.json({ api_version: 1 });
  });
  const proxy = new RunsProxy(on, 'http://daemon.test', daemon);
  const get = (path: string) =>
    proxy.respond(
      new Request(`http://ui.test${path}`, { headers: { 'sec-fetch-site': 'same-origin' } }),
    );
  await proxy.respond(post('/control/sessions', { harness: 'codex' }));
  await get('/control/sessions?shelf=archived&limit=5&x=1');
  await get(`/control/sessions/${run}`);
  await get(`/control/sessions/${run}/commands`);
  await get(`/control/sessions/${run}/files?q=src`);
  await proxy.respond(post(`/control/sessions/${run}/prompt`, { text: 'go' }));
  await proxy.respond(post(`/control/sessions/${run}/client`, { harness: 'pi' }));
  await proxy.respond(post(`/control/sessions/${run}/archive`, {}));
  await proxy.respond(post(`/control/sessions/${run}/approvals/ap-1`, { decision: 'accept' }));
  const blocked = await proxy.respond(post(`/control/sessions/${run}/block`, {}));
  assert.equal(blocked.status, 404);
  assert.deepEqual(seen, [
    'POST /control/v1/sessions',
    'GET /control/v1/sessions?shelf=archived&limit=5',
    `GET /control/v1/sessions/${run}`,
    `GET /control/v1/sessions/${run}/commands`,
    `GET /control/v1/sessions/${run}/files?q=src`,
    `POST /control/v1/sessions/${run}/prompt`,
    `POST /control/v1/sessions/${run}/client`,
    `POST /control/v1/sessions/${run}/archive`,
    `POST /control/v1/sessions/${run}/approvals/ap-1`,
  ]);
});

test('cross-site, non-JSON, unknown, and unlisted requests never reach the daemon', async () => {
  const proxy = new RunsProxy(on, 'http://daemon.test', host(never));
  const cases: [Request, number][] = [
    [
      post(
        `/control/sessions/${run}/stop`,
        {},
        { origin: 'https://evil.example', 'sec-fetch-site': 'cross-site' },
      ),
      403,
    ],
    [post(`/control/sessions/${run}/stop`, {}, { 'sec-fetch-site': 'same-site' }), 403],
    // Without Sec-Fetch-Site, the Origin's host must be this one.
    [
      new Request(`http://ui.test/control/sessions/${run}/stop`, {
        method: 'POST',
        headers: { origin: 'https://evil.example', 'content-type': 'application/json' },
        body: '{}',
      }),
      403,
    ],
    [
      new Request(`http://ui.test/control/sessions/${run}/stop`, {
        method: 'POST',
        headers: { origin: 'http://ui.test', 'content-type': 'text/plain' },
        body: '{}',
      }),
      415,
    ],
    // No Origin and no Sec-Fetch-Site: not one of this UI's pages.
    [
      new Request(`http://ui.test/control/sessions/${run}/stop`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: '{}',
      }),
      403,
    ],
    [post('/control/sessions/not-a-session/stop', {}), 404],
    [post(`/control/sessions/${run}/shred`, {}), 404],
    [post(`/control/history/sessions/${run}/shred`, {}), 404],
    [post(`/control/history/calls/${run}/delete`, {}), 404],
    [new Request('http://ui.test/control/runs/clients/hermes/models'), 404],
    [new Request('http://ui.test/control/runs/clients/claude_code/sign-in/token'), 404],
    [new Request(`http://ui.test/control/history/runs/${run}/delete`), 405],
    [post('/control/pause', {}), 404],
    [post('/control/tools/install', {}), 404],
    [new Request('http://ui.test/control/tools/change'), 405],
    [new Request(`http://ui.test/control/sessions/${run}/stop`), 405],
    [post(`/control/sessions/${run}/prompt`, { text: 'x'.repeat(300 * 1024) }), 413],
  ];
  for (const [request, status] of cases) {
    const response = await proxy.respond(request);
    assert.equal(response.status, status, `${request.method} ${request.url}`);
  }
  const off = new RunsProxy({ kind: 'off' }, 'http://daemon.test', host(never));
  assert.equal((await off.respond(post('/control/runs', {}))).status, 404);
});

test('behind a TLS proxy, an older browser Origin is checked by host only', async () => {
  const proxy = new RunsProxy(
    on,
    'http://daemon.test',
    host(() => Response.json({ api_version: 1 })),
  );
  const request = new Request(`http://ui.test/control/sessions/${run}/stop`, {
    method: 'POST',
    headers: { origin: 'https://ui.test', 'content-type': 'application/json' },
    body: '{}',
  });
  assert.equal((await proxy.respond(request)).status, 200);
});

test('with logins listed, only those Tailscale logins drive runs', async () => {
  const access = RunsAccessConfig.access('/data/control.token', ['alice@example.com']);
  const proxy = new RunsProxy(
    access,
    'http://daemon.test',
    host(() => Response.json({ api_version: 1 })),
  );
  const stop = (login?: string) =>
    proxy.respond(
      post(`/control/sessions/${run}/stop`, {}, login ? { 'tailscale-user-login': login } : {}),
    );
  assert.equal((await stop()).status, 403);
  assert.equal((await stop('mallory@example.com')).status, 403);
  assert.equal((await stop('alice@example.com')).status, 200);
});

test('a missing token file or an unreachable daemon is reported, not retried', async () => {
  const unreadable = new RunsProxy(on, 'http://daemon.test', {
    fetch: never as unknown as typeof fetch,
    readToken: async () => {
      throw new Error('ENOENT');
    },
  });
  const missing = await unreadable.respond(post(`/control/sessions/${run}/stop`, {}));
  assert.equal(missing.status, 503);
  assert.deepEqual(await missing.json(), { error: 'no_control_token' });
  const down = new RunsProxy(on, 'http://daemon.test', {
    fetch: (async () => {
      throw new TypeError('connect refused');
    }) as typeof fetch,
    readToken: async () => 'token',
  });
  const answer = await down.respond(post(`/control/sessions/${run}/stop`, {}));
  assert.equal(answer.status, 502);
});
