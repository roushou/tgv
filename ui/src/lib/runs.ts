// Agent runs: what the daemon's control API says about them (through this
// UI server's `/control/runs` proxy), what the recorded history keeps, and
// the Runs view's pure logic: the live conversation built from run events,
// what each run can be asked, and how the sidebar groups runs by folder.
// No browser APIs beyond the `fetch` handed in; tested in tests/runs.test.ts.
import {
  API_VERSION,
  RecordingId,
  versioned,
  type Harness,
  type NativeThreadId,
  type NativeTurnId,
  type UnixMillis,
} from './api.ts';
import {
  checkedSignIn,
  finishedSignIn,
  signInMethod,
  signInState,
  startedSignIn,
  waitedSignIn,
  type Login,
  type LoginOutcome,
  type SignInMethod,
  type SignInState,
  type Waited,
} from './signIn.ts';
import {
  Uuid,
  array,
  assertNever,
  boolean,
  branded,
  brandedCount,
  count,
  isRecord,
  label,
  nullable,
  object,
  string,
  unrecognised,
  ShapeError,
  type Brand,
  type Decoder,
  type Stored,
} from './decode.ts';

export type RunId = Brand<string, 'RunId'>;
export const RunId = new Uuid<'RunId'>();
/** The client's own id for a request waiting on the operator. */
/** A session an operator works in: not a recording. */
export type SessionId = Brand<string, 'SessionId'>;
export const SessionId = new Uuid<'SessionId'>();
export type ApprovalId = Brand<string, 'ApprovalId'>;
/** A turn item, named as the client named it. */
export type ItemId = Brand<string, 'ItemId'>;

const harnesses = ['claude_code', 'codex', 'pi', 'opencode'] as const;
const runEnds = [
  'spawn_failed',
  'open_refused',
  'exited',
  'stopped',
  'drained',
  'recovered',
  'idled',
] as const;
export type RunEnd = (typeof runEnds)[number];
const stopCauses = ['operator', 'shutdown', 'open_refused', 'idle'] as const;
export type StopCause = (typeof stopCauses)[number];
const decisions = ['accept', 'accept_for_session', 'decline', 'cancel'] as const;
/** How much a model reasons, from none to the most. */
export const EFFORTS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const;
export type Effort = (typeof EFFORTS)[number];
/** What a running conversation can take mid-run. */
export const MIDWAY = ['steer', 'model', 'effort', 'approvals'] as const;
export type Midway = (typeof MIDWAY)[number];
/** How a prompt goes to a run: start a turn, after the running turn, or into it. */
export type Delivery = 'start' | 'queue' | 'steer';

/** What a driven client may do without asking. */
export const APPROVAL_POLICIES = ['ask', 'edits', 'auto'] as const;
export type ApprovalPolicy = (typeof APPROVAL_POLICIES)[number];

/** A run's settings; each null is the client's own. */
export interface RunSettings {
  model: ModelId | null;
  effort: Stored<Effort> | null;
  approvals: Stored<ApprovalPolicy> | null;
}
const runSettings = object<RunSettings>({
  model: nullable(branded<'ModelId'>()),
  effort: nullable(label(EFFORTS)),
  approvals: nullable(label(APPROVAL_POLICIES)),
});
export type Decision = (typeof decisions)[number];

/** How a run's process exited. */
export type ProcessExit =
  { kind: 'code'; value: number } | { kind: 'signal'; value: number } | { kind: 'unknown' };

/** What an approval asks the operator to allow. */
export type ApprovalAsk =
  | { kind: 'command'; command: string | null; cwd: string | null; reason: string | null }
  | { kind: 'file_change'; reason: string | null }
  | { kind: 'tool'; tool: string; detail: string | null }
  | { kind: 'unrecognized'; text: string };

export interface Waiting {
  approval: ApprovalId;
  ask: ApprovalAsk;
}

/** A run's lifecycle as the daemon holds it now. */
export type RunState =
  | { phase: 'starting' }
  | { phase: 'idle'; thread: NativeThreadId }
  | {
      phase: 'working';
      thread: NativeThreadId;
      turn: NativeTurnId | null;
      approvals: Waiting[];
      /** The operator's next prompt, sent as the turn ends. */
      queued: string | null;
    }
  | { phase: 'stopping'; cause: Stored<StopCause> }
  | { phase: 'ended'; end: Stored<RunEnd>; exit: ProcessExit | null }
  /** A phase a newer daemon reports. */
  | { phase: 'unrecognized'; text: string };

export interface RunSnapshot {
  run: RunId;
  harness: Stored<Harness>;
  workdir: string;
  state: RunState;
  settings: RunSettings;
}

/** Where an item's action stands; a status a newer daemon names is kept as text. */
export type ItemStatus = 'in_progress' | 'completed' | 'failed' | 'declined' | (string & {});

/** What the daemon read from an item, the same for every client. */
export type ItemRead =
  | { as: 'prompt'; text: string }
  | { as: 'reply'; text: string }
  | { as: 'reasoning' }
  | { as: 'plan'; text: string }
  | {
      as: 'command';
      command: string;
      status: ItemStatus;
      output: string | null;
      exit_code: number | null;
    }
  | { as: 'file_change'; paths: string[]; status: ItemStatus }
  | { as: 'tool'; name: string; status: ItemStatus }
  /** Nothing read, or a reading a newer daemon gives: shown by the item's kind. */
  | { as: 'unread' };

/** One item of a turn: its kind and what the daemon read of it. */
export interface DriveItem {
  id: ItemId;
  kind: string;
  read: ItemRead;
}

export type TurnEnd =
  | { kind: 'completed' }
  | { kind: 'interrupted' }
  | { kind: 'failed'; message: string | null }
  | { kind: 'unknown'; status: string };

/** What the agent reported, as the daemon relays it. Not stored. */
export type DriveEvent =
  | { kind: 'ready'; thread: NativeThreadId }
  | { kind: 'turn_started'; turn: NativeTurnId }
  | { kind: 'delta'; item: ItemId; text: string }
  | { kind: 'item'; phase: 'started' | 'completed'; item: DriveItem }
  | { kind: 'approval'; approval: ApprovalId; ask: ApprovalAsk }
  | { kind: 'approval_resolved'; approval: ApprovalId }
  | { kind: 'turn_ended'; turn: NativeTurnId; end: TurnEnd }
  | { kind: 'error'; message: string; retry: 'will_retry' | 'final' }
  | { kind: 'refused'; request: string; message: string }
  | { kind: 'unsupported'; method: string }
  /** Unknown, unreadable, or newer events: shown by their kind only. */
  | { kind: 'other'; name: string };

export type RunUpdate =
  { kind: 'state'; run: RunId; state: RunState } | { kind: 'event'; run: RunId; event: DriveEvent };

/** A run as recorded: its start, opening, and end. */
export interface RecordedRun {
  id: RunId;
  harness: Stored<Harness>;
  workdir: string;
  started_at: UnixMillis;
  thread: NativeThreadId | null;
  /** The session its model traffic was recorded under, once a call was. */
  session: RecordingId | null;
  ended_at: UnixMillis | null;
  end: Stored<RunEnd> | null;
  exit: ProcessExit | null;
  /** When the operator archived it; null when it is not. */
  archived_at: UnixMillis | null;
  /** Its settings as started; null is the client's own (or not recorded then). */
  model: ModelId | null;
  effort: Stored<Effort> | null;
  approvals: Stored<ApprovalPolicy> | null;
}

/** Which part of a listing: what is current, or what was archived. */
export type Shelf = 'current' | 'archived';

/** What an archive or delete applies to. */
export type HistorySubject = { kind: 'session'; id: RecordingId } | { kind: 'run'; id: RunId };

/** What a history edit does. */
export type HistoryChange = 'archive' | 'unarchive' | 'delete';

const optionalString: Decoder<string | null> = (value, path) =>
  value === undefined || value === null ? null : string(value, path);

const integer: Decoder<number> = (value, path) => {
  if (typeof value !== 'number' || !Number.isInteger(value))
    throw new ShapeError(path, 'an integer');
  return value;
};

const processExit: Decoder<ProcessExit> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  switch (value.kind) {
    case 'code':
    case 'signal':
      return { kind: value.kind, value: integer(value.value, `${path}.value`) };
    default:
      return { kind: 'unknown' };
  }
};

const approvalAsk: Decoder<ApprovalAsk> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  switch (value.kind) {
    case 'command':
      return {
        kind: 'command',
        command: optionalString(value.command, `${path}.command`),
        cwd: optionalString(value.cwd, `${path}.cwd`),
        reason: optionalString(value.reason, `${path}.reason`),
      };
    case 'file_change':
      return { kind: 'file_change', reason: optionalString(value.reason, `${path}.reason`) };
    case 'tool':
      return {
        kind: 'tool',
        tool: string(value.tool, `${path}.tool`),
        detail: optionalString(value.detail, `${path}.detail`),
      };
    default:
      return { kind: 'unrecognized', text: String(value.kind) };
  }
};

const waiting = object<Waiting>({ approval: branded<'ApprovalId'>(), ask: approvalAsk });

