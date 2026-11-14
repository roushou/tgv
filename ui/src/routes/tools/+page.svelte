<script lang="ts">
  import { untrack } from 'svelte';
  import { getWorkbench } from '#lib/workbench.svelte.ts';
  import Icon from '#lib/Icon.svelte';
  import { harnessLabel } from '#lib/activity.ts';
  import {
    Standings,
    ToolGroups,
    ToolPlan,
    ToolsApi,
    ToolsError,
    type Generation,
    type PackageId,
    type ToolChange,
    type ToolListing,
    type ToolPackage,
  } from '#lib/tools.ts';

  import type { PageProps } from './$types';

  let { data }: PageProps = $props();
  getWorkbench().showScope(scope);
  const api = new ToolsApi(fetch);

  /** What the page shows: the daemon's listing, or why there is none. */
  type Loaded = { kind: 'ready'; listing: ToolListing } | { kind: 'failed'; message: string };
  /** What the plan bar is doing. */
  type Applying =
    | { kind: 'idle' }
    | { kind: 'applying' }
    | { kind: 'done'; change: ToolChange }
    | { kind: 'failed'; message: string };

  // The server's first read; later reads come from this page.
  let loaded = $state<Loaded>(untrack(() => data.initial));
  let plan = $state(ToolPlan.EMPTY);
  let applying = $state<Applying>({ kind: 'idle' });
  let selected = $state<PackageId | null>(
    untrack(() =>
      data.initial.kind === 'ready'
        ? (data.initial.listing.packages.find((p) => p.release !== null)?.id ?? null)
        : null,
    ),
  );
  let collecting = $state<string | null>(null);

  const listing = $derived(loaded.kind === 'ready' ? loaded.listing : null);
  const groups = $derived(listing ? ToolGroups.of(listing) : []);
  const changes = $derived(listing ? plan.changes(listing) : []);
  const chosen = $derived(listing?.packages.find((p) => p.id === selected) ?? null);
  const unsupported = $derived(listing !== null && listing.platform === null);

  const message = (error: unknown) =>
    error instanceof ToolsError || error instanceof Error ? error.message : String(error);

  async function load() {
    try {
      const read = await api.listing();
      loaded = { kind: 'ready', listing: read };
      plan = plan.within(read);
      if (selected === null) selected = read.packages.find((p) => p.release !== null)?.id ?? null;
    } catch (error) {
      loaded = { kind: 'failed', message: message(error) };
    }
  }

  async function apply() {
    if (plan.empty || applying.kind === 'applying') return;
    applying = { kind: 'applying' };
    try {
      const change = await api.change(plan);
      applying = { kind: 'done', change };
      if (change.outcome !== 'failed') plan = ToolPlan.EMPTY;
    } catch (error) {
      applying = { kind: 'failed', message: message(error) };
    }
    await load();
  }

  /** Make the next generation hold what `generation` held. */
  async function rollBack(generation: Generation) {
    if (applying.kind === 'applying') return;
    applying = { kind: 'applying' };
    try {
      applying = { kind: 'done', change: await api.rollBack(generation) };
      plan = ToolPlan.EMPTY;
    } catch (error) {
      applying = { kind: 'failed', message: message(error) };
    }
    await load();
  }

  async function collect() {
    try {
      const collected = await api.collect();
      const removed = collected.generations.length + collected.installs.length;
      collecting =
        removed === 0
          ? 'Nothing to remove.'
          : `Removed ${collected.generations.length} generation${collected.generations.length === 1 ? '' : 's'} and ${collected.installs.length} install${collected.installs.length === 1 ? '' : 's'}.`;
    } catch (error) {
      collecting = message(error);
    }
    await load();
  }

  /** The action a row's button takes, in words. */
  function action(pkg: ToolPackage): string | null {
    const standing = Standings.of(pkg);
    switch (standing.kind) {
      case 'available':
        return plan.add.has(pkg.id) ? 'Undo' : 'Install';
      case 'outdated':
        return plan.add.has(pkg.id) ? 'Undo' : 'Update';
      case 'installed':
        return plan.remove.has(pkg.id) ? 'Undo' : 'Remove';
      case 'system':
      case 'missing':
        return null;
    }
  }

  const tone = (pkg: ToolPackage) => Standings.of(pkg).kind;
  const short = (sha: string) => `${sha.slice(0, 12)}…`;
