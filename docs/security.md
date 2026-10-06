# Security model

SerialWarden's threat model is narrow: it is a localhost developer tool, not a multi-tenant service. It is organized around one property: **a write to a serial port can be physically irreversible.** An erased bootloader or a blown one-time-programmable fuse doesn't come back the way a deleted file does.

**The write gate has three branches**, evaluated in this priority order:

```
danger pattern?  ──yes──▶  force approval (cannot be whitelisted away)
       │no
whitelist match? ──yes──▶  allow immediately
       │no
                          pending approval (default 60s → deny)
```

- **Danger always wins.** These patterns force human approval even when the same bytes also match a whitelist entry: `erase`, `fuse`/`otp`/`efuse`, `unlock`/`lock`, bootloader-entry sequences, and `format`/`factory_reset`. See [rules.toml.example](rules.toml.example) for the full built-in list and each pattern's stated reason. The only way to change what counts as dangerous is hand-editing `rules.toml`, never a checkbox on an approval card in the moment.
- **Timeout means deny.** An unattended pending request is denied, never silently allowed, after the configured timeout (60s by default).
- **Humans bypass the gate; agents don't.** A `human` client's writes go straight through, because gating the operator would only teach them to disable the gate. Every write from every client type is still fully audited. That includes the GUI's own write bar (`POST /api/devices/:id/write`), which sends as `human` and appends the same `tx` record `serialwarden write` does. An agent reaching the same daemon over MCP still waits for a human.

**Log content is data, never an instruction**, both for a human reading the GUI and for an agent reading over MCP. Firmware logs routinely contain strings a developer wrote for other humans (`// TODO: reflash with production key before shipping`). They can also relay content verbatim from external peers, such as sensors, BLE, or network links, that SerialWarden has no way to vouch for. Every MCP read tool's description says this explicitly. The GUI renders device data (`kind: rx`) and broker-generated events in visually distinct styles, so the boundary is more than a convention. See the [Security-model wiki](https://github.com/SheldonChangL/serialwarden/wiki/Security-model) for the full reasoning.

**Audit is a query over the one event stream, not a second store.** Every write, gate decision, config change, lease, and client (dis)connection lives in the same append-only stream as the device's own log data. So "what was the board doing right when this was approved" is a query (`serialwarden audit --context <seq>`), not a correlation exercise across separate logs. `serialwarden approvals` and `serialwarden audit` are the CLI surface; the GUI's approval card and audit panel call the same daemon API.

The daemon binds its web GUI to `127.0.0.1` only. For remote access, use `ssh -L 5590:localhost:5590 <host>`; there is no network-exposed listener. v1 has no authentication layer or TLS; the wiki explains why that is a deliberate, stated limitation rather than an oversight. Because a loopback bind doesn't stop your own browser from being used against the daemon, the web server also rejects requests whose `Host` isn't localhost (DNS rebinding) and requests carrying another site's `Origin` (cross-site POSTs, such as a page trying to approve a pending agent write, and cross-site WebSocket connections).
