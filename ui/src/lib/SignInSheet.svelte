<!--
  A client's sign-in, run on the daemon's host by the client's own login:
  the daemon starts it and this shows the link it printed. Then either the
  code the page shows goes back to it (Claude Code), or this shows the
  one-time code to enter on the page and waits until the operator approves
  there and the login ends by itself (Codex). The client keeps and refreshes what it
  receives; Ren never sees it. The sheet says so, and names the client, not
  a provider, so it never passes for a provider's own page.
-->
<script lang="ts">
  import { untrack } from 'svelte';
  import CopyButton from './CopyButton.svelte';
  import Icon from './Icon.svelte';
  import { RunCommandError, type Drivable } from './runs.ts';
  import {
    SignIns,
    type Login,
    type LoginOutcome,
    type SignInMethod,
    type Waited,
  } from './signIn.ts';
  import type { Stored } from './decode.ts';

  let {
    harness,
    client,
    methods,
    start,
    code: sendCode,
    wait,
    cancel,
    onclose,
  }: {
    harness: Drivable;
    /** The client's name, as the view shows it. */
    client: string;
    methods: readonly Stored<SignInMethod>[];
    start: (method: SignInMethod) => Promise<Login>;
    code: (code: string) => Promise<LoginOutcome>;
    /** Wait a while for the login to end and be checked. */
    wait: () => Promise<Waited>;
    cancel: () => Promise<void>;
    /** The sheet closed: with the outcome when a login ended. */
    onclose: (outcome: LoginOutcome | null) => void;
  } = $props();

  /** Where the sheet stands. */
  type Step =
    | { kind: 'choosing' }
    | { kind: 'starting' }
    | { kind: 'awaiting'; link: string; said: string | null }
    | { kind: 'sending'; link: string }
    /** A device login: the operator enters `code` at `link` and approves. */
    | { kind: 'approving'; link: string; code: string | null }
    | { kind: 'done'; outcome: LoginOutcome }
    | { kind: 'failed'; message: string };

  const known = (method: Stored<SignInMethod>): method is SignInMethod =>
    method === 'claude_subscription' || method === 'anthropic_console' || method === 'chatgpt';
  const offered = $derived(methods.filter(known));
  let method = $state<SignInMethod | null>(null);
  const chosen = $derived(method ?? offered[0] ?? null);
  let step = $state<Step>({ kind: 'choosing' });
  let pasted = $state('');
  let dialog = $state<HTMLDialogElement>();

  $effect(() => {
    untrack(() => dialog?.showModal());
  });

  const reason = (error: unknown): string => {
    if (error instanceof RunCommandError) {
      switch (error.code) {
        case 'not_installed':
          return `${client} is not installed on the daemon’s host.`;
        case 'sign_in_not_offered':
          return `Ren cannot sign ${client} in yet.`;
        case 'not_awaiting_code':
        case 'no_sign_in':
          return 'This sign-in is no longer waiting for a code. Start again.';
        case 'spawn_failed':
          return `${client} could not be started on the daemon’s host.`;
        default:
          return `The daemon refused (${error.code ?? error.status}).`;
      }
    }
    return error instanceof Error ? error.message : 'The request failed.';
  };

  async function begin() {
    if (!chosen) return;
    step = { kind: 'starting' };
    pasted = '';
    try {
      const login = await start(chosen);
      switch (login.kind) {
        case 'awaiting_code':
        case 'finishing':
          step = { kind: 'awaiting', link: login.link, said: login.said };
          break;
        case 'awaiting_approval':
          step = { kind: 'approving', link: login.link, code: login.code };
          void follow();
          break;
        case 'starting':
          step = {
            kind: 'failed',
            message: `${client} has not shown a sign-in link yet${login.said ? `: ${login.said}` : '.'}`,
          };
          break;
        case 'ended':
          step = {
            kind: 'failed',
            message:
              login.end.end === 'exited' && login.end.said
                ? login.end.said
                : `${client}’s sign-in ended before showing a link.`,
          };
          break;
      }
    } catch (error) {
      step = { kind: 'failed', message: reason(error) };
    }
  }

  /** Which wait is current: a closed sheet or a new start ends the old one. */
  let following = 0;
  /** Wait on a device login, again and again, until it ends. */
  async function follow() {
    const turn = ++following;
    while (turn === following && step.kind === 'approving') {
      try {
        const waited = await wait();
        if (turn !== following || step.kind !== 'approving') return;
        if (waited.kind === 'done') {
          step = { kind: 'done', outcome: waited.outcome };
          return;
        }
        if (waited.login.kind === 'awaiting_approval' && waited.login.code) {
          step = { ...step, code: waited.login.code };
        }
      } catch (error) {
        if (turn === following) step = { kind: 'failed', message: reason(error) };
        return;
      }
    }
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (step.kind !== 'awaiting' || !pasted.trim()) return;
    const link = step.link;
    step = { kind: 'sending', link };
    try {
      step = { kind: 'done', outcome: await sendCode(pasted) };
    } catch (error) {
      step = { kind: 'failed', message: reason(error) };
    } finally {
      pasted = '';
    }
  }

  async function close() {
    const running =
      step.kind === 'awaiting' || step.kind === 'starting' || step.kind === 'approving';
    following += 1;
    const outcome = step.kind === 'done' ? step.outcome : null;
    dialog?.close();
    if (running) await cancel().catch(() => undefined);
    onclose(outcome);
  }

  const result = $derived(step.kind === 'done' ? SignIns.outcome(step.outcome, client) : null);