</script>

{#snippet scope()}
  <div class="scope-group">
    {#if listing}
      <span class="muted"
        >Generation <span class="mono">{listing.generation}</span>{listing.platform
          ? ` · ${listing.platform}`
          : ''}</span
      >
    {:else}
      <span class="muted">The daemon’s host</span>
    {/if}
  </div>
{/snippet}

<div class="tools-view">
  {#if loaded.kind === 'failed'}
    <div class="tools-empty">
      <Icon name="alert" size={18} />
      <p>{loaded.message}</p>
      <button class="button" onclick={load}><Icon name="refresh" size={12} />Try again</button>
    </div>
  {:else if listing}
    <section class="tools-main" aria-label="Packages">
      {#if unsupported}
        <p class="tools-note">
          Ren installs tools only on Linux x64, Linux arm64, and macOS arm64. This host’s packages
          are listed as found on its PATH.
        </p>
      {/if}
      <table class="tools-table">
        <colgroup>
          <col class="c-name" />
          <col class="c-standing" />
          <col class="c-version" />
          <col class="c-version" />
          <col />
          <col class="c-action" />
        </colgroup>
        <thead>
          <tr>
            <th>Package</th>
            <th>Standing</th>
            <th>Ren installs</th>
            <th>Installed</th>
            <th>On the daemon’s PATH</th>
            <th><span class="sr-only">Change</span></th>
          </tr>
        </thead>
        {#each groups as group (group.kind)}
          <tbody>
            <tr class="tools-group">
              <th colspan="6" scope="rowgroup">{group.title}</th>
            </tr>
            {#each group.packages as pkg (pkg.id)}
              {@const label = action(pkg)}
              <tr
                class="tools-row"
                class:selected={selected === pkg.id}
                class:staged={plan.staged(pkg)}
                onclick={() => (selected = pkg.id)}
              >
                <td class="mono">
                  <span class="dot {tone(pkg)}" aria-hidden="true"></span>{pkg.id}
                </td>
                <td>{Standings.word(Standings.of(pkg))}</td>
                <td class="mono">{pkg.release ?? '—'}</td>
                <td class="mono">{pkg.installed?.version ?? '—'}</td>
                <td class="mono path">
                  {#if pkg.system}
                    <span title={pkg.system.unread ?? pkg.system.path}
                      >{#if pkg.system.version}{pkg.system.version}{:else}<span class="unread"
                          >version not read</span
                        >{/if}<span class="where muted">{pkg.system.path}</span></span
                    >
                  {:else}
                    <span class="muted">not found</span>
                  {/if}
                </td>
                <td class="end">
                  {#if label && !unsupported}
                    <button
                      class="button small"
                      class:chosen={plan.staged(pkg)}
                      onclick={(event) => {
                        event.stopPropagation();
                        plan = plan.toggle(pkg);
                      }}>{label}</button
                    >
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        {/each}
      </table>
      <footer class="tools-plan" aria-live="polite">
        {#if applying.kind === 'applying'}
          <span>Downloading, verifying, and installing…</span>
        {:else if !plan.empty}
          <span class="muted">Plan</span>
          {#each changes as change (change.id)}
            <span class="mono change {change.change}"
              >{change.change === 'add' ? '+' : change.change === 'remove' ? '−' : '↑'}
              {change.id}
              {change.change === 'replace'
                ? `${change.from} → ${change.to}`
                : (change.to ?? change.from)}</span
            >
          {/each}
        {:else if applying.kind === 'done'}
          {#if applying.change.outcome === 'failed'}
            <span class="failed"
              >Nothing changed: {#each applying.change.failures as failure (failure.package)}<span
                  class="mono">{failure.package} {failure.stage}: {failure.reason}</span
                >{/each}</span
            >
          {:else if applying.change.outcome === 'activated'}
            <span
              >Generation <span class="mono">{applying.change.generation}</span> is current.</span
            >
          {:else}
            <span class="muted">Nothing to change.</span>
          {/if}
        {:else if applying.kind === 'failed'}
          <span class="failed">{applying.message}</span>
        {:else}
          <span class="muted"
            >Stage installs, updates, and removals, then apply them as one generation.</span
          >
        {/if}
        <span class="spacer"></span>
        {#if !plan.empty && applying.kind !== 'applying'}
          <button class="button" onclick={() => (plan = ToolPlan.EMPTY)}>Clear</button>
        {/if}
        <button
          class="button primary"
          disabled={plan.empty || applying.kind === 'applying'}
          onclick={apply}>Apply{plan.empty ? '' : ` → generation ${listing.generation + 1}`}</button
        >
      </footer>
    </section>
    <aside class="tools-inspector" aria-label="Details">
      {#if chosen}
        <section>
          <h2 class="mono">{chosen.id}</h2>
          <p class="muted">{Standings.word(Standings.of(chosen))}</p>
        </section>
        <section>
          <h3>Ren’s release</h3>
          {#if chosen.release}
            <dl>
              <dt>Version</dt>
              <dd class="mono">{chosen.release}</dd>
              {#if chosen.installed}
                <dt>Installed</dt>
                <dd class="mono">{chosen.installed.version}</dd>
                <dt>SHA-256</dt>
                <dd class="mono" title={chosen.installed.sha256}>
                  {short(chosen.installed.sha256)}
                </dd>
                <dt>Folder</dt>
                <dd class="mono">{chosen.installed.dir}</dd>
              {/if}
            </dl>
          {:else}
            <p class="muted">Ren does not install this; it uses what the daemon’s PATH has.</p>
          {/if}
        </section>
        <section>
          <h3>On the daemon’s PATH</h3>
          {#if chosen.system}
            <dl>
              <dt>Path</dt>
              <dd class="mono">{chosen.system.path}</dd>
              <dt>Version</dt>
              <dd class="mono">
                {#if chosen.system.version}{chosen.system.version}{:else}<span class="unread"
                    >not read</span
                  >{#if chosen.system.unread}<span class="muted">
                      · {chosen.system.unread}</span
                    >{/if}{/if}
              </dd>
            </dl>
          {:else}
            <p class="muted">Not found.</p>
          {/if}
        </section>
        <section>
          <h3>Needed by</h3>
          {#if chosen.needed_by.length > 0}
            <p>
              {chosen.needed_by.map(harnessLabel).join(', ')}
            </p>
          {:else}
            <p class="muted">No client needs it; agents may use it.</p>
          {/if}
        </section>
      {/if}
      <section>
        <h3>Generations</h3>
        {#if listing.generations.length === 0}
          <p class="muted">
            None yet. Each change makes the next one; runs keep the one they started with.
          </p>
        {:else}
          <ul class="generations">
            {#each listing.generations as generation (generation.generation)}
              <li class={generation.state}>
                <span class="mono">{generation.generation}</span>
                <span class="state"
                  >{generation.state === 'current'
                    ? 'current'
                    : generation.state === 'in_use'
                      ? 'used by a run'
                      : 'unused'}</span
                >
                <span class="mono muted installs"
                  >{generation.installs.map((i) => `${i.package} ${i.version}`).join(', ') ||
                    'empty'}</span
                >
                <span class="end">
                  {#if generation.state !== 'current' && !unsupported}
                    <button
                      class="button small"
                      title="Make a new generation that holds what generation {generation.generation} held"
                      disabled={applying.kind === 'applying'}
                      onclick={() => rollBack(generation.generation)}>Restore</button
                    >
                  {/if}
                </span>
              </li>
            {/each}
          </ul>
          <div class="collect">
            <button class="button small" onclick={collect}>Remove unused</button>
            {#if collecting}<span class="muted">{collecting}</span>{/if}
          </div>
        {/if}
      </section>
    </aside>
  {/if}
</div>

<style>
  .tools-view {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 320px;
    height: 100%;
    min-height: 0;
    background: var(--surface);
  }
  .tools-empty {
    grid-column: 1 / -1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    padding: 48px 16px;
  }
  .tools-main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    border-right: 1px solid var(--line);
  }
  .tools-note {
    margin: 0;
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
    background: var(--warning-soft);
    color: var(--warning);
  }
  .tools-table {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
    font-size: 12px;
  }
  .c-name {
    width: 132px;
  }
  .c-standing {
    width: 104px;
  }
  .c-version {
    width: 96px;
  }
  .c-action {
    width: 84px;
  }
  th,
  td {
    text-align: left;
    padding: 7px 12px;
    border-bottom: 1px solid var(--line);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  thead th {
    font-weight: 500;
    color: var(--muted);
    background: var(--surface-subtle);
  }
  .tools-group th {
    font-weight: 600;
    font-size: 11px;
    color: var(--muted);
    background: var(--surface-subtle);
    padding: 5px 12px;
  }
  .tools-row {
    cursor: pointer;
  }
  .tools-row:hover {
    background: var(--row-hover);
  }
  .tools-row.selected {
    background: var(--row-selected);
  }
  .tools-row.staged td:first-child {
    box-shadow: inset 2px 0 0 var(--accent);
  }
  td.end {
    text-align: right;
    padding: 3px 12px;
  }
  .path .where {
    margin-left: 8px;
  }
  .path .unread,
  dd .unread {
    color: var(--warning);
  }
  .dot {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    margin-right: 8px;
    vertical-align: 1px;
    background: var(--muted);
  }
  .dot.installed {
    background: var(--success);
  }
  .dot.outdated {
    background: var(--live);
  }
  .dot.available {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--muted);
  }
  .dot.system {
    background: var(--muted);
  }
  .dot.missing {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--error);
  }
  .tools-plan {
    margin-top: auto;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 10px;
    padding: 8px 12px;
    border-top: 1px solid var(--line);
    background: var(--surface-subtle);
    position: sticky;
    bottom: 0;
  }
  .spacer {
    flex: 1;
  }
  .change.add {
    color: var(--success);
  }
  .change.replace {
    color: var(--live);
  }
  .change.remove {
    color: var(--error);
  }
  .failed {
    color: var(--error);
    display: inline-flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .tools-inspector {
    min-height: 0;
    overflow: auto;
    font-size: 12px;
  }
  .tools-inspector section {
    padding: 10px 12px;
    border-bottom: 1px solid var(--line);
  }
  .tools-inspector h2 {
    margin: 0;
    font-size: 14px;
  }
  .tools-inspector h3 {
    margin: 0 0 6px;
    font-size: 11px;
    font-weight: 600;
    color: var(--muted);
  }
  .tools-inspector p {
    margin: 0;
  }
  dl {
    display: grid;
    grid-template-columns: 72px minmax(0, 1fr);
    gap: 4px 8px;
    margin: 0;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .generations {
    list-style: none;
    margin: 0;
    padding: 0;
    border-top: 1px solid var(--line);
  }
  .generations li {
    display: grid;
    grid-template-columns: 28px 88px minmax(0, 1fr) auto;
    align-items: center;
    gap: 6px;
    padding: 4px 0;
    min-height: 33px;
    border-bottom: 1px solid var(--line);
  }
  .generations li.current .state {
    color: var(--success);
  }
  .generations li.in_use .state {
    color: var(--live);
  }
  .generations .installs {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .collect {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 8px;
  }
  @media (max-width: 900px) {
    .tools-view {
      grid-template-columns: minmax(0, 1fr);
    }
    .tools-main {
      border-right: 0;
      border-bottom: 1px solid var(--line);
    }
  }
</style>
