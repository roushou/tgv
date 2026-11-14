// The tools Ren installs for its agents on the daemon's host: what the
// daemon's `/control/v1/tools` says (through this UI server's
// `/control/tools` proxy), and the Tools view's pure logic: each package's
// standing, the change the operator stages before applying it, and how the
// table groups packages. No browser APIs beyond the `fetch` handed in;
// tested in tests/tools.test.ts.
import { API_VERSION, HARNESSES, versioned, type Harness } from './api.ts';
import {
  ShapeError,
  array,
  assertNever,
  branded,
  brandedCount,
  isRecord,
  label,
  nullable,
  object,
  string,
  type Brand,
  type Decoder,
  type Stored,
} from './decode.ts';

/** A package's name in Ren's catalog (`node`, `jq`). */
export type PackageId = Brand<string, 'PackageId'>;
/** A release's version as its publisher numbers it. */
export type Version = Brand<string, 'Version'>;
/** A toolset generation's number; 0 before anything is installed. */
export type Generation = Brand<number, 'Generation'>;

export const KINDS = ['runtime', 'utility', 'client'] as const;
export type Kind = (typeof KINDS)[number];
const origins = ['builtin', 'operator'] as const;
/** Which catalog names a package: Ren's own, or the operator's `[tools]`. */
export type Origin = (typeof origins)[number];
const platforms = ['linux_x64', 'linux_arm64', 'macos_arm64'] as const;
export type Platform = (typeof platforms)[number];
const generationStates = ['current', 'in_use', 'idle'] as const;
export type GenerationState = (typeof generationStates)[number];
const outcomes = ['unchanged', 'activated', 'failed'] as const;
export type ChangeOutcome = (typeof outcomes)[number];
const changeKinds = ['add', 'replace', 'remove'] as const;
export type ChangeKind = (typeof changeKinds)[number];
const stages = ['fetch', 'verify', 'unpack', 'place'] as const;
export type Stage = (typeof stages)[number];

export interface InstalledTool {
  version: Version;
  sha256: string;
  dir: string;
}

export interface SystemTool {
  path: string;
  /** The version it reported, when the daemon could read one. */
  version: Version | null;
  /** What it printed instead, or why it did not answer. */
  unread: string | null;
}

export interface ToolPackage {
  id: PackageId;
  /** Which catalog names it; `null` for an install the catalog no longer names. */
  origin: Stored<Origin> | null;
  /** What it is for; `null` when the catalog no longer names it. */
  kind: Stored<Kind> | null;
  /** The program it provides (`rg` for `ripgrep`). */
  program: string;
  /** The release Ren installs; `null` for a package it only finds on PATH. */
  release: Version | null;
  installed: InstalledTool | null;
  system: SystemTool | null;
  needed_by: Stored<Harness>[];
}

export interface GenerationInstall {
  package: PackageId;
  version: Version;
}

export interface GenerationView {
  generation: Generation;
  state: Stored<GenerationState>;
  installs: GenerationInstall[];
}

export interface ToolListing {
  /** Where Ren installs tools; `null` on a platform it installs nothing for. */
  platform: Stored<Platform> | null;
  generation: Generation;
  /** The current generation's folder of programs; `null` before any. */
  bin: string | null;
  packages: ToolPackage[];
  generations: GenerationView[];
}

export interface PackageChange {
  change: Stored<ChangeKind>;
  package: PackageId;
  from: Version | null;
  to: Version | null;
}

export interface InstallFailure {
  package: PackageId;
  version: Version;
  stage: Stored<Stage>;
  reason: string;
}

export interface ToolChange {
  outcome: Stored<ChangeOutcome>;
  generation: Generation;
  changes: PackageChange[];
  failures: InstallFailure[];
}

export interface ToolCollection {
  generations: Generation[];
  installs: string[];
}

const packageId = branded<'PackageId'>();
const version = branded<'Version'>();
const generation = brandedCount<'Generation'>();

