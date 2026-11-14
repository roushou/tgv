/**
 * Redline's pure core: what a mark points at, and the brief an agent gets
 * from the marks on a page. The overlay reads the page (`RedlineLayer`);
 * nothing here touches the DOM, so the brief is tested without a browser.
 */
import type { Revision } from './api.ts';
import { SignIns } from './signIn.ts';
import {
  Sessions,
  type ComposeMode,
  type Drivable,
  type RunClient,
  type DeskSession,
  type RunSnapshot,
  type SessionBody,
} from './runs.ts';

/** Where a brief goes: a new session in the checkout, or one running there. */
export type Destination = { kind: 'new' } | { kind: 'session'; session: DeskSession };

/** How a brief reaches an open session: the prompt to send, or why it waits. */
export type Handoff =
  { kind: 'send'; delivery: 'start' | 'queue'; text: string } | { kind: 'wait'; reason: string };

/** The part of an element a selector is built from: what a DOM element has. */
export interface ElementLike {
  readonly tagName: string;
  readonly id: string;
  readonly classList: Iterable<string>;
  readonly parentElement: ElementLike | null;
  getAttribute(name: string): string | null;
}

/** What a mark points at, as the overlay read it from the page. */
export interface Target {
  /** A CSS selector from the nearest labelled region down to the element. */
  selector: string;
  /** The element's own text, trimmed and cut short; empty when it has none. */
  text: string;
  /** The source file and line Svelte's development build names, when known. */
  source: string | null;
  /** Its size on screen, in CSS pixels. */
  width: number;
  height: number;
}

/** One mark: what it points at, and what the operator wrote about it. */
export interface Mark {
  id: number;
  target: Target;
  note: string;
}

/** The page the marks were made on. */
export interface PageContext {
  /** Its path and query, as the address bar shows them. */
  path: string;
  /** SvelteKit's route id (`/sessions`), when the page has one. */
  route: string | null;
  title: string;
  width: number;
  height: number;
  theme: 'light' | 'dark';
  /** What the daemon serving the page was built from, when it says. */
  revision: Revision | null;
}

export class Redline {
  /** How many ancestors a selector names, below its labelled region. */
  static readonly DEPTH = 3;
  /** The most text a target keeps. */
  static readonly TEXT = 80;

  /**
   * A readable selector for `element`: up to `DEPTH` steps of tag, id, and
   * classes (without Svelte's scoping hashes), from the nearest ancestor
   * with an `aria-label`, which names where on the page it is.
   */
  static selector(element: ElementLike): string {
    const steps: string[] = [];
    let at: ElementLike | null = element;
    while (at && steps.length < Redline.DEPTH) {
      steps.unshift(Redline.#step(at));
      at = at.parentElement;
    }
    for (let up: ElementLike | null = at; up; up = up.parentElement) {
      const label = up.getAttribute('aria-label');
      if (label) return `[aria-label="${label.replaceAll('"', '\\"')}"] ${steps.join(' > ')}`;
    }
    return steps.join(' > ');
  }

  static #step(element: ElementLike): string {
    const tag = element.tagName.toLowerCase();
    if (element.id) return `${tag}#${element.id}`;
    const classes = [...element.classList].filter((name) => !name.startsWith('svelte-'));
    return classes.length ? `${tag}.${classes.join('.')}` : tag;
  }

  /** What a mark is called in the pane: the element's text, else the last
   * step of its selector. */
  static name(target: Target): string {
    if (target.text) return `“${target.text}”`;
    const steps = target.selector.split(' > ');
    return steps[steps.length - 1] ?? target.selector;
  }

  /** An element's text as a target keeps it: one line, cut to `TEXT`. */
  static text(raw: string): string {
    const line = raw.replace(/\s+/g, ' ').trim();
    return line.length > Redline.TEXT ? `${line.slice(0, Redline.TEXT - 1)}…` : line;
  }

  /**
   * `REN_SOURCE_DIR`: Ren's source checkout, where a brief is sent to be
   * worked on. Absent, Redline marks but cannot send.
   */
  static checkout(value: string | undefined): string | undefined {
    if (value === undefined || value === '') return undefined;
    if (!value.startsWith('/')) throw new Error('REN_SOURCE_DIR must be an absolute path.');
    return value.length > 1 ? value.replace(/\/+$/, '') : value;
  }

  /** The most sessions the pane offers to send into. */
  static readonly OFFERED = 4;

  /**
   * The sessions in `checkout` a brief can go into: open, with their client
   * running now (an ended or never-prompted session is no place to add to),
   * as the Sessions page orders them: one waiting for you first, then one
   * working, then the most recently active.
   */
  static offered(
    bodies: readonly SessionBody[],
    live: readonly RunSnapshot[],
    checkout: string,
  ): DeskSession[] {
    const open = bodies.filter((body) => {
      const session = Sessions.desk(body, live);
      return (
        session !== null &&
        session.shelf === 'open' &&
        session.workdir === checkout &&
        Sessions.running(session)
      );
    });
    return (Sessions.folders(open, live)[0]?.sessions ?? []).slice(0, Redline.OFFERED);
  }

