/**
 * One-click "try this baud" (issue #50).
 *
 * A suggested rate is still a guess — even the evidence-based ones
 * (`crates/serialwardend/src/baud_hint.rs`) say what they rest on, not that
 * they are right. Trying one is an experiment: apply the rate, measure what
 * arrives at it, and say what the measurement can and cannot tell.
 *
 * - **Text expected** (the suggestion's basis is history without a mode
 *   switch, direction, or the common list): the rate is kept only if it
 *   measurably decodes better; otherwise the previous rate is put back.
 *   "Didn't help" is only said when bytes arrived and still didn't decode;
 *   silence is reported as "couldn't be judged".
 * - **Binary expected** (a chip fingerprint, or the device appears to have
 *   switched modes): a download protocol is binary even at the right rate,
 *   a device in some other non-text mode may be too, and either may stay
 *   silent until spoken to, so decoding cannot confirm or reject the rate.
 *   Clean text still confirms it; anything else leaves the rate applied
 *   and says it is unconfirmed, with a one-click way back.
 * - **The device went away** (disconnect, a lease handing the port to a
 *   tool such as a flasher, or a reconnect) mid-trial: said plainly, and
 *   the previous rate is put back so the next open doesn't use an untested
 *   one.
 *
 * The measurement comes from the daemon: after a rate change, `GET
 * /api/devices/:id/config`'s `decode_health` samples only bytes recorded
 * since that change.
 *
 * Config writes per trial: the try, and at most one more (the revert, or
 * putting the saved setting back after the port refused the rate). Each is
 * one `POST /config`. The rate to go back to is read from the daemon right
 * before the try, and the daemon is re-read right before going back: if the
 * rate is no longer the one the trial set, another client changed it and
 * the trial leaves it alone.
 *
 * Trial state lives in this module, per device, not in a component: the
 * popover and the banner show the same trial, and the outcome survives the
 * log view being remounted (switching devices and back). While a trial is
 * running, leaving the page asks for confirmation, since the port would be
 * left at an untested rate.
 */
import type { DecodeHealth, DeviceConfig, SetConfigResult } from "./logStream";

/** How long to listen at the candidate rate before judging. */
export const TRIAL_WINDOW_MS = 5_000;
/** How often to re-measure during the window. */
export const TRIAL_POLL_MS = 500;
/** Fewer bytes than this is no evidence either way (the daemon's
 * `MIN_SAMPLE_BYTES`). */
export const TRIAL_MIN_BYTES = 32;
/** Enough clean bytes to keep the rate before the window ends. */
export const TRIAL_EARLY_KEEP_BYTES = 128;
/** Enough still-undecodable bytes to stop before the window ends. */
export const TRIAL_EARLY_STOP_BYTES = 512;
/** The daemon's own warning threshold. */
export const TRIAL_GARBLED_RATIO = 0.2;
/** No single request may hold the trial (and with it the unload guard and
 * the per-device lock) longer than this. */
export const TRIAL_REQUEST_TIMEOUT_MS = 4_000;

export interface TrialSample {
  checked_bytes: number;
  undecodable_ratio: number;
}

export type TrialVerdict = "keep" | "not_better" | "wait";

/** Whether `sample` (measured at the candidate rate) is a real improvement
 * on `baseline`: enough bytes to judge, below the daemon's warning
 * threshold, and at most half the baseline's undecodable ratio. */
export function improved(baseline: TrialSample, sample: TrialSample): boolean {
  return (
    sample.checked_bytes >= TRIAL_MIN_BYTES &&
    sample.undecodable_ratio < TRIAL_GARBLED_RATIO &&
    sample.undecodable_ratio <= baseline.undecodable_ratio / 2
  );
}

/** The trial's decision `elapsedMs` into the window: early when the
 * evidence is already overwhelming either way, otherwise at the end. */
export function judgeTrial(
  baseline: TrialSample,
  sample: TrialSample,
  elapsedMs: number,
  windowMs: number = TRIAL_WINDOW_MS,
): TrialVerdict {
  const better = improved(baseline, sample);
  if (better && sample.checked_bytes >= TRIAL_EARLY_KEEP_BYTES) return "keep";
  if (!better && sample.checked_bytes >= TRIAL_EARLY_STOP_BYTES) return "not_better";
  if (elapsedMs < windowMs) return "wait";
  return better ? "keep" : "not_better";
}

