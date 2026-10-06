// E2E for the timeline and port settings popover (`TASKS.md` T5.3, issue
// #20). Drives the real compiled `serialwarden daemon` binary
// (`startDaemon({ testDeviceId })`) and the real built frontend, injecting
// records through `POST /api/devices/:id/test/inject` and driving config
// changes through the real `POST /api/devices/:id/config` endpoint — no
// mocking of either side.
//
// Every wait below is for an actual observable condition — never a fixed
// `waitForTimeout` — per the timing-stability lesson from issue #39
// (`TASKS.md`'s own "測試紀律" section).
import { expect, test, type Page } from "@playwright/test";
import { startDaemon, injectLog, type DaemonHandle, type InjectOp } from "./daemon.js";

let daemon: DaemonHandle | undefined;

const DEVICE_ID = "demo";

test.afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

async function gotoConnectedLiveLog(page: Page): Promise<void> {
  daemon = await startDaemon({ testDeviceId: DEVICE_ID });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });
}

function rxLines(texts: string[]): InjectOp[] {
  return texts.map((text) => ({ kind: "rx", text: `${text}\n` }));
}

// ---- Acceptance criterion 1: timeline click jumps to and highlights the log line ----

test("clicking a timeline marker scrolls the log to it and highlights the row", async ({ page }) => {
  await gotoConnectedLiveLog(page);

  // Enough lines before and after the marker for there to be real
  // scrollable range, so "jumped to it" is a meaningful assertion (not
  // just "it happened to already be visible").
  await injectLog(daemon!, DEVICE_ID, rxLines(Array.from({ length: 40 }, (_, i) => `before ${i}`)));
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "tx", text: "status\n", client: "claude-code", client_type: "agent", gate: "whitelist" },
  ]);
  await injectLog(daemon!, DEVICE_ID, rxLines(Array.from({ length: 40 }, (_, i) => `after ${i}`)));

  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), { timeout: 10_000 })
    .toBeGreaterThanOrEqual(81);

  const marker = page.locator('[data-testid="timeline-marker"][data-marker-kind="tx"]').first();
  await expect(marker).toBeVisible({ timeout: 10_000 });
  await marker.click();

  const highlightedRow = page.locator('[data-row-kind="tx"][data-highlighted="true"]');
  await expect(highlightedRow).toBeVisible({ timeout: 5_000 });
  await expect(highlightedRow).toContainText("claude-code");

  // "Scrolled to it" — following mode must have been paused by the jump
  // (a jump into history while still auto-following the tail would be
  // immediately fighting itself).
  await expect(page.getByTestId("log-viewport")).toHaveAttribute("data-following", "false", {
    timeout: 5_000,
  });
});

// ---- Acceptance criterion 6: drag-select on the timeline ----

test("dragging across the timeline produces a selectable range", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, rxLines(Array.from({ length: 60 }, (_, i) => `line ${i}`)));
  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), { timeout: 10_000 })
    .toBeGreaterThanOrEqual(60);

  const track = page.getByTestId("timeline-track");
  const box = await track.boundingBox();
  if (!box) throw new Error("timeline track has no bounding box");

  await page.mouse.move(box.x + box.width * 0.2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * 0.8, box.y + box.height / 2, { steps: 5 });
  await page.mouse.up();

  const selection = page.getByTestId("timeline-selection");
  await expect(selection).toBeVisible({ timeout: 5_000 });
  await expect(selection).toContainText(/selected seq \d+.\d+/);
});

// ---- Acceptance criteria 2, 3, 5: baud change broadcasts, config_change +
// revert, and typing an arbitrary custom baud ----

