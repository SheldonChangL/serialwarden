// E2E for the live log view (`TASKS.md` T5.2, issue #19). Drives the real
// compiled `serialwarden daemon` binary (`startDaemon({ testDeviceId })` —
// see `daemon.ts`'s doc comment on the `TestBackend` seam this needs) and
// the real built frontend, injecting records through the real
// recorder->query->presentation->WS/tail pipeline via `injectLog`
// (`POST /api/devices/:id/test/inject`) rather than mocking anything in
// the browser.
//
// Every wait below is for an actual observable condition (a DOM attribute/
// text Playwright polls, a real scroll/wheel event, a real HTTP response)
// — never a fixed `waitForTimeout` — per the timing-stability lesson from
// issue #39. Where *time itself* is the thing under test (fps, filter
// elapsed ms), each assertion's threshold is documented with where the
// number comes from — see each test's own comment.
import { expect, test } from "@playwright/test";
import { startDaemon, injectLog, type DaemonHandle, type InjectOp } from "./daemon.js";

let daemon: DaemonHandle | undefined;

const DEVICE_ID = "demo";

test.afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

async function gotoConnectedLiveLog(page: import("@playwright/test").Page): Promise<void> {
  daemon = await startDaemon({ testDeviceId: DEVICE_ID });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });
}

function rxLines(texts: string[]): InjectOp[] {
  return texts.map((text) => ({ kind: "rx", text: `${text}\n` }));
}

test("status bar shows unavailable error counts, never a bare zero", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const counts = page.getByTestId("error-counts");
  // `TestBackend::error_counts` (crates/serialwardend/src/protocol/backend.rs)
  // always reports `Unavailable` — it has no real fd/ioctl underneath, same
  // honest reason macOS itself has none. This proves the GUI's rendering
  // of that wire shape (`{"status":"unavailable"}`), independent of which
  // platform actually runs this test (the CI job is ubuntu-only).
  await expect(counts).toContainText("framing unavailable", { timeout: 10_000 });
  await expect(counts).toContainText("overrun unavailable");
});

test("data lines and broker events are visually and structurally distinct", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "boot ok\n" },
    { kind: "tx", text: "status\n", client: "claude-code", client_type: "agent", gate: "whitelist" },
  ]);

  await expect(page.getByTestId("log-row")).toHaveCount(2, { timeout: 10_000 });
  const dataRow = page.locator('[data-row-kind="line"]').first();
  const eventRow = page.locator('[data-row-kind="tx"]').first();
  await expect(eventRow).toContainText("claude-code");

  // Structural: distinct `data-row-kind` values, asserted above by locator.
  // Visual: distinct font family and a colored left border on the event
  // row that the data row doesn't have — the UX-design wiki's "device data
  // renders in monospace on the plain surface; broker events render in
  // sans-serif with a coloured band."
  const [dataFont, eventFont] = await Promise.all([
    dataRow.evaluate((el) => getComputedStyle(el).fontFamily),
    eventRow.evaluate((el) => getComputedStyle(el).fontFamily),
  ]);
  expect(dataFont).not.toEqual(eventFont);

  // The criterion is a *coloured* band, so that is what this asserts. Every
  // row now reserves the same-width gutter (`--gutter-w`, see
  // `LogRow.svelte`'s doc comment) and device output leaves it transparent,
  // which keeps the monospace columns aligned across row kinds — so the band
  // is distinguished by color, not by width, and comparing widths would only
  // test the implementation detail that used to make the two differ.
  const [dataBand, eventBand] = await Promise.all([
    dataRow.evaluate((el) => getComputedStyle(el).borderLeftColor),
    eventRow.evaluate((el) => getComputedStyle(el).borderLeftColor),
  ]);
  expect(eventBand).not.toEqual(dataBand);
  expect(dataBand).toMatch(/rgba\(.*,\s*0\)/); // transparent on device output
});

