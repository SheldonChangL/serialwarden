---
name: Bug report
about: Something serialwrap did wrong, especially with a real board or adapter
labels: bug
---

**What happened, and what you expected instead**

**Steps to reproduce**

**Environment**

- OS and version:
- `serialwrap --version`:
- Board / SoC:
- USB-serial adapter chip (FTDI, CP210x, CH340, PL2303, native USB CDC, …):
- Baud and framing (`serialwrap config <device>`):
- Which client showed the problem (CLI, web UI, MCP agent, `serialwrap run`):

**Capture (optional but very helpful)**

`serialwrap export --format bin --last 1m -o capture.bin` for raw bytes, or `--format jsonl` when timing or events matter. Check it for keys, credentials, and internal hostnames before attaching.