test("applying a custom baud broadcasts to every open client, logs a config_change event, and reverts", async ({
  page,
  browser,
}) => {
  await gotoConnectedLiveLog(page);

  const page2 = await browser.newPage();
  await page2.goto(daemon!.url);
  await expect(page2.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", { timeout: 10_000 });

  // Open the popover and type an arbitrary, non-standard baud — 74880 is
  // the ESP8266 boot-log rate the UX-design wiki's own mockup names, and is
  // deliberately not present as a `<select>` option value that would let a
  // dropdown-only implementation fake this criterion.
  await page.getByTestId("config-chip").click();
  const popover = page.getByTestId("config-popover");
  await expect(popover).toBeVisible({ timeout: 5_000 });
  const baudInput = page.getByTestId("baud-input");
  await baudInput.fill("74880");
  await page.getByTestId("apply-config").click();

  // Criterion 5: the typed custom baud takes effect on the applying tab...
  await expect(page.getByTestId("config-chip")).toContainText("74880", { timeout: 10_000 });
  // ...and criterion 2: on every *other* open tab too, with no action taken
  // there at all — proving this is a broadcast via the shared event stream,
  // not a locally-applied setting.
  await expect(page2.getByTestId("config-chip")).toContainText("74880", { timeout: 10_000 });

  // Criterion 3: a `config_change` event row appears in the log, with a
  // working one-click revert.
  const configChangeRow = page.locator('[data-row-kind="event"][data-event-name="config_change"]').last();
  await expect(configChangeRow).toBeVisible({ timeout: 10_000 });
  const revertButton = configChangeRow.getByTestId("config-revert");
  await expect(revertButton).toBeEnabled();
  await revertButton.click();

  await expect(page.getByTestId("config-chip")).not.toContainText("74880", { timeout: 10_000 });
  await page2.close();
});

// ---- Issue #51: one Apply is one request and one config_change ----

/** Every `config_change` the daemon has stored for the test device, read
 * through the audit endpoint (the same query layer the GUI log reads). */
async function storedConfigChanges(): Promise<Array<Record<string, unknown>>> {
  const res = await fetch(`${daemon!.url}/api/devices/${DEVICE_ID}/audit`);
  if (!res.ok) throw new Error(`audit failed: ${res.status}`);
  const body = (await res.json()) as { audit: Array<Record<string, unknown>> };
  return body.audit.filter((row) => row.event === "config_change");
}

function isConfigPost(url: string, method: string): boolean {
  return method === "POST" && new URL(url).pathname === `/api/devices/${DEVICE_ID}/config`;
}

test("one Apply sends one request and records one config_change; an unchanged Apply records none", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  const configPosts: string[] = [];
  page.on("request", (req) => {
    if (isConfigPost(req.url(), req.method())) configPosts.push(req.postData() ?? "");
  });

  await page.getByTestId("config-chip").click();
  await expect(page.getByTestId("config-popover")).toBeVisible({ timeout: 5_000 });
  await page.getByTestId("baud-input").fill("115200");
  const applied = page.waitForResponse((r) => isConfigPost(r.url(), r.request().method()));
  await page.getByTestId("apply-config").click();
  await applied;
  await expect(page.getByTestId("config-chip")).toContainText("115200", { timeout: 10_000 });

  expect(configPosts, "one Apply click must send exactly one POST").toHaveLength(1);
  const stored = await storedConfigChanges();
  expect(stored).toHaveLength(1);
  expect((stored[0].new as { baud: number }).baud).toBe(115200);
  expect(stored[0].changed_by).toBe("gui");

  // Apply again without changing anything: still one request per click,
  // but the daemon records nothing for a configuration already in effect.
  await page.getByTestId("config-chip").click();
  await expect(page.getByTestId("baud-input")).toHaveValue("115200", { timeout: 5_000 });
  const reapplied = page.waitForResponse((r) => isConfigPost(r.url(), r.request().method()));
  await page.getByTestId("apply-config").click();
  expect((await reapplied).ok()).toBe(true);
  expect(configPosts).toHaveLength(2);
  expect(await storedConfigChanges()).toHaveLength(1);

  // The log shows it once too. The WS stream is ordered, so once a line
  // injected after both applies has rendered, any config_change row either
  // apply produced has rendered before it.
  await injectLog(daemon!, DEVICE_ID, rxLines(["after both applies"]));
  await expect(page.locator('[data-row-kind="line"]', { hasText: "after both applies" })).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.locator('[data-row-kind="event"][data-event-name="config_change"]')).toHaveCount(1);
});

// ---- A change the port rejects is saved, but never shown as applied ----