  /**
   * Another client a brief could go to when `harness` is signed out: the
   * first of `ready` other than it whose sign-in is not known to be out.
   * Offered for the operator to choose, never switched to on its own.
   */
  static instead(
    ready: readonly Drivable[],
    clients: readonly RunClient[],
    harness: Drivable,
  ): Drivable | null {
    return (
      ready.find((drivable) => {
        if (drivable === harness) return false;
        const entry = clients.find((client) => client.harness === drivable);
        return entry !== undefined && !SignIns.signedOut(entry.sign_in);
      }) ?? null
    );
  }

  /**
   * How `brief` goes into a session whose composer is in `mode`: it starts
   * a turn when none runs, or is queued after the running one (after
   * anything queued before, as one message), never steering it.
   */
  static handoff(mode: ComposeMode, brief: string): Handoff {
    switch (mode.kind) {
      case 'start':
      case 'continue':
        return { kind: 'send', delivery: 'start', text: brief };
      case 'during':
        return {
          kind: 'send',
          delivery: 'queue',
          text: mode.queued ? `${mode.queued}\n\n${brief}` : brief,
        };
      case 'closed':
        return { kind: 'wait', reason: mode.reason };
      default: {
        const never: never = mode;
        return never;
      }
    }
  }

  /** What the brief says of the build the page came from, if anything. */
  static built(revision: Revision | null): string | null {
    if (!revision) return null;
    switch (revision.kind) {
      case 'commit':
        return `This page was served by a build of commit ${Redline.code(revision.commit.slice(0, 12))}${revision.tree === 'modified' ? ' with uncommitted changes' : ''}; the checkout may have moved on since.`;
      case 'unknown':
        return null;
      case 'unrecognized':
        return `This page was served by a build of ${Redline.code(revision.text)}; the checkout may have moved on since.`;
      default: {
        const never: never = revision;
        return never;
      }
    }
  }

  /** The Svelte file that renders a route of Ren's UI. */
  static routeFile(route: string): string {
    return `ui/src/routes${route === '/' ? '' : route}/+page.svelte`;
  }

  /** A branch name for the change: `redline/` and the first note's words. */
  static branch(marks: readonly Mark[]): string {
    const words = (marks[0]?.note ?? '')
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, ' ')
      .trim()
      .split(' ')
      .filter((word) => word.length > 1)
      .slice(0, 5);
    return `redline/${words.length ? words.join('-') : 'change'}`;
  }

  /** `text` as a Markdown code span, fenced past any backticks inside. */
  static code(text: string): string {
    const longest = Math.max(0, ...(text.match(/`+/g) ?? []).map((run) => run.length));
    const fence = '`'.repeat(longest + 1);
    return longest ? `${fence} ${text} ${fence}` : `${fence}${text}${fence}`;
  }

  /** The brief an agent gets, in Markdown: what the first mark asks (a
   * session is named by its first line, so it stays plain), the page, each
   * mark as a list item with what it points at,
   * and how to work: questions are answered in place, and only a change
   * gets a branch. Marks without a note are left out. */
  static brief(page: PageContext, marks: readonly Mark[]): string {
    const noted = marks.filter((mark) => mark.note.trim());
    const first = noted[0]?.note.trim().split('\n')[0] ?? 'Changes to a page';
    const more = noted.length > 1 ? ` (and ${noted.length - 1} more)` : '';
    const lines = [
      `Redline: ${first}${more}`,
      '',
      `Marked on ${Redline.code(page.path)}${page.title ? ` (${page.title})` : ''}, ${page.width}×${page.height}, ${page.theme} theme.`,
    ];
    if (page.route) {
      lines.push(`In Ren's UI this page is ${Redline.code(Redline.routeFile(page.route))}.`);
    }
    const built = Redline.built(page.revision);
    if (built) lines.push(built);
    lines.push('');
    noted.forEach((mark, index) => {
      const { target } = mark;
      // A note's later lines are indented, so they stay in its item.
      const note = mark.note.trim().split('\n').join('\n   ');
      lines.push(`${index + 1}. ${note}`);
      lines.push(
        `   - Element: ${Redline.code(target.selector)}${target.text ? `, “${target.text}”` : ''}`,
      );
      if (target.source) lines.push(`   - Source: ${Redline.code(target.source)}`);
      lines.push(`   - Size: ${Math.round(target.width)}×${Math.round(target.height)}`);
    });
    lines.push(
      '',
      `Answer what the notes ask without changing anything. If a note asks for a change, make it on a new branch, ${Redline.code(Redline.branch(noted))}, in a git worktree of this repository, not in the checkout itself, and run the project's checks. Don't install or restart anything: I'll review the branch.`,
    );
    return lines.join('\n');
  }
}