const runState: Decoder<RunState> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  const thread = () => branded<'NativeThreadId'>()(value.thread, `${path}.thread`);
  switch (value.phase) {
    case 'starting':
      return { phase: 'starting' };
    case 'idle':
      return { phase: 'idle', thread: thread() };
    case 'working':
      return {
        phase: 'working',
        thread: thread(),
        turn: nullable(branded<'NativeTurnId'>())(value.turn ?? null, `${path}.turn`),
        approvals: array(waiting)(value.approvals ?? [], `${path}.approvals`),
        queued: optionalString(value.queued, `${path}.queued`),
      };
    case 'stopping':
      return { phase: 'stopping', cause: label(stopCauses)(value.cause, `${path}.cause`) };
    case 'ended':
      return {
        phase: 'ended',
        end: label(runEnds)(value.end, `${path}.end`),
        exit: value.exit == null ? null : processExit(value.exit, `${path}.exit`),
      };
    default:
      return { phase: 'unrecognized', text: String(value.phase) };
  }
};

const itemRead: Decoder<ItemRead> = (value, path) => {
  if (!isRecord(value)) return { as: 'unread' };
  const at = (key: string) => `${path}.${key}`;
  const status = () => string(value.status, at('status'));
  switch (value.as) {
    case 'prompt':
    case 'reply':
    case 'plan':
      return { as: value.as, text: string(value.text, at('text')) };
    case 'reasoning':
      return { as: 'reasoning' };
    case 'command':
      return {
        as: 'command',
        command: string(value.command, at('command')),
        status: status(),
        output: optionalString(value.output, at('output')),
        exit_code: value.exit_code == null ? null : integer(value.exit_code, at('exit_code')),
      };
    case 'file_change':
      return {
        as: 'file_change',
        paths: array(string)(value.paths, at('paths')),
        status: status(),
      };
    case 'tool':
      return { as: 'tool', name: string(value.name, at('name')), status: status() };
    default:
      return { as: 'unread' };
  }
};

const runSnapshot = object<RunSnapshot>({
  run: RunId.decode,
  harness: label(harnesses),
  workdir: string,
  state: runState,
  settings: runSettings,
});

const turnEnd: Decoder<TurnEnd> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  switch (value.kind) {
    case 'completed':
    case 'interrupted':
      return { kind: value.kind };
    case 'failed':
      return { kind: 'failed', message: optionalString(value.message, `${path}.message`) };
    default:
      return { kind: 'unknown', status: String(value.status ?? value.kind) };
  }
};

const driveEvent: Decoder<DriveEvent> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  const at = (key: string) => `${path}.${key}`;
  switch (value.kind) {
    case 'ready':
      return { kind: 'ready', thread: branded<'NativeThreadId'>()(value.thread, at('thread')) };
    case 'turn_started':
      return { kind: 'turn_started', turn: branded<'NativeTurnId'>()(value.turn, at('turn')) };
    case 'delta':
      return {
        kind: 'delta',
        item: branded<'ItemId'>()(value.item, at('item')),
        text: string(value.text, at('text')),
      };
    case 'item': {
      const item = value.item;
      if (!isRecord(item)) throw new ShapeError(at('item'), 'an object');
      const phase = value.phase === 'completed' ? 'completed' : 'started';
      return {
        kind: 'item',
        phase,
        item: {
          id: branded<'ItemId'>()(item.id, at('item.id')),
          kind: string(item.kind, at('item.kind')),
          read: itemRead(item.read, at('item.read')),
        },
      };
    }
    case 'approval':
      return {
        kind: 'approval',
        approval: branded<'ApprovalId'>()(value.approval, at('approval')),
        ask: approvalAsk(value.ask, at('ask')),
      };
    case 'approval_resolved':
      return {
        kind: 'approval_resolved',
        approval: branded<'ApprovalId'>()(value.approval, at('approval')),
      };
    case 'turn_ended':
      return {
        kind: 'turn_ended',
        turn: branded<'NativeTurnId'>()(value.turn, at('turn')),
        end: turnEnd(value.end, at('end')),
      };
    case 'error':
      return {
        kind: 'error',
        message: string(value.message, at('message')),
        retry: value.retry === 'will_retry' ? 'will_retry' : 'final',
      };
    case 'refused':
      return {
        kind: 'refused',
        request: String(value.request),
        message: string(value.message, at('message')),
      };
    case 'unsupported':
      return { kind: 'unsupported', method: string(value.method, at('method')) };
    default:
      return { kind: 'other', name: String(value.kind) };
  }
};

/** One `run` event of the update feed. */
export const runUpdate: Decoder<RunUpdate> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  const run = RunId.decode(value.run, `${path}.run`);
  switch (value.kind) {
    case 'state':
      return { kind: 'state', run, state: runState(value.state, `${path}.state`) };
    case 'event':
      return { kind: 'event', run, event: driveEvent(value.event, `${path}.event`) };
    default:
      throw new ShapeError(`${path}.kind`, 'state or event');
  }
};

const recordedRun = object<RecordedRun>({
  id: RunId.decode,
  harness: label(harnesses),
  workdir: string,
  started_at: brandedCount<'UnixMillis'>(),
  thread: nullable(branded<'NativeThreadId'>()),
  session: nullable(RecordingId.decode),
  ended_at: nullable(brandedCount<'UnixMillis'>()),
  end: nullable(label(runEnds)),
  exit: nullable(processExit),
  archived_at: nullable(brandedCount<'UnixMillis'>()),
  model: nullable(branded<'ModelId'>()),
  effort: nullable(label(EFFORTS)),
  approvals: nullable(label(APPROVAL_POLICIES)),
});

const liveRuns = versioned(object<{ runs: RunSnapshot[] }>({ runs: array(runSnapshot) }));

/** Whether a prompt has gone out in a session: a draft's client can still change. */
export const SESSION_PHASES = ['draft', 'active'] as const;
export type SessionPhase = (typeof SESSION_PHASES)[number];
const shelves = ['open', 'archived'] as const;

/** Where a session stands: on the open list or the archived one, and in what phase. */
export interface SessionState {
  shelf: Stored<(typeof shelves)[number]>;
  phase: Stored<SessionPhase>;
}

/** A session as the daemon serves it now. */
export interface ServedSession {
  session: SessionId;
  harness: Stored<Harness>;
  workdir: string;
  /** What its next turn runs on. */
  settings: RunSettings;
  state: SessionState;
  /** Its newest process, whose state the run feed carries. */
  process: RunId | null;
}

/** A session as the store holds it, with its runs newest first. */
export interface StoredSession {
  id: SessionId;
  harness: Stored<Harness>;
  workdir: string;
  created_at: UnixMillis;
  model: ModelId | null;
  effort: Stored<Effort> | null;
  approvals: Stored<ApprovalPolicy> | null;
  archived_at: UnixMillis | null;
  /** When its first turn started; null while it is a draft. */
  first_turn_at: UnixMillis | null;
  /** The first line of its first prompt, cut short; null until one is recorded. */
  first_prompt: string | null;
  runs: RecordedRun[];
}

/** One session: as stored and as served; either may be missing for a moment. */
export interface SessionBody {
  session: SessionId;
  stored: StoredSession | null;
  served: ServedSession | null;
}

const servedSession = object<ServedSession>({
  session: SessionId.decode,
  harness: label(harnesses),
  workdir: string,
  settings: runSettings,
  state: object<SessionState>({ shelf: label(shelves), phase: label(SESSION_PHASES) }),
  process: nullable(RunId.decode),
});

const storedSession = object<StoredSession>({
  id: SessionId.decode,
  harness: label(harnesses),
  workdir: string,
  created_at: brandedCount<'UnixMillis'>(),
  model: nullable(branded<'ModelId'>()),
  effort: nullable(label(EFFORTS)),
  approvals: nullable(label(APPROVAL_POLICIES)),
  archived_at: nullable(brandedCount<'UnixMillis'>()),
  first_turn_at: nullable(brandedCount<'UnixMillis'>()),
  first_prompt: nullable(string),
  runs: array(recordedRun),
});

const sessionBody = object<SessionBody>({
  session: SessionId.decode,
  stored: nullable(storedSession),
  served: nullable(servedSession),
});
const sessionList = versioned(
  object<{ sessions: SessionBody[] }>({ sessions: array(sessionBody) }),
);
const oneSession = versioned(object<{ session: SessionBody }>({ session: sessionBody }));
const createdSession = versioned(object<{ session: SessionId }>({ session: SessionId.decode }));
/** One change on the session feed: the session as served. */
export const sessionUpdate: Decoder<ServedSession> = servedSession;
/** A project's repository, as its `HEAD` reads. */
export type ProjectGit =
  | { kind: 'branch'; name: string }
  | { kind: 'detached'; commit: string }
  | { kind: 'unreadable' }
  | { kind: 'none' };

const placeKinds = ['project', 'other', 'scratch'] as const;
/** What kind of place a run can start in. */
export type PlaceKind = (typeof placeKinds)[number];

/**
 * A place a run can start in: a project under a run root (a git repository,
 * or a root's own folder), a folder of another configured place, or the
 * folder of an earlier scratch run.
 */
