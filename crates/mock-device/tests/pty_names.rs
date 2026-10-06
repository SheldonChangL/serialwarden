//! Resolving a new PTY's slave path must not fail under concurrency.
//!
//! On macOS, `ttyname_r` looks the device up in devfs, and a PTY created a
//! moment ago by another thread can be missing from that lookup, which
//! `ttyname_r` reports as `ERANGE` ("Result too large"). It surfaced as
//! intermittent `open mock device` failures whenever test binaries created
//! PTYs in parallel.

use std::thread;

use mock_device::MockDevice;

#[test]
fn many_devices_opened_concurrently_all_resolve_their_paths() {
    let threads: Vec<_> = (0..16)
        .map(|_| {
            thread::spawn(|| {
                for _ in 0..25 {
                    let device = MockDevice::new().expect("open mock device");
                    assert!(
                        device.slave_path().to_string_lossy().starts_with("/dev/"),
                        "unexpected slave path {:?}",
                        device.slave_path()
                    );
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("a thread failed to open a mock device");
    }
}
