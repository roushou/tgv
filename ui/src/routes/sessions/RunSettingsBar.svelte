<!--
  A session's settings, changed from its next turn on: model, effort, and
  approvals, each offered only where the client takes it mid-run, and
  otherwise kept in place, greyed out, saying why. A draft also offers its
  client, until its first prompt fixes it.
-->
<script lang="ts">
  import { harnessLabel } from '#lib/activity.ts';
  import Picker, { type PickerGroup } from '#lib/Picker.svelte';
  import {
    APPROVAL_POLICIES,
    EFFORTS,
    Models,
    Setup,
    type ApprovalPolicy,
    type Drivable,
    type Effort,
    type Midway,
    type ModelId,
    type RunModel,
    type RunSettings,
  } from '#lib/runs.ts';
  import type { Stored } from '#lib/decode.ts';

  let {
    harness,
    settings,
    midway,
    approvals,
    listModels,
    busy,
    onchange,
    choice = null,
  }: {
    harness: Drivable;
    settings: RunSettings;
    /** What the client takes mid-run. */
    midway: readonly Stored<Midway>[];
    /** The approval policies the client runs under. */
    approvals: readonly Stored<ApprovalPolicy>[];
    listModels: (harness: Drivable) => Promise<RunModel[]>;
    busy: boolean;
    onchange: (settings: {
      model: ModelId | null;
      effort: Effort | null;
      approvals: ApprovalPolicy | null;
    }) => void;
    /** The clients a draft can change to, and what to do with the one chosen. */
    choice?: {
      clients: readonly Drivable[];
      /** What to say of each client beside its name: its sign-in. */
      hint?: (harness: Drivable) => string | null;
      onclient: (harness: Drivable) => void;
    } | null;
  } = $props();

  /** The picker's value for "the client's own": never a model's or label's text. */
  const DEFAULT = '';
  let models = $state<RunModel[]>([]);
  $effect(() => {
    const asked = harness;
    if (!midway.includes('model') && !midway.includes('effort')) return;
    listModels(asked).then(
      (listed) => {
        if (asked === harness) models = listed;
      },
      () => {
        models = [];
      },
    );
  });

  const current = $derived(Setup.known(settings));
  const effective = $derived(Setup.efforts(models, current.model));
  const efforts = $derived(EFFORTS.filter((level) => effective?.efforts.includes(level)));
  const policies = $derived(APPROVAL_POLICIES.filter((policy) => approvals.includes(policy)));
  const why = (change: Midway) =>
    midway.includes(change)
      ? null
      : `${harnessLabel(harness)} cannot change its ${change} in a running conversation`;

  const modelGroups = $derived<PickerGroup[]>([
    {
      label: null,
      options: [
        { value: DEFAULT, label: 'Default', detail: `${harnessLabel(harness)}’s own choice` },
        ...(current.model && !models.some((offered) => offered.id === current.model)
          ? [{ value: current.model, label: current.model }]
          : []),
      ],
    },
    ...Models.groups(models).map((group) => ({
      label: group.provider ?? 'Models',
      options: group.models.map((offered) => ({
        value: offered.id,
        label: offered.name,
        detail: offered.description,
        badge: offered.choice === 'default' ? 'default' : null,
      })),
    })),
  ]);
  const effortGroups = $derived<PickerGroup[]>([
    {
      label: null,
      options: [
        { value: DEFAULT, label: 'Default', hint: effective?.effort ?? undefined },
        ...(current.effort && !efforts.includes(current.effort)
          ? [{ value: current.effort, label: current.effort }]
          : []),
        ...efforts.map((level) => ({ value: level, label: level })),
      ],
    },
  ]);
  const policyGroups = $derived<PickerGroup[]>([
    {
      label: null,
      options: [
        { value: DEFAULT, label: 'Default' },
        ...policies.map((policy) => ({
          value: policy,
          label: Setup.policy(policy),
          detail: Setup.explain(policy),
        })),
      ],
    },
  ]);

  function change(next: Partial<typeof current>) {
    onchange({ ...current, ...next });
  }
</script>

<span class="settings">
  {#if choice}
    <Picker
      label="Client for this session"
      icon="spark"
      groups={[
        {
          label: null,
          options: choice.clients.map((offered) => ({
            value: offered,
            label: harnessLabel(offered),
            hint: choice.hint?.(offered) ?? undefined,
          })),
        },
      ]}
      value={harness}
      placeholder={harnessLabel(harness)}
      search={false}
      disabled={busy ? 'Waiting for the last change' : null}
      onchange={(value) => {
        const chosen = choice.clients.find((offered) => offered === value);
        if (chosen && chosen !== harness) choice.onclient(chosen);
      }}
    />
  {/if}
  <Picker
    label="Model for the next turn"
    icon="cpu"
    groups={modelGroups}
    value={current.model ?? DEFAULT}
    placeholder="Default"
    search={models.length > 8}
    disabled={busy ? 'Waiting for the last change' : why('model')}
    onchange={(value) =>
      change({
        model: (models.find((offered) => offered.id === value)?.id ?? null) as ModelId | null,
      })}
  />
  <Picker
    label="Effort for the next turn"
    icon="thought"
    groups={effortGroups}
    value={current.effort ?? DEFAULT}
    placeholder="Default"
    search={false}
    disabled={busy ? 'Waiting for the last change' : why('effort')}
    onchange={(value) => change({ effort: efforts.find((level) => level === value) ?? null })}
  />
  {#if policies.length}
    <Picker
      label="Approvals from the next turn"
      icon="shield"
      groups={policyGroups}
      value={current.approvals ?? DEFAULT}
      placeholder="Default"
      search={false}
      disabled={busy ? 'Waiting for the last change' : why('approvals')}
      onchange={(value) =>
        change({ approvals: policies.find((policy) => policy === value) ?? null })}
    />
  {/if}
</span>

<style>
  .settings {
    display: inline-flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
  }
</style>