export type TrialOutcome =
  | { kind: "kept"; baud: number; baseline: TrialSample; sample: TrialSample }
  | { kind: "reverted"; baud: number; previous: number; baseline: TrialSample; sample: TrialSample }
  /** Binary expected: stayed at `baud`, which decoding couldn't confirm. */
  | { kind: "unconfirmed"; baud: number; previous: number; sample: TrialSample; why: BinaryReason }
  /** The device disconnected, was leased to a tool, or reconnected during
   * the trial. `restored` says whether the previous rate was put back. */
  | {
      kind: "interrupted";
      baud: number;
      previous: number;
      cause: "disconnected" | "leased" | "reconnected";
      restored: boolean;
      restoreError?: string;
    }
  | { kind: "switched_back"; baud: number; previous: number }
  /** Another client changed the rate mid-trial; nothing was reverted. */
  | { kind: "superseded"; baud: number; observed: number }
  /** The daemon saved `baud` but the port refused it, so nothing was
   * measured. `restoreError` is set if putting the saved setting back
   * failed too. */
  | { kind: "not_applied"; baud: number; previous: number; reason: string; restoreError?: string }
  | { kind: "failed"; baud: number; message: string };

export type BinaryReason = "fingerprint" | "mode_switch";

export interface TrialPlan {
  candidate: number;
  /** Further candidates, in order, offered after a trial that didn't end
   * on a working rate. */
  alternatives: number[];
  baseline: TrialSample;
  /** Why the right rate may still not decode: a chip fingerprint (its
   * download protocol is binary), or the device appears to have switched
   * modes (into something that isn't text). `null`: text is expected. */
  binary: BinaryReason | null;
  /** Whether the device appears to have switched modes — carried to the
   * alternatives, which have no fingerprint of their own. */
  modeSwitch: boolean;
}

/** Build the plan for trying a `decode_health` suggestion. */
export function planFromHealth(health: DecodeHealth): TrialPlan | null {
  const s = health.suggestion;
  const candidate = s?.baud ?? health.suggested_baud;
  if (!candidate) return null;
  return {
    candidate,
    alternatives: s?.alternatives ?? [],
    baseline: { checked_bytes: health.checked_bytes, undecodable_ratio: health.undecodable_ratio },
    binary: s?.basis === "fingerprint" ? "fingerprint" : s?.mode_switch ? "mode_switch" : null,
    modeSwitch: s?.mode_switch ?? false,
  };
}

export interface TrialDeps {
  fetchConfig: (deviceId: string) => Promise<DeviceConfig>;
  setConfig: (deviceId: string, patch: Record<string, unknown>) => Promise<SetConfigResult>;
  sleep: (ms: number) => Promise<void>;
  now: () => number;
}

export function browserDeps(fetchConfig: TrialDeps["fetchConfig"], setConfig: TrialDeps["setConfig"]): TrialDeps {
  return {
    fetchConfig,
    setConfig,
    sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
    now: () => Date.now(),
  };
}

function sampleOf(health: DecodeHealth | undefined): TrialSample {
  return {
    checked_bytes: health?.checked_bytes ?? 0,
    undecodable_ratio: health?.undecodable_ratio ?? 0,
  };
}

const errorText = (e: unknown): string => (e instanceof Error ? e.message : String(e));

/** `promise`, or a rejection after `ms` — a hung request must not hold the
 * trial open. */
function withTimeout<T>(promise: Promise<T>, ms: number = TRIAL_REQUEST_TIMEOUT_MS): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`no answer within ${ms / 1000} s`)), ms);
    promise.then(
      (v) => {
        clearTimeout(timer);
        resolve(v);
      },
      (e) => {
        clearTimeout(timer);
        reject(e);
      },
    );
  });
}

/** `deps` with every request bounded by [`TRIAL_REQUEST_TIMEOUT_MS`]. */
function bounded(deps: TrialDeps): TrialDeps {
  return {
    ...deps,
    fetchConfig: (id) => withTimeout(deps.fetchConfig(id)),
    setConfig: (id, patch) => withTimeout(deps.setConfig(id, patch)),
  };
}