test("gate rows say what the gate did, and a pending request never reads as allowed", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "gate", action: "request", reason: "danger:erase", request_seq: 1 },
    { kind: "gate", action: "deny", reason: "denied_by_operator:gui", request_seq: 1 },
    { kind: "gate", action: "request", reason: "pending", request_seq: 2 },
    { kind: "gate", action: "approve", reason: "approved_by:gui", request_seq: 2 },
    { kind: "gate", action: "allow", reason: "whitelist:^status$", request_seq: 3 },
  ]);

  const label = (action: string) =>
    page.locator(`[data-row-kind="gate"][data-gate-action="${action}"] .label`);
  await expect(label("request")).toHaveCount(2, { timeout: 10_000 });
  await expect(label("request").first()).toHaveText("Awaiting approval");
  await expect(label("deny")).toHaveText("Blocked");
  await expect(label("approve")).toHaveText("Approved");
  await expect(label("allow")).toHaveText("Allowed");
});

test("duplicate lines fold, and a short binary row shows its hex without a click", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  // Trailing 0x0a is load-bearing: `test/inject`'s `data_b64` op writes raw
  // bytes as-is (see `crates/serialwardend/src/web/api.rs`'s `resolve_bytes`
  // doc comment) with no auto-appended newline, and the query layer's line
  // assembler only ever completes a line on an actual `\n` — an
  // unterminated chunk stays an invisible in-progress "partial" forever.
  const binary = Buffer.from([0xff, 0xfe, 0xfd, 0xfc, 1, 2, 3, 0xff, 0xfe, 0xfd, 0xfc, 1, 2, 3, 0x0a]);
  await injectLog(daemon!, DEVICE_ID, [
    ...rxLines(["read timeout", "read timeout", "read timeout", "read timeout"]),
    { kind: "rx", data_b64: binary.toString("base64") },
  ]);

  const foldRow = page.locator('[data-folded="true"]').first();
  await expect(foldRow).toContainText("expand", { timeout: 10_000 });
  await expect(foldRow).toContainText("4");
  await foldRow.locator(".fold-toggle").click();
  await expect(foldRow).toContainText("collapse");

  // This 14-byte row used to collapse to "14 bytes of binary · view as hex"
  // and need a click (issue #48 changed that: at or under 32 bytes the hex
  // is the row). The long-row collapse/expand behavior it used to cover now
  // lives in the "long binary rows stay collapsed" test below.
  const binaryRow = page.locator('[data-binary="true"]').first();
  await expect(binaryRow).toContainText(/ff fe fd fc 01 02 03 ff fe fd fc 01 02 03/, {
    timeout: 10_000,
  });
  await expect(binaryRow).not.toContainText("view as hex");
  await expect(binaryRow.locator(".binary-toggle")).toHaveCount(0);
});

// Issue #48. Binary rows for these tests: bytes 0x80..0xbf are lone UTF-8
// continuation bytes (always invalid), so every row is classified binary;
// the trailing 0x0a is the terminator the line assembler needs (see the note
// in the test above) and is not part of the row's bytes.
function binLine(length: number, first = 0): Buffer {
  const bytes = Array.from({ length }, (_, i) => 0x80 + ((first + i) % 0x40));
  return Buffer.from([...bytes, 0x0a]);
}

function hexOf(buf: Buffer): string {
  return Array.from(buf.subarray(0, buf.length - 1))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join(" ");
}

const rxBytes = (buf: Buffer): InjectOp => ({ kind: "rx", data_b64: buf.toString("base64") });

test("binary rows up to 32 bytes show their hex inline; 33 bytes and up stay collapsed", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  const one = binLine(1);
  const at32 = binLine(32, 3);
  const at33 = binLine(33, 5);
  // Text rows between them: each binary row is its own run, so this tests the
  // threshold alone, not merging.
  await injectLog(daemon!, DEVICE_ID, [
    rxBytes(one),
    ...rxLines(["boot ok"]),
    rxBytes(at32),
    ...rxLines(["next"]),
    rxBytes(at33),
  ]);

  const binaryRows = page.locator('[data-binary="true"]');
  await expect(binaryRows).toHaveCount(3, { timeout: 10_000 });

  // 1 byte and exactly 32 bytes: the hex is on screen with no interaction,
  // and there is nothing to click.
  for (const [i, buf] of [one, at32].entries()) {
    const row = binaryRows.nth(i);
    await expect(row.getByTestId("binary-hex")).toHaveText(hexOf(buf));
    await expect(row).not.toContainText("view as hex");
    await expect(row.locator(".binary-toggle")).toHaveCount(0);
  }

  // 33 bytes: collapsed, hex not shown until asked for.
  const long = binaryRows.nth(2);
  await expect(long).toContainText("33 bytes of binary");
  await expect(long).toContainText("view as hex");
  await expect(long.getByTestId("binary-hex")).toHaveCount(0);
});

