# SerialWarden

[![CI](https://github.com/SheldonChangL/serialwarden/actions/workflows/ci.yml/badge.svg)](https://github.com/SheldonChangL/serialwarden/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SheldonChangL/serialwarden)](https://github.com/SheldonChangL/serialwarden/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

English | [繁體中文](README.zh-TW.md)

**A serial-port broker for firmware development.** One daemon owns the serial port and records everything. Your terminal, a browser, flashing tools, and AI agents all attach to it as clients, locally or from another machine.

```text
                                    ┌── CLI (tail / write / export)
                                    ├── Web UI, local or over ssh -L
UART / USB serial ── serialwarden ──┼── AI agent over MCP, local or over ssh
                       daemon       ├── your scripts
                     (records       └── flashing tool ◀── temporary lease
                      always)
```

![SerialWarden demo: typing into a remote board from the browser, denying an AI agent's erase request, and a flashing tool borrowing the port](docs/media/demo.gif)

<sub>Recorded on real hardware: a Realtek Ameba board on a CH340 adapter, plugged into Ubuntu 22.04, with the browser connected over an SSH tunnel. The lease step uses a stand-in script that holds the port for 4 seconds and writes nothing; it isn't a real flash.</sub>

## The problems it solves

- **"I can't flash while the monitor is open, and when I reopen it the boot log is gone."** The daemon holds the port and records from the moment the board enumerates. `serialwarden run -- esptool.py ...` lends the port to the flasher and takes it back when the tool exits, so the post-flash boot log lands in the same log, with the gap marked.
- **"The board is plugged into the lab machine, and I'm on my laptop."** Forward one port with `ssh -L` and you get the full web UI in your browser: the live log, a write bar to type commands (with history, CR/LF/CRLF selection, hex mode, and Tab completion from paths the device has printed), the timeline, and export.
- **"I want an AI agent to watch the same console I'm watching."** The agent connects over MCP, locally or with `ssh lab-host serialwarden mcp`, and reads the same stream you see in the browser. Its writes show up in your log attributed to it, and each one waits for you to click **Allow once** in the web UI unless you've whitelisted it. Risky commands such as `erase`, `efuse`, or bootloader entry always wait, whitelist or not. Its reads are bounded, so it neither blocks forever nor floods its context.
- **"What did the board print last night when it rebooted?"** Recording doesn't depend on anyone having a terminal open. Export any window as text, JSONL, or raw bytes.
- **"Which ttyUSB is it today?"** Devices are identified by USB serial number when the adapter reports one, so settings follow the board across replugs. Many CH340s don't report one; those are keyed by port path.

macOS and Linux, as a single binary with the web UI built in.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/SheldonChangL/serialwarden/main/install.sh | sh
```

Installs the prebuilt binary for macOS (arm64/x86_64) or Linux (x86_64/aarch64) from the [latest release](https://github.com/SheldonChangL/serialwarden/releases) into `~/.local/bin`, after verifying its SHA-256. If there's no prebuilt binary for your platform, it builds from source instead, which needs Rust and Node 22+. There's also a `.deb`. On Linux you'll need to be in the `dialout` group; on macOS some adapters need a vendor driver. See [docs/setup.md](docs/setup.md).

## Quickstart

```sh
serialwarden service install      # start the daemon now and at every login (or run it in the foreground: serialwarden daemon)
serialwarden devices              # what's plugged in
serialwarden tail -f              # follow the log in the terminal
open http://127.0.0.1:5590        # or the web UI (Linux: xdg-open)
```

Flash without closing anything:

```sh
serialwarden run -- esptool.py --port "$SERIALWARDEN_LEASE_PATH" write_flash 0x0 firmware.bin
```

Board on another machine (`lab-host`, with the SerialWarden daemon running there, and key-based SSH):

```sh
ssh -N -L 15590:localhost:5590 lab-host              # then open http://127.0.0.1:15590
claude mcp add lab-board -- ssh lab-host '~/.local/bin/serialwarden' mcp   # let an agent read it too
```

Agent on the same machine: `claude mcp add serialwarden -- serialwarden mcp`. Any MCP host that can launch a stdio server works.

More in [docs/usage.md](docs/usage.md): remote boards, flashing, and the MCP tools.

## Why not screen / minicom, or a serial MCP server?

Those are built around one process that opens the port and owns it. That works until a second program needs the port.

| | screen / minicom / picocom | A typical serial MCP server | SerialWarden |
|---|---|---|---|
| Records while nobody is watching | No | Not the usual model | Yes, from enumeration |
| Flash while monitoring | Close it first | Varies | Lease; the gap is an explicit event |
| Several clients at once (you, a teammate, an agent) | No | Varies; often one client | Yes |
| Use it from another machine | Via SSH + a terminal session | Varies | Web UI over `ssh -L`, MCP over `ssh` |
| Human approval for risky agent writes | n/a | Usually not described | Yes |

Related tools: [ser2net](https://github.com/cminyard/ser2net), [conserver](https://www.conserver.com/) (the closest classic design), `tio --socket`.

## Status

Early (v0.x): the client protocol and on-disk format may still change. It's used day to day to monitor and flash an RTL8735B board on Ubuntu 22.04. Real hardware has already caught things the mock-device tests missed. One example: a Realtek RTL8735B that mixes CR-only and LF line endings in one stream. The tests now generate both. Not yet verified on hardware: custom baud rates such as 74880, DTR-safe open on an Arduino Uno, and a full `esptool` flash. See [docs/manual-checklist.md](docs/manual-checklist.md).

Limitations:

- macOS and Linux only.
- The web UI has no login. It listens on localhost only and rejects cross-site requests; remote access is through SSH.
- Timestamps are host arrival time, so USB adapter buffering limits their precision ([docs/timestamps.md](docs/timestamps.md)).

Upgrading from the old name `serialwrap`? See [docs/setup.md](docs/setup.md#upgrading-from-serialwrap). Recorded data migrates automatically.

## Docs

- [docs/usage.md](docs/usage.md): remote boards, flashing, AI agents
- [docs/setup.md](docs/setup.md): install options, Linux permissions, macOS drivers, service, upgrading, building from source
- [docs/security.md](docs/security.md): the write gate, approvals, and why device output is treated as data, not instructions
- [Wiki](https://github.com/SheldonChangL/serialwarden/wiki): architecture, event stream format, client protocol
- [CONTRIBUTING.md](CONTRIBUTING.md): building, tests, and reporting hardware bugs

MIT licensed.