export interface RunProject {
  path: string;
  name: string;
  /** The path as the operator reads it: `~/…` under their home. */
  shown: string;
  kind: Stored<PlaceKind>;
  git: ProjectGit;
}

/** Where runs can start, and the folder scratch runs get theirs in. */
export interface RunPlaces {
  projects: RunProject[];
  /** Null when runs cannot start in scratch on the daemon's host. */
  scratch: string | null;
}

const projectGit: Decoder<ProjectGit> = (value, path) => {
  if (!isRecord(value)) throw new ShapeError(path, 'an object');
  switch (value.kind) {
    case 'branch':
      return { kind: 'branch', name: string(value.name, `${path}.name`) };
    case 'detached':
      return { kind: 'detached', commit: string(value.commit, `${path}.commit`) };
    case 'none':
      return { kind: 'none' };
    default:
      return { kind: 'unreadable' };
  }
};
const runProjects = versioned(
  object<RunPlaces>({
    projects: array(
      object<RunProject>({
        path: string,
        name: string,
        shown: string,
        kind: label(placeKinds),
        git: projectGit,
      }),
    ),
    scratch: nullable(string),
  }),
);
const recordedRuns = versioned(object<{ runs: RecordedRun[] }>({ runs: array(recordedRun) }));

const clientStatuses = ['ready', 'not_installed', 'not_drivable'] as const;
/** Whether a run can start with a client on the daemon's host. */
export type ClientStatus = (typeof clientStatuses)[number];

/** A client, and whether a run can start with it on the daemon's host. */
export interface RunClient {
  harness: Stored<Harness>;
  status: Stored<ClientStatus>;
  /** The approval policies it can run under; none for a client that never asks. */
  approvals: Stored<ApprovalPolicy>[];
  /** What a running conversation takes mid-run. */
  midway: Stored<Midway>[];
  /** Its sign-in on the daemon's host, as its status command last said. */
  sign_in: SignInState;
  /** How Ren can sign it in, first the usual way; none where it cannot. */
  sign_in_methods: Stored<SignInMethod>[];
}

const runClients = versioned(
  object<{ clients: RunClient[] }>({
    clients: array(
      object<RunClient>({
        harness: label(harnesses),
        status: label(clientStatuses),
        approvals: array(label(APPROVAL_POLICIES)),
        midway: array(label(MIDWAY)),
        sign_in: (value, path) =>
          value === undefined ? { kind: 'not_checked' } : signInState(value, path),
        sign_in_methods: (value, path) =>
          value === undefined ? [] : array(signInMethod)(value, path),
      }),
    ),
  }),
);
/** A model, named as its client names it (`--model`, or Pi's `provider/id`). */
export type ModelId = Brand<string, 'ModelId'>;
const modelChoices = ['default', 'other'] as const;

/** A model a client offers, as the client itself lists it. */
export interface RunModel {
  id: ModelId;
  name: string;
  description: string | null;
  /** The provider serving it, where the client offers several (Pi). */
  provider: string | null;
  /** Whether a run gets it when no model is chosen. */
  choice: Stored<(typeof modelChoices)[number]>;
  /** The efforts it takes, least first; empty when it offers no choice. */
  efforts: Stored<Effort>[];
  /** The effort it runs at when none is chosen, when the client says. */
  effort: Stored<Effort> | null;
}

const runModels = versioned(
  object<{ models: RunModel[] }>({
    models: array(
      object<RunModel>({
        id: branded<'ModelId'>(),
        name: string,
        description: nullable(string),
        provider: nullable(string),
        choice: label(modelChoices),
        efforts: array(label(EFFORTS)),
        effort: nullable(label(EFFORTS)),
      }),
    ),
  }),
);

/** What a command is to its client. */
export const COMMAND_KINDS = ['command', 'skill', 'template'] as const;
export type CommandKind = (typeof COMMAND_KINDS)[number];
/** How a prompt names a command: `/name` (Claude Code, Pi) or `$name` (Codex's skills). */
export const SIGILS = ['/', '$'] as const;
export type Sigil = (typeof SIGILS)[number];

/** A command, skill, or prompt template a run's client offers, as it listed it. */
export interface ClientCommand {
  name: string;
  /** Empty when the client gives none. */
  description: string;
  /** What it takes after its name, when the client says. */
  hint: string | null;
  kind: Stored<CommandKind>;
  sigil: Stored<Sigil>;
  /** What a prompt writes to run it: its sigil, then its name. */
  invocation: string;
}

const clientCommands = versioned(
  object<{ commands: ClientCommand[] }>({
    commands: array(
      object<ClientCommand>({
        name: string,
        description: string,
        hint: nullable(string),
        kind: label(COMMAND_KINDS),
        sigil: label(SIGILS),
        invocation: string,
      }),
    ),
  }),
);

export const PATH_KINDS = ['file', 'directory'] as const;
export type PathKind = (typeof PATH_KINDS)[number];
/** A path of a run's folder that matched a search, with the characters (by index) that matched. */
export interface FileMatch {
  /** Relative to the run's folder, `/`-separated. */
  path: string;
  kind: Stored<PathKind>;
  positions: number[];
}

const fileMatches = versioned(
  object<{ files: FileMatch[] }>({
    files: array(
      object<FileMatch>({
        path: string,
        kind: label(PATH_KINDS),
        positions: array(count),
      }),
    ),
  }),
);

const versionedRecordedRun = versioned(recordedRun);
const storedConversation = versioned(
  object<StoredConversation>({
    entries: array(
      object<StoredConversation['entries'][number]>({
        run: RunId.decode,
        seq: count,
        event: (value, path) => driveEvent(value, path),
      }),
    ),
    earlier: boolean,
  }),
);

/** Why the daemon or this UI server refused a run command, by its error code. */
export class RunCommandError extends Error {
  readonly status: number;
  readonly code: string | null;
  constructor(status: number, code: string | null) {
    super(RunCommandError.describe(status, code));
    this.status = status;
    this.code = code;
  }

  static describe(status: number, code: string | null): string {
    switch (code) {
      case 'runs_off':
        return 'This UI server does not drive runs: set REN_CONTROL_TOKEN_FILE.';
      case 'runs_disabled':
        return 'Runs are off on the daemon: add a folder to [runs] roots.';
      case 'invalid_directory':
        return 'That folder is not under one of the daemon’s run roots.';
      case 'directory_not_found':
        return 'That folder does not exist on the daemon’s host.';
      case 'not_drivable':
        return 'Ren cannot drive that client yet.';
      case 'spawn_failed':
        return 'The client could not be started; is it installed on the daemon’s host?';
      case 'nothing_queued':
        return 'Nothing is queued.';
      case 'not_offered':
        return 'That client cannot do this in a running conversation.';
      case 'no_approvals':
        return 'That client asks for nothing, so it takes no approval policy.';
      case 'models_unavailable':
        return 'The client could not list its models.';
      case 'unrecorded':
        return 'The recorder has no room to record a new run right now.';
      case 'busy':
        return 'A turn is already running.';
      case 'starting':
        return 'The run is still opening its conversation.';
      case 'not_working':
      case 'turn_not_started':
        return 'No turn is running.';
      case 'no_such_approval':
        return 'That approval no longer waits.';
      case 'stopping':
      case 'ended':
        return 'The run is stopping or has ended.';
      case 'not_found':
        return 'The daemon does not know that run.';
      case 'call_open':
        return 'A call of it is still open; delete it once the call ends.';
      case 'run_running':
        return 'A run of it is still running; stop it before deleting.';
      case 'cross_origin':
      case 'login_not_allowed':
        return 'This UI does not let you drive runs.';
      case 'no_control_token':
        return 'The UI server cannot read the daemon’s control token. Is the daemon running?';
      case 'daemon_unreachable':
        return 'Cannot reach the Ren daemon.';
      case 'unauthorized':
      case 'forbidden':
      case 'cross_site':
        return 'The daemon refused the UI server’s control token.';
      default:
        return `The command was refused (${status}${code ? ` ${code}` : ''}).`;
    }
  }
}

/** The clients the daemon can drive, in the order a draft offers them. */
export const DRIVABLE = [
  'codex',
  'claude_code',
  'pi',
  'opencode',
] as const satisfies readonly Harness[];
export type Drivable = (typeof DRIVABLE)[number];

/** One read the page starts with: what was read, or why it was not. */
export type Read<T> = { state: 'read'; value: T } | { state: 'failed'; reason: string };

/** Everything the Sessions page shows before it starts following the feed. */
export interface RunsStart {
  live: Read<RunSnapshot[]>;
  places: Read<RunPlaces>;
  clients: Read<RunClient[]>;
  recorded: Read<RecordedRun[]>;
  archived: Read<RecordedRun[]>;
  sessions: Read<SessionBody[]>;
  archivedSessions: Read<SessionBody[]>;
}

/** Run commands and reads through a capability-supplied `fetch`. */
export class RunsClient {
  readonly #fetch: typeof fetch;
  constructor(fetcher: typeof fetch = fetch) {
    this.#fetch = fetcher;
  }