test("long binary rows stay collapsed and expand and collapse on click", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  // 100 bytes: past the 64-byte preview the daemon sends, so expanding shows
  // the preview with an ellipsis rather than pretending to be the whole row.
  const long = binLine(100);
  await injectLog(daemon!, DEVICE_ID, [rxBytes(long)]);

  const row = page.locator('[data-binary="true"]').first();
  await expect(row).toContainText("100 bytes of binary", { timeout: 10_000 });
  await expect(row).toContainText("view as hex");
  await expect(row.getByTestId("binary-hex")).toHaveCount(0);

  await row.locator(".binary-toggle").click();
  const preview = hexOf(long).split(" ").slice(0, 64).join(" ");
  await expect(row.getByTestId("binary-hex")).toContainText(preview);
  await expect(row.getByTestId("binary-hex")).toContainText("…");
  await expect(row).not.toContainText("100 bytes of binary");

  await row.locator(".binary-toggle").click();
  await expect(row.getByTestId("binary-hex")).toHaveCount(0);
  await expect(row).toContainText("100 bytes of binary");
});

test("adjacent binary fragments render as one run, each seq still traceable", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  // The issue's example: 1-, 4- and 6-byte pieces of one burst. Sent as
  // separate recorder records (distinct seqs) in one request, so they land
  // within the merge window of each other.
  const a = binLine(1, 0);
  const b = binLine(4, 1);
  const c = binLine(6, 5);
  await injectLog(daemon!, DEVICE_ID, [rxBytes(a), rxBytes(b), rxBytes(c)]);

  const rows = page.locator('[data-binary="true"]');
  await expect(rows).toHaveCount(1, { timeout: 10_000 });
  const row = rows.first();
  // One segment carrying all 11 bytes in order, inline (11 <= 32), not three
  // rows and not behind a click.
  await expect(row.getByTestId("binary-hex")).toHaveText(
    [hexOf(a), hexOf(b), hexOf(c)].join(" "),
  );
  await expect(row.getByTestId("binary-parts")).toHaveText("· 3 fragments");
  await expect(row.locator(".binary-toggle")).toHaveCount(0);
  await expect(page.locator('[data-row-kind="line"]')).toHaveCount(1);

  // Traceability: the row spans the first fragment's seq through the last's,
  // and hovering lists every fragment with its own seq, time and size.
  const first = Number(await row.getAttribute("data-seq"));
  const last = Number(await row.getAttribute("data-last-seq"));
  expect(last).toBeGreaterThan(first);
  const title = (await row.locator(".binary-inline").getAttribute("title")) ?? "";
  const fragments = title.split("\n");
  expect(fragments).toHaveLength(3);
  expect(fragments[0]).toContain(`seq ${first} `);
  expect(fragments[0]).toContain("1 byte");
  expect(fragments[1]).toContain("4 bytes");
  expect(fragments[2]).toContain(`seq ${last} `);
  expect(fragments[2]).toContain("6 bytes");
});

test("fragments from one read (same record) also merge into one run", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  // What a wrong-baud board really produces: one read containing several
  // CR/LF bytes, so the daemon assembles it into several short "lines" that
  // share a timestamp (and a seq — see `export.rs` on same-record lines).
  const chunk = Buffer.concat([binLine(1, 0), binLine(4, 1), binLine(6, 5)]);
  await injectLog(daemon!, DEVICE_ID, [rxBytes(chunk)]);

  const rows = page.locator('[data-binary="true"]');
  await expect(rows).toHaveCount(1, { timeout: 10_000 });
  await expect(rows.first().getByTestId("binary-parts")).toHaveText("· 3 fragments");
  await expect(page.locator('[data-row-kind="line"]')).toHaveCount(1);
});

test("a text row between binary rows ends the run", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    rxBytes(binLine(3, 0)),
    ...rxLines(["ok"]),
    rxBytes(binLine(3, 9)),
  ]);
  await expect(page.locator('[data-row-kind="line"]')).toHaveCount(3, { timeout: 10_000 });
  await expect(page.locator('[data-binary="true"]')).toHaveCount(2);
  await expect(page.getByTestId("binary-parts")).toHaveCount(0);
});

