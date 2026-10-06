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
  await expect(page.getByTestId("baud-suggestion-explanation")).toContainText("next common rate up");
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

/** Every `POST .../config` body the page sends — a trial sends at most two
 * (the try and the revert), so at most two `config_change` records. */
function recordConfigPosts(page: Page): Array<Record<string, unknown>> {
  const posts: Array<Record<string, unknown>> = [];
  page.on("request", (req) => {
    if (req.method() === "POST" && new URL(req.url()).pathname === `/api/devices/${DEVICE_ID}/config`) {
      posts.push(req.postDataJSON() as Record<string, unknown>);
    }
  });
  return posts;
}

test("a recorded chip banner turns the suggestion into that chip's rate, with its source", async ({ page }) => {
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
  const source = page.getByTestId("baud-suggestion-source");
  await expect(source).toHaveAttribute("href", REALTEK_SOURCE);
  await expect(source).toHaveAttribute("target", "_blank");
});

test("trying a suggested baud that doesn't decode better switches back on its own", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const original = await currentBaud();
  await injectRtlModeSwitch();
  const posts = recordConfigPosts(page);

  await page.getByTestId("config-chip").click();
  await page.getByTestId("use-suggested-baud").click();
  await expect(page.getByTestId("baud-trial-status")).toHaveAttribute("data-state", "running");
  await expect.poll(currentBaud, { timeout: 10_000 }).toBe(1_500_000);
  // Still garbage at the new rate, and plenty of it to judge by.
  await injectLog(daemon!, DEVICE_ID, downloadModeOps(20));

  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "reverted", { timeout: 15_000 });
  await expect(status).toContainText("didn't help");
  await expect(status).toContainText(`switched back to ${original}`);
  expect(await currentBaud()).toBe(original);
  // Exactly the two real changes: the try and the revert.
  expect(posts).toEqual([{ baud: 1_500_000 }, { baud: original }]);
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

test("a trial that hears nothing at the new rate switches back when its window ends", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const original = await currentBaud();
  await injectRtlModeSwitch();
  const posts = recordConfigPosts(page);

  await page.getByTestId("config-chip").click();
  await page.getByTestId("use-suggested-baud").click();

  // Silence is not an improvement: after the full measuring window the
  // trial reverts and says it had nothing to judge.
  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "reverted", { timeout: 15_000 });
  await expect(status).toContainText("Nothing to judge arrived at 1500000");
  expect(await currentBaud()).toBe(original);
  expect(posts).toEqual([{ baud: 1_500_000 }, { baud: original }]);
});

test("trying a suggested baud that decodes keeps it", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectRtlModeSwitch();
  const posts = recordConfigPosts(page);

  await page.getByTestId("config-chip").click();
  await page.getByTestId("use-suggested-baud").click();
  await expect.poll(currentBaud, { timeout: 10_000 }).toBe(1_500_000);
  await injectLog(
    daemon!,
    DEVICE_ID,
    rxLines(Array.from({ length: 6 }, (_, i) => `[ucfg] download ack ${i}, flash write ok`)),
  );

  const status = page.getByTestId("baud-trial-status");
  await expect(status).toHaveAttribute("data-state", "kept", { timeout: 15_000 });
  await expect(status).toContainText("Kept 1500000");
  expect(await currentBaud()).toBe(1_500_000);
  expect(posts).toEqual([{ baud: 1_500_000 }]);
});
