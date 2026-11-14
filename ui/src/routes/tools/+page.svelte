<script lang="ts">
  import { untrack } from 'svelte';
  import { getWorkbench } from '#lib/workbench.svelte.ts';
  import Icon from '#lib/Icon.svelte';
  import { harnessLabel } from '#lib/activity.ts';
  import {
    Platforms,
    RowActions,
    Supplies,
    ToolGroups,
    ToolPlan,
    ToolsApi,
    ToolsError,
    type Generation,
    type GenerationView,
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
      collecting = `${collected.generations.length + collected.installs.length} freed`;
    } catch (error) {
      collecting = message(error);
    }
    await load();
  }

  /** A row's button label, or `null` for a package Ren does not install. */
  function action(pkg: ToolPackage): string | null {
    const row = RowActions.of(pkg);
    if (row === null) return null;
    if (plan.staged(pkg)) return 'Undo';
    switch (row.kind) {
      case 'install':
        return `Install ${row.version}`;
      case 'update':
        return `Update ${row.version}`;
      case 'remove':
        return 'Remove';
    }
  }

  const tally = $derived(listing ? Supplies.tally(listing) : null);
  const short = (sha: string) => `${sha.slice(0, 12)}…`;
  const stateLabel = (state: GenerationView['state']) =>
    state === 'current' ? 'current' : state === 'in_use' ? 'in use' : state === 'idle' ? '' : state;
</script>

