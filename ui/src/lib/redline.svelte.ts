/**
 * Redline's state for one page view: whether marking is on, the marks made,
 * and the one whose note is being written. The layout owns it; the layer,
 * the panel, and the status line's toggle read it.
 */
import { Redline, type Mark, type Target } from './redline.ts';
import {
  DRIVABLE,
  RunCommandError,
  RunsClient,
  type DeskSession,
  type Drivable,
  type RunClient,
} from './runs.ts';

/** Where a brief can go: the clients ready here, for a new session, and the
 * sessions running in the checkout. */
export interface Destinations {
  ready: Drivable[];
  /** Each client as the daemon describes it: its approval policies, and
   * what it takes mid-run. */
  clients: RunClient[];
  sessions: DeskSession[];
}

/** Reading the destinations: not yet, under way, read, or failed. */
export type Reading =
  | { state: 'idle' }
  | { state: 'reading' }
  | { state: 'read'; value: Destinations }
  | { state: 'failed'; reason: string };

/** A mark, with the element it points at while that element is on the page. */
export interface Placed extends Mark {
  element: Element;
}

export class RedlineDesk {
  open = $state(false);
  marks = $state<Placed[]>([]);
  /** The mark whose note is being written. */
  editing = $state<number | null>(null);
  /** Where a brief can go, read once per page view. */
  destinations = $state.raw<Reading>({ state: 'idle' });
  #next = 1;
  /** Ren's source checkout, where briefs go; null when the UI is not told. */
  readonly #checkout: () => string | null;
  /** Whether what was read is from an earlier opening, to be read again. */
  #stale = false;

  constructor(checkout: () => string | null) {
    this.#checkout = checkout;
  }

  /** Read where a brief can go: the clients, and the sessions open in the
   * checkout. Started as the pointer nears the toggle, so the pane usually
   * opens with them; read again for each opening, keeping the last reading
   * shown meanwhile, since sessions come and go. */
  prepare() {
    const { state } = this.destinations;
    if (state === 'reading' || (state === 'read' && !this.#stale)) return;
    if (state !== 'read') this.destinations = { state: 'reading' };
    this.#stale = false;
    const checkout = this.#checkout();
    const client = new RunsClient();
    Promise.all([
      client.clients(),
      checkout ? client.sessions() : [],
      checkout ? client.live() : [],
    ]).then(
      ([clients, bodies, live]) => {
        this.destinations = {
          state: 'read',
          value: {
            ready: DRIVABLE.filter((drivable) =>
              clients.some((entry) => entry.harness === drivable && entry.status === 'ready'),
            ),
            clients,
            sessions: checkout ? Redline.offered(bodies, live, checkout) : [],
          },
        };
      },
      (failure: unknown) => {
        this.destinations = {
          state: 'failed',
          reason:
            failure instanceof RunCommandError
              ? failure.message
              : 'Cannot read the clients and sessions.',
        };
      },
    );
  }

  /** Read the clients and sessions again now, keeping what is shown
   * meanwhile: after a client's sign-in changed. */
  reread() {
    this.#stale = true;
    this.prepare();
  }

  toggle() {
    this.open = !this.open;
    if (this.open) this.prepare();
    if (!this.open) this.#stale = true;
    if (!this.open) this.drop(this.editing);
  }

  /** Mark `element`, and start its note. */
  add(element: Element) {
    const box = element.getBoundingClientRect();
    const target: Target = {
      selector: Redline.selector(element),
      text: Redline.text(element.textContent ?? ''),
      source: RedlineDesk.source(element),
      width: box.width,
      height: box.height,
    };
    const id = this.#next++;
    this.marks.push({ id, target, note: '', element });
    this.editing = id;
  }

  /** Keep `note` on mark `id`; a blank note drops the mark. */
  save(id: number, note: string) {
    const mark = this.marks.find((entry) => entry.id === id);
    if (mark) mark.note = note.trim();
    if (!note.trim()) this.drop(id);
    this.editing = null;
  }

  drop(id: number | null) {
    if (id === null) return;
    this.marks = this.marks.filter((mark) => mark.id !== id);
    if (this.editing === id) this.editing = null;
  }

  clear() {
    this.marks = [];
    this.editing = null;
    this.#next = 1;
  }

  /** The source file and line Svelte's development build attaches to an
   * element or its nearest ancestor; production builds attach none. */
  static source(element: Element): string | null {
    let at: Element | null = element;
    for (let depth = 0; at && depth < 6; depth += 1) {
      const meta = (at as Element & { __svelte_meta?: { loc?: { file?: string; line?: number } } })
        .__svelte_meta;
      if (meta?.loc?.file) return `${meta.loc.file}:${meta.loc.line ?? 0}`;
      at = at.parentElement;
    }
    return null;
  }
}
