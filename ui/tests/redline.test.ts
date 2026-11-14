import test from 'node:test';
import assert from 'node:assert/strict';
import { Redline, type ElementLike, type Mark, type PageContext } from '../src/lib/redline.ts';
import type { RunClient, RunId, RunSnapshot, SessionBody, SessionId } from '../src/lib/runs.ts';

function element(
  tagName: string,
  classes: string[] = [],
  parent: ElementLike | null = null,
  attributes: Record<string, string> = {},
  id = '',
): ElementLike {
  return {
    tagName: tagName.toUpperCase(),
    id,
    classList: classes,
    parentElement: parent,
    getAttribute: (name) => attributes[name] ?? null,
  };
}

const page: PageContext = {
  path: '/sessions?session=s1',
  route: '/sessions',
  title: 'Ren - Sessions',
  width: 1400,
  height: 820,
  theme: 'dark',
  revision: { kind: 'commit', commit: '97aa93b'.padEnd(40, '0'), tree: 'clean' },
};

function mark(id: number, note: string): Mark {
  return {
    id,
    note,
    target: {
      selector: '[aria-label="Run"] div.entry.tool > span.chip.success',
      text: 'completed',
      source: id === 1 ? 'src/routes/sessions/+page.svelte:861' : null,
      width: 62.4,
      height: 18,
    },
  };
}

test('a selector names the labelled region, then the element and two ancestors', () => {
  const region = element('section', ['run-main'], null, { 'aria-label': 'Run' });
  const convo = element('div', ['conversation', 'svelte-1x2y3z'], region);
  const entry = element('div', ['entry', 'tool'], convo);
  const chip = element('span', ['chip', 'success', 'svelte-1x2y3z'], entry);
  assert.equal(
    Redline.selector(chip),
    '[aria-label="Run"] div.conversation > div.entry.tool > span.chip.success',
  );
  // An id stands for the element; with no labelled region, the steps alone.
  const lone = element('button', ['x'], element('main', [], null, {}, 'top'));
  assert.equal(Redline.selector(lone), 'main#top > button.x');
});

test('text is one line, cut short', () => {
  assert.equal(Redline.text('  run\n  the   tests '), 'run the tests');
  const long = Redline.text('a'.repeat(200));
  assert.equal(long.length, Redline.TEXT);
  assert.ok(long.endsWith('…'));
});

test('the branch is named from the first note', () => {
  assert.equal(
    Redline.branch([mark(1, 'The status chip floats mid-row; move it!')]),
    'redline/the-status-chip-floats-mid',
  );
  assert.equal(Redline.branch([]), 'redline/change');
});

test('the brief lists each noted mark with what it points at', () => {
  const brief = Redline.brief(page, [mark(1, 'Move it to the end.'), mark(2, '  ')]);
  assert.equal(
    brief,
    [
      'Redline: Move it to the end.',
      '',
      'Marked on `/sessions?session=s1` (Ren - Sessions), 1400×820, dark theme.',
      "In Ren's UI this page is `ui/src/routes/sessions/+page.svelte`.",
      'This page was served by a build of commit `97aa93b00000`; the checkout may have moved on since.',
      '',
      '1. Move it to the end.',
      '   - Element: `[aria-label="Run"] div.entry.tool > span.chip.success`, “completed”',
      '   - Source: `src/routes/sessions/+page.svelte:861`',
      '   - Size: 62×18',
      '',
      "Answer what the notes ask without changing anything. If a note asks for a change, make it on a new branch, `redline/move-it-to-the-end`, in a git worktree of this repository, not in the checkout itself, and run the project's checks. Don't install or restart anything: I'll review the branch.",
    ].join('\n'),
  );
  assert.equal(Redline.routeFile('/'), 'ui/src/routes/+page.svelte');
});

test('the build is named when the daemon says what it was built from', () => {
  assert.equal(
    Redline.built({ kind: 'commit', commit: 'a'.repeat(40), tree: 'modified' }),
    'This page was served by a build of commit `aaaaaaaaaaaa` with uncommitted changes; the checkout may have moved on since.',
  );
  assert.equal(Redline.built({ kind: 'unknown' }), null);
  assert.equal(Redline.built(null), null);
});

test('the source checkout is an absolute path, or absent', () => {
  assert.equal(Redline.checkout(undefined), undefined);
  assert.equal(Redline.checkout(''), undefined);
  assert.equal(Redline.checkout('/home/you/ren/'), '/home/you/ren');
  assert.equal(Redline.checkout('/'), '/');
  assert.throws(() => Redline.checkout('ren'), /absolute/);
});