test("a baud the port rejects keeps the popover open with the error and logs it as not applied", async ({
  page,
}) => {
  const portError = "Invalid argument (os error 22)";
  daemon = await startDaemon({ testDeviceId: DEVICE_ID, applyError: portError });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });

  await page.getByTestId("config-chip").click();
  const popover = page.getByTestId("config-popover");
  await expect(popover).toBeVisible({ timeout: 5_000 });
  await page.getByTestId("baud-input").fill("74880");
  await page.getByTestId("apply-config").click();

  // Not dismissed as if it had worked: the popover stays, naming the error.
  const error = page.getByTestId("config-apply-error");
  await expect(error).toBeVisible({ timeout: 10_000 });
  await expect(error).toContainText("NOT applied");
  await expect(error).toContainText(portError);
  await expect(popover).toBeVisible();

  // The timeline row says the same, and so does the stored record.
  const row = page.locator('[data-row-kind="event"][data-event-name="config_change"]').last();
  await expect(row).toContainText("NOT applied", { timeout: 10_000 });
  const stored = await storedConfigChanges();
  expect(stored).toHaveLength(1);
  expect(stored[0].applied).toBe(false);
  expect(stored[0].apply).toBe("failed");
  expect(stored[0].apply_error).toBe(portError);
});

// ---- Acceptance criterion 4: garbled-stream baud suggestion ----

test("a mostly-undecodable rx burst surfaces a baud suggestion in the settings popover", async ({ page }) => {
  await gotoConnectedLiveLog(page);

  // 300 bytes stepping through the 0x80-0xFF range is overwhelmingly
  // invalid UTF-8 (continuation/lead bytes with no valid sequence around
  // them) — the same fixture shape
  // `crates/serialwardend/src/web/api.rs`'s own
  // `config_endpoint_surfaces_a_baud_suggestion_after_a_garbled_burst`
  // unit test uses, here driven through the real HTTP/WS pipeline instead.
  // No history and no chip banner: issue #50's direction rule applies.
  const garbled = Buffer.concat([
    Buffer.from(Array.from({ length: 300 }, (_, i) => (0x80 + (i % 128)) & 0xff)),
    Buffer.from("\n"),
  ]);
  await injectLog(daemon!, DEVICE_ID, [{ kind: "rx", data_b64: garbled.toString("base64") }]);

  await page.getByTestId("config-chip").click();
  const hint = page.getByTestId("decode-health-hint");
  await expect(hint).toBeVisible({ timeout: 10_000 });
  await expect(hint).toContainText("failed to decode");
  await expect(hint).toHaveAttribute("data-basis", "direction");
  await expect(page.getByTestId("baud-suggestion-explanation")).toContainText("Faster rates are tried first");
  await expect(page.getByTestId("use-suggested-baud")).toBeVisible();
});

// ---- Issue #50: the suggestion is inferred from recorded evidence ----

/** The issue's reference data: an RTL8735B's normal-mode boot banner,
 * then what its UART download mode looks like read at the boot rate — no
 * line break anywhere in it, so not one assembled line. */
const RTL_BOOT = ["voe   :RTL8735B_VOE_1.7.1.0", "Set H264 default HIGH profile", "[video_pre_init_procedure] START"];
const DOWNLOAD_MODE = Buffer.from([
  0xf7, 0x08, 0x32, 0x08, 0xc8, 0x86, 0x84, 0x08, 0x04, 0x85, 0xe6, 0xc4, 0x08, 0x08, 0x88, 0x8f, 0x08, 0x3e, 0x06,
  0x81, 0xe6, 0xc4, 0x08, 0x08, 0x88, 0x8f, 0x08, 0x3f, 0x06, 0x87, 0xe6, 0xf4, 0x08,
]);
const REALTEK_SOURCE = "https://aiot.realmcu.com/en/latest/tools/image_tool/index.html";

function downloadModeOps(n: number): InjectOp[] {
  return Array.from({ length: n }, () => ({ kind: "rx", data_b64: DOWNLOAD_MODE.toString("base64") }));
}

async function injectRtlModeSwitch(): Promise<void> {
  const boot: InjectOp[] = [];
  for (let i = 0; i < 3; i++) {
    for (const t of RTL_BOOT) boot.push({ kind: "rx", text: `${t}\r\n` });
  }
  await injectLog(daemon!, DEVICE_ID, [...boot, ...downloadModeOps(4)]);
}

async function currentBaud(): Promise<number> {
  const res = await fetch(`${daemon!.url}/api/devices/${DEVICE_ID}/config`);
  const body = (await res.json()) as { config: { baud: number } };
  return body.config.baud;
}

/** Every `POST .../config` body the page sends — a trial sends the try and
 * at most one more (the revert, or restoring the saved setting after the
 * port refused the rate). */
