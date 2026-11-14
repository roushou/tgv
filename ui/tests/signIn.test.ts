import test from 'node:test';
import assert from 'node:assert/strict';
import { RunsClient } from '../src/lib/runs.ts';
import { SignIns, login, signInState, type LoginOutcome } from '../src/lib/signIn.ts';

test('sign-in states parse, keeping states a newer daemon adds', () => {
  assert.deepEqual(signInState({ kind: 'signed_out' }, ''), { kind: 'signed_out' });
  assert.deepEqual(signInState({ kind: 'signed_in', method: 'claude.ai' }, ''), {
    kind: 'signed_in',
    method: 'claude.ai',
  });
  assert.deepEqual(signInState({ kind: 'expiring' }, ''), {
    kind: 'unrecognized',
    label: 'expiring',
  });
  assert.throws(() => signInState({ kind: 'unknown' }, ''));
});

test('a login shows only an https link, and an ended one says how it ended', () => {
  assert.deepEqual(
    login({ kind: 'awaiting_code', link: 'https://login.example.test/a?s=1', said: null }, ''),
    { kind: 'awaiting_code', link: 'https://login.example.test/a?s=1', said: null },
  );
  assert.throws(() => login({ kind: 'awaiting_code', link: 'javascript:alert(1)' }, ''));
  assert.throws(() => login({ kind: 'awaiting_code', link: 'http://login.example.test' }, ''));
  assert.deepEqual(
    login(
      {
        kind: 'ended',
        end: 'exited',
        exit: { kind: 'code', value: 1 },
        said: 'Login failed: no.',
      },
      '',
    ),
    {
      kind: 'ended',
      end: { end: 'exited', exit: { kind: 'code', value: 1 }, said: 'Login failed: no.' },
    },
  );
});

test('an outcome is a success only when the check after it says signed in', () => {
  const failed: LoginOutcome = {
    end: { end: 'exited', exit: { kind: 'code', value: 1 }, said: 'Login failed: no.' },
    state: { kind: 'signed_out' },
  };
  assert.deepEqual(SignIns.outcome(failed, 'Claude Code'), {
    ok: false,
    text: 'Login failed: no. Claude Code is still signed out.',
  });
  // Exit 0 is not proof: the status check decides.
  const unconfirmed: LoginOutcome = {
    end: { end: 'exited', exit: { kind: 'code', value: 0 }, said: 'Login successful.' },
    state: { kind: 'unknown', reason: 'timed out' },
  };
  assert.equal(SignIns.outcome(unconfirmed, 'Claude Code').ok, false);
  const signedIn: LoginOutcome = {
    end: { end: 'exited', exit: { kind: 'code', value: 0 }, said: null },
    state: { kind: 'signed_in', method: 'claude.ai' },
  };
  assert.deepEqual(SignIns.outcome(signedIn, 'Claude Code'), {
    ok: true,
    text: 'Claude Code is signed in (claude.ai).',
  });
  assert.equal(SignIns.hint({ kind: 'signed_out' }), 'signed out');
  assert.equal(SignIns.hint({ kind: 'not_checked' }), null);
  assert.equal(SignIns.method('anthropic_console'), 'Anthropic Console');
});

test('the client starts a sign-in, hands over the code, and reads clients without sign-in fields', async () => {
  const seen: string[] = [];
  const client = new RunsClient((async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    seen.push(`${init?.method} ${url} ${init?.body ?? ''}`);
    if (url.endsWith('/sign-in')) {
      return Response.json({
        api_version: 1,
        method: 'claude_subscription',
        login: { kind: 'awaiting_code', link: 'https://login.example.test/a', said: null },
      });
    }
    if (url.endsWith('/sign-in/answer')) {
      return Response.json({
        api_version: 1,
        outcome: {
          end: { end: 'exited', exit: { kind: 'code', value: 0 }, said: null },
          state: { kind: 'signed_in', method: 'claude.ai' },
        },
      });
    }
    // An older daemon: no sign-in fields.
    return Response.json({
      api_version: 1,
      clients: [{ harness: 'codex', status: 'ready', approvals: [], midway: [] }],
    });
  }) as typeof fetch);
  const started = await client.startSignIn('claude_code', 'claude_subscription');
  assert.equal(started.kind, 'awaiting_code');
  const outcome = await client.signInAnswer('claude_code', 'abc');
  assert.equal(outcome.state.kind, 'signed_in');
  const [codex] = await client.clients();
  assert.deepEqual(codex?.sign_in, { kind: 'not_checked' });
  assert.deepEqual(codex?.sign_in_methods, []);
  assert.deepEqual(seen.slice(0, 2), [
    'POST /control/runs/clients/claude_code/sign-in {"method":"claude_subscription"}',
    'POST /control/runs/clients/claude_code/sign-in/answer {"answer":"abc"}',
  ]);
});