test("two binary bursts a moment apart stay two rows", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  // Nothing sits between them, but their similarity to each other is the
  // baud-mismatch tell, and merging them would bury it. Time itself is the
  // thing under test, so this waits a real 100ms (well past the 20ms merge
  // window) between the two injections.
  await injectLog(daemon!, DEVICE_ID, [rxBytes(binLine(4, 20))]);
  const rows = page.locator('[data-binary="true"]');
  await expect(rows).toHaveCount(1, { timeout: 10_000 });
  await new Promise((resolve) => setTimeout(resolve, 100));
  await injectLog(daemon!, DEVICE_ID, [rxBytes(binLine(4, 20))]);
  await expect(rows).toHaveCount(2, { timeout: 10_000 });
  await expect(page.getByTestId("binary-parts")).toHaveCount(0);
});

test("a merged run longer than 32 bytes stays collapsed and expands to all its bytes", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  const parts = [binLine(20, 0), binLine(20, 20), binLine(10, 40)];
  await injectLog(daemon!, DEVICE_ID, parts.map(rxBytes));

  const row = page.locator('[data-binary="true"]');
  await expect(row).toHaveCount(1, { timeout: 10_000 });
  await expect(row).toContainText("50 bytes of binary");
  await expect(row.getByTestId("binary-parts")).toHaveText("· 3 fragments");
  await expect(row.getByTestId("binary-hex")).toHaveCount(0);

  await row.locator(".binary-toggle").click();
  await expect(row.getByTestId("binary-hex")).toHaveText(parts.map(hexOf).join(" "));
  await expect(row.getByTestId("binary-parts")).toHaveText("· 50 bytes in 3 fragments");
});

test("ANSI color codes render as color, never as visible [1;34m noise", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  // What a busybox `ls` on a firmware console actually emits: SGR-colored
  // names with resets, plus an uncolored tail on the same line. The exact
  // regression this guards: ESC is invisible in HTML, so without parsing,
  // the row displayed `[1;34mbin[0m …` (issue observed live on a SigmaStar
  // camera console, 2026-08-06).
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "\u001b[1;34mbin\u001b[0m  \u001b[1;36mlinuxrc\u001b[0m  plain\n" },
  ]);

  const row = page.locator('[data-row-kind="line"]').first();
  // Stripped text, with the run of spaces between columns preserved.
  await expect(row).toHaveText(/bin {2}linuxrc {2}plain/, { timeout: 10_000 });
  await expect(row).not.toContainText("[1;34m");

  // The color must actually land: the styled span picks up a palette
  // foreground different from the row's default text color.
  const colored = row.locator("span", { hasText: "bin" }).last();
  const [spanColor, rowColor] = await Promise.all([
    colored.evaluate((el) => getComputedStyle(el).color),
    row.evaluate((el) => getComputedStyle(el).color),
  ]);
  expect(spanColor).not.toEqual(rowColor);

  // And the filter box matches the *visible* text — the regex runs against
  // the ANSI-stripped string, not the raw bytes.
  await page.getByTestId("filter-input").fill("linuxrc");
  await expect(page.locator('[data-row-kind="line"]')).toHaveCount(1);
});

test("sending from the write bar returns focus to the entry for the next command", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  const input = page.getByTestId("write-input");
  await input.fill("ls");
  await input.press("Enter");

  // `TestBackend` has no writer registered over HTTP (`register_writer` is
  // a Rust-test-only seam — see `web/api.rs`'s module docs), so this send
  // completes through the *error* path. That is fine for what this guards:
  // the entry is disabled while sending, and a `focus()` issued before it
  // re-enables is silently ignored, dumping keyboard focus on <body> after
  // every send — success or failure alike. The error path additionally
  // keeps the rejected payload in place, ready to fix and resend.
  await expect(page.getByTestId("write-error")).toBeVisible({ timeout: 10_000 });
  await expect(input).toHaveValue("ls");
  await expect(input).toBeFocused();
});

