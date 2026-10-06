# Timestamp precision

Every record carries a monotonic clock reading (`t_mono`) and a wall-clock timestamp (`t_wall`). Both are taken **when the daemon reads bytes from the host-side serial port**, not when the device emitted them. They are host-side arrival times and are labeled as such.

**USB buffering distorts them.** A USB-serial adapter's firmware batches bytes before handing them to the host. An FTDI chip's *latency timer* defaults to **16ms**, so up to 16ms of device output can be coalesced into what the daemon sees as a single, later read. Two lines the device emitted 1ms apart can show up in `serialwarden tail` or the GUI as arriving together, or with a gap that reflects USB scheduling rather than firmware timing. The limitation is structural and no daemon-side change fixes it, so SerialWarden claims no timing accuracy finer than USB buffering anywhere in its output.

**If you're debugging something timing-sensitive** (an ISR latency question, a race between two log lines), on Linux you can lower an FTDI device's latency timer:

```sh
# find the right device first (replace ttyUSB0 with yours):
cat /sys/bus/usb-serial/devices/ttyUSB0/latency_timer   # current value, ms
echo 1 | sudo tee /sys/bus/usb-serial/devices/ttyUSB0/latency_timer   # 1ms minimum
```

This trades USB bus overhead (more, smaller transfers) for lower coalescing latency, and reverts on replug or reboot. SerialWarden ships no persistent equivalent: it is a per-device, per-session tradeoff to make deliberately when timing precision matters for the task at hand, not a default worth changing system-wide. CH340/CP210x-family chips buffer comparably but don't expose an equivalent tunable through sysfs.