  async live(signal?: AbortSignal): Promise<RunSnapshot[]> {
    return (await this.#send('GET', '/control/runs', undefined, liveRuns, signal)).runs;
  }
  /**
   * Everything the Sessions page starts with, read at once; each read that
   * fails says why, and the others stand.
   */
  async opening(): Promise<RunsStart> {
    const read = async <T>(reading: Promise<T>): Promise<Read<T>> => {
      try {
        return { state: 'read', value: await reading };
      } catch (error) {
        return {
          state: 'failed',
          reason: error instanceof Error ? error.message : 'The read failed.',
        };
      }
    };
    const [live, places, clients, recorded, archived, sessions, archivedSessions] =
      await Promise.all([
        read(this.live()),
        read(this.projects()),
        read(this.clients()),
        read(this.recorded(100)),
        read(this.recorded(100, 'archived')),
        read(this.sessions()),
        read(this.sessions('archived')),
      ]);
    return {
      live,
      places,
      clients,
      recorded,
      archived,
      sessions,
      archivedSessions,
    };
  }
  /** Where runs can start, and the scratch folder. */
  async projects(signal?: AbortSignal): Promise<RunPlaces> {
    return this.#send('GET', '/control/runs/projects', undefined, runProjects, signal);
  }
  /** Every client, and whether a run can start with it on the daemon's host. */
  async clients(signal?: AbortSignal): Promise<RunClient[]> {
    return (await this.#send('GET', '/control/runs/clients', undefined, runClients, signal))
      .clients;
  }
  /** Check `harness`'s sign-in now, with its own status command. */
  async checkSignIn(harness: Drivable): Promise<SignInState> {
    const path = `/control/runs/clients/${harness}/sign-in/check`;
    return (await this.#send('POST', path, {}, checkedSignIn, undefined, 25_000)).sign_in;
  }
  /** Start `harness`'s own login; answers once it shows its link. */
  async startSignIn(harness: Drivable, method: SignInMethod): Promise<Login> {
    const path = `/control/runs/clients/${harness}/sign-in`;
    return (await this.#send('POST', path, { method }, startedSignIn, undefined, 28_000)).login;
  }
  /** Hand `harness`'s login what it waits for (a code, a key, or an address); answers once it ended and was checked. */
  async signInAnswer(harness: Drivable, answer: string): Promise<LoginOutcome> {
    const path = `/control/runs/clients/${harness}/sign-in/answer`;
    return (await this.#send('POST', path, { answer }, finishedSignIn, undefined, 28_000)).outcome;
  }
  /** Wait up to about 20 seconds for `harness`'s login to end and be checked. */
  async waitSignIn(harness: Drivable): Promise<Waited> {
    const path = `/control/runs/clients/${harness}/sign-in/wait`;
    return (await this.#send('POST', path, {}, waitedSignIn, undefined, 28_000)).waited;
  }
  /** Stop `harness`'s running login. */
  async cancelSignIn(harness: Drivable): Promise<void> {
    const path = `/control/runs/clients/${harness}/sign-in/cancel`;
    await this.#send('POST', path, {}, null);
  }
  /** The models `harness` offers, as it lists them on the daemon's host. */
  async models(harness: Drivable, signal?: AbortSignal): Promise<RunModel[]> {
    const path = `/control/runs/clients/${harness}/models`;
    return (await this.#send('GET', path, undefined, runModels, signal, 30_000)).models;
  }
  /** Sessions on `shelf`, most recently active first. */
  async sessions(shelf: Shelf = 'current', signal?: AbortSignal): Promise<SessionBody[]> {
    const path = `/control/sessions?shelf=${shelf}`;
    return (await this.#send('GET', path, undefined, sessionList, signal)).sessions;
  }
  async session(session: SessionId, signal?: AbortSignal): Promise<SessionBody> {
    const path = `/control/sessions/${session}`;
    return (await this.#send('GET', path, undefined, oneSession, signal)).session;
  }
  /** A new session in `dir` (null: a scratch folder of its own); its client starts at once. */
  async createSession(
    harness: Drivable,
    dir: string | null,
    settings: {
      model?: ModelId | null;
      effort?: Effort | null;
      approvals?: ApprovalPolicy | null;
    } = {},
  ): Promise<SessionId> {
    const body = {
      harness,
      ...(dir ? { dir } : {}),
      ...(settings.model ? { model: settings.model } : {}),
      ...(settings.effort ? { effort: settings.effort } : {}),
      ...(settings.approvals ? { approvals: settings.approvals } : {}),
    };
    return (await this.#send('POST', '/control/sessions', body, createdSession)).session;
  }
  /** One command to a session: through its process, or a new one for a prompt. */
  async toSession(
    session: SessionId,
    command:
      | { kind: 'prompt'; text: string; delivery: Delivery }
      | {
          kind: 'settings';
          model: ModelId | null;
          effort: Effort | null;
          approvals: ApprovalPolicy | null;
        }
      | { kind: 'client'; harness: Drivable }
      | { kind: 'answer'; approval: ApprovalId; decision: Decision }
      | { kind: 'unqueue' | 'interrupt' | 'stop' | 'archive' | 'unarchive' | 'delete' },
  ): Promise<void> {
    const base = `/control/sessions/${session}`;
    switch (command.kind) {
      case 'prompt':
        return this.#send(
          'POST',
          `${base}/prompt`,
          { text: command.text, delivery: command.delivery },
          null,
        );
      case 'settings': {
        const { model, effort, approvals } = command;
        return this.#send('POST', `${base}/settings`, { model, effort, approvals }, null);
      }
      case 'client':
        return this.#send('POST', `${base}/client`, { harness: command.harness }, null);
      case 'answer':
        return this.#send(
          'POST',
          `${base}/approvals/${encodeURIComponent(command.approval)}`,
          { decision: command.decision },
          null,
        );
      case 'unqueue':
      case 'interrupt':
      case 'stop':
      case 'archive':
      case 'unarchive':
      case 'delete':
        return this.#send('POST', `${base}/${command.kind}`, {}, null);
      default:
        return assertNever(command);
    }
  }
  /** What the session's client offers a prompt. */
  async sessionCommands(session: SessionId, signal?: AbortSignal): Promise<ClientCommand[]> {
    const path = `/control/sessions/${session}/commands`;
    return (await this.#send('GET', path, undefined, clientCommands, signal)).commands;
  }
  /** The paths of the session's folder that best match `query`. */
  async sessionFiles(
    session: SessionId,
    query: string,
    limit = 20,
    signal?: AbortSignal,
  ): Promise<FileMatch[]> {
    const search = new URLSearchParams({ q: query, limit: String(limit) });
    const path = `/control/sessions/${session}/files?${search}`;
    return (await this.#send('GET', path, undefined, fileMatches, signal)).files;
  }
  /**
   * Archive, unarchive, or delete what was recorded of a session or a run.
   * The daemon applies it after what it is recording; the live feed says
   * when.
   */
  async edit(subject: HistorySubject, change: HistoryChange): Promise<void> {
    const kind = subject.kind === 'session' ? 'sessions' : 'runs';
    await this.#send('POST', `/control/history/${kind}/${subject.id}/${change}`, {}, null);
  }
  /** Recorded runs on `shelf`, newest first, from the read-only management API. */
  async recorded(
    limit = 100,
    shelf: Shelf = 'current',
    signal?: AbortSignal,
  ): Promise<RecordedRun[]> {
    const query = shelf === 'archived' ? `&shelf=archived` : '';
    return (
      await this.#send('GET', `/api/runs?limit=${limit}${query}`, undefined, recordedRuns, signal)
    ).runs;
  }
  /** A run's conversation, whole, as the daemon stored it. */
  async conversation(run: RunId, signal?: AbortSignal): Promise<StoredConversation> {
    return this.#send(
      'GET',
      `/api/runs/${run}/conversation`,
      undefined,
      storedConversation,
      signal,
    );
  }
  async recordedRun(run: RunId, signal?: AbortSignal): Promise<RecordedRun> {
    return this.#send('GET', `/api/runs/${run}`, undefined, versionedRecordedRun, signal);
  }

  async #send<T>(
    method: 'GET' | 'POST',
    path: string,
    body: unknown,
    decode: Decoder<T> | null,
    signal?: AbortSignal,
    wait = 15_000,
  ): Promise<T> {
    const timeout = AbortSignal.timeout(wait);
    const response = await this.#fetch(path, {
      method,
      cache: 'no-store',
      headers:
        body === undefined
          ? { accept: 'application/json' }
          : { accept: 'application/json', 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
    let parsed: unknown = null;
    try {
      parsed = await response.json();
    } catch {
      parsed = null;
    }
    if (!response.ok) {
      const code = isRecord(parsed) && typeof parsed.error === 'string' ? parsed.error : null;
      throw new RunCommandError(response.status, code);
    }
    if (decode === null) {
      if (!isRecord(parsed) || parsed.api_version !== API_VERSION) {
        throw new ShapeError('api_version', `${API_VERSION}`);
      }
      return undefined as T;
    }
    return decode(parsed, '');
  }
}

/** One entry of a run's conversation, built from its events. */
export type Entry =
  | { kind: 'prompt'; item: ItemId | null; text: string }
  | { kind: 'message'; item: ItemId; text: string; done: boolean }
  | { kind: 'reasoning'; item: ItemId; done: boolean }
  | {
      kind: 'command';
      item: ItemId;
      command: string;
      status: string;
      output: string | null;
      exit: number | null;
    }
  | { kind: 'file_change'; item: ItemId; paths: string[]; status: string }
  | { kind: 'tool'; item: ItemId; name: string; done: boolean }
  | { kind: 'turn_end'; turn: NativeTurnId; end: TurnEnd }
  | { kind: 'notice'; tone: 'warn' | 'danger'; text: string };

/** A run's conversation as the daemon stored it (`/api/runs/{id}/conversation`). */
export interface StoredConversation {
  entries: { run: RunId; seq: number; event: DriveEvent }[];
  /** Older entries were left out of the read. */
  earlier: boolean;
}

/**
 * A run's conversation, whole: read once from what the daemon stored of it
 * (every turn of the run, and of the runs before it on its thread), then kept
 * current by the run feed. Both say the same events, so they merge by what
 * each entry is: an item by its id, a turn's end by its turn, anything else by
 * what it says. Nothing is cut, and nothing gives way to a shorter copy.
 */
export class Transcript {
  readonly entries: Entry[];
  /** The turn the newest entries belong to, once the client named it. */
  readonly turn: NativeTurnId | null;
  /** Whether what the daemon stored has been read. */
  readonly read: boolean;
  /** Older entries were left out of the read. */
  readonly earlier: boolean;

  constructor(
    entries: Entry[] = [],
    turn: NativeTurnId | null = null,
    read = false,
    earlier = false,
  ) {
    this.entries = entries;
    this.turn = turn;
    this.read = read;
    this.earlier = earlier;
  }

  /** What the daemon stored, with what streamed in meanwhile kept. */
  load(stored: StoredConversation): Transcript {
    let loaded = new Transcript();
    for (const entry of stored.entries) loaded = loaded.apply(entry.event);
    const entries = loaded.entries.slice();
    for (const live of this.entries) {
      const at = entries.findIndex((entry) => Transcript.same(entry, live));
      if (at === -1) entries.push(live);
      else if (Transcript.done(live) && !Transcript.done(entries[at]!)) entries[at] = live;
    }
    return new Transcript(entries, this.turn ?? loaded.turn, true, stored.earlier);
  }

  /** The prompt the operator just sent; the client's echo of it is the same prompt. */
  prompted(text: string): Transcript {
    return this.#push({ kind: 'prompt', item: null, text });
  }

  /** Every prompt sent, oldest first, to recall. */
  prompts(): string[] {
    return this.entries.flatMap((entry) => (entry.kind === 'prompt' ? [entry.text] : []));
  }

  /** The first prompt shown, to name the run by. */
  firstPrompt(): string | null {
    const prompt = this.entries.find((entry) => entry.kind === 'prompt');
    return prompt?.kind === 'prompt' ? prompt.text : null;
  }

  static #itemOf(entry: Entry): ItemId | null {
    return 'item' in entry ? entry.item : null;
  }

  static done(entry: Entry): boolean {
    switch (entry.kind) {
      case 'message':
      case 'reasoning':
      case 'tool':
        return entry.done;
      case 'command':
      case 'file_change':
        return entry.status !== 'in_progress';
      case 'prompt':
      case 'turn_end':
      case 'notice':
        return true;
      default:
        return assertNever(entry);
    }
  }

  /** Whether `one` and `two` are the same entry, said twice. */
  static same(one: Entry, two: Entry): boolean {
    const [first, second] = [Transcript.#itemOf(one), Transcript.#itemOf(two)];
    if (first !== null && second !== null) return first === second;
    if (one.kind === 'turn_end' && two.kind === 'turn_end') return one.turn === two.turn;
    if (one.kind === 'prompt' && two.kind === 'prompt') return one.text === two.text;
    return JSON.stringify(one) === JSON.stringify(two);
  }

  apply(event: DriveEvent): Transcript {
    switch (event.kind) {
      case 'turn_started':
        return new Transcript(this.entries, event.turn, this.read, this.earlier);
      case 'delta': {
        const index = this.#find(event.item);
        const entry = index === -1 ? undefined : this.entries[index];
        if (entry?.kind === 'message') {
          return this.#replace(index, { ...entry, text: entry.text + event.text });
        }
        return this.#push({ kind: 'message', item: event.item, text: event.text, done: false });
      }
      case 'item':
        return this.#item(event.item, event.phase === 'completed');
      case 'turn_ended':
        return new Transcript(this.entries, event.turn, this.read, this.earlier).#push({
          kind: 'turn_end',
          turn: event.turn,
          end: event.end,
        });
      case 'error':
        return this.#append({
          kind: 'notice',
          tone: event.retry === 'will_retry' ? 'warn' : 'danger',
          text: event.retry === 'will_retry' ? `${event.message} (retrying)` : event.message,
        });
      case 'refused':
        return this.#append({ kind: 'notice', tone: 'danger', text: event.message });
      case 'ready':
      case 'approval':
      case 'approval_resolved':
      case 'unsupported':
      case 'other':
        // Approvals live in the run's state; the rest says nothing to show.
        return this;
      default:
        return assertNever(event);
    }
  }

  #item(item: DriveItem, done: boolean): Transcript {
    const read = item.read;
    let entry: Entry | null;
    switch (read.as) {
      case 'prompt':
        entry = read.text === '' ? null : { kind: 'prompt', item: item.id, text: read.text };
        break;
      case 'reply': {
        const index = this.#find(item.id);
        const before = index === -1 ? undefined : this.entries[index];
        const streamed = before?.kind === 'message' ? before.text : '';
        entry = { kind: 'message', item: item.id, text: read.text || streamed, done };
        break;
      }
      case 'reasoning':
        entry = { kind: 'reasoning', item: item.id, done };
        break;
      case 'command':
        entry = {
          kind: 'command',
          item: item.id,
          command: read.command,
          status: read.status,
          output: read.output,
          exit: read.exit_code,
        };
        break;
      case 'file_change':
        entry = { kind: 'file_change', item: item.id, paths: read.paths, status: read.status };
        break;
      case 'plan':
        entry = read.text ? { kind: 'message', item: item.id, text: read.text, done } : null;
        break;
      case 'tool':
        entry = { kind: 'tool', item: item.id, name: read.name, done };
        break;
      case 'unread':
        entry = { kind: 'tool', item: item.id, name: item.kind, done };
        break;
      default:
        return assertNever(read);
    }
    return entry === null ? this : this.#push(entry);
  }

  #find(item: ItemId): number {
    return this.entries.findLastIndex((entry) => Transcript.#itemOf(entry) === item);
  }

  #replace(index: number, entry: Entry): Transcript {
    const entries = this.entries.slice();
    entries[index] = entry;
    return new Transcript(entries, this.turn, this.read, this.earlier);
  }

  #append(entry: Entry): Transcript {
    return new Transcript([...this.entries, entry], this.turn, this.read, this.earlier);
  }

  /**
   * Add `entry`, or put it in the place of the same entry said before. A
   * prompt sent from here and echoed by the client is one prompt, in the
   * current turn.
   */
  #push(entry: Entry): Transcript {
    const start =
      entry.kind === 'prompt'
        ? this.entries.findLastIndex((shown) => shown.kind === 'turn_end') + 1
        : 0;
    const at = this.entries.findLastIndex(
      (shown, index) => index >= start && Transcript.same(shown, entry),
    );
    if (at === -1) return this.#append(entry);
    const shown = this.entries[at]!;
    // A prompt sent from here is named by the client's echo, not replaced by a resend.
    if (entry.kind === 'prompt' && entry.item === null) return this;
    if (shown.kind === 'prompt' && entry.kind === 'prompt' && entry.item === null) return this;
    return this.#replace(at, entry);
  }
}

/** What the operator can ask of a run in its current state. */
export interface RunControls {
  prompt: boolean;
  interrupt: boolean;
  stop: boolean;
  /** The approvals waiting for an answer, oldest first. */
  approvals: Waiting[];
}

/** How a run's state reads in the sidebar and header. */
export interface PhaseMark {
  label: string;
  tone: 'live' | 'warn' | 'neutral' | 'danger' | 'ok';
}

export class Runs {
  /** A conversation's turns: each starts at its prompt. */
  static turns(entries: readonly Entry[]): Entry[][] {
    const turns: Entry[][] = [];
    for (const entry of entries) {
      if (entry.kind === 'prompt' || turns.length === 0) turns.push([]);
      turns.at(-1)!.push(entry);
    }
    return turns;
  }

