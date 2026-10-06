# Demo recording script

A 20–30 second recording for the top of the README and for launch posts. It must be recorded on **real hardware**: the lease and the post-flash boot log are the point, and the test backend can't produce a real lease. Don't present a simulated recording as this demo.

## Setup

- A board that prints a recognizable boot banner and can be flashed over the same USB-serial port (an ESP32 dev board with `esptool.py` is the most familiar choice for viewers).
- Terminal at about 100×30, a large font, and a clean prompt. Browser at about 1280×720.
- The daemon already running (`serialwarden service install`, or `serialwarden daemon` in a hidden tab), and the board already plugged in so its log is flowing.
- Pre-build the firmware so the flash itself is short. Set `--lease-timeout` generously.

Layout: browser (web UI) on the left half, two terminal panes on the right: top for the agent, bottom for the flasher.

## Shot list

| t (s) | Screen | What it shows |
|---|---|---|
| 0–4 | Web UI log streaming; bottom pane `serialwarden tail -f` | Two clients on one port, live |
| 4–8 | Top pane: agent calls `wait_for` with `pattern: "boot:|rst:"` (Claude Code using the MCP tool, or a small script against the CLI) | The agent waits without blocking anyone else |
| 8–10 | Bottom pane: `serialwarden run -- esptool.py --port "$SERIALWARDEN_LEASE_PATH" write_flash 0x0 build/app.bin` | No terminal was closed first |
| 10–12 | Web UI timeline shows `lease_start`; the tail pane prints the event | The gap is explicit, not a silent disconnect |
| 12–20 | esptool progress (speed up the middle in the edit, and label it as sped up) | A real flash |
| 20–23 | `lease_end` with exit code; port reclaimed | The daemon takes the port back by itself |
| 23–27 | Boot banner appears immediately in the web UI and the tail | The post-flash boot log isn't lost |
| 27–30 | Agent's `wait_for` returns the matching boot line | The agent kept going across the flash |

Optional second clip (10 s): the agent writes `erase_flash` (or anything that matches a danger pattern), the web UI shows the approval card, and you click **Deny**. It shows the gate without needing any narration.

## Recording and export

- macOS: QuickTime screen recording, or OBS. Linux: OBS or `wf-recorder`.
- Export an MP4 (for launch posts) and a GIF under 10 MB (for the README). For example: `ffmpeg -i demo.mp4 -vf "fps=12,scale=960:-1:flags=lanczos" -loop 0 demo.gif`, or `gifski` for better quality.
- Put the GIF at `docs/media/demo.gif` and add it to the README directly under the architecture diagram, with a one-line caption naming the board and the flasher used.

## Honesty rules for the edit

- Name the board, adapter, and OS in the caption.
- Label anything sped up.
- Don't cut out a failed `wait_for` or an error and re-splice. Re-record instead.