function notAppliedReason(result: SetConfigResult): string {
  if (result.apply === "not_connected") return "the port isn't open";
  return result.apply_error ?? "the port refused it";
}

/** Apply `plan.candidate`, measure, and keep, revert or leave it
 * unconfirmed — see the module docs. Never throws. */
export async function runBaudTrial(
  deviceId: string,
  plan: TrialPlan,
  deps: TrialDeps,
  onProgress?: (sample: TrialSample, elapsedMs: number) => void,
  windowMs: number = TRIAL_WINDOW_MS,
): Promise<TrialOutcome> {
  deps = bounded(deps);
  const candidate = plan.candidate;
  let previous: number;
  let connects: number | undefined;
  try {
    const before = await deps.fetchConfig(deviceId);
    previous = Number(before.config.baud);
    connects = before.decode_health?.connects;
  } catch (e) {
    return { kind: "failed", baud: candidate, message: errorText(e) };
  }
  if (previous === candidate) {
    return { kind: "failed", baud: candidate, message: `the port is already set to ${candidate}` };
  }
  let applied: SetConfigResult;
  try {
    applied = await deps.setConfig(deviceId, { baud: candidate });
  } catch (e) {
    return { kind: "failed", baud: candidate, message: errorText(e) };
  }
  if (applied.applied === false) {
    // Saved but not running: nothing at `candidate` can be measured. Put
    // the saved setting back so the next connect doesn't open at a rate
    // nobody tested.
    const reason = notAppliedReason(applied);
    try {
      await deps.setConfig(deviceId, { baud: previous });
      return { kind: "not_applied", baud: candidate, previous, reason };
    } catch (e) {
      return { kind: "not_applied", baud: candidate, previous, reason, restoreError: errorText(e) };
    }
  }

  const started = deps.now();
  let sample: TrialSample = { checked_bytes: 0, undecodable_ratio: 0 };
  for (;;) {
    await deps.sleep(TRIAL_POLL_MS);
    const elapsed = deps.now() - started;
    try {
      const full = await deps.fetchConfig(deviceId);
      const observed = Number(full.config.baud);
      if (observed !== candidate) return { kind: "superseded", baud: candidate, observed };
      const health = full.decode_health;
      const cause =
        health?.port === "disconnected"
          ? "disconnected"
          : health?.port === "leased"
            ? "leased"
            : connects !== undefined && health?.connects !== undefined && health.connects !== connects
              ? "reconnected"
              : null;
      if (cause !== null) {
        return revertTo(deviceId, candidate, previous, deps, (prev) => ({
          kind: "interrupted",
          baud: candidate,
          previous: prev,
          cause,
          restored: true,
        })).then((o) =>
          o.kind === "failed"
            ? { kind: "interrupted", baud: candidate, previous, cause, restored: false, restoreError: o.message }
            : o,
        );
      }
      sample = sampleOf(health);
      onProgress?.(sample, elapsed);
    } catch {
      // A failed poll is no evidence; the window still ends on time.
    }
    const verdict = judgeTrial(plan.baseline, sample, elapsed, windowMs);
    if (verdict === "wait") continue;
    if (verdict === "keep") return { kind: "kept", baud: candidate, baseline: plan.baseline, sample };
    if (plan.binary !== null) return { kind: "unconfirmed", baud: candidate, previous, sample, why: plan.binary };
    return revertTo(deviceId, candidate, previous, deps, (prev) => ({
      kind: "reverted",
      baud: candidate,
      previous: prev,
      baseline: plan.baseline,
      sample,
    }));
  }
}

/** Put `previous` back — unless the rate is no longer `candidate`, in which
 * case someone else changed it and it is theirs. */
async function revertTo(
  deviceId: string,
  candidate: number,
  previous: number,
  deps: TrialDeps,
  done: (previous: number) => TrialOutcome,
): Promise<TrialOutcome> {
  try {
    const observed = Number((await deps.fetchConfig(deviceId)).config.baud);
    if (observed !== candidate) return { kind: "superseded", baud: candidate, observed };
    await deps.setConfig(deviceId, { baud: previous });
  } catch (e) {
    return {
      kind: "failed",
      baud: candidate,
      message: `switching back to ${previous} failed: ${errorText(e)}`,
    };
  }
  return done(previous);
}