test("the write bar ends lines with CR by default and remembers a change per device", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  const ending = page.getByTestId("write-line-ending");
  await expect(ending).toHaveValue("cr");

  await ending.selectOption("crlf");
  await page.reload();
  await expect(page.getByTestId("write-line-ending")).toHaveValue("crlf", { timeout: 10_000 });
});

test("Tab completes paths harvested from device output, shell-style", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "capture thumbnail /mnt/mmc/DCIM/A/REC_001.jpg\n" },
    { kind: "rx", text: "update dcf for /mnt/mmc/THUMBNAIL/B/REC_001.jpg\n" },
  ]);
  // The harvest happens on render, so wait for the lines to land first.
  await expect(page.locator('[data-row-kind="line"]')).toHaveCount(2, { timeout: 10_000 });

  const input = page.getByTestId("write-input");
  await input.fill("ls /mn");
  await input.press("Tab");
  // One candidate at this depth → applied outright, one segment at a time.
  await expect(input).toHaveValue("ls /mnt/");
  await input.press("Tab");
  await expect(input).toHaveValue("ls /mnt/mmc/");

  // Two candidates now: the strip appears and Tab cycles through it.
  await input.press("Tab");
  const strip = page.getByTestId("write-suggestions");
  await expect(strip.locator(".suggestion")).toHaveCount(2);
  await input.press("Tab");
  await expect(input).toHaveValue("ls /mnt/mmc/DCIM/");

  // A bare word completes against path segments (`ls DCIM`-style relative
  // commands), not just absolute prefixes.
  await input.fill("ls THUMB");
  await input.press("Tab");
  await expect(input).toHaveValue("ls THUMBNAIL");
});

test("scrolling up pauses following; the pill count is correct; clicking it returns to the tail", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);

  // More than a viewport's worth (the viewport is a fixed 24rem/22px-rows
  // tall — roughly 17 rows) so there's real scrollable range.
  await injectLog(
    daemon!,
    DEVICE_ID,
    rxLines(Array.from({ length: 80 }, (_, i) => `line ${i}`)),
  );

  const viewport = page.getByTestId("log-viewport");
  await expect(viewport).toHaveAttribute("data-following", "true", { timeout: 10_000 });
  await expect(page.getByTestId("log-row").last()).toContainText("line 79", { timeout: 10_000 });

  const box = await viewport.boundingBox();
  if (!box) throw new Error("live log viewport has no bounding box");
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  // A real wheel gesture — this is what a user scrolling up does, and
  // what the "up-scroll auto-pauses" acceptance criterion means. Not a
  // synthetic `scrollTop` assignment.
  await page.mouse.wheel(0, -400);

  await expect(viewport).toHaveAttribute("data-following", "false", { timeout: 5_000 });
  await expect(page.getByTestId("paused-indicator")).toBeVisible();

  await injectLog(
    daemon!,
    DEVICE_ID,
    rxLines(Array.from({ length: 15 }, (_, i) => `late ${i}`)),
  );
  const pill = page.getByTestId("resume-following-pill");
  await expect(pill).toContainText("15 new lines", { timeout: 10_000 });

  await pill.click();
  await expect(viewport).toHaveAttribute("data-following", "true", { timeout: 5_000 });
  await expect(page.getByTestId("log-row").last()).toContainText("late 14", { timeout: 10_000 });
  await expect(pill).toHaveCount(0);
});

test("DOM node count does not grow linearly with total lines received (virtual scroll)", async ({
  page,
}) => {
  test.setTimeout(60_000);
  await gotoConnectedLiveLog(page);

  async function injectBulk(count: number, offset: number): Promise<void> {
    const CHUNK = 2000;
    for (let start = 0; start < count; start += CHUNK) {
      const n = Math.min(CHUNK, count - start);
      await injectLog(
        daemon!,
        DEVICE_ID,
        rxLines(Array.from({ length: n }, (_, i) => `bulk ${offset + start + i}`)),
      );
    }
  }

  await injectBulk(2_000, 0);
  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), {
      timeout: 20_000,
    })
    .toBeGreaterThanOrEqual(2_000);
  const domCountAt2k = await page.getByTestId("log-row").count();

  await injectBulk(20_000, 2_000);
  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), {
      timeout: 30_000,
    })
    .toBeGreaterThanOrEqual(22_000);
  const domCountAt22k = await page.getByTestId("log-row").count();

  // The point of virtual scrolling: 11x more total lines buffered, but the
  // mounted row count barely moves (a few rows of slack for the exact
  // scroll position at sampling time) instead of scaling with the total.
  expect(domCountAt22k).toBeLessThanOrEqual(domCountAt2k + 5);
  // Sanity bound independent of the comparison above: the fixed 24rem
  // viewport at 22px/row is ~17 rows visible plus 2x10 rows of overscan
  // (`LiveLog.svelte`'s `OVERSCAN`), so lands well under 200 regardless of
  // how many total lines were ever received.
  expect(domCountAt22k).toBeLessThan(200);
});