test('a client package finds its client, and a chip says its sign-in', () => {
  const clients = [
    { harness: 'claude_code', sign_in: { kind: 'signed_out' as const } },
    { harness: 'codex', sign_in: { kind: 'not_checked' as const } },
  ];
  assert.equal(
    SignIns.clientOf({ kind: 'client', needed_by: ['claude_code'] }, clients)?.harness,
    'claude_code',
  );
  // A runtime needed by a client is not that client.
  assert.equal(SignIns.clientOf({ kind: 'runtime', needed_by: ['pi'] }, clients), null);
  assert.deepEqual(SignIns.chip({ kind: 'signed_out' }), { text: 'signed out', tone: 'warning' });
  assert.deepEqual(SignIns.chip({ kind: 'signed_in', method: 'claude.ai' }), {
    text: 'claude.ai',
    tone: 'success',
  });
  assert.equal(SignIns.chip({ kind: 'not_checked' }), null);
});

test('a device login carries its one-time code, and a wait is going or done', async () => {
  assert.deepEqual(
    login(
      {
        kind: 'awaiting_approval',
        link: 'https://login.example.test/codex/device',
        code: 'ACME-12345',
        said: null,
      },
      '',
    ),
    {
      kind: 'awaiting_approval',
      link: 'https://login.example.test/codex/device',
      code: 'ACME-12345',
      said: null,
    },
  );
  assert.equal(SignIns.method('chatgpt'), 'ChatGPT account');
  const replies = [
    {
      kind: 'going',
      login: {
        kind: 'awaiting_approval',
        link: 'https://l.example.test/d',
        code: 'ACME-1234',
        said: null,
      },
    },
    {
      kind: 'done',
      outcome: {
        end: { end: 'exited', exit: { kind: 'code', value: 0 }, said: 'Successfully logged in' },
        state: { kind: 'signed_in', method: 'ChatGPT' },
      },
    },
  ];
  const client = new RunsClient((async () =>
    Response.json({ api_version: 1, waited: replies.shift() })) as unknown as typeof fetch);
  assert.equal((await client.waitSignIn('codex')).kind, 'going');
  const done = await client.waitSignIn('codex');
  assert.equal(done.kind === 'done' && done.outcome.state.kind, 'signed_in');
});

test('a key login asks at once, a redirect login waits on an address, and a broken one says why', () => {
  assert.deepEqual(login({ kind: 'awaiting_key', said: null }, ''), {
    kind: 'awaiting_key',
    said: null,
  });
  assert.equal(
    login(
      { kind: 'awaiting_redirect', link: 'https://auth.example.test/authorize', said: null },
      '',
    ).kind,
    'awaiting_redirect',
  );
  assert.deepEqual(login({ kind: 'finishing', link: null, said: null }, ''), {
    kind: 'finishing',
    link: null,
    said: null,
  });
  assert.equal(SignIns.method('anthropic_api_key'), 'Anthropic API key');
  assert.equal(SignIns.method('openai_api_key'), 'OpenAI API key');
  assert.deepEqual(
    SignIns.outcome(
      {
        end: { end: 'broken', said: 'the address could not be opened on the host' },
        state: { kind: 'signed_out' },
      },
      'OpenCode',
    ),
    {
      ok: false,
      text: 'the address could not be opened on the host. OpenCode is still signed out.',
    },
  );
});
