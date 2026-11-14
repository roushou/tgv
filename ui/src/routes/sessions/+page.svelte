<script lang="ts">
  import { onMount, tick, untrack } from 'svelte';
  import { SvelteMap } from 'svelte/reactivity';
  import { goto } from '$app/navigation';
  import { ManagementApi, clock, short, type CallView } from '#lib/api.ts';
  import { ago } from '#lib/overview.ts';
  import { harnessLabel } from '#lib/activity.ts';
  import { callStatus, statusBadge, tokenSummary } from '#lib/display.ts';
  import { getWorkbench } from '#lib/workbench.svelte.ts';
  import {
    Desk,
    RunCommandError,
    Runs,
    RunsClient,
    Transcript,
    runUpdate,
    type Decision,
    type DeskRun,
    type Entry,
    type Remembered,
    Compose,
    Setup,
    type RunClient,
    type Drivable,
    type RunProject,
    type HistoryChange,
    type Read,
    type RunId,
    type Shelf,
    type RunSnapshot,
    type Waiting,
    Sessions,
    Memory,
    MIDWAY,
    DRIVABLE,
    type DeskSession,
    type SessionBody,
    type SessionId,
    type ClientCommand,
    type FileMatch,
  } from '#lib/runs.ts';
  import { Mentions } from '#lib/mention.ts';
  import ComposerMenu, { type MenuEntry } from './ComposerMenu.svelte';
  import Icon from '#lib/Icon.svelte';
  import Markdown from '#lib/Markdown.svelte';
  import ActionMenu, { type ActionItem } from '#lib/ActionMenu.svelte';
  import { assertNever } from '#lib/decode.ts';
  import NewSession from './NewSession.svelte';
  import SignInSheet from '#lib/SignInSheet.svelte';
  import { SignIns } from '#lib/signIn.ts';
  import RunSettingsBar from './RunSettingsBar.svelte';
  import SessionList from './SessionList.svelte';
  import type { PageProps } from './$types';

  let { data }: PageProps = $props();
  getWorkbench().showScope(scope);

  const client = new RunsClient();
  const api = new ManagementApi();

  /** What the server read for the first render; the feed follows from here. */
  const initial = untrack(() => data.initial);
  const first = <T,>(read: Read<T> | undefined, none: T): T =>
    read?.state === 'read' ? read.value : none;
  let live = $state.raw<RunSnapshot[]>(first(initial?.live, []));
  /** The sessions, current and archived, as stored and served. */
  let sessions = $state.raw<SessionBody[]>(first(initial?.sessions, []));
  let archivedSessions = $state.raw<SessionBody[]>(first(initial?.archivedSessions, []));
  /** Which sessions the list shows. */
  let shelf = $state<Shelf>('current');
  /** The session whose delete waits for a second click. */
  let confirming = $state<SessionId | null>(null);
  /** Why runs cannot be listed or driven, when they cannot. */
  let unavailable = $state<string | null>(
    initial?.live.state === 'failed' ? initial.live.reason : null,
  );
  let feed = $state<'connecting' | 'open' | 'closed'>('connecting');
  /** Each run's conversation since this page started following it. */
  const transcripts = new SvelteMap<RunId, Transcript>();
  // The selected run's conversation, as the page was loaded with it.
  untrack(() => {
    if (data.conversation) {
      transcripts.set(data.conversation.run, new Transcript().load(data.conversation.conversation));
    }
  });
  /** The session the page shows, from the URL. */
  const chosen = $derived(data.session);
  /** The folder scratch sessions get theirs in; null when there is none. */
  let scratch = $state<string | null>(first(initial?.places, null)?.scratch ?? null);
  const folders = $derived(
    Sessions.folders(shelf === 'current' ? sessions : archivedSessions, live, scratch),
  );
  const session = $derived.by((): DeskSession | null => {
    const body = [...sessions, ...archivedSessions].find((entry) => entry.session === chosen);
    return body ? Sessions.desk(body, live) : null;
  });
  /** The run the page shows: the session's newest process. */
  const selected = $derived(session?.run ?? null);
  const current = $derived.by((): DeskRun | null => {
    if (!session?.run) return null;
    const run = session.run;
    const record = session.runs.find((entry) => entry.id === run) ?? null;
    return {
      run,
      harness: session.harness,
      workdir: session.workdir,
      started_at: record?.started_at ?? null,
      state: live.find((entry) => entry.run === run)?.state ?? null,
      recorded: record,
      settings: session.settings,
    };
  });
  const runState = $derived(current ? Desk.state(current) : null);
  const controls = $derived(runState ? Runs.controls(runState) : null);
  const mark = $derived(runState ? Runs.mark(runState) : null);
  /**
   * How the session stands, in its header: while its client runs, when it
   * could not start, or that it is a draft. Resting, it says nothing: the
   * next prompt starts its client again.
   */
  const headerMark = $derived.by(() => {
    if (!session) return null;
    const marked = Sessions.mark(session, '');
    return marked.label === '' ? null : marked;
  });
  const transcript = $derived(selected ? (transcripts.get(selected) ?? new Transcript()) : null);
  const waiting = $derived(folders.reduce((sum, folder) => sum + folder.waiting, 0));
  const liveCount = $derived(live.filter((run) => run.state.phase !== 'ended').length);

  /** The selected run's recorded turns, oldest first, and the session they belong to. */
  /** Why the selected run's conversation could not be read. */
  let readError = $state<string | null>(null);

  /** The projects the daemon offers to start a run in. */
  let offered = $state.raw<RunProject[]>(first(initial?.places, null)?.projects ?? []);
  /** The clients a run can start with, and whether each is ready. */
  let clients = $state.raw<RunClient[]>(first(initial?.clients, []));
  /** The client whose sign-in sheet is open. */
  let signingIn = $state<Drivable | null>(null);
  const tabs = [
    ['conversation', 'Conversation'],
    ['activity', 'Activity'],
    ['files', 'Files'],
    ['calls', 'Calls'],
  ] as const;
  /** What the selected run shows. */
  let tab = $state<(typeof tabs)[number][0]>('conversation');
  /** The selected run's model calls, read when its Calls tab opens. */
  let calls = $state.raw<{ session: string; calls: CallView[] } | null>(null);
  /** Now, for how long ago runs started; moves every half minute. */
  let now = $state(Date.now());
  let newSession = $state<ReturnType<typeof NewSession>>();
  let busy = $state(false);
  let notice = $state<string | null>(null);
  let conversation = $state<HTMLElement>();
  let composer = $state<HTMLTextAreaElement>();
  /** Follow the newest entry unless the operator scrolled up. */
  let following = true;

  function message(error: unknown): string {
    if (error instanceof RunCommandError) return error.message;
    if (error instanceof TypeError) return 'Cannot reach the Ren UI server.';
    return error instanceof Error ? error.message : 'Something went wrong.';
  }

  async function readLive() {
    if (!data.runsControl) return;
    try {
      live = await client.live();
      unavailable = null;
    } catch (error) {
      unavailable = message(error);
    }
  }

  async function readProjects() {
    if (!data.runsControl) return;
    try {
      ({ projects: offered, scratch } = await client.projects());
    } catch {
      // The list is a convenience; a typed path still starts a run.
    }
  }

  async function readClients() {
    if (!data.runsControl) return;
    try {
      clients = await client.clients();
    } catch {
      // Without the list, both drivable clients are offered.
    }
  }

  async function readCalls(session: string) {
    try {
      const page = await api.sessionCalls(session as never, null);
      calls = { session, calls: page.calls };
    } catch (error) {
      notice = message(error);
    }
  }

  /** What a session was asked first: as stored, else from its conversation here. */
  function title(entry: { run: RunId | null; asked?: string | null }): string | null {
    const text =
      entry.asked ?? (entry.run ? (transcripts.get(entry.run)?.firstPrompt() ?? null) : null);
    return text ? text.split('\n')[0]!.trim() || null : null;
  }

  async function readSessions() {
    if (!data.runsControl) return;
    try {
      [sessions, archivedSessions] = await Promise.all([
        client.sessions(),
        client.sessions('archived'),
      ]);
    } catch {
      // The list stays as it was; the feed brings changes.
    }
  }

  /**
   * Archive, unarchive, or delete the selected session. Archiving stops its
   * process; deleting removes the session with its ended runs and their
   * recorded calls, even one whose client never ran. The daemon applies each
   * after what it is recording, so the lists are read again until the
   * session moved.
   */
  async function edit(change: HistoryChange) {
    const target = chosen;
    if (!target) return;
    confirming = null;
    await act(async () => {
      await client.toSession(target, { kind: change });
      for (const delay of [150, 400, 1000, 2000]) {
        await new Promise((resolve) => setTimeout(resolve, delay));
        await readSessions();
        const listed = (bodies: SessionBody[]) => bodies.some((entry) => entry.session === target);
        const moved =
          change === 'archive'
            ? listed(archivedSessions)
            : change === 'unarchive'
              ? listed(sessions)
              : !listed(sessions) && !listed(archivedSessions);
        if (moved) break;
      }
      if (change !== 'unarchive') select(null);
    });
  }

  /**
   * Read the run's conversation, whole, as the daemon stored it, keeping
   * what streamed in meanwhile; and its record, for its session.
   */
  async function readConversation(run: RunId) {
    try {
      const conversation = await client.conversation(run);
      transcripts.set(run, (transcripts.get(run) ?? new Transcript()).load(conversation));
      if (selected === run) readError = null;
    } catch (error) {
      if (selected === run) readError = message(error);
    }
  }

  async function act(action: () => Promise<unknown>) {
    busy = true;
    notice = null;
    try {
      await action();
    } catch (error) {
      notice = message(error);
    } finally {
      busy = false;
    }
  }

  function select(next: SessionId | null) {
    void goto(next ? `/sessions?session=${next}` : '/sessions');
  }

  /** The client and settings last chosen here: a new session starts with them. */
  let remembered = $state<Remembered>(untrack(() => Memory.parse(data.remembered)));

  function remember(next: Remembered) {
    remembered = next;
    // Remembering is a convenience: a refused cookie changes nothing else.
    document.cookie = Memory.cookie(next);
  }

  /** A session in `dir` at once (null: a scratch folder of its own), opened:
   * on the client last chosen, with the settings kept for it that still
   * apply; or the draft there that was never asked anything. */
  async function create(dir: string | null) {
    // A draft there that was never asked anything is opened instead.
    const unused = Sessions.unused(sessions, live, scratch, dir);
    if (unused) {
      select(unused);
      return;
    }
    await act(async () => {
      const harness = remembered.harness;
      // The daemon keeps each client's models; without them, the client's own.
      const models = await client.models(harness).catch(() => []);
      const policies = clients.find((entry) => entry.harness === harness)?.approvals ?? [];
      const made = await client.createSession(
        harness,
        dir,
        Memory.settings(remembered, harness, models, policies),
      );
      await Promise.all([readSessions(), readLive()]);
      select(made);
    });
  }

  /** The sessions in the list's order, for j and k. */
  const order = $derived(folders.flatMap((folder) => folder.sessions));

  function typing(target: EventTarget | null): boolean {
    return (
      target instanceof HTMLElement &&
      (target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName))
    );
  }

  function shortcut(event: KeyboardEvent) {
    if (event.metaKey || event.ctrlKey || event.altKey || typing(event.target)) return;
    switch (event.key) {
      case 'n':
        event.preventDefault();
        select(null);
        void tick().then(() => newSession?.open());
        break;
      case 'j':
      case 'k': {
        if (!order.length) return;
        event.preventDefault();
        const at = order.findIndex((entry) => entry.session === chosen);
        const next = event.key === 'j' ? Math.min(at + 1, order.length - 1) : Math.max(at - 1, 0);
        select(order[at === -1 ? 0 : next]!.session);
        break;
      }
      case 'y':
      case 'a':
      case 'd': {
        const waiting = controls?.approvals[0];
        if (!waiting) return;
        event.preventDefault();
        answer(
          waiting,
          event.key === 'y' ? 'accept' : event.key === 'a' ? 'accept_for_session' : 'decline',
        );
        break;
      }
    }
  }

  /** The key that sends into a running turn, as this platform names it. */
  const navigatorKey =
    typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘' : 'Ctrl';
  /** Each session's draft, kept while the page is open. */
  const drafts = new SvelteMap<SessionId, string>();
  /** How far back ↑ has recalled the run's prompts; null: not recalling. */
  let recalling = $state<number | null>(null);
  /** What was written before recalling, for ↓ to come back to. */
  let beforeRecall = '';
  const draft = $derived(chosen ? (drafts.get(chosen) ?? '') : '');
  function write(text: string) {
    if (chosen) drafts.set(chosen, text);
  }
  /** Where the caret is in the prompt, for what the word there asks to be offered. */
  let caret = $state(0);
  /** Where an offer closed with Esc starts: it stays closed until another word asks. */
  let dismissed = $state<number | null>(null);
  // A closed composer's prompt is disabled, so nothing asks there.
  const mention = $derived(Mentions.at(draft, Math.min(caret, draft.length)));
  const offering = $derived(mention && mention.start !== dismissed ? mention : null);
  /** Each run's commands, as its client listed them. */
  const commandLists = new SvelteMap<RunId, ClientCommand[]>();
  let commandsNote = $state<string | null>(null);
  /** The newest file search's answer, with what it searched for. */
  let found = $state<{ session: SessionId; query: string; files: FileMatch[] } | null>(null);
  let filesNote = $state<string | null>(null);
  let active = $state(0);
  const menuId = 'composer-offers';

  $effect(() => {
    // The run's commands, read when a word first asks for them; asked again
    // while the client has listed none, as it lists them once it opens.
    const run = selected;
    const target = chosen;
    const asking = offering?.kind === 'command' ? offering.start : null;
    if (!run || !target || asking === null) return;
    if ((untrack(() => commandLists.get(run))?.length ?? 0) > 0) return;
    commandsNote = 'Reading the commands…';
    const abort = new AbortController();
    client
      .sessionCommands(target, abort.signal)
      .then((commands) => {
        commandLists.set(run, commands);
        commandsNote =
          commands.length > 0
            ? null
            : `${harnessLabel(current?.harness ?? 'codex')} lists its commands once the conversation opens.`;
      })
      .catch((error: unknown) => {
        if (!abort.signal.aborted) commandsNote = message(error);
      });
    return () => abort.abort();
  });

  $effect(() => {
    // A short pause after typing, then the folder is searched.
    const target = chosen;
    const query = offering?.kind === 'file' ? offering.query : null;
    if (!target || query === null) return;
    const abort = new AbortController();
    const timer = setTimeout(() => {
      client
        .sessionFiles(target, query, 20, abort.signal)
        .then((files) => {
          found = { session: target, query, files };
          filesNote = files.length > 0 ? null : 'No file matches.';
        })
        .catch((error: unknown) => {
          if (!abort.signal.aborted) filesNote = message(error);
        });
    }, 90);
    return () => {
      clearTimeout(timer);
      abort.abort();
    };
  });

  const offeredCommands = $derived(
    offering?.kind === 'command' && selected
      ? Mentions.commands(commandLists.get(selected) ?? [], offering)
      : [],
  );
  const offeredFiles = $derived(
    offering?.kind === 'file' && found && found.session === chosen ? found.files : [],
  );
  const entries = $derived.by((): MenuEntry[] => {
    if (offering?.kind === 'command') {
      return offeredCommands.map((command) => {
        const at = command.name.toLowerCase().indexOf(offering.query.toLowerCase());
        const hits =
          offering.query === '' || at < 0
            ? []
            : Array.from(offering.query, (_, index) => command.sigil.length + at + index);
        return {
          key: command.invocation,
          parts: Mentions.marked(command.invocation, hits),
          hint: command.hint,
          tag: command.kind === 'command' ? null : command.kind,
          detail: command.description,
        };
      });
    }
    if (offering?.kind === 'file') {
      return offeredFiles.map((file) => ({
        key: file.path,
        parts: Mentions.marked(file.path, file.positions),
        hint: null,
        tag: file.kind === 'directory' ? 'folder' : null,
        detail: '',
      }));
    }
    return [];
  });
  $effect(() => {
    // A new list starts at its best entry.
    void entries;
    active = 0;
  });

  /** Write the chosen entry in place of the word that asked for it. */
  function pick(index: number) {
    if (!offering) return;
    const written =
      offering.kind === 'command'
        ? offeredCommands[index] && Mentions.command(draft, offering, offeredCommands[index])
        : offeredFiles[index] && Mentions.file(draft, offering, offeredFiles[index]);
    if (!written) return;
    write(written.text);
    caret = written.caret;
    void tick().then(() => {
      composer?.focus();
      composer?.setSelectionRange(written.caret, written.caret);
    });
  }

  function trackCaret(event: Event) {
    caret = (event.currentTarget as HTMLTextAreaElement).selectionStart;
  }

  /** What the selected run's client takes mid-run. */
  const client_ = $derived(clients.find((entry) => entry.harness === session?.harness) ?? null);

  /**
   * Send what is written to the session: it starts a turn (starting or
   * resuming its client when none runs), or, while a turn runs, is queued
   * for after it or steers it.
   */
  async function send(delivery: 'queue' | 'steer' = 'queue') {
    const text = draft.trim();
    const target = chosen;
    const run = selected;
    if (!target || !text || !compose) return;
    // Shown at once where its process runs; a new process reads it back.
    const shown = (sent: string) => {
      if (run && session && Sessions.running(session)) {
        transcripts.set(run, (transcripts.get(run) ?? new Transcript()).prompted(sent));
      }
    };
    await act(async () => {
      switch (compose.kind) {
        case 'start':
        case 'continue':
          await client.toSession(target, { kind: 'prompt', text, delivery: 'start' });
          shown(text);
          break;
        case 'during':
          if (delivery === 'steer' && compose.steer) {
            await client.toSession(target, { kind: 'prompt', text, delivery: 'steer' });
            shown(text);
          } else {
            // Queued after what was queued before: one message, in order.
            const next = compose.queued ? `${compose.queued}\n\n${text}` : text;
            await client.toSession(target, { kind: 'prompt', text: next, delivery: 'queue' });
          }
          break;
        case 'closed':
          return;
        default:
          return assertNever(compose);
      }
      await Promise.all([readSessions(), readLive()]);
      drafts.delete(target);
      recalling = null;
      following = true;
    });
  }

  function answer(waiting: Waiting, decision: Decision) {
    const target = chosen;
    if (!target) return;
    void act(() =>
      client.toSession(target, { kind: 'answer', approval: waiting.approval, decision }),
    );
  }

  /** Take the queued prompt back into the composer to change it. */
  async function editQueued(queued: string) {
    const target = chosen;
    if (!target) return;
    await act(async () => {
      await client.toSession(target, { kind: 'unqueue' });
      write(draft ? `${queued}\n\n${draft}` : queued);
      composer?.focus();
    });
  }

  function composerKey(event: KeyboardEvent) {
    if (event.isComposing) return;
    // While something is offered, the arrows, Enter, and Tab are its.
    if (offering) {
      const count = entries.length;
      if (event.key === 'Escape') {
        event.preventDefault();
        dismissed = offering.start;
        return;
      }
      if (count > 0 && (event.key === 'ArrowDown' || event.key === 'ArrowUp')) {
        event.preventDefault();
        active = (active + (event.key === 'ArrowDown' ? 1 : count - 1)) % count;
        return;
      }
      if (
        count > 0 &&
        (event.key === 'Tab' ||
          (event.key === 'Enter' && !event.shiftKey && !event.metaKey && !event.ctrlKey))
      ) {
        event.preventDefault();
        pick(active);
        return;
      }
    }
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      // Cmd or Ctrl+Enter sends into the running turn, where the client can.
      void send(event.metaKey || event.ctrlKey ? 'steer' : 'queue');
      return;
    }
    const field = event.currentTarget as HTMLTextAreaElement;
    const atStart = field.selectionStart === 0 && field.selectionEnd === 0;
    if (event.key === 'ArrowUp' && atStart && (draft === '' || recalling !== null)) {
      const prompts = selected ? (transcripts.get(selected)?.prompts() ?? []) : [];
      if (recalling === null) beforeRecall = draft;
      const next = Compose.recall(prompts, recalling, true);
      if (!next) return;
      event.preventDefault();
      recalling = next.at;
      write(next.text);
    } else if (event.key === 'ArrowDown' && recalling !== null) {
      const prompts = selected ? (transcripts.get(selected)?.prompts() ?? []) : [];
      const next = Compose.recall(prompts, recalling, false);
      if (!next) return;
      event.preventDefault();
      recalling = next.at;
      write(next.at === null ? beforeRecall : next.text);
    }
  }

  function onScroll() {
    if (!conversation) return;
    following = conversation.scrollHeight - conversation.scrollTop - conversation.clientHeight < 48;
  }

  $effect(() => {
    // Keep the newest entry in view as the conversation grows.
    void transcript?.entries;
    if (!following || !conversation) return;
    void tick().then(() => conversation?.scrollTo({ top: conversation.scrollHeight }));
  });

  $effect(() => {
    const run = selected;
    readError = null;
    calls = null;
    confirming = null;
    tab = 'conversation';
    following = true;
    if (!run) return;
    // Loaded with the page when it was opened on this run; read otherwise.
    // Untracked: the effect follows the selection, not the transcripts it
    // writes, or writing one would run it again.
    untrack(() => {
      const given = data.conversation;
      if (given?.run === run) {
        transcripts.set(run, (transcripts.get(run) ?? new Transcript()).load(given.conversation));
      } else {
        void readConversation(run);
      }
    });
  });

  $effect(() => {
    // The Calls tab reads its session's calls when it opens.
    const session = current?.recorded?.session ?? null;
    if (tab === 'calls' && session && calls?.session !== session) void readCalls(session);
  });

  $effect(() => {
    // The composer is where the keyboard goes once a run takes prompts.
    if (controls?.prompt) composer?.focus({ preventScroll: true });
  });

  onMount(() => {
    const ticking = setInterval(() => (now = Date.now()), 30_000);
    // The server read them for the first render; only what it could not
    // read is asked for again. Live runs are read once the feed opens.
    if (initial?.places.state !== 'read') void readProjects();
    if (initial?.clients.state !== 'read') void readClients();
    if (initial?.sessions.state !== 'read' || initial.archivedSessions.state !== 'read')
      void readSessions();
    if (!initial) void readLive();
    if (!data.runsControl) return () => clearInterval(ticking);
    const source = new EventSource('/control/runs/events');
    source.addEventListener('open', () => {
      feed = 'open';
      // Whatever changed while the feed was closed.
      void readLive();
    });
    source.addEventListener('error', () => {
      feed = source.readyState === EventSource.CLOSED ? 'closed' : 'connecting';
    });
    source.addEventListener('lagged', () => void readLive());
    // Sessions change as they are made, configured, archived, or get a new
    // process: each change reads the list again.
    const sessionFeed = new EventSource('/control/sessions/events');
    sessionFeed.addEventListener('session', () => void readSessions());
    sessionFeed.addEventListener('lagged', () => void readSessions());
    source.addEventListener('run', (event) => {
      let update;
      try {
        update = runUpdate(JSON.parse((event as MessageEvent<string>).data), '');
      } catch {
        return;
      }
      if (update.kind === 'state') {
        if (!live.some((run) => run.run === update.run)) {
          void readLive();
        } else {
          live = Runs.update(live, update);
        }
        if (update.state.phase === 'ended' || update.state.phase === 'idle') void readSessions();
        return;
      }
      transcripts.set(
        update.run,
        (transcripts.get(update.run) ?? new Transcript()).apply(update.event),
      );
    });
    return () => {
      source.close();
      sessionFeed.close();
      clearInterval(ticking);
    };
  });

  /** Its first prompt, whole, for the title's tooltip. */
  function firstPrompt(run: DeskRun): string | null {
    return transcripts.get(run.run)?.firstPrompt()?.trim() || null;
  }

  /** The session's facts after its state: client and settings, place, and when. */
  const facts = $derived(
    session
      ? [
          [harnessLabel(session.harness), ...Setup.words(session.settings)].join(' · '),
          Runs.place(session.workdir, scratch, offered),
          ...(session.activeAt ? [ago(session.activeAt, now)] : []),
        ]
      : [],
  );

  /** What the run's menu says of it: the details the header leaves out. */
  const details = $derived(
    session && current
      ? [
          `Session ${short(session.session)}`,
          `Process ${short(current.run)}`,
          current.workdir,
          ...(runState?.phase === 'ended'
            ? [Runs.exitDetail(runState.exit)].filter((d) => d !== null)
            : []),
        ]
      : [],
  );

  /** The run's other actions, least risky first; destructive ones last. */
  const menu = $derived.by((): ActionItem[] => {
    if (!current || !controls || !session) return [];
    const items: ActionItem[] = [];
    const target = session.session;
    if (data.runsControl && controls.stop) {
      items.push({
        label: 'Stop its client',
        onselect: () => void act(() => client.toSession(target, { kind: 'stop' })),
      });
    }
    const recording = current.recorded?.session;
    if (recording)
      items.push({
        label: 'Open its calls',
        onselect: () => void goto(`/recordings/${recording}`),
      });
    if (data.runsControl) {
      const archived_ = session.shelf === 'archived';
      items.push({
        label: archived_ ? 'Unarchive' : 'Archive',
        onselect: () => void edit(archived_ ? 'unarchive' : 'archive'),
      });
      if (!Sessions.running(session)) {
        items.push({ label: 'Delete…', danger: true, onselect: () => (confirming = target) });
      }
    }
    return items;
  });

  const compose = $derived(
    session ? Compose.session(current ? runState : null, client_?.midway ?? []) : null,
  );