const pct = (r: number): number => Math.round(r * 100);

/** One or two sentences saying how the trial ended and on what evidence. */
export function describeOutcome(outcome: TrialOutcome, windowMs: number = TRIAL_WINDOW_MS): string {
  const secs = Math.round(windowMs / 1000);
  switch (outcome.kind) {
    case "kept":
      return (
        `Kept ${outcome.baud}: ${pct(outcome.sample.undecodable_ratio)}% of ${outcome.sample.checked_bytes} bytes ` +
        `failed to decode at it, down from ${pct(outcome.baseline.undecodable_ratio)}%.`
      );
    case "reverted":
      if (outcome.sample.checked_bytes < TRIAL_MIN_BYTES) {
        return (
          `Nothing arrived at ${outcome.baud} within ${secs} s, so it couldn't be judged; ` +
          `switched back to ${outcome.previous}.`
        );
      }
      return (
        `${outcome.baud} didn't help — ${pct(outcome.sample.undecodable_ratio)}% of ${outcome.sample.checked_bytes} ` +
        `bytes still failed to decode — so it was switched back to ${outcome.previous}.`
      );
    case "unconfirmed": {
      const fingerprint = outcome.why === "fingerprint";
      const silent = outcome.sample.checked_bytes < TRIAL_MIN_BYTES;
      const heard = silent
        ? `nothing arrived within ${secs} s, and ` +
          (fingerprint
            ? "a ROM in download mode often waits silently for its host"
            : "a device in a mode that isn't text may stay silent until spoken to")
        : `${outcome.sample.checked_bytes} bytes arrived, ${pct(outcome.sample.undecodable_ratio)}% not text, and ` +
          (fingerprint
            ? "download traffic is binary even at the right rate"
            : "the device may be in a mode that isn't text");
      const check = fingerprint ? "Check with the flashing tool" : "Check what the device is doing";
      return (
        `Staying at ${outcome.baud}, unconfirmed: ${heard}, so decoding can't tell. ` +
        `${check}, or switch back to ${outcome.previous}.`
      );
    }
    case "interrupted": {
      const what =
        outcome.cause === "disconnected"
          ? "The device disconnected during the trial"
          : outcome.cause === "leased"
            ? "A tool (such as a flasher) took the port during the trial"
            : "The device reconnected during the trial";
      return outcome.restored
        ? `${what}, so ${outcome.baud} couldn't be judged. The saved rate was set back to ${outcome.previous}.`
        : `${what}, so ${outcome.baud} couldn't be judged, and setting the rate back to ${outcome.previous} ` +
            `failed: ${outcome.restoreError}`;
    }
    case "switched_back":
      return `Switched back from ${outcome.baud} to ${outcome.previous}.`;
    case "superseded":
      return (
        `Another client changed the rate to ${outcome.observed} while ${outcome.baud} was being tried, ` +
        `so the trial stopped and left it alone.`
      );
    case "not_applied":
      return (
        `Couldn't apply ${outcome.baud}: ${outcome.reason}. Nothing was measured; ` +
        (outcome.restoreError
          ? `putting the saved setting back to ${outcome.previous} failed too: ${outcome.restoreError}`
          : `the saved setting was put back to ${outcome.previous}.`)
      );
    case "failed":
      return `Couldn't try ${outcome.baud}: ${outcome.message}`;
  }
}

// ---- Per-device trial state, shared by every component on the page ----

export interface TrialView {
  /** The rate being tried right now, or `null`. */
  running: number | null;
  outcome: TrialOutcome | null;
  /** The next candidate to offer, when the last trial didn't end on a
   * working rate. */
  next: number | null;
  /** A follow-up (switching back) is in flight: its buttons are disabled
   * and a second click is ignored. */
  busy: boolean;
}

interface DeviceTrial {
  view: TrialView;
  /** Remaining candidates after the one last tried. */
  remaining: number[];
  baseline: TrialSample;
  modeSwitch: boolean;
}

const trials = new Map<string, DeviceTrial>();
const listeners = new Map<string, Set<(view: TrialView) => void>>();
const EMPTY: TrialView = { running: null, outcome: null, next: null, busy: false };

