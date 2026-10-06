<script lang="ts">
  /**
   * How the current or last baud trial on a device went, with the follow-up
   * actions it leaves (issue #50) — shared by the port settings popover and
   * the on-screen baud warning, which show the same per-device trial state
   * from `baudTrial.ts`.
   */
  import { onDestroy } from "svelte";
  import { fetchDeviceConfig, setDeviceConfig } from "./logStream";
  import {
    browserDeps,
    describeOutcome,
    dismissTrial,
    subscribeTrial,
    switchBack,
    tryNext,
    type TrialView,
  } from "./baudTrial";

  interface Props {
    deviceId: string;
    /** `data-testid` of the status element; buttons get `<testid>-…`. */
    testid: string;
    /** Element class, so each host can style it as its own. */
    class?: string;
    /** Called after the trial changes the port's rate. */
    onChanged?: () => void;
  }
  const { deviceId, testid, class: className = "", onChanged }: Props = $props();

  let view = $state<TrialView>({ running: null, outcome: null, next: null });
  let unsubscribe: (() => void) | undefined;
  $effect(() => {
    unsubscribe?.();
    let last: TrialView | null = null;
    unsubscribe = subscribeTrial(deviceId, (v) => {
      // A trial that just finished has changed (or restored) the rate.
      if (last?.running != null && v.running === null) onChanged?.();
      last = v;
      view = v;
    });
  });
  onDestroy(() => unsubscribe?.());

  const deps = browserDeps(fetchDeviceConfig, setDeviceConfig);
</script>

{#if view.running !== null}
  <div class={className} role="status" data-testid={testid} data-state="running">
    <span>Trying {view.running} — measuring what arrives at it…</span>
  </div>
{:else if view.outcome}
  <div class={className} role="status" data-testid={testid} data-state={view.outcome.kind}>
    <span>{describeOutcome(view.outcome)}</span>
    {#if view.outcome.kind === "unconfirmed"}
      <button type="button" data-testid="{testid}-switch-back" onclick={() => void switchBack(deviceId, deps)}>
        Switch back to {view.outcome.previous}
      </button>
    {/if}
    {#if view.next !== null}
      <button type="button" data-testid="{testid}-try-next" onclick={() => void tryNext(deviceId, deps)}>
        Try {view.next} next
      </button>
    {/if}
    <button type="button" class="dismiss" data-testid="{testid}-dismiss" onclick={() => dismissTrial(deviceId)}>
      Dismiss
    </button>
  </div>
{/if}
