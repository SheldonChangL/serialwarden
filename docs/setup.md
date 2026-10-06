# Setup and operations

Install details, OS permissions and drivers, running the daemon as a service, upgrading, and building from source. The [README](../README.md) has the one-line install.

## Install options

You can also download a tarball or `.deb` from the Releases page yourself. If you download in a browser on macOS, clear the quarantine flag on the unsigned binary before running it: `xattr -d com.apple.quarantine ./serialwarden`.

## Linux: permissions

Serial devices are owned by the `dialout` group on most distributions. Add yourself to it once:

```sh
sudo usermod -aG dialout "$USER"
# then log out and back in — group membership doesn't apply to your current session
```

Some minimal or hardened setups lack the distro's generic USB-serial udev rule. If `serialwarden devices` still can't see your adapter after the group change, install the rule template. `install.sh` puts it in `~/.local/share/doc/serialwarden/`; it is also in this repo:

```sh
sudo cp packaging/linux/60-serialwarden.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Reconnect the device after either step.

## macOS: USB-serial drivers

- **CH340/CH341** (common on cheap ESP32/Arduino clones): install [WCH's official driver](https://www.wch-ic.com/downloads/CH341SER_MAC_ZIP.html), then reboot. macOS will likely warn about an unsigned kernel extension the first time; approve it in **System Settings → Privacy & Security**.
- **CP2102/CP210x** (Silicon Labs): install the [CP210x VCP driver](https://www.silabs.com/developer-tools/usb-to-uart-bridge-vcp-drivers), with the same approval step as above.
- **FTDI** (FT232/FT230X): recent macOS versions include a native driver (`AppleUSBFTDI`) for many FTDI chips. If `serialwarden devices` doesn't see the board, install [FTDI's own VCP driver](https://ftdichip.com/drivers/vcp-drivers/) instead.
- **Prolific PL2303**: with one older PL2303 adapter, macOS's built-in driver corrupted RX bytes and dropped the USB connection repeatedly, while the same cable worked on Linux. If an adapter of this family shows garbage at a baud rate you know is right, try Prolific's driver or a different adapter before suspecting the baud setting.

After installing a driver, re-plug the device and confirm with `serialwarden devices`. SerialWarden always opens the `/dev/cu.*` node, never `/dev/tty.*`. Keep that in mind when comparing against a manual `screen`/`minicom` session: `tty.*` blocks waiting for a carrier signal that a USB-serial adapter typically never raises.

## Running as a service

`serialwarden service install` writes and loads a `launchd` user agent on macOS (`~/Library/LaunchAgents/com.serialwarden.daemon.plist`) or a `systemctl --user` unit on Linux (`~/.config/systemd/user/com.serialwarden.daemon.service`). Pass `--dry-run` to preview the generated file without writing or registering anything. `serialwarden service uninstall` reverses it.

## Upgrading

stop the service, replace the binary, start it again. On Linux, `cp` over a binary that is still running (the daemon, or an MCP bridge an agent host started) fails with `Text file busy`; re-run `install.sh`, or copy to a new name and `mv` it into place. Recorded data lives outside the binary and is kept across upgrades.

### Upgrading from serialwrap

This project was called `serialwrap` until October 2026. The binary, environment variables (`SERIALWRAP_*` → `SERIALWARDEN_*`), socket, service label, and data directories all use the new name. To move an existing install:

```sh
serialwrap service uninstall      # with the old binary, if you installed the service
# install serialwarden (see the README's Install section), then:
serialwarden service install      # or run `serialwarden daemon` once
claude mcp remove serialwrap && claude mcp add serialwarden -- serialwarden mcp
```

On its first start, the daemon moves the old data directory (recordings and device profiles) and config directory (`rules.toml`) to the new locations, as long as the new ones don't exist yet and no old daemon is still running. The old `~/.serialwrap/` socket directory on macOS can be deleted afterwards.

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
git clone https://github.com/SheldonChangL/serialwarden.git
cd serialwarden
(cd webui && npm ci && npm run build)     # the web GUI, embedded into the binary
cargo build --release -p serialwarden
cp target/release/serialwarden ~/.local/bin/
```

**Do not skip the `npm run build` step.** If `webui/dist/` is missing, `crates/serialwardend/build.rs` writes a placeholder page there so that a Rust-only checkout can still `cargo build`. A binary built that way embeds the placeholder instead of the real GUI, and every `cargo build` prints a `cargo:warning` for as long as the placeholder is what's embedded. CI and the release workflow always build the frontend first.
