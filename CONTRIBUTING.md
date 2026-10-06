# Contributing to SerialWarden

Bug reports from real boards and adapters are the most valuable contribution right now. The test suite runs against a PTY mock device, and several of the most important fixes so far came from real hardware doing something the mock didn't (see the README's "Tested against real hardware").

## Reporting a hardware bug

Use the **Bug report** issue template. The things that make a serial bug reproducible:

- OS and version, and `serialwarden --version` (it includes the git commit the binary was built from)
- the board/SoC and the USB-serial adapter chip (FTDI, CP210x, CH340, PL2303, native USB CDC, …)
- baud and framing (`serialwarden config <device>`)
- a short raw capture: `serialwarden export --format bin --last 1m -o capture.bin`, or `--format jsonl` if timing or events matter

Check a capture for anything private (keys, Wi-Fi credentials, internal hostnames) before attaching it. Firmware logs often contain more than you expect.

## Building

You need Rust (via rustup), Node.js 22+, and a C toolchain. Build the frontend before the daemon, because the web UI is embedded at compile time:

```sh
(cd webui && npm ci && npm run build)
cargo build -p serialwarden
```

## Running the checks CI runs

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --all
cargo test --all -- --ignored            # long-running acceptance runs

cd webui
npm run lint
npm run check
cargo build --release -p serialwarden      # E2E drives the real release binary
npx playwright install chromium          # first time only
npx playwright test --config=e2e/playwright.config.ts
```

CI runs the Rust jobs on both macOS and Linux. Tests must not use sleeps or wall-clock timing as synchronization; TASKS.md explains the discipline and why it matters for CI.

## Hardware-only checks

Some behavior can't be tested without a real device: custom baud rates, DTR/RTS behavior on open, real flashing through a lease. [docs/manual-checklist.md](docs/manual-checklist.md) lists these. If you run one, add the date, platform, device, and result under that item.

## Design docs

The [wiki](https://github.com/SheldonChangL/serialwarden/wiki) holds the architecture, record schema, client protocol, and security model. A change to the wire protocol, the record format, or the write gate's behavior should update the matching wiki page in the same change.

## Pull requests

- One logical change per PR, with a conventional-commit subject (`fix(daemon): …`, `feat(webui): …`, `docs: …`).
- New behavior comes with a test. A fix for a hardware-found bug should make the mock device reproduce it, so the bug can't come back silently.
- Don't weaken the write gate's defaults (danger patterns, timeout-means-deny, agents-are-gated) as a side effect of another change. Propose that separately, with the reasoning.
