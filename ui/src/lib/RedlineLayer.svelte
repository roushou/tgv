<!--
  Redline's layer over the page: while marking is on, the element under the
  pointer is outlined and named, a click marks it instead of reaching the
  page, and each mark's numbered pin follows its element as the page
  scrolls. A new mark's note is written in place. `m` toggles marking and
  `Esc` leaves it; everything Redline draws itself carries `data-redline`,
  so it stays usable while the page is held still.
-->
<script lang="ts">
  import { tick } from 'svelte';
  import { Redline } from './redline.ts';
  import { RedlineDesk } from './redline.svelte.ts';

  let { desk }: { desk: RedlineDesk } = $props();

  /** The element under the pointer, while marking. */
  let hovered = $state<{ box: DOMRect; label: string } | null>(null);
  /** Bumped on scroll and resize, so pins and the note follow their elements. */
  let frame = $state(0);
  let draft = $state('');
  let field = $state<HTMLTextAreaElement>();

  const ours = (target: EventTarget | null) =>
    target instanceof Element && !!target.closest('[data-redline]');
  const typing = (target: EventTarget | null) =>
    target instanceof HTMLElement &&
    (target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName));

  /** An element's box now; `at` only ties the read to the current frame. */
  function place(element: Element, at: number): DOMRect | null {
    return at >= 0 && element.isConnected ? element.getBoundingClientRect() : null;
  }

  $effect(() => {
    if (!desk.open) {
      hovered = null;
      return;
    }
    const move = (event: PointerEvent) => {
      const target = event.target;
      if (desk.editing !== null || ours(target) || !(target instanceof Element)) {
        hovered = null;
        return;
      }
      hovered = {
        box: target.getBoundingClientRect(),
        label: RedlineDesk.source(target) ?? Redline.selector(target),
      };
    };
    // The page is held still: presses, drags, and submits never reach it.
    const hold = (event: Event) => {
      if (ours(event.target)) return;
      event.preventDefault();
      event.stopPropagation();
    };
    const mark = (event: MouseEvent) => {
      if (ours(event.target)) return;
      event.preventDefault();
      event.stopPropagation();
      if (desk.editing !== null || !(event.target instanceof Element)) return;
      desk.add(event.target);
      draft = '';
      hovered = null;
      void tick().then(() => field?.focus());
    };
    const follow = () => {
      frame += 1;
    };
    const held = [
      'pointerdown',
      'mousedown',
      'pointerup',
      'mouseup',
      'auxclick',
      'dblclick',
      'submit',
    ];
    window.addEventListener('pointermove', move, true);
    window.addEventListener('click', mark, true);
    for (const type of held) window.addEventListener(type, hold, true);
    window.addEventListener('scroll', follow, true);
    window.addEventListener('resize', follow);
    return () => {
      window.removeEventListener('pointermove', move, true);
      window.removeEventListener('click', mark, true);
      for (const type of held) window.removeEventListener(type, hold, true);
      window.removeEventListener('scroll', follow, true);
      window.removeEventListener('resize', follow);
    };
  });

  function key(event: KeyboardEvent) {
    if (event.metaKey || event.ctrlKey || event.altKey || typing(event.target)) return;
    // A dialog over the page (a client's sign-in) has its own keys.
    if (event.target instanceof Element && event.target.closest('dialog[open]')) return;
    if (event.key === 'm') {
      event.preventDefault();
      desk.toggle();
    } else if (event.key === 'Escape' && desk.open && desk.editing === null) {
      event.preventDefault();
      event.stopPropagation();
      desk.toggle();
    }
  }

  const editing = $derived(desk.marks.find((mark) => mark.id === desk.editing) ?? null);
  /** Marks are numbered by place, so removing one renumbers those after. */
  const number = $derived(editing ? desk.marks.indexOf(editing) + 1 : 0);
  /** Where the note opens: beside its element, kept on screen. */
  const notePlace = $derived.by(() => {
    if (!editing) return null;
    const box = place(editing.element, frame);
    const width = 300;
    const left = Math.min(Math.max(8, (box?.left ?? 40) + 12), window.innerWidth - width - 8);
    const below = (box?.bottom ?? 40) + 8;
    const top = below + 150 < window.innerHeight ? below : Math.max(8, (box?.top ?? 40) - 158);
    return { left, top, width };
  });

  function noteKey(event: KeyboardEvent) {
    if (!editing) return;
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      desk.save(editing.id, draft);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      desk.drop(editing.id);
    }
  }