const listing: Decoder<ToolListing> = versioned(
  object<ToolListing>({
    platform: nullable(label(platforms)),
    generation,
    bin: nullable(string),
    packages: array(
      object<ToolPackage>({
        id: packageId,
        origin: nullable(label(origins)),
        kind: nullable(label(KINDS)),
        program: string,
        release: nullable(version),
        installed: nullable(object<InstalledTool>({ version, sha256: string, dir: string })),
        system: nullable(
          object<SystemTool>({
            path: string,
            version: nullable(version),
            unread: nullable(string),
          }),
        ),
        needed_by: array(label(HARNESSES)),
      }),
    ),
    generations: array(
      object<GenerationView>({
        generation,
        state: label(generationStates),
        installs: array(object<GenerationInstall>({ package: packageId, version })),
      }),
    ),
  }),
);

const change: Decoder<ToolChange> = versioned(
  object<ToolChange>({
    outcome: label(outcomes),
    generation,
    changes: array(
      object<PackageChange>({
        change: label(changeKinds),
        package: packageId,
        from: nullable(version),
        to: nullable(version),
      }),
    ),
    failures: array(
      object<InstallFailure>({
        package: packageId,
        version,
        stage: label(stages),
        reason: string,
      }),
    ),
  }),
);

const collection: Decoder<ToolCollection> = versioned(
  object<ToolCollection>({ generations: array(generation), installs: array(string) }),
);

/** Where a package stands on the daemon's host. */
export type Standing =
  /** Ren installed the catalog's release. */
  | { kind: 'installed'; version: Version }
  /** Ren installed another release than the catalog's. */
  | { kind: 'outdated'; installed: Version; release: Version }
  /** Ren can install it and has not. */
  | { kind: 'available'; release: Version }
  /** Ren only finds it on PATH, and it is there. */
  | { kind: 'system' }
  /** Ren only finds it on PATH, and it is not there. */
  | { kind: 'missing' }
  /** Installed, and since taken out of the catalog: it can only be removed. */
  | { kind: 'uncatalogued'; installed: Version };

export class Standings {
  static of(pkg: ToolPackage): Standing {
    if (pkg.origin === null && pkg.installed !== null) {
      return { kind: 'uncatalogued', installed: pkg.installed.version };
    }
    if (pkg.release === null) return pkg.system === null ? { kind: 'missing' } : { kind: 'system' };
    if (pkg.installed === null) return { kind: 'available', release: pkg.release };
    return pkg.installed.version === pkg.release
      ? { kind: 'installed', version: pkg.release }
      : { kind: 'outdated', installed: pkg.installed.version, release: pkg.release };
  }
}

/**
 * What a new run gets for a package: Ren's install, first on its PATH (over
 * any copy the daemon's PATH has), the daemon's PATH's copy, or nothing.
 */
export type Supply =
  | { kind: 'ren'; version: Version; shadows: SystemTool | null }
  | { kind: 'path'; system: SystemTool }
  | { kind: 'none' };

export class Supplies {
  static of(pkg: ToolPackage): Supply {
    if (pkg.installed !== null) {
      return { kind: 'ren', version: pkg.installed.version, shadows: pkg.system };
    }
    if (pkg.system !== null) return { kind: 'path', system: pkg.system };
    return { kind: 'none' };
  }

  /**
   * The row's dot: Ren's install, the PATH's copy, nothing, or nothing
   * where a client needs it.
   */
  static tone(pkg: ToolPackage): 'ren' | 'path' | 'none' | 'lacking' {
    const supply = Supplies.of(pkg);
    switch (supply.kind) {
      case 'ren':
      case 'path':
        return supply.kind;
      case 'none':
        return pkg.needed_by.length > 0 ? 'lacking' : 'none';
      default:
        return assertNever(supply);
    }
  }

  /** What new runs get from Ren, in listing order. */
  static fromRen(listing: ToolListing): { id: PackageId; version: Version }[] {
    return listing.packages.flatMap((p) =>
      p.installed === null ? [] : [{ id: p.id, version: p.installed.version }],
    );
  }
}

export class Platforms {
  /** A platform as people name it. */
  static label(platform: Stored<Platform>): string {
    switch (platform) {
      case 'linux_x64':
        return 'Linux x64';
      case 'linux_arm64':
        return 'Linux arm64';
      case 'macos_arm64':
        return 'macOS arm64';
      default:
        return platform;
    }
  }
}