</script>

{#snippet scope()}
  <div class="scope-group scope-identity">
    <span class="scope-title">Sessions</span>
    <span class="muted"
      >{liveCount} running{waiting ? ` · ${waiting} waiting for you` : ''}{data.runsControl &&
      feed !== 'open'
        ? ' · updates reconnecting'
        : ''}</span
    >
  </div>
{/snippet}

{#snippet ask(waiting: Waiting, index: number)}
  {@const line = Runs.ask(waiting.ask)}
  <div class="approval" role="group" aria-label="Approval {index + 1}">
    <div class="approval-text">
      <strong><Icon name="shield" size={13} /> {line.action}</strong>
      {#if line.detail}<pre class="approval-detail mono">{line.detail}</pre>{/if}
      {#if waiting.ask.kind === 'command' && waiting.ask.cwd}<span class="muted mono"
          >in {waiting.ask.cwd}</span
        >{/if}
      {#if line.reason}<span class="muted">{line.reason}</span>{/if}
    </div>
    <div class="approval-actions">
      <button class="button primary small" disabled={busy} onclick={() => answer(waiting, 'accept')}
        >Allow</button
      >
      <button
        class="button small"
        disabled={busy}
        onclick={() => answer(waiting, 'accept_for_session')}
        title="Allow this, and the same kind of action for the rest of the session"
        >Allow for this run</button
      >
      <button
        class="button small"
        disabled={busy}
        onclick={() => answer(waiting, 'decline')}
        title="Refuse it; the turn goes on">Deny</button
      >
      <button
        class="button small"
        disabled={busy}
        onclick={() => answer(waiting, 'cancel')}
        title="Refuse it and end the turn">Deny and stop</button
      >
    </div>
  </div>
{/snippet}

{#snippet entryView(entry: Entry)}
  {#if entry.kind === 'prompt'}
    <div class="entry prompt"><Markdown text={entry.text} /></div>
  {:else if entry.kind === 'message'}
    <div class="entry reply" class:streaming={!entry.done}>
      <Markdown text={entry.text} />
    </div>
  {:else if entry.kind === 'reasoning'}
    {#if !entry.done}<div class="entry tool muted">
        <Icon name="thought" size={12} /> Thinking…
      </div>{/if}
  {:else if entry.kind === 'command'}
    <details class="entry tool command">
      <summary>
        <Icon name="terminal" size={12} />
        <span class="tool-target mono">{entry.command}</span>
        <span
          class="chip {entry.status === 'completed' && (entry.exit === 0 || entry.exit === null)
            ? 'success'
            : entry.status === 'in_progress'
              ? 'live'
              : entry.status === 'declined'
                ? 'neutral'
                : 'error'}"
          >{entry.status === 'in_progress'
            ? 'running'
            : entry.exit !== null
              ? `exit ${entry.exit}`
              : entry.status}</span
        >
      </summary>
      {#if entry.output}<pre class="activity-pre">{entry.output}</pre>{/if}
    </details>
  {:else if entry.kind === 'file_change'}
    <div class="entry tool">
      <Icon name="pencil" size={12} />
      <span class="tool-target mono" title={entry.paths.join('\n') || undefined}
        >{entry.paths.map((path) => Runs.within(path, session?.workdir ?? null)).join(', ') ||
          'files'}</span
      >
      <span class="chip {entry.status === 'completed' ? 'success' : 'neutral'}"
        >{entry.status === 'in_progress' ? 'editing' : entry.status}</span
      >
    </div>
  {:else if entry.kind === 'tool'}
    <div class="entry tool">
      <Icon name="tool" size={12} /><span class="tool-name">{entry.name}</span>
      {#if !entry.done}<span class="chip live">running</span>{/if}
    </div>
  {:else if entry.kind === 'turn_end'}
    {#if entry.end.kind !== 'completed'}<p
        class="entry notice-entry {entry.end.kind === 'interrupted' ? 'warn' : 'danger'}"
      >
        Turn {entry.end.kind === 'interrupted'
          ? 'interrupted'
          : entry.end.kind === 'failed'
            ? `failed${entry.end.message ? `: ${entry.end.message}` : ''}`
            : `ended: ${entry.end.status}`}
      </p>{/if}
  {:else if entry.kind === 'notice'}
    <p class="entry notice-entry {entry.tone}">{entry.text}</p>
  {/if}
{/snippet}

{#snippet activity()}
  {@const steps = Runs.turns(transcript?.entries ?? []).map((turn) => ({
    prompt: turn[0]?.kind === 'prompt' ? turn[0].text : 'Turn',
    steps: turn.filter(
      (entry) => entry.kind === 'command' || entry.kind === 'file_change' || entry.kind === 'tool',
    ),
  }))}
  {#each steps.filter((turn) => turn.steps.length) as turn, index (index)}
    <div class="band">{turn.prompt.split('\n')[0]}</div>
    {#each turn.steps as entry, step (step)}{@render entryView(entry)}{/each}
  {:else}
    <p class="quiet-empty">No steps yet.</p>
  {/each}
{/snippet}

{#snippet files()}
  {@const written = [
    ...new Set(
      (transcript?.entries ?? []).flatMap((entry) =>
        entry.kind === 'file_change' ? entry.paths : [],
      ),
    ),
  ]}
  <div class="band">Written <span class="count">{written.length}</span></div>
  {#each written as path (path)}<div class="entry tool mono" title={path}>
      <Icon name="pencil" size={11} />
      {Runs.within(path, session?.workdir ?? null)}
    </div>{:else}<p class="quiet-empty">Nothing written yet.</p>{/each}
{/snippet}

{#snippet callList()}
  {#if !current?.recorded?.session}
    <p class="quiet-empty">No calls recorded yet.</p>
  {:else if !calls}
    <p class="quiet-empty">Reading calls…</p>
  {:else}
    <table class="calls">
      <thead
        ><tr><th>Started</th><th>Model</th><th>Ended</th><th>Time</th><th>Tokens</th></tr></thead
      >
      <tbody>
        {#each calls.calls as call (call.id)}
          {@const badge = statusBadge(callStatus(call))}
          {@const tokens = tokenSummary(call.usage)}
          <tr>
            <td class="mono muted">{clock(call.started_at)}</td>
            <td class="mono">{call.model ?? '—'}</td>
            <td><span class="chip {badge.tone}">{badge.label}</span></td>
            <td class="mono muted"
              >{call.ended_at
                ? `${Math.round((call.ended_at - call.started_at) / 100) / 10} s`
                : '…'}</td
            >
            <td class="mono muted" title={tokens?.title}
              >{tokens ? `${tokens.input} in · ${tokens.output} out` : '—'}</td
            >
          </tr>
        {/each}
      </tbody>
    </table>
    <a class="foot-link" href="/recordings/{current.recorded.session}"
      >Open its recording <Icon name="arrow" size={11} /></a
    >
  {/if}
{/snippet}

<svelte:window onkeydown={shortcut} />

<div class="runs-page">
  <aside class="runs-sidebar" aria-label="Runs">
    {#if unavailable}<p class="sidebar-note danger">{unavailable}</p>{/if}
    <SessionList
      {folders}
      selected={chosen}
      {now}
      {title}
      place={(workdir) => Runs.place(workdir, scratch, offered)}
      {scratch}
      {shelf}
      archivedCount={archivedSessions.length}
      {busy}
      onshelf={(next) => {
        shelf = next;
        select(null);
      }}
      onselect={(next) => select(next)}
      oncreate={(dir) => void create(dir)}
      onnew={() => {
        select(null);
        void tick().then(() => newSession?.open());
      }}
    />
  </aside>

  <section class="run-main" aria-label="Run">
    {#if !current || !runState || !mark || !controls}
      {#if data.runsControl}
        {#if chosen && !session}<p class="lost muted">
            This session is not listed. Start another below.
          </p>{/if}
        <NewSession
          bind:this={newSession}
          {offered}
          {scratch}
          runs={folders.flatMap((folder) =>
            folder.sessions.map((entry) => ({
              workdir: entry.workdir,
              started_at: entry.runs[0]?.started_at ?? null,
            })),
          )}
          {busy}
          oncreate={(dir) => void create(dir)}
        />
        {#if notice}<div class="notice danger" role="alert">
            <Icon name="alert" size={14} /><span>{notice}</span>
          </div>{/if}
      {:else}
        <div class="empty-state">
          <div class="empty-icon"><Icon name="lock" size={26} /></div>
          <h2>This UI is read-only</h2>
          <p>
            To start and drive agent runs here, give the UI server the daemon’s control token file
            with <code>REN_CONTROL_TOKEN_FILE</code> (and, when published with Tailscale Serve, the
            allowed logins in <code>REN_CONTROL_LOGINS</code>), and add folders to the daemon’s
            <code>[runs] roots</code>.
          </p>
        </div>
      {/if}
    {:else}
      <header class="run-head">
        <div class="run-heading">
          <h2 class="run-title" title={firstPrompt(current) ?? undefined}>
            {title({ run: current.run, asked: session?.asked }) ??
              `New ${harnessLabel(current.harness)} session`}
          </h2>
          <p class="run-facts">
            {#if headerMark}<span class="state tone-{headerMark.tone}"
                ><span class="dot"></span>{headerMark.label}</span
              >{/if}
            {#each facts as fact, index (fact)}{#if headerMark || index > 0}<span
                  class="sep"
                  aria-hidden="true">·</span
                >{/if}<span>{fact}</span>{/each}
          </p>
        </div>
        {#if confirming === chosen}
          <span class="run-actions confirming" role="alert">
            <span class="confirm-text">Delete this session and its recorded calls for good?</span>
            <button
              class="button small danger confirm"
              disabled={busy}
              onclick={() => edit('delete')}>Delete</button
            >
            <button class="button small" onclick={() => (confirming = null)}>Cancel</button>
          </span>
        {:else}
          <span class="run-actions">
            {#if data.runsControl && controls.interrupt}<button
                class="button small"
                disabled={busy}
                onclick={() =>
                  chosen && void act(() => client.toSession(chosen, { kind: 'interrupt' }))}
                >Interrupt</button
              >{/if}
            <ActionMenu label="Session actions" items={menu} {details} disabled={busy} />
          </span>
        {/if}
      </header>

      <div class="tabs" role="tablist" aria-label="What the run shows">
        {#each tabs as [name, label] (name)}
          <button
            role="tab"
            aria-selected={tab === name}
            class:chosen={tab === name}
            onclick={() => (tab = name)}>{label}</button
          >
        {/each}
      </div>

      {#if tab === 'conversation'}
        <div class="conversation" bind:this={conversation} onscroll={onScroll}>
          {#if readError}<p class="entry notice-entry danger">{readError}</p>{/if}
          {#if transcript?.earlier}<p class="quiet-empty">
              Earlier turns are not shown: the conversation is longer than one read.
            </p>{/if}
          <!-- The whole conversation, every turn open, as one scroll. -->
          {#each Runs.turns(transcript?.entries ?? []) as turn, index (index)}
            <section class="turn">
              {#each turn as entry, at (at)}{@render entryView(entry)}{/each}
            </section>
          {/each}
          {#if runState.phase === 'working' && !controls.approvals.length}<div
              class="entry tool muted working"
            >
              <span class="phase-dot tone-live"></span> Working…
            </div>{/if}
          {#if transcript?.read && !transcript.entries.length && runState.phase === 'idle'}
            <p class="quiet-empty">The conversation is open. Say what to do.</p>
          {/if}
        </div>
      {:else if tab === 'activity'}
        <div class="conversation">
          {@render activity()}
        </div>
      {:else if tab === 'files'}
        <div class="conversation">
          {@render files()}
        </div>
      {:else}
        <div class="conversation">
          {@render callList()}
        </div>
      {/if}

      {#if controls.approvals.length}
        <div class="approvals" aria-label="Waiting for your answer">
          {#each controls.approvals as waiting, index (waiting.approval)}{@render ask(
              waiting,
              index,
            )}{/each}
        </div>
      {/if}
      {#if notice}<div class="notice danger" role="alert">
          <Icon name="alert" size={14} /><span>{notice}</span>
        </div>{/if}
      {#if data.runsControl && compose && !(compose.kind === 'closed' && runState.phase === 'ended')}
        {#if compose.kind === 'during' && compose.queued}
          <div class="queued" role="status">
            <span class="queued-label">After this turn</span>
            <span class="queued-text">{compose.queued}</span>
            <button
              class="text-button"
              disabled={busy}
              onclick={() => editQueued(compose.queued ?? '')}>Edit</button
            >
            <button
              class="text-button"
              disabled={busy}
              onclick={() =>
                chosen && void act(() => client.toSession(chosen, { kind: 'unqueue' }))}
              >Remove</button
            >
          </div>
        {/if}
        {#if client_ && SignIns.signedOut(client_.sign_in) && Runs.drivable(current.harness)}
          <div class="signed-out" role="status">
            <span
              >{harnessLabel(current.harness)} is signed out on the daemon’s host, so a prompt would fail.</span
            >
            {#if client_.sign_in_methods.length > 0}
              <button
                class="button small"
                type="button"
                onclick={() => (signingIn = Runs.drivable(current.harness))}>Sign in</button
              >
            {/if}
          </div>
        {/if}
        <form
          class="composer"
          onsubmit={(event) => {
            event.preventDefault();
            void send('queue');
          }}
        >
          {#if offering}
            <ComposerMenu
              id={menuId}
              heading={offering.kind === 'file'
                ? 'Files in the run’s folder'
                : offering.sigil === '$'
                  ? `${harnessLabel(current.harness)} skills`
                  : `${harnessLabel(current.harness)} commands`}
              {entries}
              {active}
              note={offering.kind === 'file'
                ? (filesNote ?? (found ? null : 'Searching…'))
                : (commandsNote ??
                  ((commandLists.get(selected ?? ('' as RunId))?.length ?? 0) > 0
                    ? 'No command matches.'
                    : null))}
              onpick={pick}
              onpoint={(index) => (active = index)}
            />
          {/if}
          <textarea
            bind:this={composer}
            value={draft}
            oninput={(event) => {
              recalling = null;
              dismissed = null;
              write(event.currentTarget.value);
              trackCaret(event);
            }}
            onkeydown={composerKey}
            onkeyup={trackCaret}
            onclick={trackCaret}
            onfocus={trackCaret}
            role="combobox"
            aria-autocomplete="list"
            aria-expanded={offering !== null}
            aria-controls={offering ? menuId : undefined}
            aria-activedescendant={offering && entries.length > 0
              ? `${menuId}-${active}`
              : undefined}
            rows="3"
            disabled={compose.kind === 'closed'}
            placeholder={compose.kind === 'start'
              ? `Ask ${harnessLabel(current.harness)} to do something. Enter sends, ↑ recalls an earlier prompt.`
              : compose.kind === 'during'
                ? compose.steer
                  ? `Enter queues for after this turn; ${navigatorKey}+Enter sends it into the turn now.`
                  : `Enter queues for after this turn; ${harnessLabel(current.harness)} reads it then.`
                : compose.kind === 'continue'
                  ? `Write to ${harnessLabel(current.harness)} to go on.`
                  : compose.reason}
            aria-label="Prompt"></textarea>
          <div class="composer-bar">
            {#if Runs.drivable(current.harness) && client_ && session}
              <RunSettingsBar
                harness={Runs.drivable(current.harness) ?? 'codex'}
                settings={session.settings}
                midway={Sessions.running(session) ? client_.midway : MIDWAY}
                approvals={client_.approvals}
                listModels={(harness) => client.models(harness)}
                {busy}
                onchange={(settings) => {
                  const harness = Runs.drivable(current.harness);
                  if (harness) remember(Memory.set(remembered, harness, settings));
                  if (chosen)
                    void act(async () => {
                      await client.toSession(chosen, { kind: 'settings', ...settings });
                      await Promise.all([readSessions(), readLive()]);
                    });
                }}
                choice={session.phase === 'draft'
                  ? {
                      clients: DRIVABLE.filter((harness) =>
                        clients.some(
                          (entry) => entry.harness === harness && entry.status === 'ready',
                        ),
                      ),
                      hint: (harness) => {
                        const entry = clients.find((offered) => offered.harness === harness);
                        return entry ? SignIns.hint(entry.sign_in) : null;
                      },
                      onclient: (harness) => {
                        remember(Memory.chose(remembered, harness));
                        if (chosen)
                          void act(async () => {
                            await client.toSession(chosen, { kind: 'client', harness });
                            await Promise.all([readSessions(), readLive()]);
                          });
                      },
                    }
                  : null}
              />
            {/if}
            <span class="composer-actions">
              {#if compose.kind === 'during' && compose.steer}<button
                  type="button"
                  class="button"
                  disabled={busy || !draft.trim()}
                  title="Add it to the turn that is running"
                  onclick={() => void send('steer')}>Send now</button
                >{/if}
              <button
                class="button primary"
                disabled={busy || !draft.trim() || compose.kind === 'closed'}
                >{compose.kind === 'during'
                  ? 'Queue'
                  : compose.kind === 'continue'
                    ? 'Continue'
                    : 'Send'}</button
              >
            </span>
          </div>
        </form>
      {/if}
    {/if}
  </section>
</div>
{#if signingIn}
  {@const harness = signingIn}
  <SignInSheet
    {harness}
    client={harnessLabel(harness)}
    methods={clients.find((entry) => entry.harness === harness)?.sign_in_methods ?? []}
    start={(method) => client.startSignIn(harness, method)}
    code={(code) => client.signInCode(harness, code)}
    cancel={() => client.cancelSignIn(harness)}
    onclose={() => {
      signingIn = null;
      void readClients();
    }}
  />
{/if}

<style>
  .runs-page {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: minmax(220px, 280px) 1fr;
    background: var(--canvas);
  }
  .runs-sidebar {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-right: 1px solid var(--line);
    background: var(--sidebar);
  }
  .sidebar-note {
    margin: 0;
    padding: 12px;
    font-size: 12px;
    color: var(--muted);
  }
  .sidebar-note.danger {
    color: var(--error);
    border-bottom: 1px solid var(--line);
  }
  .phase-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    flex-shrink: 0;
    background: var(--muted);
  }
  .tone-live.phase-dot {
    background: var(--live);
  }
  .tone-warn.phase-dot {
    background: var(--warning);
  }
  .tone-ok.phase-dot {
    background: var(--success);
  }
  .tone-danger.phase-dot {
    background: var(--error);
  }
  .tabs {
    display: flex;
    border-bottom: 1px solid var(--line);
    padding: 0 12px;
  }
  .tabs button {
    padding: 8px 12px;
    border: 0;
    border-bottom: 2px solid transparent;
    background: none;
    color: var(--muted);
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }
  .tabs button.chosen {
    color: var(--ink);
    border-bottom-color: var(--accent, var(--ink));
  }
  .lost {
    margin: 0;
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
    font-size: 12px;
  }
  /* A heading row: what the rows under it belong to. */
  .band {
    padding: 7px 16px;
    border-bottom: 1px solid var(--line);
    background: var(--sidebar);
    color: var(--muted);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .band .count {
    margin-left: 4px;
    color: var(--ink);
  }
  table.calls {
    font-size: 12px;
  }
  table.calls th:first-child,
  table.calls td:first-child {
    padding-left: 16px;
  }
  table.calls td {
    vertical-align: middle;
  }
  .foot-link {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 8px 16px;
    border-bottom: 1px solid var(--line);
    font-size: 12px;
  }
  .run-main :global(.quiet-empty) {
    margin: 0;
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
    font-size: 12px;
  }
  .run-main > .empty-state {
    margin: auto;
  }
  .run-main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    background: var(--surface);
  }
  .run-head {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
    font-size: 12px;
  }
  .run-heading {
    flex: 1;
    min-width: 0;
  }
  .run-title {
    margin: 0;
    overflow: hidden;
    font-size: 14px;
    font-weight: 600;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .run-facts {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 3px 0 0;
    overflow: hidden;
    color: var(--muted);
    white-space: nowrap;
  }
  .run-facts > span {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .run-facts .sep {
    flex: none;
  }
  .state {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    flex: none;
    color: var(--ink);
  }
  .state .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--muted);
  }
  .state.tone-live .dot {
    background: var(--live);
  }
  .state.tone-ok .dot {
    background: var(--success);
  }
  .state.tone-warn .dot {
    background: var(--warning);
  }
  .state.tone-danger .dot {
    background: var(--error);
  }
  .confirm-text {
    color: var(--error);
  }
  .run-actions {
    display: flex;
    flex: none;
    align-items: center;
    gap: 6px;
  }
  .conversation {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  /* A turn: its prompt, steps, and reply as rows, ruled off from the next. */
  .turn {
    border-bottom: 1px solid var(--line);
  }
  .entry {
    padding: 0 16px;
    font-size: 13px;
  }
  /* A prompt reads as Markdown, like a reply, with its line breaks kept
     as written. */
  .entry.prompt :global(.markdown) {
    padding: 10px 0;
  }
  .entry.prompt :global(.markdown p) {
    white-space: pre-wrap;
  }
  .entry.prompt {
    background: var(--surface-inset);
    box-shadow: inset 2px 0 var(--role-user);
    border-bottom: 1px solid var(--line);
  }
  .entry.reply {
    padding-top: 10px;
    padding-bottom: 10px;
    box-shadow: inset 2px 0 var(--role-assistant);
  }
  .entry.reply.streaming :global(.markdown > :last-child::after) {
    content: '▍';
    color: var(--muted);
  }
  .entry.tool {
    display: flex;
    align-items: center;
    gap: 8px;
    padding-top: 6px;
    padding-bottom: 6px;
    border-bottom: 1px solid var(--line-subtle);
    font-size: 12px;
    color: var(--ink);
  }
  .entry.tool.muted {
    color: var(--muted);
  }
  .entry.tool.command {
    display: block;
  }
  .entry.tool.command summary {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
  }
  .tool-name {
    color: var(--role-tool);
  }
  /* A step's state sits at the row's end, however long its target. */
  .entry.tool > .chip,
  .entry.tool.command summary > .chip {
    flex: none;
    margin-left: auto;
  }
  .tool-target {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .notice-entry {
    margin: 0;
    padding-top: 8px;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--line-subtle);
    font-size: 12px;
  }
  .notice-entry.warn {
    color: var(--warning);
  }
  .notice-entry.danger {
    color: var(--error);
  }
  .working {
    color: var(--muted);
  }
  .approvals {
    border-top: 1px solid var(--warning-border);
    background: var(--warning-soft);
  }
  .approval {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 10px 16px;
    font-size: 12px;
  }
  .approval + .approval {
    border-top: 1px solid var(--warning-border);
  }
  .approval-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    flex: 1;
  }
  .approval-text strong {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--warning);
  }
  .approval-detail {
    margin: 0;
    padding: 6px 8px;
    background: var(--surface);
    border: 1px solid var(--warning-border);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 160px;
    overflow: auto;
  }
  .approval-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  /* The prompt fills its strip; only the Send control sits inside it. */
  .composer {
    position: relative;
    display: flex;
    flex-direction: column;
    border-top: 1px solid var(--line);
    background: var(--surface);
  }
  .composer:focus-within {
    box-shadow: inset 2px 0 var(--accent);
  }
  .composer textarea {
    min-width: 0;
    resize: vertical;
    min-height: 64px;
    max-height: 40vh;
    padding: 12px 16px;
    border: 0;
    outline: none;
    background: transparent;
    color: var(--ink);
    font: inherit;
    font-size: 13px;
  }
  .composer-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 0 12px 10px 16px;
  }
  .composer-actions {
    display: flex;
    gap: 6px;
    margin-left: auto;
  }
  /* The session's client is signed out: said above the composer. */
  .signed-out {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 16px;
    border-top: 1px solid var(--warning-border);
    background: var(--warning-soft);
    color: var(--warning);
    font-size: 12px;
  }
  .signed-out span {
    flex: 1;
  }
  /* The prompt queued for after the turn, ruled off above the composer. */
  .queued {
    display: flex;
    align-items: baseline;
    gap: 10px;
    padding: 8px 16px;
    border-top: 1px solid var(--line);
    background: var(--surface);
    font-size: 12px;
  }
  .queued-label {
    flex: none;
    color: var(--muted);
  }
  .queued-text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  @media (max-width: 760px) {
    .runs-page {
      grid-template-columns: 1fr;
      grid-template-rows: auto 1fr;
    }
    .runs-sidebar {
      border-right: 0;
      border-bottom: 1px solid var(--line);
      max-height: 40vh;
    }
    .approval {
      flex-direction: column;
    }
  }
</style>