</script>

<dialog
  bind:this={dialog}
  class="sign-in"
  aria-labelledby="sign-in-title"
  data-harness={harness}
  oncancel={(event) => {
    event.preventDefault();
    void close();
  }}
>
  <header>
    <div>
      <h2 id="sign-in-title">{client}’s sign-in</h2>
      <p class="muted">
        Ren runs {client}’s own login on the daemon’s host and passes its link and code through.
        {client} keeps and refreshes the sign-in; Ren never sees it.
      </p>
    </div>
    <button class="icon-button" aria-label="Close" onclick={() => void close()}
      ><Icon name="close" size={12} /></button
    >
  </header>

  {#if step.kind === 'choosing' || step.kind === 'failed'}
    {#if offered.length > 1}
      <fieldset class="methods">
        <legend>Sign in with</legend>
        {#each offered as offer (offer)}
          <label>
            <input
              type="radio"
              name="sign-in-method"
              value={offer}
              checked={chosen === offer}
              onchange={() => (method = offer)}
            />
            {SignIns.method(offer)}
          </label>
        {/each}
      </fieldset>
    {/if}
    {#if step.kind === 'failed'}
      <p class="note error" role="alert">{step.message}</p>
    {/if}
    <footer>
      <button class="button" onclick={() => void close()}>Cancel</button>
      <button class="button primary" disabled={!chosen} onclick={() => void begin()}
        >{step.kind === 'failed' ? 'Start again' : `Start ${client}’s sign-in`}</button
      >
    </footer>
  {:else if step.kind === 'starting'}
    <p class="note muted" role="status">Starting {client}’s login…</p>
    <footer>
      <button class="button" onclick={() => void close()}>Cancel</button>
    </footer>
  {:else if step.kind === 'awaiting' || step.kind === 'sending'}
    <ol class="steps">
      <li>
        <span class="step-title">Open the link {client} printed, and approve it there</span>
        <span class="link">
          <a class="mono" href={step.link} target="_blank" rel="noopener noreferrer">{step.link}</a>
          <CopyButton text={step.link} label="Copy the link" />
          <a class="button small" href={step.link} target="_blank" rel="noopener noreferrer"
            >Open ↗</a
          >
        </span>
      </li>
      <li>
        <form onsubmit={submit}>
          <label class="step-title" for="sign-in-code">Paste the code the page shows</label>
          <span class="code">
            <input
              id="sign-in-code"
              class="mono"
              type="text"
              autocomplete="off"
              spellcheck="false"
              placeholder="Code"
              disabled={step.kind === 'sending'}
              bind:value={pasted}
            />
            <button class="button primary" disabled={step.kind === 'sending' || !pasted.trim()}
              >{step.kind === 'sending'
                ? 'Waiting for ' + client + '…'
                : 'Send to ' + client}</button
            >
          </span>
          <span class="muted small-note"
            >It goes only to {client}’s login. Ren doesn’t keep or record it.</span
          >
        </form>
      </li>
    </ol>
    <footer>
      <button class="button" onclick={() => void close()}>Cancel sign-in</button>
    </footer>
  {:else if step.kind === 'approving'}
    <ol class="steps">
      <li>
        <span class="step-title">Open the page {client} printed, and sign in there</span>
        <span class="link">
          <a class="mono" href={step.link} target="_blank" rel="noopener noreferrer">{step.link}</a>
          <CopyButton text={step.link} label="Copy the link" />
          <a class="button small" href={step.link} target="_blank" rel="noopener noreferrer"
            >Open ↗</a
          >
        </span>
      </li>
      <li>
        <span class="step-title">Enter this one-time code on that page</span>
        {#if step.code}
          <span class="device">
            <output class="mono" aria-label="One-time code">{step.code}</output>
            <CopyButton text={step.code} label="Copy the code" />
          </span>
          <span class="muted small-note"
            >Only enter it on a page you opened from this sheet. It expires in about 15 minutes.</span
          >
        {:else}
          <span class="muted">{client} has not shown its code yet…</span>
        {/if}
      </li>
      <li class="waiting" role="status">
        <span class="spinner" aria-hidden="true"></span>
        Waiting for you to approve. {client} finishes by itself, and this sheet follows.
      </li>
    </ol>
    <footer>
      <button class="button" onclick={() => void close()}>Cancel sign-in</button>
    </footer>
  {:else if step.kind === 'done' && result}
    <p class="note" class:ok={result.ok} class:error={!result.ok} role="status">
      <Icon name={result.ok ? 'check' : 'alert'} size={13} />
      {result.text}
    </p>
    <footer>
      {#if !result.ok}
        <button class="button" onclick={() => void begin()}>Start again</button>
      {/if}
      <button class="button primary" onclick={() => void close()}>Done</button>
    </footer>
  {/if}
</dialog>

<style>
  .sign-in {
    width: min(560px, calc(100vw - 32px));
    padding: 0;
    border: 1px solid var(--popover-border);
    border-radius: 0;
    background: var(--popover);
    color: var(--ink);
    box-shadow: var(--popover-shadow);
    font-size: 13px;
  }
  .sign-in::backdrop {
    background: rgb(0 0 0 / 0.45);
  }
  header {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    padding: 14px 16px;
    border-bottom: 1px solid var(--line);
  }
  header div {
    flex: 1;
  }
  h2 {
    margin: 0 0 6px;
    font-size: 15px;
  }
  header p {
    margin: 0;
    line-height: 1.5;
  }
  .methods {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 16px;
    margin: 0;
    padding: 12px 16px;
    border: 0;
    border-bottom: 1px solid var(--line);
  }
  .methods legend {
    float: left;
    width: 100%;
    margin-bottom: 4px;
    color: var(--muted);
    font-size: 12px;
  }
  .methods label {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .steps {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .steps li {
    padding: 12px 16px;
    border-bottom: 1px solid var(--line);
  }
  .steps form {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .step-title {
    display: block;
    margin-bottom: 8px;
    font-weight: 600;
  }
  .steps form .step-title {
    margin-bottom: 0;
  }
  .link {
    display: flex;
    gap: 6px;
    align-items: center;
    min-width: 0;
  }
  .link a.mono {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    padding: 6px 8px;
    border: 1px solid var(--line);
    background: var(--surface-inset);
    color: var(--ink);
    font-size: 12px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .code {
    display: flex;
    gap: 6px;
  }
  .code input {
    flex: 1;
    min-width: 0;
    padding: 6px 8px;
    border: 1px solid var(--line);
    border-radius: 2px;
    background: var(--surface);
    color: var(--ink);
    font-size: 13px;
  }
  .device {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-bottom: 6px;
  }
  .device output {
    padding: 8px 14px;
    border: 1px solid var(--line);
    background: var(--surface-inset);
    font-size: 24px;
    letter-spacing: 0.14em;
    user-select: all;
  }
  .waiting {
    display: flex;
    gap: 8px;
    align-items: center;
    color: var(--muted);
  }
  .spinner {
    width: 10px;
    height: 10px;
    border: 2px solid var(--line);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 0.9s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation: none;
    }
  }
  .small-note {
    font-size: 12px;
  }
  .note {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    margin: 0;
    padding: 12px 16px;
    border-bottom: 1px solid var(--line);
    line-height: 1.5;
  }
  .note.error {
    background: var(--error-soft);
    color: var(--error);
  }
  .note.ok {
    background: var(--success-soft);
    color: var(--success);
  }
  footer {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    padding: 12px 16px;
  }
</style>
