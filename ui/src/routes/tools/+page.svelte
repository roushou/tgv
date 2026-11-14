<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import SignInSheet from '#lib/SignInSheet.svelte';
  import { SignIns } from '#lib/signIn.ts';
  import { Runs, RunsClient, type Drivable, type RunClient } from '#lib/runs.ts';
  import { getWorkbench } from '#lib/workbench.svelte.ts';
  import Icon from '#lib/Icon.svelte';
  import { harnessLabel } from '#lib/activity.ts';
  import {
    Platforms,
    RowActions,
    Supplies,
    ToolFilters,
    ToolGroups,
    type Only,
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
  let filter = $state(ToolFilters.ALL);
  const groups = $derived(listing ? ToolGroups.of(listing, filter) : []);
  const narrow = (only: Only) => (filter = { ...filter, only: filter.only === only ? null : only });
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
  const effects = $derived(listing ? plan.effects(listing) : []);
  const staged = $derived(new Map(effects.map((effect) => [effect.id, effect])));
  const short = (sha: string) => `${sha.slice(0, 12)}…`;

  // Each client's sign-in on this host, read from the run API when it is on.
  const runs = new RunsClient(fetch);
  let clients = $state.raw<RunClient[]>([]);
  let signingIn = $state<Drivable | null>(null);
  let checking = $state<Drivable | null>(null);
  async function readClients() {
    try {
      clients = await runs.clients();
    } catch {
      // Runs are off or unreachable: no sign-in is shown.
    }
  }
  onMount(() => void readClients());
  /** The client a package is, with its sign-in; null for other packages. */
  const clientOf = (pkg: ToolPackage) => SignIns.clientOf(pkg, clients);
  const signedOut = $derived(
    listing?.packages.filter((pkg) => {
      const entry = clientOf(pkg);
      return entry !== null && SignIns.signedOut(entry.sign_in);
    }) ?? [],
  );
  async function check(harness: Drivable) {
    checking = harness;
    try {
      await runs.checkSignIn(harness);
    } catch {
      // Shown as it was; the next read says.
    } finally {
      checking = null;
    }
    await readClients();
  }
  const stateLabel = (state: GenerationView['state']) =>
    state === 'current' ? 'current' : state === 'in_use' ? 'in use' : state === 'idle' ? '' : state;
</script>

{#snippet scope()}
  {#if listing && tally}
    <div class="scope-group tally" aria-label="Tools">
      <label class="search-input search">
        <Icon name="search" size={12} />
        <input
          type="search"
          placeholder="Filter"
          aria-label="Filter tools"
          value={filter.query}
          oninput={(event) => (filter = { ...filter, query: event.currentTarget.value })}
        />
      </label>
      {#snippet narrowing(only: Only, count: number, word: string, tone: string)}
        <button
          class="chip filter {tone}"
          class:active={filter.only === only}
          aria-pressed={filter.only === only}
          onclick={() => narrow(only)}
          ><span class="dot {only === 'updates' ? 'managed' : only}"></span>{count} {word}</button
        >
      {/snippet}
      {@render narrowing('managed', tally.managed, 'managed', 'success')}
      {@render narrowing('system', tally.system, 'system', '')}
      {#if tally.missing > 0}{@render narrowing('missing', tally.missing, 'missing', 'error')}{/if}
      {#if tally.updates > 0}{@render narrowing(
          'updates',
          tally.updates,
          tally.updates === 1 ? 'update' : 'updates',
          'live',
        )}{/if}
      {#if signedOut.length > 0}
        <button
          class="chip filter warning"
          title="Show the first client that is signed out"
          onclick={() => (selected = signedOut[0]?.id ?? selected)}
          ><Icon name="lock" size={10} />{signedOut.length} signed out</button
        >
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
        {#each groups as group (group.kind)}
          <tbody>
            <tr class="tools-group">
              <th colspan="4" scope="rowgroup"
                >{group.title} <span class="count">{group.packages.length}</span></th
              >
            </tr>
            {#each group.packages as pkg (pkg.id)}
              {@const label = action(pkg)}
              {@const supply = Supplies.of(pkg)}
              {@const change = staged.get(pkg.id)}
              <tr
                class="tools-row"
                class:selected={selected === pkg.id}
                class:staged={change !== undefined}
                onclick={() => (selected = pkg.id)}
              >
                <td class="mono name"
                  >{@render dot(pkg)}{pkg.id}{#if pkg.origin === 'operator'}<span
                      class="chip tag"
                      title="Added in the daemon’s configuration">custom</span
                    >{:else if pkg.origin === null}<span
                      class="chip warning tag"
                      title="No longer in the daemon’s configuration">removed</span
                    >{/if}</td
                >
                <td class="mono"
                  >{#if change}<s class="muted">{change.from ?? '—'}</s><span class="arrow">→</span
                    ><span class="to {change.toKind}">{change.to ?? '—'}</span
                    >{:else if supply.kind === 'managed'}{supply.version}{:else if supply.kind === 'system'}{#if supply.system.version}{supply
                        .system.version}{:else}<span
                        class="unknown"
                        title={supply.system.unread ?? ''}>?</span
                      >{/if}{:else}<span class="muted">—</span>{/if}</td
                >
                <td class="source"
                  >{#if supply.kind === 'managed'}<span class="chip success">Ren</span
                    >{#if supply.shadows}<span
                        class="mono muted path shadowed"
                        title="Runs find Ren’s first">{supply.shadows.path}</span
                      >{/if}{:else if supply.kind === 'system'}<span class="chip">system</span><span
                      class="mono muted path">{supply.system.path}</span
                    >{:else if pkg.needed_by.length > 0}<span class="chip error"
                      >missing · {pkg.needed_by.map(harnessLabel).join(', ')}</span
                    >{/if}{#if clientOf(pkg)}{@const shown = SignIns.chip(
                      clientOf(pkg)?.sign_in ?? { kind: 'not_checked' },
                    )}{#if shown}<span
                        class="chip sign-in {shown.tone}"
                        title="Sign-in on this host, as the client says"
                        >{#if shown.tone === 'warning'}<Icon
                            name="lock"
                            size={10}
                          />{/if}{shown.text}</span
                      >{/if}{/if}</td
                >
                <td class="end">
                  {#if label && listing.platform}
                    <button
                      class="button small"
                      class:chosen={change !== undefined}
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
        {:else}
          <tbody>
            <tr><td colspan="4" class="muted">No tool matches.</td></tr>
          </tbody>
        {/each}
      </table>
    </section>
    <aside class="tools-inspector" aria-label="Details">
      {#if !plan.empty || applying.kind !== 'idle'}
        <section class="changes" aria-live="polite">
          <h3>
            Changes
            {#if applying.kind === 'done' || applying.kind === 'failed'}
              <button
                class="icon-button"
                aria-label="Dismiss"
                onclick={() => (applying = { kind: 'idle' })}
                ><Icon name="close" size={12} /></button
              >
            {/if}
          </h3>
          {#if applying.kind === 'applying'}
            <p class="muted">Installing…</p>
          {:else if !plan.empty}
            <ul class="change-list">
              {#each effects as change (change.id)}
                <li class={change.change}>
                  <span class="sign"
                    >{change.change === 'add' ? '+' : change.change === 'remove' ? '−' : '↑'}</span
                  >
                  <span class="mono">{change.id}</span>
                  <span class="mono muted">{change.from ?? '—'}</span>
                  <span class="mono muted">→</span>
                  <span class="mono to {change.toKind}">{change.to ?? '—'}</span>
                </li>
              {/each}
            </ul>
            <div class="change-actions">
              <button class="button" onclick={() => (plan = ToolPlan.EMPTY)}>Clear</button>
              <button class="button primary" onclick={apply}
                >Apply {changes.length} change{changes.length === 1 ? '' : 's'}</button
              >
            </div>
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
        </section>
      {/if}
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
        {@const entry = clientOf(chosen)}
        {@const harness = entry ? Runs.drivable(entry.harness) : null}
        {#if entry && harness && (entry.sign_in.kind !== 'not_checked' || entry.sign_in_methods.length > 0)}
          {@const shown = SignIns.chip(entry.sign_in)}
          <section class="sign-in" class:out={SignIns.signedOut(entry.sign_in)}>
            <h3>
              Sign-in
              {#if shown}<span class="chip {shown.tone}">{shown.text}</span>{/if}
            </h3>
            <p class="muted">
              {#if entry.sign_in.kind === 'signed_out'}
                {harnessLabel(harness)} is signed out here: its sessions can’t send until it is signed
                in.
              {:else if entry.sign_in.kind === 'unknown'}
                {entry.sign_in.reason}
              {:else if entry.sign_in.kind === 'signed_in'}
                As {harnessLabel(harness)}’s own status command says.
              {:else}
                Not read yet.
              {/if}
            </p>
            <div class="change-actions">
              <button
                class="button small"
                disabled={checking === harness}
                onclick={() => void check(harness)}
                >{checking === harness ? 'Checking…' : 'Check again'}</button
              >
              {#if entry.sign_in_methods.length > 0}
                <button
                  class="button small"
                  class:primary={entry.sign_in.kind !== 'signed_in'}
                  onclick={() => (signingIn = harness)}
                  >{entry.sign_in.kind === 'signed_in' ? 'Sign in again' : 'Sign in'}</button
                >
              {/if}
            </div>
          </section>
        {/if}
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

{#if signingIn}
  {@const harness = signingIn}
  <SignInSheet
    {harness}
    client={harnessLabel(harness)}
    methods={clients.find((entry) => entry.harness === harness)?.sign_in_methods ?? []}
    start={(method) => runs.startSignIn(harness, method)}
    answer={(answer) => runs.signInAnswer(harness, answer)}
    wait={() => runs.waitSignIn(harness)}
    cancel={() => runs.cancelSignIn(harness)}
    onclose={() => {
      signingIn = null;
      void readClients();
    }}
  />
{/if}

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
  .tools-main {
    overflow: auto;
  }
  .tools-table {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
    font-size: 12px;
  }
  .c-name {
    width: 220px;
  }
  .c-version {
    width: 160px;
  }
  .c-action {
    width: 128px;
  }
  td,
  th {
    text-align: left;
    padding: 0 12px;
    height: 30px;
    vertical-align: middle;
    border-bottom: 1px solid var(--line);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .tools-group th {
    height: 24px;
    font-size: 11px;
    font-weight: 600;
    color: var(--muted);
    background: var(--surface-subtle);
  }
  .tools-group .count {
    font-weight: 400;
    margin-left: 4px;
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
    padding: 0 8px;
  }
  td.end .button {
    vertical-align: middle;
  }
  .name {
    font-weight: 600;
  }
  .arrow {
    margin: 0 4px;
    color: var(--muted);
  }
  .to.managed {
    color: var(--success);
  }
  .to.none {
    color: var(--error);
  }
  td > .chip {
    margin: 0;
    padding: 0 6px;
    font-size: 11px;
    line-height: 16px;
    vertical-align: 1px;
  }
  td > .tag {
    margin-left: 8px;
    font-weight: 400;
    font-family: inherit;
  }
  .source .path {
    margin-left: 8px;
  }
  .search {
    width: 160px;
    height: 24px;
    border: 1px solid var(--control-border);
    border-radius: 2px;
  }
  .chip.filter {
    cursor: pointer;
    font: inherit;
  }
  .chip.filter.active {
    box-shadow: inset 0 0 0 1px currentColor;
  }
  .path {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 11px;
  }
  .shadowed {
    text-decoration: line-through;
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
  .changes h3 {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .change-list {
    list-style: none;
    margin: 0 0 8px;
    padding: 0;
  }
  .change-list li {
    display: grid;
    grid-template-columns: 14px minmax(0, 1fr) auto auto auto;
    gap: 8px;
    padding: 3px 0;
  }
  .change-list .sign {
    font-weight: 700;
  }
  .change-list .add .sign {
    color: var(--success);
  }
  .change-list .replace .sign {
    color: var(--accent);
  }
  .change-list .remove .sign {
    color: var(--error);
  }
  .change-actions {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
  }
  .failed {
    color: var(--error);
  }
  .tools-inspector {
    min-height: 0;
    overflow: auto;
    font-size: 12px;
  }
  .sign-in p {
    margin: 4px 0 8px;
    line-height: 1.5;
  }
  .sign-in.out {
    background: var(--warning-soft);
  }
  .chip.sign-in {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    margin-left: 6px;
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
