/**
 * One-click "try this baud" with automatic revert (issue #50).
 *
 * A suggested rate is still a guess — even the evidence-based ones
 * (`crates/serialwardend/src/baud_hint.rs`) say what they rest on, not that
 * they are right. So trying one is an experiment with a verdict: apply the
 * rate, measure what arrives at it, keep it only if it measurably decodes
 * better, otherwise put the previous rate back and say so. Nobody should
 * have to sweep a dozen rates by hand, and nobody should be left on a rate
 * that made things no better.
 *
 * The measurement comes from the daemon: after a rate change, `GET
 * /api/devices/:id/config`'s `decode_health` samples only bytes recorded
 * since that change, so polling it is a clean before/after comparison.
 *
 * Exactly two config writes at most: the try, and (if needed) the revert.
 * Each is one `POST /config`, and therefore one `config_change` record —
 * the trial never re-applies or retries a write.
 */
import type { DecodeHealth, DeviceConfig } from "./logStream";

/** How long to listen at the candidate rate before judging. */
export const TRIAL_WINDOW_MS = 5_000;
/** How often to re-measure during the window. */
export const TRIAL_POLL_MS = 500;
/** Fewer bytes than this is no evidence either way (matches the daemon's
 * `MIN_SAMPLE_BYTES`). */
export const TRIAL_MIN_BYTES = 32;
/** Enough clean bytes to keep the rate before the window ends. */
export const TRIAL_EARLY_KEEP_BYTES = 128;
/** Enough still-garbled bytes to give up before the window ends. */
export const TRIAL_EARLY_REVERT_BYTES = 512;
/** The daemon's own warning threshold: a sample at or above this is still
 * "mostly not text". */
export const TRIAL_GARBLED_RATIO = 0.2;

export interface TrialSample {
  checked_bytes: number;
  undecodable_ratio: number;
}

export type TrialVerdict = "keep" | "revert" | "wait";

/** Whether `sample` (measured at the candidate rate) is a real improvement
 * on `baseline` (what triggered the suggestion): enough bytes to judge,
 * below the daemon's warning threshold, and at most half the baseline's
 * undecodable ratio. */
export function improved(baseline: TrialSample, sample: TrialSample): boolean {
  return (
    sample.checked_bytes >= TRIAL_MIN_BYTES &&
    sample.undecodable_ratio < TRIAL_GARBLED_RATIO &&
    sample.undecodable_ratio <= baseline.undecodable_ratio / 2
  );
}

/** The trial's decision at `elapsedMs` into the window. Decides early when
 * the evidence is already overwhelming either way; otherwise waits for the
 * whole window and then keeps only a measured improvement — silence at the
 * new rate is not an improvement. */
export function judgeTrial(
  baseline: TrialSample,
  sample: TrialSample,
  elapsedMs: number,
  windowMs: number = TRIAL_WINDOW_MS,
): TrialVerdict {
  const better = improved(baseline, sample);
  if (better && sample.checked_bytes >= TRIAL_EARLY_KEEP_BYTES) return "keep";
  if (!better && sample.checked_bytes >= TRIAL_EARLY_REVERT_BYTES) return "revert";
  if (elapsedMs < windowMs) return "wait";
  return better ? "keep" : "revert";
}

export type TrialOutcome =
  | { kind: "kept"; baud: number; baseline: TrialSample; sample: TrialSample }
  | { kind: "reverted"; baud: number; previous: number; baseline: TrialSample; sample: TrialSample }
  /** The config changed under the trial (another tab, an agent): the trial
   * stops without reverting, since reverting would clobber that change. */
  | { kind: "superseded"; baud: number; observed: number }
  | { kind: "failed"; baud: number; previous: number; message: string; revertFailed: boolean };

export interface TrialDeps {
  fetchConfig: (deviceId: string) => Promise<DeviceConfig>;
  setConfig: (deviceId: string, patch: Record<string, unknown>) => Promise<Record<string, unknown>>;
  sleep: (ms: number) => Promise<void>;
  now: () => number;
}

function sampleOf(health: DecodeHealth | undefined): TrialSample {
  return {
    checked_bytes: health?.checked_bytes ?? 0,
    undecodable_ratio: health?.undecodable_ratio ?? 0,
  };
}

const inFlight = new Set<string>();

/** Whether a trial is already running for `deviceId` (from any component
 * on this page — the banner and the popover share this). */
export function trialRunning(deviceId: string): boolean {
  return inFlight.has(deviceId);
}

/** Apply `candidate`, measure, keep or revert to `previous`. `onProgress`
 * gets each intermediate sample. Never throws: every way it can end is a
 * `TrialOutcome`. */
export async function runBaudTrial(
  deviceId: string,
  previous: number,
  candidate: number,
  baseline: TrialSample,
  deps: TrialDeps,
  onProgress?: (sample: TrialSample, elapsedMs: number) => void,
  windowMs: number = TRIAL_WINDOW_MS,
): Promise<TrialOutcome> {
  if (inFlight.has(deviceId)) {
    return { kind: "failed", baud: candidate, previous, message: "a trial is already running", revertFailed: false };
  }
  inFlight.add(deviceId);
  try {
    try {
      await deps.setConfig(deviceId, { baud: candidate });
    } catch (e) {
      return {
        kind: "failed",
        baud: candidate,
        previous,
        message: e instanceof Error ? e.message : String(e),
        revertFailed: false,
      };
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
        sample = sampleOf(full.decode_health);
        onProgress?.(sample, elapsed);
      } catch {
        // A failed poll is no evidence; the window still ends on time.
      }
      const verdict = judgeTrial(baseline, sample, elapsed, windowMs);
      if (verdict === "wait") continue;
      if (verdict === "keep") return { kind: "kept", baud: candidate, baseline, sample };
      try {
        await deps.setConfig(deviceId, { baud: previous });
      } catch (e) {
        return {
          kind: "failed",
          baud: candidate,
          previous,
          message: e instanceof Error ? e.message : String(e),
          revertFailed: true,
        };
      }
      return { kind: "reverted", baud: candidate, previous, baseline, sample };
    }
  } finally {
    inFlight.delete(deviceId);
  }
}

const pct = (r: number): number => Math.round(r * 100);

/** One sentence saying how the trial ended and on what evidence. */
export function describeOutcome(outcome: TrialOutcome, windowMs: number = TRIAL_WINDOW_MS): string {
  switch (outcome.kind) {
    case "kept":
      return (
        `Kept ${outcome.baud}: ${pct(outcome.sample.undecodable_ratio)}% of ${outcome.sample.checked_bytes} bytes ` +
        `failed to decode at it, down from ${pct(outcome.baseline.undecodable_ratio)}%.`
      );
    case "reverted":
      if (outcome.sample.checked_bytes < TRIAL_MIN_BYTES) {
        return (
          `Nothing to judge arrived at ${outcome.baud} within ${Math.round(windowMs / 1000)} s ` +
          `(${outcome.sample.checked_bytes} bytes), so it was switched back to ${outcome.previous}.`
        );
      }
      return (
        `${outcome.baud} didn't help — ${pct(outcome.sample.undecodable_ratio)}% of ${outcome.sample.checked_bytes} ` +
        `bytes still failed to decode — so it was switched back to ${outcome.previous}.`
      );
    case "superseded":
      return `The rate was changed to ${outcome.observed} elsewhere during the trial, so it stopped without reverting.`;
    case "failed":
      return outcome.revertFailed
        ? `Switching back to ${outcome.previous} after trying ${outcome.baud} failed: ${outcome.message}`
        : `Couldn't try ${outcome.baud}: ${outcome.message}`;
  }
}