  static controls(state: RunState): RunControls {
    switch (state.phase) {
      case 'idle':
        return { prompt: true, interrupt: false, stop: true, approvals: [] };
      case 'working':
        return {
          prompt: false,
          interrupt: state.turn !== null,
          stop: true,
          approvals: state.approvals,
        };
      case 'starting':
        return { prompt: false, interrupt: false, stop: true, approvals: [] };
      case 'stopping':
      case 'ended':
      case 'unrecognized':
        return { prompt: false, interrupt: false, stop: false, approvals: [] };
      default:
        return assertNever(state);
    }
  }

  /** A run's state in a few words, with the tone of its dot. */
  static mark(state: RunState): PhaseMark {
    switch (state.phase) {
      case 'starting':
        return { label: 'Starting', tone: 'neutral' };
      case 'idle':
        return { label: 'Your turn', tone: 'ok' };
      case 'working':
        return state.approvals.length
          ? { label: 'Needs you', tone: 'warn' }
          : { label: 'Working', tone: 'live' };
      case 'stopping':
        return { label: 'Stopping', tone: 'neutral' };
      case 'ended':
        return {
          label: Runs.endLabel(state.end, state.exit),
          tone: Runs.endTone(state.end, state.exit),
        };
      case 'unrecognized':
        return { label: state.text, tone: 'neutral' };
      default:
        return assertNever(state);
    }
  }