function recordConfigPosts(page: Page): Array<Record<string, unknown>> {
  const posts: Array<Record<string, unknown>> = [];
  page.on("request", (req) => {
    if (req.method() === "POST" && new URL(req.url()).pathname === `/api/devices/${DEVICE_ID}/config`) {
      posts.push(req.postDataJSON() as Record<string, unknown>);
    }
  });
  return posts;
}

async function decodeHealth(): Promise<{ suggested_baud: number | null; suggestion: { tried_bauds: number[] } | null }> {
  const res = await fetch(`${daemon!.url}/api/devices/${DEVICE_ID}/config`);
  return ((await res.json()) as { decode_health: never }).decode_health;
}

/** Whether leaving the page right now would ask for confirmation. */
async function unloadGuarded(page: Page): Promise<boolean> {
  return page.evaluate(() => {
    const e = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(e);
    return e.defaultPrevented;
  });
}

async function openPopoverAndTry(page: Page, baud: number): Promise<void> {
  await page.getByTestId("config-chip").click();
  await expect(page.getByTestId("use-suggested-baud")).toHaveText(`Try ${baud}`, { timeout: 10_000 });
  await page.getByTestId("use-suggested-baud").click();
}

test("a recorded chip banner turns the suggestion into that chip's rate, with its sources", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectRtlModeSwitch();

  await page.getByTestId("config-chip").click();
  const hint = page.getByTestId("decode-health-hint");
  await expect(hint).toBeVisible({ timeout: 10_000 });
  await expect(hint).toHaveAttribute("data-basis", "fingerprint");
  await expect(page.getByTestId("use-suggested-baud")).toHaveText("Try 1500000");
  const explanation = page.getByTestId("baud-suggestion-explanation");
  await expect(explanation).toContainText("RTL8735B");
  await expect(explanation).toContainText("switched modes");
  // Says what 1500000 is — a tool default — and what it isn't.
  await expect(explanation).toContainText("Image Tool");
  await expect(explanation).toContainText("3000000");
  const source = page.getByTestId("baud-suggestion-source");
  await expect(source).toHaveAttribute("href", REALTEK_SOURCE);
  await expect(source).toHaveAttribute("target", "_blank");
  await expect(page.getByTestId("baud-suggestion-also-see")).toHaveAttribute("href", /ameba-doc-rtos-pro2-sdk/);
});

test("the on-screen baud warning states its basis, even for binary with no complete line", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  // Binary only, no line break at all: the log view has no line item to
  // time "recent" by, so the banner has to go by the daemon's sample time.
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  await page.reload();
  const banner = page.getByTestId("baud-warning");
  await expect(banner).toBeVisible({ timeout: 10_000 });
  await expect(banner).toHaveAttribute("data-basis", "direction");
  await expect(page.getByTestId("baud-warning-apply")).toHaveText("Try 19200");

  // Once a chip banner is on record, the warning names it and its source.
  await injectLog(daemon!, DEVICE_ID, [{ kind: "rx", text: `${RTL_BOOT[0]}\r\n` }, ...downloadModeOps(4)]);
  await page.reload();
  await expect(banner).toHaveAttribute("data-basis", "fingerprint", { timeout: 10_000 });
  await expect(page.getByTestId("baud-warning-basis")).toContainText("RTL8735B");
  await expect(page.getByTestId("baud-warning-source")).toHaveAttribute("href", REALTEK_SOURCE);
  await expect(page.getByTestId("baud-warning-apply")).toHaveText("Try 1500000");
});

// A fingerprint or mode-switch suggestion expects download traffic: binary
// even at the right rate, often silent. Decoding can't confirm or reject it.

test("a fingerprint trial that hears binary stays at the rate, unconfirmed, with a way back", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const original = await currentBaud();
  await injectRtlModeSwitch();
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 1_500_000);
  await expect.poll(currentBaud, { timeout: 10_000 }).toBe(1_500_000);
  // What download mode really sends at its rate: binary, no line breaks.
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(20));

  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "unconfirmed", { timeout: 15_000 });
  await expect(status).toContainText("Staying at 1500000, unconfirmed");
  await expect(status).toContainText("download traffic is binary even at the right rate");
  await expect(status).not.toContainText("didn't help");
  expect(await currentBaud()).toBe(1_500_000);
  expect(posts).toEqual([{ baud: 1_500_000 }]);

  // A double click switches back once: one more POST, and no false
  // "another client changed it" from the second click racing the first.
  await page.getByTestId("baud-trial-status-switch-back").dblclick();
  await expect(status).toHaveAttribute("data-state", "switched_back", { timeout: 10_000 });
  expect(await currentBaud()).toBe(original);
  expect(posts).toEqual([{ baud: 1_500_000 }, { baud: original }]);
  // The next candidate is on offer.
  await expect(page.getByTestId("baud-trial-status-try-next")).toBeVisible();
});