test("regex filter over ~100k lines completes in <=100ms", async ({ page }) => {
  test.setTimeout(90_000);
  await gotoConnectedLiveLog(page);

  const TOTAL = 100_000;
  const CHUNK = 5_000;
  for (let start = 0; start < TOTAL; start += CHUNK) {
    const ops = rxLines(
      Array.from({ length: CHUNK }, (_, i) => {
        const n = start + i;
        // Every 500th line is a real match, so the filter isn't just
        // scanning to an empty result.
        return n % 500 === 0 ? `NEEDLE ${n}` : `line ${n} filler text`;
      }),
    );
    await injectLog(daemon!, DEVICE_ID, ops);
  }

  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), {
      timeout: 60_000,
    })
    .toBeGreaterThanOrEqual(TOTAL);

  await page.getByTestId("filter-input").fill("NEEDLE");
  // `fill()` dispatches a real `input` event, running the production
  // `applyFilter()` code path (`LiveLog.svelte`) synchronously — the same
  // function call the E2E measures via `filter-elapsed-ms`.
  const elapsedMs = await expect
    .poll(
      async () => {
        const text = await page.getByTestId("filter-elapsed-ms").textContent();
        const value = Number(text);
        return Number.isFinite(value) && value > 0 ? value : null;
      },
      { timeout: 10_000 },
    )
    .not.toBeNull()
    .then(() => page.getByTestId("filter-elapsed-ms").textContent())
    .then((text) => Number(text));

  // The report explicitly asks for the actual measured number, not just
  // pass/fail.
  console.log(`live-log filter over ${TOTAL} lines took ${elapsedMs}ms`);
  expect(elapsedMs).toBeLessThanOrEqual(100);
});

test("sustains >=30fps while a mock device streams 5,000 lines/sec", async ({ page }) => {
  test.setTimeout(30_000);
  await gotoConnectedLiveLog(page);

  const DURATION_MS = 3_000;
  const RATE_PER_SEC = 5_000;
  const BATCH = 100;
  const intervalMs = (BATCH / RATE_PER_SEC) * 1000;

  const fpsPromise = page.evaluate((durationMs) => {
    return new Promise<number>((resolve) => {
      let frames = 0;
      const start = performance.now();
      function tick(): void {
        frames++;
        const elapsed = performance.now() - start;
        if (elapsed < durationMs) {
          requestAnimationFrame(tick);
        } else {
          resolve((frames / elapsed) * 1000);
        }
      }
      requestAnimationFrame(tick);
    });
  }, DURATION_MS);

  const deadline = Date.now() + DURATION_MS;
  let n = 0;
  while (Date.now() < deadline) {
    const batchStart = Date.now();
    const ops = rxLines(Array.from({ length: BATCH }, () => `stream line ${n++}`));
    await injectLog(daemon!, DEVICE_ID, ops);
    const elapsed = Date.now() - batchStart;
    if (elapsed < intervalMs) {
      await new Promise((r) => setTimeout(r, intervalMs - elapsed));
    }
  }

  const fps = await fpsPromise;
  // The report explicitly asks for the actual measured fps, not just
  // pass/fail.
  console.log(`live-log fps while streaming ~${RATE_PER_SEC}/sec: ${fps.toFixed(1)}`);
  // 30fps is the acceptance criterion itself; see the PR report for the
  // actual measured number on this run and on CI, and for why headless
  // Chromium doing simple DOM text updates over a ~40-row virtualized
  // window comfortably clears it with margin.
  expect(fps).toBeGreaterThanOrEqual(30);
});
