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

// ---- Acceptance criterion 4: garbled-stream baud suggestion ----

test("a mostly-undecodable rx burst surfaces a baud suggestion in the settings popover", async ({ page }) => {
  await gotoConnectedLiveLog(page);

  // 300 bytes stepping through the 0x80-0xFF range is overwhelmingly
  // invalid UTF-8 (continuation/lead bytes with no valid sequence around
  // them) — the same fixture shape
  // `crates/serialwardend/src/web/api.rs`'s own
  // `compute_decode_health_suggests_a_different_baud_for_a_mostly_garbled_sample`
  // unit test uses, here driven through the real HTTP/WS pipeline instead.
  const garbled = Buffer.concat([
    Buffer.from(Array.from({ length: 300 }, (_, i) => (0x80 + (i % 128)) & 0xff)),
    Buffer.from("\n"),
  ]);
  await injectLog(daemon!, DEVICE_ID, [{ kind: "rx", data_b64: garbled.toString("base64") }]);

  await page.getByTestId("config-chip").click();
  const hint = page.getByTestId("decode-health-hint");
  await expect(hint).toBeVisible({ timeout: 10_000 });
  await expect(hint).toContainText("failed to decode");
  await expect(page.getByTestId("use-suggested-baud")).toBeVisible();
});