test("a fingerprint trial that hears nothing stays unconfirmed instead of calling the rate wrong", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  await injectRtlModeSwitch();
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 1_500_000);
  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "unconfirmed", { timeout: 15_000 });
  await expect(status).toContainText("nothing arrived");
  await expect(status).toContainText("waits silently for its host");
  expect(posts).toEqual([{ baud: 1_500_000 }]);
});

test("a mode-switch trial without a fingerprint doesn't claim download traffic", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    ...rxLines(Array.from({ length: 8 }, (_, i) => `[app] heartbeat tick ${i}`)),
    ...downloadModeOps(4),
  ]);

  await openPopoverAndTry(page, 19_200);
  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "unconfirmed", { timeout: 15_000 });
  await expect(status).toContainText("a mode that isn't text");
  await expect(status).not.toContainText("download");
  await expect(status).not.toContainText("flashing tool");
});

for (const [name, event, says] of [
  ["disconnects", "disconnect", "The device disconnected during the trial"],
  ["is leased to a tool", "lease_start", "A tool (such as a flasher) took the port during the trial"],
] as const) {
  test(`a trial whose device ${name} says so and sets the rate back`, async ({ page }) => {
    await gotoConnectedLiveLog(page);
    const original = await currentBaud();
    await injectRtlModeSwitch();
    const posts = recordConfigPosts(page);

    await openPopoverAndTry(page, 1_500_000);
    await expect.poll(currentBaud, { timeout: 10_000 }).toBe(1_500_000);
    await injectLog(daemon!, DEVICE_ID, [{ kind: "event", name: event }]);

    const status = page.getByTestId("baud-trial-status");
    await expect(status).toHaveAttribute("data-state", "interrupted", { timeout: 15_000 });
    await expect(status).toContainText(says);
    await expect(status).toContainText(`set back to ${original}`);
    await expect(status).not.toContainText("unconfirmed");
    expect(await currentBaud()).toBe(original);
    expect(posts).toEqual([{ baud: 1_500_000 }, { baud: original }]);
  });
}

// Direction/common suggestions expect text: kept only if it decodes better.

test("a text-expecting trial that still doesn't decode switches back and offers the next rate", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const original = await currentBaud();
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 19_200);
  await expect(page.getByTestId("baud-trial-status")).toHaveAttribute("data-state", "running");
  await expect.poll(currentBaud, { timeout: 10_000 }).toBe(19_200);
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(20));

  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "reverted", { timeout: 15_000 });
  await expect(status).toContainText("didn't help");
  await expect(status).toContainText(`switched back to ${original}`);
  expect(await currentBaud()).toBe(original);
  expect(posts).toEqual([{ baud: 19_200 }, { baud: original }]);
  await expect(page.getByTestId("baud-trial-status-try-next")).toHaveText("Try 38400 next");
  // The daemon won't suggest the tried rate again either.
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  const health = await decodeHealth();
  expect(health.suggested_baud).toBe(38_400);
  expect(health.suggestion?.tried_bauds).toEqual([19_200]);
});

test("a text-expecting trial that hears nothing says it couldn't judge, then switches back", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const original = await currentBaud();
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 19_200);
  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "reverted", { timeout: 15_000 });
  await expect(status).toContainText("couldn't be judged");
  await expect(status).not.toContainText("didn't help");
  expect(await currentBaud()).toBe(original);
  expect(posts).toEqual([{ baud: 19_200 }, { baud: original }]);
});

test("a text-expecting trial that decodes keeps the rate; leaving mid-trial asks first", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 19_200);
  await expect.poll(currentBaud, { timeout: 10_000 }).toBe(19_200);
  expect(await unloadGuarded(page)).toBe(true);
  await injectLog(daemon!, DEVICE_ID, rxLines(Array.from({ length: 6 }, (_, i) => `[app] sensor ${i} ready, 25.3C`)));

  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "kept", { timeout: 15_000 });
  await expect(status).toContainText("Kept 19200");
  expect(await currentBaud()).toBe(19_200);
  expect(posts).toEqual([{ baud: 19_200 }]);
  expect(await unloadGuarded(page)).toBe(false);
});