/** One row of a staged plan, as it will change the toolset. */
export interface PlannedChange {
  change: ChangeKind;
  id: PackageId;
  from: Version | null;
  to: Version | null;
}

/**
 * The change the operator stages before applying it: packages to add (or
 * bring to the catalog's release) and packages to remove. Immutable; each
 * toggle makes a new plan.
 */
export class ToolPlan {
  readonly add: ReadonlySet<PackageId>;
  readonly remove: ReadonlySet<PackageId>;

  constructor(add: Iterable<PackageId> = [], remove: Iterable<PackageId> = []) {
    this.add = new Set(add);
    this.remove = new Set(remove);
  }

  static readonly EMPTY = new ToolPlan();

  get empty(): boolean {
    return this.add.size === 0 && this.remove.size === 0;
  }

  /** Whether `pkg` is staged, either way. */
  staged(pkg: ToolPackage): boolean {
    return this.add.has(pkg.id) || this.remove.has(pkg.id);
  }

  /**
   * Stage or unstage the one change `pkg` can take: install it, update it to
   * the catalog's release, or remove an installed one. A package Ren only
   * finds on PATH takes none, and the plan is returned unchanged.
   */
  toggle(pkg: ToolPackage): ToolPlan {
    const add = new Set(this.add);
    const remove = new Set(this.remove);
    const flip = (set: Set<PackageId>) => (set.has(pkg.id) ? set.delete(pkg.id) : set.add(pkg.id));
    const standing = Standings.of(pkg);
    switch (standing.kind) {
      case 'available':
      case 'outdated':
        flip(add);
        break;
      case 'installed':
      case 'uncatalogued':
        flip(remove);
        break;
      case 'system':
      case 'missing':
        return this;
      default:
        return assertNever(standing);
    }
    return new ToolPlan(add, remove);
  }

  /** Stage removing an outdated install instead of updating it. */
  toggleRemove(pkg: ToolPackage): ToolPlan {
    if (pkg.installed === null) return this;
    const add = new Set(this.add);
    const remove = new Set(this.remove);
    add.delete(pkg.id);
    if (remove.has(pkg.id)) remove.delete(pkg.id);
    else remove.add(pkg.id);
    return new ToolPlan(add, remove);
  }

  /** What applying this plan to `listing` changes, in listing order. */
  changes(listing: ToolListing): PlannedChange[] {
    const planned: PlannedChange[] = [];
    for (const pkg of listing.packages) {
      if (this.remove.has(pkg.id) && pkg.installed !== null) {
        planned.push({ change: 'remove', id: pkg.id, from: pkg.installed.version, to: null });
      } else if (this.add.has(pkg.id) && pkg.release !== null) {
        if (pkg.installed === null) {
          planned.push({ change: 'add', id: pkg.id, from: null, to: pkg.release });
        } else if (pkg.installed.version !== pkg.release) {
          planned.push({
            change: 'replace',
            id: pkg.id,
            from: pkg.installed.version,
            to: pkg.release,
          });
        }
      }
    }
    return planned;
  }

  /** Only what still applies to `listing`, after it changed. */
  within(listing: ToolListing): ToolPlan {
    const changes = this.changes(listing);
    return new ToolPlan(
      changes.filter((c) => c.change !== 'remove').map((c) => c.id),
      changes.filter((c) => c.change === 'remove').map((c) => c.id),
    );
  }

  /** The edit `/control/tools/change` takes. */
  body(): { add: PackageId[]; remove: PackageId[] } {
    return { add: [...this.add].sort(), remove: [...this.remove].sort() };
  }
}

/**
 * Packages grouped by kind, in the order the table shows them; installs the
 * catalog no longer names (`kind` null) come last.
 */
export interface KindGroup {
  kind: Stored<Kind> | null;
  title: string;
  packages: ToolPackage[];
}