  /** How a run ended, as the operator would say it; the process's exit is a
   * detail ({@link Runs.exitDetail}), not the headline. */
  static endLabel(end: Stored<RunEnd>, exit: ProcessExit | null = null): string {
    switch (end) {
      case 'spawn_failed':
        return 'Couldn’t start';
      case 'open_refused':
        return 'Couldn’t open its conversation';
      case 'exited':
        switch (exit?.kind) {
          case 'code':
            return exit.value === 0 ? 'Ended' : 'Ended with an error';
          case 'signal':
            return 'Killed';
          case 'unknown':
          case undefined:
            return 'Ended';
          default:
            return assertNever(exit);
        }
      case 'stopped':
        return 'Stopped';
      case 'drained':
        return 'Stopped when Ren shut down';
      case 'recovered':
        return 'Lost when Ren restarted';
      case 'idled':
        return 'Resting';
      default:
        return unrecognised(end);
    }
  }

  static endTone(end: Stored<RunEnd>, exit: ProcessExit | null = null): PhaseMark['tone'] {
    switch (end) {
      case 'stopped':
      case 'idled':
        return 'neutral';
      case 'exited':
        return exit?.kind === 'code' && exit.value === 0 ? 'neutral' : 'warn';
      case 'drained':
      case 'recovered':
        return 'warn';
      case 'spawn_failed':
      case 'open_refused':
        return 'danger';
      default:
        unrecognised(end);
        return 'neutral';
    }
  }

  /** How the process exited, for the run's details: `exit code 1`, or the
   * signal by name, `killed by SIGKILL (signal 9)`. */
  static exitDetail(exit: ProcessExit | null): string | null {
    const signals: Record<number, string> = {
      1: 'SIGHUP',
      2: 'SIGINT',
      6: 'SIGABRT',
      9: 'SIGKILL',
      11: 'SIGSEGV',
      15: 'SIGTERM',
    };
    switch (exit?.kind) {
      case 'code':
        return `exit code ${exit.value}`;
      case 'signal': {
        const name = signals[exit.value];
        return name ? `killed by ${name} (signal ${exit.value})` : `killed by signal ${exit.value}`;
      }
      case 'unknown':
      case undefined:
        return null;
      default:
        return assertNever(exit);
    }
  }

  /**
   * Where a run works, as the operator would say it: Scratch for a scratch
   * run, else its folder as `~/…` where a place offered shows the home.
   */
  static place(workdir: string, scratch: string | null, offered: readonly RunProject[]): string {
    if (scratch && (workdir === scratch || workdir.startsWith(`${scratch}/`))) return 'Scratch';
    const known = offered.find((place) => place.path === workdir);
    if (known) return known.shown;
    // The home, as a place offered under it shows it.
    const homed = offered.find((place) => place.shown.startsWith('~/'));
    const home = homed ? homed.path.slice(0, homed.path.length - homed.shown.length + 1) : null;
    return home && workdir.startsWith(`${home}/`) ? `~${workdir.slice(home.length)}` : workdir;
  }

  /** One line saying what an approval asks. */
  static ask(ask: ApprovalAsk): { action: string; detail: string | null; reason: string | null } {
    switch (ask.kind) {
      case 'command':
        return {
          action: 'Run a command',
          detail: ask.command,
          reason: ask.reason,
        };
      case 'file_change':
        return { action: 'Change files', detail: null, reason: ask.reason };
      case 'tool':
        return { action: `Use ${ask.tool}`, detail: ask.detail, reason: null };
      case 'unrecognized':
        return { action: `Allow ${ask.text}`, detail: null, reason: null };
      default:
        return assertNever(ask);
    }
  }

  /** Why a run cannot start with a client, or null when it can. */
  static unavailable(status: Stored<ClientStatus>): string | null {
    switch (status) {
      case 'ready':
        return null;
      case 'not_installed':
        return 'not installed on the daemon’s host';
      case 'not_drivable':
        return 'Ren cannot drive it yet';
      default:
        return unrecognised(status);
    }
  }

  /**
   * Projects grouped by the folder they are in, groups in path order, and
   * each group's projects by name.
   */
  /** Places by the folder they are in, as the operator reads it (`~/dev`). */
  static places(projects: readonly RunProject[]): { parent: string; projects: RunProject[] }[] {
    const groups = new Map<string, RunProject[]>();
    for (const project of projects) {
      const cut = project.shown.lastIndexOf('/');
      const parent = cut > 0 ? project.shown.slice(0, cut) : project.shown;
      groups.set(parent, [...(groups.get(parent) ?? []), project]);
    }
    return [...groups]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([parent, projects]) => ({
        parent,
        projects: projects.toSorted((a, b) => a.name.localeCompare(b.name)),
      }));
  }

  /** The projects matching what is typed, by name or path, ignoring case. */
  static choose(projects: readonly RunProject[], typed: string): RunProject[] {
    const text = typed.trim().toLowerCase();
    return projects.filter(
      (project) =>
        project.name.toLowerCase().includes(text) || project.shown.toLowerCase().includes(text),
    );
  }

  /** What a project's repository is on, in a few words. */
  /** Whether a folder is a git repository whose `HEAD` was read. */
  static versioned(git: ProjectGit): boolean {
    return git.kind === 'branch' || git.kind === 'detached';
  }

  static branch(git: ProjectGit): string {
    switch (git.kind) {
      case 'branch':
        return git.name;
      case 'detached':
        return `at ${git.commit}`;
      case 'unreadable':
        return 'git';
      case 'none':
        return 'no git';
      default:
        return assertNever(git);
    }
  }

  /** A run's client, when the daemon can drive it to resume the conversation. */
  static drivable(harness: Stored<Harness>): Drivable | null {
    return DRIVABLE.find((drivable) => drivable === harness) ?? null;
  }

  /** The folder's last component, as the sidebar names a project. */
  /** `path` as read from inside `workdir`: relative there, else as it is. */
  static within(path: string, workdir: string | null): string {
    if (!workdir) return path;
    const root = workdir.endsWith('/') ? workdir : `${workdir}/`;
    return path.startsWith(root) && path.length > root.length ? path.slice(root.length) : path;
  }

  static folder(workdir: string): string {
    const parts = workdir.split('/').filter((part) => part !== '');
    return parts.at(-1) ?? workdir;
  }

  /** The thread a run's conversation has, while the run has one open. */
  static thread(state: RunState): NativeThreadId | null {
    switch (state.phase) {
      case 'idle':
      case 'working':
        return state.thread;
      case 'starting':
      case 'stopping':
      case 'ended':
      case 'unrecognized':
        return null;
      default:
        return assertNever(state);
    }
  }

  /** Apply an update to the live list: a new run appears, a known one changes. */
  static update(runs: RunSnapshot[], update: RunUpdate): RunSnapshot[] {
    if (update.kind !== 'state') return runs;
    const index = runs.findIndex((run) => run.run === update.run);
    if (index === -1) {
      // A run started elsewhere; its folder arrives with the next listing.
      return runs;
    }
    const next = runs.slice();
    next[index] = { ...runs[index]!, state: update.state };
    return next;
  }
}

/** A run in the sidebar: live, or recorded and ended. */
export interface DeskRun {
  run: RunId;
  harness: Stored<Harness>;
  workdir: string;
  started_at: UnixMillis | null;
  /** The live state, while the daemon holds the run. */
  state: RunState | null;
  recorded: RecordedRun | null;
  /** What it was started with: as the daemon holds it, or as recorded. */
  settings: RunSettings;
}

/** Runs grouped by folder, the folder with the newest run first. */
export interface Project {
  workdir: string;
  name: string;
  runs: DeskRun[];
  /** Approvals waiting across the folder's runs. */
  waiting: number;
}