test("a trial leaves alone a rate another client changed mid-trial", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 19_200);
  await expect.poll(currentBaud, { timeout: 10_000 }).toBe(19_200);
  const other = await fetch(`${daemon!.url}/api/devices/${DEVICE_ID}/config`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ baud: 57_600 }),
  });
  expect(other.ok).toBe(true);

  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "superseded", { timeout: 15_000 });
  await expect(status).toContainText("Another client changed the rate to 57600");
  expect(await currentBaud()).toBe(57_600);
  expect(posts).toEqual([{ baud: 19_200 }]);
});

test("a trial whose rate the port refuses stops at once and says why", async ({ page }) => {
  const portError = "Invalid argument (os error 22)";
  daemon = await startDaemon({ testDeviceId: DEVICE_ID, applyError: portError });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", { timeout: 10_000 });
  const original = await currentBaud();
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  const posts = recordConfigPosts(page);

  await openPopoverAndTry(page, 19_200);
  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "not_applied", { timeout: 10_000 });
  await expect(status).toContainText(`Couldn't apply 19200: ${portError}`);
  await expect(status).toContainText("Nothing was measured");
  // The try, then the saved setting put back — nothing measured in between.
  expect(posts).toEqual([{ baud: 19_200 }, { baud: original }]);
  expect(await currentBaud()).toBe(original);
  await expect(page.getByTestId("baud-trial-status-try-next")).toBeVisible();
  // A rate the port never ran isn't a tested rate.
  const health = await decodeHealth();
  expect(health.suggestion?.tried_bauds).toEqual([]);
  expect(health.suggested_baud).toBe(19_200);
});


// ---- The chip and the banner say when the port refused a rate ----

test("the config chip shows what the port runs, not a saved rate it refused", async ({ page }) => {
  const portError = "Invalid argument (os error 22)";
  daemon = await startDaemon({ testDeviceId: DEVICE_ID, applyError: portError });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", { timeout: 10_000 });
  const chip = page.getByTestId("config-chip");
  await expect(chip).toHaveText("9600 8N1", { timeout: 10_000 });
  await expect(chip).toHaveAttribute("data-applied", "true");

  await chip.click();
  await page.getByTestId("baud-input").fill("74880");
  await page.getByTestId("apply-config").click();
  await expect(page.getByTestId("config-apply-error")).toBeVisible({ timeout: 10_000 });

  // The port is still on 9600; 74880 is only saved.
  await expect(chip).toHaveText("9600 8N1 (saved: 74880 8N1, not applied)", { timeout: 10_000 });
  await expect(chip).toHaveAttribute("data-applied", "false");
  await expect(chip).toHaveAttribute("title", new RegExp(portError.replace(/[()]/g, "\\$&")));

  // The same state, from the API every client reads.
  const res = await fetch(`${daemon.url}/api/devices/${DEVICE_ID}/config`);
  const body = (await res.json()) as {
    config: { baud: number };
    applied: boolean;
    apply: string;
    apply_error?: string;
    last_applied?: { baud: number };
  };
  expect(body.config.baud).toBe(74_880);
  expect(body.applied).toBe(false);
  expect(body.apply).toBe("failed");
  expect(body.apply_error).toBe(portError);
  expect(body.last_applied?.baud).toBe(9600);
});

test("a trial started from the on-screen baud warning reports a refused rate there", async ({ page }) => {
  const portError = "Invalid argument (os error 22)";
  daemon = await startDaemon({ testDeviceId: DEVICE_ID, applyError: portError });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", { timeout: 10_000 });
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(4));
  await page.reload();
  await expect(page.getByTestId("baud-warning-apply")).toHaveText("Try 19200", { timeout: 10_000 });

  await page.getByTestId("baud-warning-apply").click();
  const banner = page.getByTestId("baud-trial-banner");
  await expect(banner).toHaveAttribute("data-state", "not_applied", { timeout: 10_000 });
  await expect(banner).toContainText(`Couldn't apply 19200: ${portError}`);
  // The saved rate went back to 9600, but on this port putting it back is a
  // live apply too, and it failed as well: the chip doesn't claim 9600 is
  // simply running.
  await expect(page.getByTestId("config-chip")).toHaveText("9600 8N1 (last apply failed)", {
    timeout: 10_000,
  });
});