export class ToolGroups {
  static of(listing: ToolListing): KindGroup[] {
    const known: Stored<Kind>[] = [...KINDS];
    const unknown = new Set<Stored<Kind>>();
    for (const p of listing.packages) {
      if (p.kind !== null && !known.includes(p.kind)) unknown.add(p.kind);
    }
    const kinds: (Stored<Kind> | null)[] = [...known, ...unknown, null];
    return kinds
      .map((kind) => ({
        kind,
        title: ToolGroups.title(kind),
        packages: listing.packages.filter((p) => p.kind === kind),
      }))
      .filter((group) => group.packages.length > 0);
  }

  static title(kind: Stored<Kind> | null): string {
    if (kind === null) return 'Not in the catalog';
    switch (kind) {
      case 'runtime':
        return 'Runtimes';
      case 'utility':
        return 'Utilities';
      case 'client':
        return 'Clients';
      default:
        return kind;
    }
  }
}

/** Why a tools request did not succeed, in the operator's words. */
export class ToolsError extends Error {
  readonly status: number;
  readonly code: string | null;

  constructor(status: number, code: string | null, message: string | null) {
    super(ToolsError.describe(status, code, message));
    this.status = status;
    this.code = code;
  }

  static describe(status: number, code: string | null, message: string | null): string {
    switch (code) {
      case 'tools_refused':
        return message === null ? 'The daemon refused the change.' : `${message}.`;
      case 'tools_no_generation':
      case 'tools_collected':
        return message === null ? 'That generation cannot be restored.' : `${message}.`;
      case 'tools_busy':
        return 'Another change to the tools is in progress. Try again when it ends.';
      case 'tools_unsupported':
        return 'Ren installs tools only on Linux x64, Linux arm64, and macOS arm64.';
      case 'tools_disabled':
        return 'The daemon could not set up its tools. Its log says why.';
      case 'tools_failed':
        return 'The daemon could not read or write its tool store. Its log says why.';
      case 'runs_off':
        return 'Set REN_CONTROL_TOKEN_FILE on the UI server to manage tools here.';
      case 'no_control_token':
        return 'The UI server cannot read the daemon’s control token.';
      case 'login_not_allowed':
        return 'Your Tailscale login may not change tools here.';
      case 'daemon_unreachable':
        return 'The daemon did not answer.';
      default:
        return `The request was refused (${status}${code ? ` ${code}` : ''}).`;
    }
  }
}

/** The daemon's tools, through this UI server's proxy. */
export class ToolsApi {
  readonly #fetch: typeof fetch;

  constructor(fetcher: typeof fetch) {
    this.#fetch = fetcher;
  }

  listing(signal?: AbortSignal): Promise<ToolListing> {
    return this.#request('GET', '/control/tools', undefined, listing, signal);
  }

  change(plan: ToolPlan): Promise<ToolChange> {
    return this.#request('POST', '/control/tools/change', plan.body(), change);
  }

  /** Make the next generation hold what a kept `generation` held. */
  rollBack(generation: Generation): Promise<ToolChange> {
    return this.#request('POST', '/control/tools/rollback', { generation }, change);
  }

  collect(): Promise<ToolCollection> {
    return this.#request('POST', '/control/tools/gc', {}, collection);
  }

  async #request<T>(
    method: 'GET' | 'POST',
    path: string,
    body: unknown,
    decode: Decoder<T>,
    signal?: AbortSignal,
  ): Promise<T> {
    const response = await this.#fetch(path, {
      method,
      cache: 'no-store',
      headers:
        body === undefined
          ? { accept: 'application/json' }
          : { accept: 'application/json', 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
    });
    let parsed: unknown = null;
    try {
      parsed = await response.json();
    } catch {
      parsed = null;
    }
    if (!response.ok) {
      const code = isRecord(parsed) && typeof parsed.error === 'string' ? parsed.error : null;
      const message =
        isRecord(parsed) && typeof parsed.message === 'string' ? parsed.message : null;
      throw new ToolsError(response.status, code, message);
    }
    if (!isRecord(parsed) || parsed.api_version !== API_VERSION) {
      throw new ShapeError('api_version', `${API_VERSION}`);
    }
    return decode(parsed, '');
  }
}