{#snippet scope()}
  {#if listing && tally}
    <div class="scope-group tally" aria-label="Tools">
      <span class="chip success" title="Installed by Ren, first on runs’ PATH"
        ><span class="dot managed"></span>{tally.managed} managed</span
      >
      <span class="chip" title="Found on the daemon’s PATH"
        ><span class="dot system"></span>{tally.system} system</span
      >
      {#if tally.missing > 0}
        <span class="chip error" title="Needed by a client, found nowhere"
          ><span class="dot missing"></span>{tally.missing} missing</span
        >
      {/if}
      {#if tally.updates > 0}
        <span class="chip live">{tally.updates} update{tally.updates === 1 ? '' : 's'}</span>
      {/if}
      {#if listing.platform}
        <span class="chip mono">{Platforms.label(listing.platform)}</span>
      {:else}
        <span class="chip warning" title="Ren installs on Linux x64, Linux arm64, and macOS arm64"
          >no installs on this platform</span
        >
      {/if}
    </div>
  {/if}
{/snippet}

{#snippet dot(pkg: ToolPackage)}
  <span class="dot {Supplies.tone(pkg)}" aria-hidden="true"></span>
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
      <table class="tools-table">
        <colgroup>
          <col class="c-name" />
          <col class="c-version" />
          <col />
          <col class="c-action" />
        </colgroup>
        <thead>
          <tr>
            <th>Tool</th>
            <th>Version</th>
            <th>Source</th>
            <th><span class="sr-only">Change</span></th>
          </tr>
        </thead>
        {#each groups as group (group.kind)}
          <tbody>
            <tr class="tools-group">
              <th colspan="4" scope="rowgroup">{group.title}</th>
            </tr>
            {#each group.packages as pkg (pkg.id)}
              {@const label = action(pkg)}
              {@const supply = Supplies.of(pkg)}
              <tr
                class="tools-row"
                class:selected={selected === pkg.id}
                class:staged={plan.staged(pkg)}
                onclick={() => (selected = pkg.id)}
              >
                <td class="mono"
                  >{@render dot(pkg)}{pkg.id}{#if pkg.origin === 'operator'}<span
                      class="chip tag"
                      title="Added in the daemon’s configuration">custom</span
                    >{:else if pkg.origin === null}<span
                      class="chip warning tag"
                      title="No longer in the daemon’s configuration">removed</span
                    >{/if}</td
                >
                {#if supply.kind === 'managed'}
                  <td class="mono">{supply.version}</td>
                  <td
                    ><span class="chip success">Ren</span>{#if supply.shadows}<span
                        class="mono muted shadowed"
                        title="Runs find Ren’s first">{supply.shadows.path}</span
                      >{/if}</td
                  >
                {:else if supply.kind === 'system'}
                  <td class="mono"
                    >{#if supply.system.version}{supply.system.version}{:else}<span
                        class="unknown"
                        title={supply.system.unread ?? ''}>?</span
                      >{/if}</td
                  >
                  <td
                    ><span class="chip">system</span><span class="mono muted where"
                      >{supply.system.path}</span
                    ></td
                  >
                {:else}
                  <td class="mono muted">—</td>
                  <td
                    >{#if pkg.needed_by.length > 0}<span class="chip error"
                        >missing · {pkg.needed_by.map(harnessLabel).join(', ')}</span
                      >{/if}</td
                  >
                {/if}
                <td class="end">
                  {#if label && listing.platform}
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
      {#if !plan.empty || applying.kind !== 'idle'}
        <footer class="tools-plan" aria-live="polite">
          {#if applying.kind === 'applying'}
            <span class="muted">Installing…</span>
          {:else if !plan.empty}
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
              {#each applying.change.failures as failure (failure.package)}
                <span class="chip error mono" title={failure.reason}
                  >{failure.package} · {failure.stage} failed</span
                >
              {/each}
            {:else if applying.change.outcome === 'activated'}
              <span class="chip success">applied</span>
            {:else}
              <span class="chip">no change</span>
            {/if}
          {:else if applying.kind === 'failed'}
            <span class="chip error">{applying.message}</span>
          {/if}
          <span class="spacer"></span>
          {#if !plan.empty && applying.kind !== 'applying'}
            <button class="button" onclick={() => (plan = ToolPlan.EMPTY)}>Clear</button>
            <button class="button primary" onclick={apply}>Apply</button>
          {:else if applying.kind === 'done' || applying.kind === 'failed'}
            <button
              class="icon-button"
              aria-label="Dismiss"
              onclick={() => (applying = { kind: 'idle' })}><Icon name="close" size={12} /></button
            >
          {/if}
        </footer>
      {/if}
    </section>
    <aside class="tools-inspector" aria-label="Details">
      {#if chosen}
        {@const supply = Supplies.of(chosen)}
        <section class="head">
          <h2 class="mono">{@render dot(chosen)}{chosen.id}</h2>
          <span class="chip-row">
            {#if supply.kind === 'managed'}<span class="chip success">Ren</span
              >{:else if supply.kind === 'system'}<span class="chip">system</span
              >{:else if chosen.needed_by.length > 0}<span class="chip error">missing</span
              >{:else}<span class="chip">not installed</span>{/if}
            {#if chosen.kind}<span class="chip">{chosen.kind}</span>{/if}
            {#if chosen.origin === 'operator'}<span class="chip">custom</span
              >{:else if chosen.origin === null}<span class="chip warning">removed</span>{/if}
          </span>
        </section>
        <section>
          <dl>
            <dt>Runs use</dt>
            <dd class="mono">
              {#if supply.kind === 'managed'}{supply.version}{:else if supply.kind === 'system'}{supply
                  .system.version ?? '?'}{:else}—{/if}
            </dd>
            {#if chosen.release}
              <dt>Pinned</dt>
              <dd class="mono">{chosen.release}</dd>
            {/if}
            {#if chosen.installed}
              <dt>SHA-256</dt>
              <dd class="mono" title={chosen.installed.sha256}>{short(chosen.installed.sha256)}</dd>
              <dt>Install</dt>
              <dd class="mono">{chosen.installed.dir}</dd>
            {/if}
            {#if chosen.system}
              <dt>PATH</dt>
              <dd class="mono" class:shadowed={chosen.installed !== null}>
                {chosen.system.path}
                <span class="muted">{chosen.system.version ?? chosen.system.unread ?? ''}</span>
              </dd>
            {/if}
            {#if chosen.needed_by.length > 0}
              <dt>Needed by</dt>
              <dd>{chosen.needed_by.map(harnessLabel).join(', ')}</dd>
            {/if}
          </dl>
        </section>
      {/if}
      {#if listing.generations.length > 0}
        <section>
          <h3>History</h3>
          <ul class="generations">
            {#each listing.generations as generation (generation.generation)}
              <li class={generation.state}>
                <span class="mono muted">#{generation.generation}</span>
                <span class="mono installs"
                  >{generation.installs.map((i) => `${i.package} ${i.version}`).join(', ') ||
                    '—'}</span
                >
                <span class="end">
                  {#if generation.state === 'idle' || generation.state === 'in_use'}
                    {#if generation.state === 'in_use'}<span class="chip live">in use</span>{/if}
                    {#if listing.platform}
                      <button
                        class="button small"
                        disabled={applying.kind === 'applying'}
                        onclick={() => rollBack(generation.generation)}>Restore</button
                      >
                    {/if}
                  {:else}
                    <span class="chip success">{stateLabel(generation.state)}</span>
                  {/if}
                </span>
              </li>
            {/each}
          </ul>
          <div class="collect">
            <button class="button small" onclick={collect}>Clean up</button>
            {#if collecting}<span class="muted">{collecting}</span>{/if}
          </div>
        </section>
      {/if}
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
  .tally {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .tally .chip .dot {
    margin-right: 2px;
  }
  .tools-table {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
    font-size: 12px;
  }
  .c-name {
    width: 196px;
  }
  .c-version {
    width: 104px;
  }
  .c-action {
    width: 140px;
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
  .where,
  .shadowed {
    margin-left: 8px;
  }
  .shadowed {
    text-decoration: line-through;
  }
  .tools-table td > .chip {
    margin: 0;
    padding: 0 6px;
    font-size: 11px;
    line-height: 16px;
    vertical-align: 1px;
  }
  .tools-table td > .tag {
    margin-left: 8px;
    font-family: inherit;
  }
  .unknown {
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
  .dot.managed {
    background: var(--success);
  }
  .dot.system {
    background: var(--muted);
  }
  .dot.none {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--muted);
  }
  .dot.missing {
    background: transparent;
    box-shadow: inset 0 0 0 1.5px var(--error);
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
  .head .chip-row {
    margin: 6px 0 0;
  }
  .tools-inspector h3 {
    margin: 0 0 6px;
    font-size: 11px;
    font-weight: 600;
    color: var(--muted);
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
    grid-template-columns: 28px minmax(0, 1fr) auto;
    align-items: center;
    gap: 6px;
    padding: 4px 0;
    min-height: 33px;
    border-bottom: 1px solid var(--line);
  }
  .generations .end {
    display: inline-flex;
    align-items: center;
    gap: 6px;
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
