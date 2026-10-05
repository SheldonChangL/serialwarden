# serialwrap

[![CI](https://github.com/SheldonChangL/serialwrap/actions/workflows/ci.yml/badge.svg)](https://github.com/SheldonChangL/serialwrap/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platforms: macOS | Linux](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-lightgrey.svg)

**A serial-port broker for firmware development.** One daemon owns the physical serial port. Your terminal, the web UI, scripts, flashing tools, and AI agents all attach to it as clients.

> Opening `screen` means you can't flash.
> Flashing means you miss the boot log.

serialwrap fixes that by never letting a client own the port. The daemon opens the device the moment it enumerates and records everything to an append-only log. Clients read that log; they never hold the file descriptor.

```text
                                  ┌── CLI (tail / write / export)
                                  ├── Web UI  http://127.0.0.1:5590
UART / USB serial ── serialwrap ──┼── MCP agent (Claude Code, …)
                       daemon     ├── your scripts (Unix socket)
                     (records     └── flashing tool ◀── temporary lease:
                      always)                           daemon releases the port,
                                                        records lease_start / lease_end,
                                                        reopens it when the tool exits
```

## What you get

- **The boot log is already there.** Recording starts when the device enumerates, not when you open a terminal, so the first lines after a reset or a flash are captured even if nobody was looking.
- **Flash without closing anything.** `serialwrap run -- esptool.py ...` lends the port to the flasher, then takes it back. Every other client sees a `lease_start`/`lease_end` event in the stream instead of a disconnect, and the post-flash boot log lands in the same log.
- **Many clients, one port.** A CLI `tail -f`, two browser tabs, and an agent can all watch the same board at once.
- **Primitives that suit scripts and agents.** `wait_for(pattern, timeout)`, cursor-based `read_since`, and size-bounded `tail`, instead of a blocking `cat` that never returns.
- **Gated writes.** Agent writes go through a whitelist / approval gate; patterns like `erase`, `efuse`, or bootloader entry always need a human click.
- **One timeline.** RX, TX, config changes, leases, approvals, and client connects all live in the same stream, so "what was the board printing when that was written?" is a single query.
- **Lossless export.** JSONL, plain text, or the raw bytes.
- **Stable device identity.** Devices are keyed by USB serial number, so settings follow the board across replugs and `ttyUSB0 → ttyUSB1` renumbering.

macOS and Linux; one Rust binary with the web UI embedded.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/SheldonChangL/serialwrap/main/install.sh | sh
```

The script downloads the prebuilt binary for your machine (macOS arm64/x86_64, Linux x86_64/aarch64) from the latest [GitHub Release](https://github.com/SheldonChangL/serialwrap/releases), checks its SHA-256 against the release's `SHA256SUMS`, and installs it to `~/.local/bin`. It needs no Rust or Node. If no prebuilt binary exists for your platform, it [builds from source](#building-from-source) instead, which does need Rust and Node.

> **Before the first tagged release** the script has no binary to download, so it builds from source.

You can also download a tarball or `.deb` from the Releases page yourself. If you download in a browser on macOS, clear the quarantine flag on the unsigned binary before running it: `xattr -d com.apple.quarantine ./serialwrap`.

On Linux, see [Linux: permissions](#linux-permissions) for the `dialout` group. On macOS, see [macOS: USB-serial drivers](#macos-usb-serial-drivers).

## Quickstart

```sh
serialwrap daemon                 # foreground; or `serialwrap service install` to start at login
```

Plug in a board, then pick any client:

```sh
serialwrap devices                # what the daemon can see
serialwrap tail -f                # follow the log (one device: no id needed)
open http://127.0.0.1:5590        # web UI: live log, timeline, approvals, export (Linux: xdg-open)
```

Flash without closing either of those:

```sh
serialwrap run -- esptool.py --port "$SERIALWRAP_LEASE_PATH" write_flash 0x0 firmware.bin
```

Give an agent the same board:

```sh
claude mcp add serialwrap -- serialwrap mcp
```

Export what happened:

```sh
serialwrap export --format txt --boot          # since the last (re)connect
serialwrap export --format jsonl --last 10m    # every record, both clocks
```

**No hardware?** `cargo test --all` drives the same recording and query path end to end against a PTY-backed mock device (`crates/mock-device`).

## Why not screen / minicom, or a serial MCP server?

Terminal programs and most serial MCP servers are built around one process that opens the port and owns it. That works fine until a second program needs the same port. serialwrap moves ownership into a daemon so the second, third, and fourth program can join.

| | screen / minicom / picocom | A typical serial MCP server | serialwrap |
|---|---|---|---|
| Who holds the port | The terminal you opened | The server process, usually one per agent | A long-running daemon |
| Capture while nobody is watching | No | Not the usual model | Yes, from enumeration |
| Flash while monitoring | Close the terminal first | Varies by implementation | `serialwrap run` lease; the gap is an explicit event |
| Several clients at once | No; one process holds the port (picocom and minicom use lock files) | Varies (some document single-client, exclusive access) | Yes: CLI, web UI, MCP, scripts |
| Agent-friendly reads (`wait_for`, cursors, bounded output) | No | Varies by implementation | Yes |
| Human approval for risky agent writes | n/a | Usually not described | Yes; danger patterns can't be whitelisted |
| One audit timeline (RX, TX, config, leases, approvals) | Your scrollback or a capture file | Varies | Yes |

Related tools worth knowing: [ser2net](https://github.com/cminyard/ser2net) (serial over TCP), [conserver](https://www.conserver.com/) (multi-user logged consoles, the closest classic design), and `tio --socket` (shares an interactive tio session over a socket). serialwrap's particular combination is daemon-owned recording, a lease for external flashing tools, and an approval gate for agent writes, in one binary.

## Tested against real hardware

The architecture was built test-first against a PTY mock device, and real boards then broke assumptions the mock had baked in. Examples:

- **CR-only line endings.** A Realtek RTL8735B (AmebaPro2) ends most lines with a bare `\r` and its periodic statistics lines with `\n`, in the same stream. The original LF-only line assembly rendered its whole log as binary, and `wait_for` could never match. Lines now end on either byte, and the mock device generates all three conventions so this can't regress silently. ([#52](https://github.com/SheldonChangL/serialwrap/issues/52); commits `1422330`, `0a9481a`, `b617e26`.)
- **Long-running daemons.** On a daemon that had been recording one board for days, `tail` returned old history instead of current output, and the web UI replayed it. Both paths are now bounded to the newest records (`d23dbba`, `5c769e7`).
- **Real flashing.** `serialwrap run` has been used day to day to flash an RTL8735B board through a vendor UART burn tool on Ubuntu 22.04, with the post-flash boot log captured by the same daemon.
- **Still open.** Download-mode traffic at a different baud shows up as screens of short binary chunks, which the web UI doesn't summarize well ([#48](https://github.com/SheldonChangL/serialwrap/issues/48)). The baud suggestion guesses instead of reasoning from what was recorded ([#50](https://github.com/SheldonChangL/serialwrap/issues/50)).

Not yet verified on hardware: custom baud rates such as 74880, DTR-safe open on an Arduino Uno, and a full `esptool` flash. [docs/manual-checklist.md](docs/manual-checklist.md) tracks these and other hardware-only checks. Reports from other boards and adapters are very welcome.

## Limitations

- Early software (0.x). The wire protocol and on-disk format may still change.
- macOS and Linux only. No Windows.
- Local only. The web UI binds to `127.0.0.1` with no authentication or TLS. For remote access, use `ssh -L 5590:localhost:5590 <host>`.
- Timestamps are host-side arrival times, so USB adapter buffering limits their precision (see [Timestamp precision](#timestamp-precision)).
- The MCP bridge doesn't expose `export` yet. Use the CLI or the web UI.

---

## Flashing (lease mode)

`serialwrap run` hands the port to an external tool for the duration of one command, then reclaims it:

```sh
serialwrap run -- esptool.py --port "$SERIALWRAP_LEASE_PATH" write_flash 0x0 firmware.bin
serialwrap run --lease-timeout 600 <device> -- <your vendor flasher> -p "$SERIALWRAP_LEASE_PATH" ...
```

`serialwrap run` doesn't rewrite the flashing tool's arguments. It sets `SERIALWRAP_LEASE_PATH` in the child's environment to the device path the daemon just released, so your command line picks it up explicitly (as above). You can also hardcode a known `/dev/cu.*` or `/dev/ttyUSB*` path. The gap is recorded as a `lease_start`/`lease_end` event, and any other client's `tail -f` sees that event rather than a disconnect. Recording resumes the moment the command exits, so the post-flash boot log is never lost. `--lease-timeout` kills the command and reclaims the port if it hangs; the daemon enforces the same deadline itself, even if the `serialwrap run` process dies.

## Connecting an AI agent (MCP)

```sh
claude mcp add serialwrap -- serialwrap mcp
```

This registers `serialwrap mcp` as a stdio MCP server with Claude Code; any MCP host that can launch a stdio server works the same way. The bridge connects to the same daemon over the same Unix domain socket every other client uses. It registers as `client_type: agent`, which is what routes its writes through the gate described below. Tools exposed: `list_devices`, `get_config`, `tail`, `read_since`, `wait_for`, `write`, `set_config`, `dtr_pulse`. See the [Client protocol wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol#mcp-tool-surface) for each tool's exact shape.

Read results are sized for a context window. `tail` and `read_since` cap their output, fold repeated lines, and summarize binary runs instead of dumping them. `wait_for` matches only fully assembled lines and returns a structured timeout rather than blocking forever.

Every read tool's result is data about the device, never instructions for the agent. See the next section.

## Security model

serialwrap's threat model is narrow: it is a localhost developer tool, not a multi-tenant service. It is organized around one property: **a write to a serial port can be physically irreversible.** An erased bootloader or a blown one-time-programmable fuse doesn't come back the way a deleted file does.

**The write gate has three branches**, evaluated in this priority order:

```
danger pattern?  ──yes──▶  force approval (cannot be whitelisted away)
       │no
whitelist match? ──yes──▶  allow immediately
       │no
                          pending approval (default 60s → deny)
```

- **Danger always wins.** These patterns force human approval even when the same bytes also match a whitelist entry: `erase`, `fuse`/`otp`/`efuse`, `unlock`/`lock`, bootloader-entry sequences, and `format`/`factory_reset`. See `docs/rules.toml.example` for the full built-in list and each pattern's stated reason. The only way to change what counts as dangerous is hand-editing `rules.toml`, never a checkbox on an approval card in the moment.
- **Timeout means deny.** An unattended pending request is denied, never silently allowed, after the configured timeout (60s by default).
- **Humans bypass the gate; agents don't.** A `human` client's writes go straight through, because gating the operator would only teach them to disable the gate. Every write from every client type is still fully audited. That includes the GUI's own write bar (`POST /api/devices/:id/write`), which sends as `human` and appends the same `tx` record `serialwrap write` does. An agent reaching the same daemon over MCP still waits for a human.

**Log content is data, never an instruction**, both for a human reading the GUI and for an agent reading over MCP. Firmware logs routinely contain strings a developer wrote for other humans (`// TODO: reflash with production key before shipping`). They can also relay content verbatim from external peers, such as sensors, BLE, or network links, that serialwrap has no way to vouch for. Every MCP read tool's description says this explicitly. The GUI renders device data (`kind: rx`) and broker-generated events in visually distinct styles, so the boundary is more than a convention. See the [Security-model wiki](https://github.com/SheldonChangL/serialwrap/wiki/Security-model) for the full reasoning.

**Audit is a query over the one event stream, not a second store.** Every write, gate decision, config change, lease, and client (dis)connection lives in the same append-only stream as the device's own log data. So "what was the board doing right when this was approved" is a query (`serialwrap audit --context <seq>`), not a correlation exercise across separate logs. `serialwrap approvals` and `serialwrap audit` are the CLI surface; the GUI's approval card and audit panel call the same daemon API.

The daemon binds its web GUI to `127.0.0.1` only. For remote access, use `ssh -L 5590:localhost:5590 <host>`; there is no network-exposed listener. v1 has no authentication layer or TLS; the wiki explains why that is a deliberate, stated limitation rather than an oversight.

## Linux: permissions

Serial devices are owned by the `dialout` group on most distributions. Add yourself to it once:

```sh
sudo usermod -aG dialout "$USER"
# then log out and back in — group membership doesn't apply to your current session
```

Some minimal or hardened setups lack the distro's generic USB-serial udev rule. If `serialwrap devices` still can't see your adapter after the group change, install the rule template. `install.sh` puts it in `~/.local/share/serialwrap/`; it is also in this repo:

```sh
sudo cp packaging/linux/60-serialwrap.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Reconnect the device after either step.

## macOS: USB-serial drivers

- **CH340/CH341** (common on cheap ESP32/Arduino clones): install [WCH's official driver](https://www.wch-ic.com/downloads/CH341SER_MAC_ZIP.html), then reboot. macOS will likely warn about an unsigned kernel extension the first time; approve it in **System Settings → Privacy & Security**.
- **CP2102/CP210x** (Silicon Labs): install the [CP210x VCP driver](https://www.silabs.com/developer-tools/usb-to-uart-bridge-vcp-drivers), with the same approval step as above.
- **FTDI** (FT232/FT230X): recent macOS versions include a native driver (`AppleUSBFTDI`) for many FTDI chips. If `serialwrap devices` doesn't see the board, install [FTDI's own VCP driver](https://ftdichip.com/drivers/vcp-drivers/) instead.
- **Prolific PL2303**: with one older PL2303 adapter, macOS's built-in driver corrupted RX bytes and dropped the USB connection repeatedly, while the same cable worked on Linux. If an adapter of this family shows garbage at a baud rate you know is right, try Prolific's driver or a different adapter before suspecting the baud setting.

After installing a driver, re-plug the device and confirm with `serialwrap devices`. serialwrap always opens the `/dev/cu.*` node, never `/dev/tty.*`. Keep that in mind when comparing against a manual `screen`/`minicom` session: `tty.*` blocks waiting for a carrier signal that a USB-serial adapter typically never raises.

## Timestamp precision

Every record carries a monotonic clock reading (`t_mono`) and a wall-clock timestamp (`t_wall`). Both are taken **when the daemon reads bytes from the host-side serial port**, not when the device emitted them. They are host-side arrival times and are labeled as such.

**USB buffering distorts them.** A USB-serial adapter's firmware batches bytes before handing them to the host. An FTDI chip's *latency timer* defaults to **16ms**, so up to 16ms of device output can be coalesced into what the daemon sees as a single, later read. Two lines the device emitted 1ms apart can show up in `serialwrap tail` or the GUI as arriving together, or with a gap that reflects USB scheduling rather than firmware timing. The limitation is structural and no daemon-side change fixes it, so serialwrap claims no timing accuracy finer than USB buffering anywhere in its output.

**If you're debugging something timing-sensitive** (an ISR latency question, a race between two log lines), on Linux you can lower an FTDI device's latency timer:

```sh
# find the right device first (replace ttyUSB0 with yours):
cat /sys/bus/usb-serial/devices/ttyUSB0/latency_timer   # current value, ms
echo 1 | sudo tee /sys/bus/usb-serial/devices/ttyUSB0/latency_timer   # 1ms minimum
```

This trades USB bus overhead (more, smaller transfers) for lower coalescing latency, and reverts on replug or reboot. serialwrap ships no persistent equivalent: it is a per-device, per-session tradeoff to make deliberately when timing precision matters for the task at hand, not a default worth changing system-wide. CH340/CP210x-family chips buffer comparably but don't expose an equivalent tunable through sysfs.

## Building from source

You need a C toolchain, Rust (via [rustup](https://rustup.rs)), and Node.js 22+.

**macOS:**

```sh
xcode-select --install                    # cc/linker, if you don't already have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
# Node.js 22+ — via nvm, or https://nodejs.org
```

**Linux (Debian/Ubuntu):**

```sh
sudo apt-get update && sudo apt-get install -y build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
# Node.js 22+ — via https://github.com/nodesource/distributions or nvm
```

Then:

```sh
git clone https://github.com/SheldonChangL/serialwrap.git
cd serialwrap
(cd webui && npm ci && npm run build)     # the web GUI, embedded into the binary
cargo build --release -p serialwrap
cp target/release/serialwrap ~/.local/bin/
```

**Do not skip the `npm run build` step.** If `webui/dist/` is missing, `crates/serialwrapd/build.rs` writes a placeholder page there so that a Rust-only checkout can still `cargo build`. A binary built that way embeds the placeholder instead of the real GUI, and every `cargo build` prints a `cargo:warning` for as long as the placeholder is what's embedded. CI and the release workflow always build the frontend first.

`serialwrap service install` writes and loads a `launchd` user agent on macOS (`~/Library/LaunchAgents/com.serialwrap.daemon.plist`) or a `systemctl --user` unit on Linux (`~/.config/systemd/user/com.serialwrap.daemon.service`). Pass `--dry-run` to preview the generated file without writing or registering anything. `serialwrap service uninstall` reverses it.

**Upgrading:** stop the service, replace the binary, start it again. On Linux, `cp` over a binary that is still running (the daemon, or an MCP bridge an agent host started) fails with `Text file busy`; re-run `install.sh`, or copy to a new name and `mv` it into place. Recorded data lives outside the binary and is kept across upgrades.

## Documentation

- [Wiki](https://github.com/SheldonChangL/serialwrap/wiki): architecture, event stream and storage schema, client protocol, security model, UX design.
- [CONTRIBUTING.md](CONTRIBUTING.md): building, the test suites, and what makes a useful hardware bug report.
- [docs/manual-checklist.md](docs/manual-checklist.md): hardware-dependent acceptance items that CI can't verify (real baud timing, real DTR/RTS behavior, a genuinely clean install VM), with who verified what and when.
- [TASKS.md](TASKS.md): the task-by-task breakdown and acceptance criteria the project was built against.
- `packaging/`: the udev rule template and the Homebrew formula renderer used by the release workflow.

## License

MIT