export class Desk {
  /**
   * Join live and recorded runs and group them by folder. Ended runs in
   * `hidden` (archived) are left out, though the daemon still lists them.
   */
  static projects(
    live: RunSnapshot[],
    recorded: RecordedRun[],
    hidden: ReadonlySet<RunId> = new Set(),
    scratch: string | null = null,
  ): Project[] {
    const byRun = new Map<RunId, DeskRun>();
    for (const run of recorded) {
      byRun.set(run.id, {
        run: run.id,
        harness: run.harness,
        workdir: run.workdir,
        started_at: run.started_at,
        state: null,
        recorded: run,
        settings: { model: run.model, effort: run.effort, approvals: run.approvals },
      });
    }
    for (const run of live) {
      if (run.state.phase === 'ended' && hidden.has(run.run)) continue;
      const known = byRun.get(run.run);
      byRun.set(run.run, {
        run: run.run,
        harness: run.harness,
        workdir: run.workdir,
        started_at: known?.started_at ?? null,
        state: run.state,
        recorded: known?.recorded ?? null,
        settings: run.settings,
      });
    }
    // Scratch runs each have a folder of their own: they share one group.
    const group = (workdir: string) =>
      scratch && workdir.startsWith(`${scratch}/`) ? scratch : workdir;
    const groups = new Map<string, DeskRun[]>();
    for (const run of byRun.values()) {
      const runs = groups.get(group(run.workdir)) ?? [];
      runs.push(run);
      groups.set(group(run.workdir), runs);
    }
    // Live runs first, then newest first; a run not yet recorded is newest.
    const newest = (run: DeskRun) => run.started_at ?? Number.MAX_SAFE_INTEGER;
    const live_ = (run: DeskRun) => (run.state !== null && run.state.phase !== 'ended' ? 1 : 0);
    const projects = [...groups].map(([workdir, runs]) => {
      runs.sort((a, b) => live_(b) - live_(a) || newest(b) - newest(a));
      return {
        workdir,
        name: workdir === scratch ? 'Scratch' : Runs.folder(workdir),
        runs,
        waiting: runs.reduce(
          (sum, run) => sum + (run.state ? Runs.controls(run.state).approvals.length : 0),
          0,
        ),
      };
    });
    projects.sort(
      (a, b) =>
        b.waiting - a.waiting ||
        Math.max(...b.runs.map(live_)) - Math.max(...a.runs.map(live_)) ||
        newest(b.runs[0]!) - newest(a.runs[0]!),
    );
    return projects;
  }

  /** The projects runs worked in, most recently started first, at most `count`. */
  static recent(
    runs: readonly { workdir: string; started_at: UnixMillis | null }[],
    offered: readonly RunProject[],
    count = 3,
  ): RunProject[] {
    const newest = runs
      .filter((run) => run.started_at !== null)
      .toSorted((a, b) => (b.started_at ?? 0) - (a.started_at ?? 0));
    const recent: RunProject[] = [];
    for (const run of newest) {
      const project = offered.find((project) => project.path === run.workdir);
      if (project && !recent.includes(project)) recent.push(project);
      if (recent.length === count) break;
    }
    return recent;
  }

  /** A folder's runs that have not ended, then those that have, newest first. */
  static split(project: Project): { open: DeskRun[]; ended: DeskRun[] } {
    const ended = (run: DeskRun) => Desk.state(run).phase === 'ended';
    return {
      open: project.runs.filter((run) => !ended(run)),
      ended: project.runs.filter(ended),
    };
  }

  /** The state a sidebar run shows: live if held, else its recorded end. */
  static state(run: DeskRun): RunState {
    if (run.state) return run.state;
    const recorded = run.recorded;
    if (recorded?.end) return { phase: 'ended', end: recorded.end, exit: recorded.exit };
    // Recorded as started but not held by the daemon: it ended without a record yet.
    return { phase: 'unrecognized', text: 'not running' };
  }
}

/** A session in the sidebar, with the state of its newest process. */
export interface DeskSession {
  session: SessionId;
  harness: Stored<Harness>;
  workdir: string;
  /** When it was last active: its newest run's start, else its creation; null until stored. */
  activeAt: number | null;
  phase: Stored<SessionPhase>;
  shelf: Stored<'open' | 'archived'>;
  /** What it was first asked, as stored; null until a prompt is recorded. */
  asked: string | null;
  /** What its next turn runs on. */
  settings: RunSettings;
  /** Its newest process, once one started. */
  run: RunId | null;
  /** That process's state: live while the daemon holds it, else its recorded end. */
  state: RunState | null;
  /** Its runs as recorded, newest first. */
  runs: RecordedRun[];
}

/** Sessions grouped by folder, folders that need you first. */
export interface SessionFolder {
  workdir: string;
  name: string;
  sessions: DeskSession[];
  /** Approvals waiting across the folder's sessions. */
  waiting: number;
}

export class Sessions {
  /** One session from what the store holds and what the daemon serves. */
  static desk(body: SessionBody, live: readonly RunSnapshot[]): DeskSession | null {
    const { stored, served } = body;
    const first = served ?? stored;
    if (!first) return null;
    const run = served?.process ?? stored?.runs[0]?.id ?? null;
    const held = run ? live.find((entry) => entry.run === run) : undefined;
    const recordedRun = run ? stored?.runs.find((entry) => entry.id === run) : undefined;
    const state: RunState | null = held
      ? held.state
      : recordedRun?.end
        ? { phase: 'ended', end: recordedRun.end, exit: recordedRun.exit }
        : run
          ? { phase: 'unrecognized', text: 'not running' }
          : null;
    return {
      session: body.session,
      harness: first.harness,
      workdir: first.workdir,
      activeAt: stored ? (stored.runs[0]?.started_at ?? stored.created_at) : null,
      phase: served?.state.phase ?? (stored?.first_turn_at ? 'active' : 'draft'),
      shelf: served?.state.shelf ?? (stored?.archived_at ? 'archived' : 'open'),
      asked: stored?.first_prompt ?? null,
      settings: served?.settings ?? {
        model: stored?.model ?? null,
        effort: stored?.effort ?? null,
        approvals: stored?.approvals ?? null,
      },
      run,
      state,
      runs: stored?.runs ?? [],
    };
  }

  /** Whether the session's process is running now. */
  static running(session: DeskSession): boolean {
    return session.state !== null && !['ended', 'unrecognized'].includes(session.state.phase);
  }

  /**
   * The sessions grouped by folder: scratch sessions share one group.
   * Folders with an approval waiting come first, then those with a process
   * running, then the most recently active; the same within a folder.
   */
  static folders(
    bodies: readonly SessionBody[],
    live: readonly RunSnapshot[],
    scratch: string | null = null,
  ): SessionFolder[] {
    const group = (workdir: string) =>
      scratch && workdir.startsWith(`${scratch}/`) ? scratch : workdir;
    const groups = new Map<string, DeskSession[]>();
    for (const body of bodies) {
      const session = Sessions.desk(body, live);
      if (!session) continue;
      const sessions = groups.get(group(session.workdir)) ?? [];
      sessions.push(session);
      groups.set(group(session.workdir), sessions);
    }
    const waiting = (session: DeskSession) =>
      session.state ? Runs.controls(session.state).approvals.length : 0;
    // A session not stored yet was just made: it is the newest.
    const newest = (session: DeskSession) => session.activeAt ?? Number.MAX_SAFE_INTEGER;
    const running = (session: DeskSession) => (Sessions.running(session) ? 1 : 0);
    const order = (a: DeskSession, b: DeskSession) =>
      waiting(b) - waiting(a) || running(b) - running(a) || newest(b) - newest(a);
    const folders = [...groups].map(([workdir, sessions]) => ({
      workdir,
      name: workdir === scratch ? 'Scratch' : Runs.folder(workdir),
      sessions: sessions.toSorted(order),
      waiting: sessions.reduce((sum, session) => sum + waiting(session), 0),
    }));
    return folders.toSorted((a, b) => order(a.sessions[0]!, b.sessions[0]!));
  }

  /**
   * How a session stands in a word: its process's state while one runs (a
   * draft's idle one says Draft); with none running, a client that could
   * not start says so, a draft says Draft, and any other names its client
   * quietly, as writing to it starts one again.
   */
  static mark(session: DeskSession, client: string): PhaseMark {
    const state = session.state;
    if (state && Sessions.running(session)) {
      return state.phase === 'idle' && session.phase === 'draft'
        ? { label: 'Draft', tone: 'neutral' }
        : Runs.mark(state);
    }
    if (state?.phase === 'ended' && Runs.endTone(state.end, state.exit) === 'danger') {
      return Runs.mark(state);
    }
    return session.phase === 'draft'
      ? { label: 'Draft', tone: 'neutral' }
      : { label: client, tone: 'neutral' };
  }

  /**
   * A draft in `dir` (null: any scratch folder) that was never asked
   * anything, the newest, for a new session there to reuse instead of
   * making another; null when there is none.
   */
  static unused(
    bodies: readonly SessionBody[],
    live: readonly RunSnapshot[],
    scratch: string | null,
    dir: string | null,
  ): SessionId | null {
    const here = (workdir: string) =>
      dir === null ? scratch !== null && workdir.startsWith(`${scratch}/`) : workdir === dir;
    const drafts = bodies
      .map((body) => Sessions.desk(body, live))
      .filter(
        (session): session is DeskSession =>
          session !== null &&
          session.phase === 'draft' &&
          session.shelf === 'open' &&
          session.asked === null &&
          here(session.workdir),
      );
    const newest = (session: DeskSession) => session.activeAt ?? Number.MAX_SAFE_INTEGER;
    return drafts.toSorted((a, b) => newest(b) - newest(a))[0]?.session ?? null;
  }