</script>

<svelte:window onkeydown={key} />

{#if desk.open}
  {#if hovered}
    <div
      class="outline"
      aria-hidden="true"
      style:left="{hovered.box.left}px"
      style:top="{hovered.box.top}px"
      style:width="{hovered.box.width}px"
      style:height="{hovered.box.height}px"
    >
      <span class="tag" class:below={hovered.box.top < 24}>{hovered.label}</span>
    </div>
  {/if}
  {#each desk.marks as mark, index (mark.id)}
    {@const box = place(mark.element, frame)}
    {#if box}
      <div
        class="marked"
        aria-hidden="true"
        style:left="{box.left}px"
        style:top="{box.top}px"
        style:width="{box.width}px"
        style:height="{box.height}px"
      ></div>
      <span
        class="pin"
        aria-hidden="true"
        style:left="{Math.max(10, box.left)}px"
        style:top="{Math.max(10, box.top)}px">{index + 1}</span
      >
    {/if}
  {/each}
  {#if editing && notePlace}
    <div
      class="note"
      data-redline
      role="dialog"
      aria-label="Note for mark {number}"
      style:left="{notePlace.left}px"
      style:top="{notePlace.top}px"
      style:width="{notePlace.width}px"
    >
      <div class="what" title={editing.target.selector}>
        {number} · {editing.target.source ?? Redline.name(editing.target)}
      </div>
      <textarea
        bind:this={field}
        bind:value={draft}
        onkeydown={noteKey}
        rows="3"
        placeholder="What's wrong here?"
        aria-label="Note"></textarea>
      <div class="foot">
        <span><kbd>Enter</kbd> saves · <kbd>Esc</kbd> drops</span>
        <button class="button small redline-save" onclick={() => desk.save(editing.id, draft)}
          >Save</button
        >
      </div>
    </div>
  {/if}
{/if}

<style>
  /* Redline's marks and note sit over everything but its own pane. */
  .outline,
  .marked {
    position: fixed;
    z-index: 2147483644;
    pointer-events: none;
  }
  .outline {
    border: 1px dashed var(--redline);
    background: color-mix(in srgb, var(--redline) 7%, transparent);
  }
  .marked {
    border: 1.5px solid var(--redline);
  }
  .tag {
    position: absolute;
    left: -1px;
    bottom: 100%;
    max-width: 60vw;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding: 1px 6px;
    background: var(--redline);
    color: var(--on-redline);
    font:
      11px 'SFMono-Regular',
      Consolas,
      monospace;
  }
  .tag.below {
    bottom: auto;
    top: 100%;
  }
  .pin {
    position: fixed;
    z-index: 2147483645;
    width: 18px;
    height: 18px;
    display: grid;
    place-items: center;
    transform: translate(-50%, -50%);
    pointer-events: none;
    background: var(--redline);
    color: var(--on-redline);
    font:
      700 11px 'SFMono-Regular',
      Consolas,
      monospace;
  }
  .note {
    position: fixed;
    z-index: 2147483646;
    display: grid;
    background: var(--popover);
    border: 1px solid var(--popover-border);
    border-top: 2px solid var(--redline);
    box-shadow: var(--popover-shadow);
  }
  .what {
    padding: 7px 10px;
    border-bottom: 1px solid var(--popover-border);
    color: var(--muted);
    font:
      11px 'SFMono-Regular',
      Consolas,
      monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  textarea {
    border: 0;
    background: transparent;
    color: var(--ink);
    font: inherit;
    font-size: 13px;
    padding: 8px 10px;
    resize: none;
    outline: none;
  }
  .foot {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border-top: 1px solid var(--popover-border);
    color: var(--muted);
    font-size: 11px;
  }
  .foot button {
    margin-left: auto;
  }
  /* Filled in Redline's color, also on hover, where the shared button
     rule would otherwise clear it. */
  .redline-save,
  .redline-save:hover:not(:disabled) {
    background: var(--redline);
    border-color: var(--redline);
    color: var(--on-redline);
  }
  .redline-save:hover:not(:disabled) {
    background: var(--redline-hover);
    border-color: var(--redline-hover);
  }
</style>