function guardUnload(e: BeforeUnloadEvent): void {
  e.preventDefault();
  // Older browsers only show the prompt when `returnValue` is set.
  e.returnValue = "";
}

function syncUnloadGuard(): void {
  if (typeof window === "undefined") return;
  const anyRunning = [...trials.values()].some((t) => t.view.running !== null || t.view.busy);
  window.removeEventListener("beforeunload", guardUnload);
  if (anyRunning) window.addEventListener("beforeunload", guardUnload);
}

function publish(deviceId: string, view: TrialView): void {
  const trial = trials.get(deviceId);
  if (trial) trial.view = view;
  syncUnloadGuard();
  for (const cb of listeners.get(deviceId) ?? []) cb(view);
}

/** Current trial state for `deviceId`, then every change, until the
 * returned function is called. */
export function subscribeTrial(deviceId: string, cb: (view: TrialView) => void): () => void {
  let set = listeners.get(deviceId);
  if (!set) {
    set = new Set();
    listeners.set(deviceId, set);
  }
  set.add(cb);
  cb(trials.get(deviceId)?.view ?? EMPTY);
  return () => set.delete(cb);
}

function nextAfter(outcome: TrialOutcome, remaining: number[]): number | null {
  const ended =
    outcome.kind === "reverted" ||
    outcome.kind === "not_applied" ||
    outcome.kind === "switched_back" ||
    (outcome.kind === "interrupted" && outcome.restored);
  return ended ? (remaining[0] ?? null) : null;
}

async function run(deviceId: string, trial: DeviceTrial, plan: TrialPlan, deps: TrialDeps): Promise<void> {
  publish(deviceId, { running: plan.candidate, outcome: null, next: null, busy: false });
  const outcome = await runBaudTrial(deviceId, plan, deps);
  trial.remaining = plan.alternatives.filter((b) => b !== plan.candidate);
  publish(deviceId, { running: null, outcome, next: nextAfter(outcome, trial.remaining), busy: false });
}

/** Start trying `plan.candidate` on `deviceId`, unless a trial is already
 * running there. */
export async function startTrial(deviceId: string, plan: TrialPlan, deps: TrialDeps): Promise<void> {
  const existing = trials.get(deviceId)?.view;
  if (existing && (existing.running !== null || existing.busy)) return;
  const trial: DeviceTrial = {
    view: EMPTY,
    remaining: plan.alternatives,
    baseline: plan.baseline,
    modeSwitch: plan.modeSwitch,
  };
  trials.set(deviceId, trial);
  await run(deviceId, trial, plan, deps);
}

/** Try the next candidate the last trial left on offer. */
export async function tryNext(deviceId: string, deps: TrialDeps): Promise<void> {
  const trial = trials.get(deviceId);
  const candidate = trial?.view.next;
  if (!trial || candidate == null || trial.view.running !== null || trial.view.busy) return;
  await run(
    deviceId,
    trial,
    {
      candidate,
      alternatives: trial.remaining.slice(1),
      baseline: trial.baseline,
      binary: trial.modeSwitch ? "mode_switch" : null,
      modeSwitch: trial.modeSwitch,
    },
    deps,
  );
}

/** After an unconfirmed trial: go back to the rate before it. */
export async function switchBack(deviceId: string, deps: TrialDeps): Promise<void> {
  const trial = trials.get(deviceId);
  const outcome = trial?.view.outcome;
  if (!trial || outcome?.kind !== "unconfirmed" || trial.view.running !== null || trial.view.busy) return;
  publish(deviceId, { running: null, outcome, next: null, busy: true });
  const result = await revertTo(deviceId, outcome.baud, outcome.previous, bounded(deps), (previous) => ({
    kind: "switched_back",
    baud: outcome.baud,
    previous,
  }));
  publish(deviceId, { running: null, outcome: result, next: nextAfter(result, trial.remaining), busy: false });
}

export function dismissTrial(deviceId: string): void {
  const view = trials.get(deviceId)?.view;
  if (view && (view.running !== null || view.busy)) return;
  trials.delete(deviceId);
  publish(deviceId, EMPTY);
}