  /** The session one of whose runs is `run`, for links to a run. */
  static of(bodies: readonly SessionBody[], run: RunId): SessionId | null {
    return (
      bodies.find(
        (body) =>
          body.served?.process === run || body.stored?.runs.some((entry) => entry.id === run),
      )?.session ?? null
    );
  }
}

/** A client's models as the new-run form offers them. */
export class Models {
  /** The models by provider, in the order the client lists them. */
  static groups(models: readonly RunModel[]): { provider: string | null; models: RunModel[] }[] {
    const groups: { provider: string | null; models: RunModel[] }[] = [];
    for (const model of models) {
      const group = groups.find((group) => group.provider === model.provider);
      if (group) group.models.push(model);
      else groups.push({ provider: model.provider, models: [model] });
    }
    return groups;
  }

  /** The model a run gets when none is chosen, when the client says. */
  static fallback(models: readonly RunModel[]): RunModel | null {
    return models.find((model) => model.choice === 'default') ?? null;
  }

  /** `wanted` when the client still lists it; otherwise no choice. */
  static keep(models: readonly RunModel[], wanted: string | null): ModelId | null {
    return models.find((model) => model.id === wanted)?.id ?? null;
  }
}

/** A run's settings as people read them. */
export class Setup {
  /** An approval policy in a few words. */
  static policy(policy: Stored<ApprovalPolicy>): string {
    switch (policy) {
      case 'ask':
        return 'Ask first';
      case 'edits':
        return 'Edits alone';
      case 'auto':
        return 'Auto';
      default:
        return policy;
    }
  }

  /** What each policy lets the client do, in a sentence. */
  static explain(policy: ApprovalPolicy): string {
    switch (policy) {
      case 'ask':
        return 'Every edit and command waits for you.';
      case 'edits':
        return 'Edits in the folder go ahead; commands wait for you.';
      case 'auto':
        return 'The client decides what is safe and asks only beyond that.';
      default:
        return assertNever(policy);
    }
  }

  /** The settings that are not the client's own, as short words. */
  static words(settings: RunSettings): string[] {
    return [
      settings.model,
      settings.effort ? `${settings.effort} effort` : null,
      settings.approvals ? Setup.policy(settings.approvals).toLowerCase() : null,
    ].filter((word): word is string => word !== null);
  }

  /** The settings to start again with: labels this UI does not know are
   * left to the client. */
  static known(settings: RunSettings): {
    model: ModelId | null;
    effort: Effort | null;
    approvals: ApprovalPolicy | null;
  } {
    return {
      model: settings.model,
      effort: EFFORTS.find((level) => level === settings.effort) ?? null,
      approvals: APPROVAL_POLICIES.find((policy) => policy === settings.approvals) ?? null,
    };
  }

  /** The efforts the chosen model takes: the default's are its model's. */
  static efforts(models: readonly RunModel[], model: ModelId | null): RunModel | null {
    return (
      (model ? models.find((offered) => offered.id === model) : Models.fallback(models)) ?? null
    );
  }
}

/** The settings last chosen for a client, as kept for the viewer. */
export interface Kept {
  model?: string;
  effort?: string;
  approvals?: string;
}

/** The client last chosen for a session, and each client's last settings. */
export interface Remembered {
  harness: Drivable;
  settings: Partial<Record<Drivable, Kept>>;
}

/**
 * What the Sessions page remembers for this viewer, so a new session starts
 * as the last one was set: kept in a cookie. Only labels: never a prompt, a
 * path, or anything secret. Read as untrusted text.
 */
export class Memory {
  static readonly COOKIE = 'ren_runs';
  static readonly NONE: Remembered = { harness: 'codex', settings: {} };

  /** The cookie's value, read; anything unreadable is the default. */
  static parse(raw: string | null | undefined): Remembered {
    if (!raw) return Memory.NONE;
    let saved: unknown;
    try {
      saved = JSON.parse(raw);
    } catch {
      return Memory.NONE;
    }
    if (!isRecord(saved)) return Memory.NONE;
    const settings: Partial<Record<Drivable, Kept>> = {};
    const stored = saved.settings;
    if (isRecord(stored)) {
      for (const drivable of DRIVABLE) {
        const kept = stored[drivable];
        if (!isRecord(kept)) continue;
        const text = (key: string) => {
          const value = kept[key];
          return typeof value === 'string' && value.length <= 200 ? value : undefined;
        };
        settings[drivable] = {
          model: text('model'),
          effort: text('effort'),
          approvals: text('approvals'),
        };
      }
    }
    return {
      harness: DRIVABLE.find((drivable) => drivable === saved.harness) ?? 'codex',
      settings,
    };
  }

  /** The cookie that keeps `remembered` for a year, for the Sessions page only. */
  static cookie(remembered: Remembered): string {
    const value = encodeURIComponent(JSON.stringify(remembered));
    return `${Memory.COOKIE}=${value}; Path=/sessions; Max-Age=31536000; SameSite=Lax`;
  }

  /** `remembered`, with `harness` the client last chosen. */
  static chose(remembered: Remembered, harness: Drivable): Remembered {
    return { ...remembered, harness };
  }

  /** `remembered`, with `settings` the last chosen for `harness`; none is the client's own. */
  static set(
    remembered: Remembered,
    harness: Drivable,
    settings: { model: ModelId | null; effort: Effort | null; approvals: ApprovalPolicy | null },
  ): Remembered {
    return {
      ...remembered,
      settings: {
        ...remembered.settings,
        [harness]: {
          model: settings.model ?? undefined,
          effort: settings.effort ?? undefined,
          approvals: settings.approvals ?? undefined,
        },
      },
    };
  }

  /** The settings a new session on `harness` starts with: those kept for it
   * that still apply, the model one it lists, the effort one that model
   * takes, and the approval policy one it runs under. */
  static settings(
    remembered: Remembered,
    harness: Drivable,
    models: readonly RunModel[],
    policies: readonly Stored<ApprovalPolicy>[],
  ): { model: ModelId | null; effort: Effort | null; approvals: ApprovalPolicy | null } {
    const kept = remembered.settings[harness] ?? {};
    const model = Models.keep(models, kept.model ?? null);
    const takes = Setup.efforts(models, model)?.efforts ?? [];
    return {
      model,
      effort: EFFORTS.find((level) => level === kept.effort && takes.includes(level)) ?? null,
      approvals:
        APPROVAL_POLICIES.find(
          (policy) => policy === kept.approvals && policies.includes(policy),
        ) ?? null,
    };
  }
}

/** What the composer of a run does, by how the run stands. */
export type ComposeMode =
  /** No turn runs: Enter starts one. */
  | { kind: 'start' }
  /** A turn runs: Enter queues for after it; "send now" steers it where the client can. */
  | { kind: 'during'; steer: boolean; queued: string | null }
  /** The run ended with a conversation: writing continues it in a new run. */
  | { kind: 'continue' }
  /** Nothing can be sent yet, or any more: why. */
  | { kind: 'closed'; reason: string };

export class Compose {
  /** How the composer of a run in `state` works, for a client taking `midway`. */
  static mode(state: RunState, midway: readonly Stored<Midway>[], resumable: boolean): ComposeMode {
    switch (state.phase) {
      case 'idle':
        return { kind: 'start' };
      case 'working':
        return { kind: 'during', steer: midway.includes('steer'), queued: state.queued };
      case 'starting':
        return { kind: 'closed', reason: 'The run is opening its conversation…' };
      case 'stopping':
        return { kind: 'closed', reason: 'The run is stopping.' };
      case 'ended':
        return resumable
          ? { kind: 'continue' }
          : { kind: 'closed', reason: 'This run ended before it opened a conversation.' };
      case 'unrecognized':
        return { kind: 'closed', reason: 'The daemon reports a state this page does not know.' };
      default:
        return assertNever(state);
    }
  }

  /**
   * The earlier prompt ↑ or ↓ recalls: `prompts` oldest first, `at` how
   * far back the composer is now (null: not recalling). Null when there is
   * nothing further that way, `at` null again when back past the newest.
   */
  /**
   * How a session's composer works: a prompt always goes, starting or
   * resuming its process when none runs, or into or after a running turn;
   * only while its process stops does it wait.
   */
  static session(state: RunState | null, midway: readonly Stored<Midway>[]): ComposeMode {
    if (!state) return { kind: 'start' };
    switch (state.phase) {
      case 'working':
        return { kind: 'during', steer: midway.includes('steer'), queued: state.queued };
      case 'stopping':
        return { kind: 'closed', reason: 'Its client is stopping; write again in a moment.' };
      case 'idle':
      case 'starting':
      case 'ended':
      case 'unrecognized':
        return { kind: 'start' };
      default:
        return assertNever(state);
    }
  }

  static recall(
    prompts: readonly string[],
    at: number | null,
    older: boolean,
  ): { at: number | null; text: string } | null {
    if (older) {
      const next = at === null ? 0 : at + 1;
      const text = prompts[prompts.length - 1 - next];
      return text === undefined ? null : { at: next, text };
    }
    if (at === null) return null;
    if (at === 0) return { at: null, text: '' };
    return { at: at - 1, text: prompts[prompts.length - at]! };
  }
}