function stored(id: string, workdir: string, createdAt: number, archived = false): SessionBody {
  return {
    session: id as SessionId,
    served: null,
    stored: {
      id: id as SessionId,
      harness: 'codex',
      workdir,
      created_at: createdAt as never,
      model: null,
      effort: null,
      approvals: null,
      archived_at: archived ? (createdAt as never) : null,
      first_turn_at: createdAt as never,
      first_prompt: `asked ${id}`,
      runs: [],
    },
  };
}

test('the sessions offered are those running in the checkout, newest first', () => {
  const ids = [1, 2, 3, 4, 5].map((n) => `0190f3a0-0000-7000-8000-00000000a00${n}`);
  const bodies = [
    stored(ids[0]!, '/home/you/ren', 100),
    stored(ids[1]!, '/home/you/ren', 300),
    stored(ids[2]!, '/home/you/ren', 400, true),
    stored(ids[3]!, '/home/you/ren-other', 500),
    // Ended: its client is not running, so it is not offered.
    stored(ids[4]!, '/home/you/ren', 600),
  ];
  const live = ids.slice(0, 4).map((id, index) => {
    const body = bodies[index]!;
    const run = `0190f3a0-0000-7000-8000-00000000b00${index}` as RunId;
    body.stored!.runs = [
      {
        id: run,
        harness: 'codex',
        workdir: body.stored!.workdir,
        started_at: body.stored!.created_at,
        thread: null,
        session: null,
        operator_session: id,
        ended_at: null,
        end: null,
        exit: null,
        archived_at: null,
        model: null,
        effort: null,
        approvals: null,
      } as never,
    ];
    return {
      run,
      harness: 'codex',
      workdir: body.stored!.workdir,
      state: { phase: 'idle', thread: 'thread' as never },
      settings: { model: null, effort: null, approvals: null },
    } satisfies RunSnapshot;
  });
  assert.deepEqual(
    Redline.offered(bodies, live, '/home/you/ren').map((session) => session.session),
    [ids[1], ids[0]],
  );
  assert.deepEqual(Redline.offered(bodies, live, '/nowhere'), []);
});

test("a mark is named by the element's text, else its selector's last step", () => {
  const target = { selector: '[aria-label="Run"] div.entry > span.chip', width: 1, height: 1 };
  assert.equal(Redline.name({ ...target, text: 'completed', source: null }), '“completed”');
  assert.equal(Redline.name({ ...target, text: '', source: null }), 'span.chip');
});

test('a brief starts a turn, or is queued after the running one', () => {
  assert.deepEqual(Redline.handoff({ kind: 'start' }, 'B'), {
    kind: 'send',
    delivery: 'start',
    text: 'B',
  });
  assert.deepEqual(Redline.handoff({ kind: 'continue' }, 'B').kind, 'send');
  // Queued after what was queued before, as one message: a queue replaces.
  assert.deepEqual(Redline.handoff({ kind: 'during', steer: true, queued: 'A' }, 'B'), {
    kind: 'send',
    delivery: 'queue',
    text: 'A\n\nB',
  });
  assert.deepEqual(Redline.handoff({ kind: 'during', steer: false, queued: null }, 'B'), {
    kind: 'send',
    delivery: 'queue',
    text: 'B',
  });
  assert.deepEqual(Redline.handoff({ kind: 'closed', reason: 'stopping' }, 'B'), {
    kind: 'wait',
    reason: 'stopping',
  });
});

test("a brief's code spans fence past backticks inside, and a note's lines stay in its item", () => {
  assert.equal(Redline.code('a.b'), '`a.b`');
  assert.equal(Redline.code('a `b` c'), '`` a `b` c ``');
  const brief = Redline.brief({ ...page, route: null, revision: null }, [
    mark(1, 'First line\nsecond line'),
  ]);
  assert.ok(brief.includes('1. First line\n   second line\n   - Element:'));
});

test('a signed-out client is never switched from on its own, only offered another', () => {
  const client = (harness: RunClient['harness'], sign_in: RunClient['sign_in']): RunClient => ({
    harness,
    status: 'ready',
    approvals: [],
    midway: [],
    sign_in,
    sign_in_methods: [],
  });
  const clients = [
    client('claude_code', { kind: 'signed_out' }),
    client('codex', { kind: 'not_checked' }),
    client('pi', { kind: 'signed_out' }),
  ];
  assert.equal(Redline.instead(['claude_code', 'pi', 'codex'], clients, 'claude_code'), 'codex');
  assert.equal(Redline.instead(['claude_code', 'pi'], clients, 'claude_code'), null);
});
