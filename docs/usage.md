# Using SerialWarden

Details behind the [README](../README.md)'s quickstart: working with a board on another machine, flashing through a lease, and connecting AI agents over MCP.

## A board on another machine

The daemon's web UI and socket only listen on the machine the board is plugged into. To work with it from your laptop, forward the web port over SSH. Pick a free local port, so a local daemon on 5590 can keep running:

```sh
ssh -N -L 15590:localhost:5590 lab-host
```

Then open `http://127.0.0.1:15590`. It's the full web UI: the live log, the write bar, approvals, and export. To keep the tunnel up across sleeps and network changes, run it under launchd or systemd with automatic restart.

An agent on your laptop reaches the remote board by running the MCP bridge over SSH. No tunnel is needed for this, because the bridge talks to the daemon's local socket on the remote host:

```sh
claude mcp add lab-board -- ssh lab-host serialwarden mcp
```

Use the binary's full path (for example `/home/you/.local/bin/serialwarden`) if `~/.local/bin` isn't on the remote host's non-interactive `PATH`. To flash a remote board, run `serialwarden run` there over SSH. Single-quote the command so `$SERIALWARDEN_LEASE_PATH` expands on the remote side:

```sh
ssh lab-host 'serialwarden run -- esptool.py --port "$SERIALWARDEN_LEASE_PATH" write_flash 0x0 firmware.bin'
```

## Flashing (lease mode)

`serialwarden run` hands the port to an external tool for the duration of one command, then reclaims it:

```sh
serialwarden run -- esptool.py --port "$SERIALWARDEN_LEASE_PATH" write_flash 0x0 firmware.bin
serialwarden run --lease-timeout 600 <device> -- <your vendor flasher> -p "$SERIALWARDEN_LEASE_PATH" ...
```

`serialwarden run` doesn't rewrite the flashing tool's arguments. It sets `SERIALWARDEN_LEASE_PATH` in the child's environment to the device path the daemon just released, so your command line picks it up explicitly (as above). You can also hardcode a known `/dev/cu.*` or `/dev/ttyUSB*` path. The gap is recorded as a `lease_start`/`lease_end` event, and any other client's `tail -f` sees that event rather than a disconnect. Recording resumes the moment the command exits, so the post-flash boot log is never lost. `--lease-timeout` kills the command and reclaims the port if it hangs; the daemon enforces the same deadline itself, even if the `serialwarden run` process dies.

## Connecting an AI agent (MCP)

```sh
claude mcp add serialwarden -- serialwarden mcp
```

This registers `serialwarden mcp` as a stdio MCP server with Claude Code; any MCP host that can launch a stdio server works the same way. The bridge connects to the same daemon over the same Unix domain socket every other client uses. It registers as `client_type: agent`, which is what routes its writes through the gate described below. Tools exposed: `list_devices`, `get_config`, `tail`, `read_since`, `wait_for`, `write`, `set_config`, `dtr_pulse`. See the [Client protocol wiki](https://github.com/SheldonChangL/serialwarden/wiki/Client-protocol#mcp-tool-surface) for each tool's exact shape.

Read results are sized for a context window. `tail` and `read_since` cap their output, fold repeated lines, and summarize binary runs instead of dumping them. `wait_for` matches only fully assembled lines and returns a structured timeout rather than blocking forever.

Every read tool's result is data about the device, never instructions for the agent. See [security.md](security.md).
