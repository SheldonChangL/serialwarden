//! Acceptance tests for T1.3 (port I/O and config core, issue #5) against
//! the real PTY-backed mock-device fixture (T0.2) — the tests here need a
//! genuine, independently-openable device path, unlike the sleep-free unit
//! tests in `src/port_config.rs`/`src/port_io.rs`/`src/error_counts.rs`/
//! `src/device_profile.rs` that use plain data or fake regular files.
//!
//! Covers: profile persistence + automatic re-application across a real
//! disconnect/reconnect cycle (acceptance criterion 4), `PortConfigApi`'s
//! error behavior for unknown/disconnected devices, and — Linux only — a
//! real `TCGETS2`/`TCSETS2` round-trip proving the `BOTHER` path actually
//! accepts 74880 through a real ioctl call, not just this crate's own pure
//! encoding function (see `src/port_config.rs`'s module docs for why the
//! equivalent macOS `IOSSIOSPEED` round-trip cannot be automated at all —
//! it fails `ENOTTY` against any PTY, verified empirically during this
//! task).

use std::io;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use mock_device::MockDevice;
use serialwardend::port::testing::ScriptedEnumerator;
use serialwardend::port::{
    DeviceId, EnumeratedDevice, HotplugConfig, HotplugDetector, UsbMetadata,
};
use serialwardend::port_config::{FlowControl, PortConfig};
use serialwardend::recorder::RecorderConfig;
use warden_proto::Record;

fn poll_until(
    detector: &mut HotplugDetector,
    timeout: Duration,
    mut check: impl FnMut(&mut HotplugDetector) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let _ = detector.poll_once();
        if check(detector) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn tiny_poll_config() -> HotplugConfig {
    HotplugConfig {
        poll_interval: Duration::from_millis(5),
        recorder_config: RecorderConfig::default(),
    }
}

fn event_count(records: &[Record], event_name: &str) -> usize {
    records
        .iter()
        .filter(|r| matches!(r, Record::Event { event, .. } if event == event_name))
        .count()
}

fn config_change_new_bauds(records: &[Record]) -> Vec<u64> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == "config_change" => extra
                .get("new")
                .and_then(|v| v.get("baud"))
                .and_then(|v| v.as_u64()),
            _ => None,
        })
        .collect()
}

/// Acceptance criterion 4: a config saved via
/// `PortConfigApi::set_port_config` must be automatically re-applied the
/// next time the *same device* reconnects — proven here through the real
/// `HotplugDetector` connect/disconnect/reconnect flow (`device_profile.rs`'s
/// own unit tests already cover `ProfileStore` save/load in isolation; this
/// proves the wiring on top of it).
#[test]
fn set_port_config_persists_and_is_reapplied_after_reconnect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x1a86,
        pid: 0x7523,
        serial_number: Some("PROFILE-PERSIST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");
    let old_path = device.slave_path().to_path_buf();

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: old_path.clone(),
        usb: Some(usb.clone()),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        tiny_poll_config(),
    );

    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );

    // Explicitly set a non-default, non-standard config while connected.
    let api = detector.port_config_api();
    let custom = PortConfig {
        baud: 74_880,
        flow_control: FlowControl::Hardware,
        ..PortConfig::default()
    };
    api.set_port_config(&id, custom.clone(), "test:persist")
        .expect("set_port_config on a connected, known device must succeed");

    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                event_count(&r.read_since(0, usize::MAX).unwrap().records, "disconnect") == 1
            })
        }),
        "expected a disconnect event"
    );

    device.reconnect().expect("reconnect");
    let new_path = device.slave_path().to_path_buf();
    enumerator.replace_path(&old_path, new_path);

    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                event_count(&r.read_since(0, usize::MAX).unwrap().records, "connect") == 2
            })
        }),
        "expected a reconnect (second connect event)"
    );

    let recorders = detector.recorders();
    let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let bauds = config_change_new_bauds(&records);
    // Sequence: [initial connect -> default 9600, explicit set -> 74880,
    // reconnect -> must be 74880 again, not reset to the 9600 default].
    assert_eq!(
        bauds.last().copied(),
        Some(74_880),
        "reconnect must re-apply the persisted 74880 baud, not the 9600 default; full config_change baud sequence was {bauds:?}"
    );

    // And the profile really did hit disk under this device's own
    // directory (not just held in memory) — `device_profile.rs`'s own
    // tests check `ProfileStore` in isolation; this confirms the full
    // wiring actually calls `save`.
    let profile_path = tmp
        .path()
        .join("data")
        .join("devices")
        .join(&id.0)
        .join("profile.json");
    assert!(profile_path.is_file(), "expected {profile_path:?} to exist");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&profile_path).unwrap()).unwrap();
    assert_eq!(saved["config"]["baud"], 74_880);
}

/// `config_change` records filtered to one `changed_by`.
fn config_changes_by<'a>(
    records: &'a [Record],
    changed_by: &str,
) -> Vec<&'a serde_json::Map<String, serde_json::Value>> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. }
                if event == "config_change"
                    && extra.get("changed_by").and_then(|v| v.as_str()) == Some(changed_by) =>
            {
                Some(extra)
            }
            _ => None,
        })
        .collect()
}

/// Issue #51, against the real `HotplugDetector`/`PortConfigApi` (not the
/// `TestBackend` double): one `set_port_config` call records exactly one
/// `config_change`; repeating it with the configuration already in effect
/// records none; and each (re)connect records exactly one `system:connect`
/// profile application with `old: null`.
#[test]
fn config_change_is_recorded_once_per_real_change_and_once_per_connect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x067b,
        pid: 0x2303,
        serial_number: Some("ISSUE-51".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");
    let old_path = device.slave_path().to_path_buf();

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: old_path.clone(),
        usb: Some(usb),
    });
    let mut detector = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );
    let recorder = Arc::clone(detector.recorders().lock().unwrap().get(&id).unwrap());
    let records = || recorder.read_since(0, usize::MAX).unwrap().records;

    let connects = config_changes_by(&records(), "system:connect").len();
    assert_eq!(
        connects, 1,
        "first connect must record exactly one profile application"
    );
    assert!(config_changes_by(&records(), "system:connect")[0]["old"].is_null());

    let api = detector.port_config_api();
    let custom = PortConfig {
        baud: 115_200,
        ..PortConfig::default()
    };
    api.set_port_config(&id, custom.clone(), "test:once")
        .expect("set_port_config");
    let changes = config_changes_by(&records(), "test:once")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        changes.len(),
        1,
        "one set_port_config, one config_change: {changes:?}"
    );
    assert_eq!(changes[0]["old"]["baud"], 9600);
    assert_eq!(changes[0]["new"]["baud"], 115_200);

    // The same configuration again is a no-op, not a second change.
    api.set_port_config(&id, custom.clone(), "test:once")
        .expect("repeat set_port_config");
    assert_eq!(
        config_changes_by(&records(), "test:once").len(),
        1,
        "old == new must not append a config_change"
    );

    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |_| event_count(
            &records(),
            "disconnect"
        ) == 1),
        "expected a disconnect event"
    );
    device.reconnect().expect("reconnect");
    enumerator.replace_path(&old_path, device.slave_path().to_path_buf());
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |_| event_count(
            &records(),
            "connect"
        ) == 2),
        "expected a reconnect"
    );

    let after = records();
    let connects = config_changes_by(&after, "system:connect");
    assert_eq!(
        connects.len(),
        2,
        "each connect records exactly one profile application, never a duplicate"
    );
    assert!(connects[1]["old"].is_null());
    assert_eq!(connects[1]["new"]["baud"], 115_200);
    assert_eq!(event_count(&after, "config_change"), 3);
}

/// The real-hardware failure behind this test: a macOS PL2303 adapter
/// rejected every live baud change on the daemon's long-lived fd with
/// `EINVAL`, while the daemon told the caller and the timeline the change
/// had taken effect. A PTY cannot reproduce that, so the live applier is
/// replaced with one that fails the same way while `fail` is set.
#[test]
fn a_live_apply_the_port_rejects_is_saved_reported_and_recorded_as_not_applied() {
    use serialwardend::port::LiveConfigApplier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use warden_proto::ConfigApply;

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x067b,
        pid: 0x23a3,
        serial_number: Some("PL2303-EINVAL".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");
    let old_path = device.slave_path().to_path_buf();
    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: old_path.clone(),
        usb: Some(usb),
    });

    let fail = Arc::new(AtomicBool::new(true));
    let fail_in_applier = Arc::clone(&fail);
    let applier: LiveConfigApplier = Arc::new(move |_fd, _config| {
        if fail_in_applier.load(Ordering::SeqCst) {
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        } else {
            Ok(())
        }
    });
    let mut detector = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        tiny_poll_config(),
    )
    .with_live_config_applier(applier);
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );
    let recorder = Arc::clone(detector.recorders().lock().unwrap().get(&id).unwrap());
    let events = |name: &str| -> Vec<serde_json::Map<String, serde_json::Value>> {
        recorder
            .read_since(0, usize::MAX)
            .unwrap()
            .records
            .into_iter()
            .filter_map(|r| match r {
                Record::Event { event, extra, .. } if event == name => Some(extra),
                _ => None,
            })
            .collect()
    };
    let api = detector.port_config_api();
    let esp = PortConfig {
        baud: 74_880,
        ..PortConfig::default()
    };

    // The port rejects it: the caller learns that, with the error.
    let outcome = api
        .set_port_config(&id, esp.clone(), "test:einval")
        .expect("a rejected live apply is not an Err: the config was saved");
    assert!(outcome.changed);
    assert_eq!(outcome.port.apply, ConfigApply::Failed);
    let error = outcome.port.error.clone().expect("the port's error");
    assert!(error.contains("Invalid argument"), "{error}");
    assert!(!outcome.wire().applied);

    // Saved for the next open regardless.
    assert_eq!(api.get_config(&id).unwrap().baud, 74_880);

    // The timeline does not claim it was applied.
    let changes: Vec<_> = events("config_change")
        .into_iter()
        .filter(|e| e["changed_by"] == "test:einval")
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["applied"], false);
    assert_eq!(changes[0]["apply"], "failed");
    assert_eq!(changes[0]["apply_error"], error.as_str());

    // Asking again for the same config retries the port instead of
    // claiming it is already in effect, and is not a second change.
    let retry = api
        .set_port_config(&id, esp.clone(), "test:einval")
        .unwrap();
    assert!(!retry.changed);
    assert_eq!(retry.port.apply, ConfigApply::Failed);
    fail.store(false, Ordering::SeqCst);
    let retry = api
        .set_port_config(&id, esp.clone(), "test:einval")
        .unwrap();
    assert_eq!(retry.port.apply, ConfigApply::Live);
    let reapplied = events("config_reapplied");
    assert_eq!(reapplied.len(), 2, "{reapplied:?}");
    assert_eq!(reapplied[0]["applied"], false);
    assert_eq!(reapplied[1]["applied"], true);

    // Now the port is running it: the same request is a true no-op.
    let noop = api
        .set_port_config(&id, esp.clone(), "test:einval")
        .unwrap();
    assert_eq!(noop.port.apply, ConfigApply::AlreadyApplied);
    assert_eq!(events("config_reapplied").len(), 2);
    assert_eq!(
        events("config_change")
            .iter()
            .filter(|e| e["changed_by"] == "test:einval")
            .count(),
        1
    );

    // A change while disconnected is saved, says it was not applied, and
    // carries no error: there was no port to reject it.
    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |_| events(
            "disconnect"
        )
        .len()
            == 1),
        "expected a disconnect event"
    );
    let offline = api
        .set_port_config(
            &id,
            PortConfig {
                baud: 115_200,
                ..PortConfig::default()
            },
            "test:offline",
        )
        .unwrap();
    assert_eq!(offline.port.apply, ConfigApply::NotConnected);
    let last = events("config_change").pop().unwrap();
    assert_eq!(last["changed_by"], "test:offline");
    assert_eq!(last["applied"], false);
    assert_eq!(last["apply"], "not_connected");
    assert!(!last.contains_key("apply_error"));

    // The next open applies the saved config, and its own record says
    // whether that worked (a PTY rejects macOS's IOSSIOSPEED, so only the
    // field's presence is portable here).
    device.reconnect().expect("reconnect");
    enumerator.replace_path(&old_path, device.slave_path().to_path_buf());
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |_| events("connect")
            .len()
            == 2),
        "expected a reconnect"
    );
    let connect_apply = events("config_change").pop().unwrap();
    assert_eq!(connect_apply["changed_by"], "system:connect");
    assert_eq!(connect_apply["new"]["baud"], 115_200);
    assert!(connect_apply["applied"].is_boolean(), "{connect_apply:?}");
}

/// `PortConfigApi` methods must not silently succeed against a device the
/// detector has never seen at all.
#[test]
fn set_port_config_on_unknown_device_errors_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let enumerator = ScriptedEnumerator::new();
    let detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    let api = detector.port_config_api();

    let unknown_id = DeviceId::from_path(std::path::Path::new("/dev/does-not-exist"));
    let err = api
        .set_port_config(&unknown_id, PortConfig::default(), "test")
        .expect_err("must error for a device the detector has never tracked");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let err = api
        .error_counts(&unknown_id)
        .expect_err("error_counts on an unknown device must also error");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// DTR/RTS and error-count operations need a live fd — they must fail
/// clearly (not silently no-op, not panic) once a previously-connected
/// device has disconnected.
#[test]
fn dtr_and_error_counts_error_once_disconnected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x2341,
        pid: 0x0043,
        serial_number: Some("NOT-CONNECTED-TEST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );

    let api = detector.port_config_api();
    device.disconnect().expect("disconnect");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                event_count(&r.read_since(0, usize::MAX).unwrap().records, "disconnect") == 1
            })
        }),
        "expected a disconnect event"
    );

    assert_eq!(
        api.set_dtr(&id, true, "test").unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
    assert_eq!(
        api.set_rts(&id, false, "test").unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
    assert_eq!(
        api.dtr_pulse(&id, Duration::from_millis(10), "test")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotConnected
    );
    assert_eq!(
        api.error_counts(&id).unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
}

/// Sanity check that the real `open_and_configure` path (not just the
/// fake-regular-file devices `src/port.rs`'s own unit tests use) actually
/// connects successfully against a real PTY and keeps receiving bytes —
/// regardless of platform, and regardless of whether every configuration
/// step (in particular macOS's `IOSSIOSPEED`, see module docs) fully
/// applied.
#[test]
fn connecting_through_a_real_pty_still_receives_bytes_after_full_config_application() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x0403,
        pid: 0x6001,
        serial_number: Some("REAL-PTY-CONNECT".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected a connect event even though this is a real tty going through full termios/DTR configuration"
    );

    device
        .write_device_output(b"hello after config\n")
        .expect("write");
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| {
            let recorders = d.recorders();
            let guard = recorders.lock().unwrap();
            guard.get(&id).is_some_and(|r| {
                r.read_since(0, usize::MAX)
                    .unwrap()
                    .records
                    .iter()
                    .any(|rec| matches!(rec, Record::Rx { .. }))
            })
        }),
        "expected to still receive rx bytes after the full open+configure sequence"
    );
}

/// A PTY has no real modem-control lines, so — discovered empirically
/// while writing this test suite, on this task's macOS development
/// machine — `TIOCMBIS`/`TIOCMBIC` against a pty slave fail with the same
/// `ENOTTY` (raw OS error 25) that `IOSSIOSPEED` does (see
/// `src/port_config.rs`'s module docs). `PortConfigApi::set_dtr`/`set_rts`/
/// `dtr_pulse` deliberately do *not* swallow that failure the way
/// open-time config application does (see `port_io`'s module docs on why
/// *that* case is best-effort): these are explicit, user-invoked
/// operations, and — especially for `dtr_pulse`, whose entire purpose is
/// reliably resetting a board — silently reporting success when the
/// underlying ioctl did nothing would be actively dishonest. So this test
/// accepts either a real success (asserted event recorded) or the
/// well-understood PTY-specific `ENOTTY`, but not any other failure.
/// Real DTR/RTS electrical behavior itself is, as ever,
/// `docs/manual-checklist.md` §2's job, not this test's.
fn accept_success_or_pty_enotty(result: io::Result<()>, what: &str) -> bool {
    match result {
        Ok(()) => true,
        Err(e) if e.raw_os_error() == Some(libc::ENOTTY) => false,
        Err(e) => {
            panic!("{what}: expected success or ENOTTY (PTY has no real modem lines), got {e:?}")
        }
    }
}

#[test]
fn manual_dtr_rts_assert_and_pulse_succeed_or_fail_only_with_pty_enotty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let usb = UsbMetadata {
        vid: 0x10c4,
        pid: 0xea60,
        serial_number: Some("DTR-LIVE-TEST".to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");

    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });

    let mut detector = HotplugDetector::new(
        Box::new(enumerator),
        tmp.path().join("data"),
        tiny_poll_config(),
    );
    assert!(
        poll_until(&mut detector, Duration::from_secs(2), |d| d
            .recorders()
            .lock()
            .unwrap()
            .contains_key(&id)),
        "expected initial connect"
    );

    let api = detector.port_config_api();
    let dtr_ok = accept_success_or_pty_enotty(api.set_dtr(&id, true, "test:dtr"), "set_dtr");
    let rts_ok = accept_success_or_pty_enotty(api.set_rts(&id, false, "test:rts"), "set_rts");
    let pulse_ok = accept_success_or_pty_enotty(
        api.dtr_pulse(&id, Duration::from_millis(5), "test:pulse"),
        "dtr_pulse",
    );

    let recorders = detector.recorders();
    let recorder = Arc::clone(recorders.lock().unwrap().get(&id).unwrap());
    let records = recorder.read_since(0, usize::MAX).unwrap().records;
    let expected_control_line_changes = usize::from(dtr_ok) + usize::from(rts_ok);
    assert_eq!(
        event_count(&records, "control_line_change"),
        expected_control_line_changes
    );
    assert_eq!(event_count(&records, "dtr_pulse"), usize::from(pulse_ok));
}

/// Real, Linux-only verification of the actual `BOTHER`/`TCSETS2` ioctl
/// path (not just `encode_linux_baud`'s pure logic, already covered by
/// `src/port_config.rs`'s unit tests): a real PTY genuinely accepts an
/// arbitrary, non-standard baud rate through this crate's real
/// `apply_termios`, and a subsequent real `TCGETS2` read-back shows the
/// exact rate, not a rounded one.
///
/// No macOS equivalent exists: `IOSSIOSPEED` against a PTY fails `ENOTTY`
/// (confirmed empirically during this task — see `src/port_io.rs`'s
/// module docs), so a real macOS round-trip needs actual hardware
/// (`docs/manual-checklist.md` §1).
#[cfg(target_os = "linux")]
#[test]
fn linux_real_ioctl_round_trip_accepts_74880_via_bother() {
    use std::os::fd::AsRawFd;

    let device = MockDevice::new().expect("open mock device");
    let file = device.open_slave().expect("open slave");
    let fd = file.as_raw_fd();

    let config = PortConfig {
        baud: 74_880,
        ..PortConfig::default()
    };
    serialwardend::port_io::apply_termios(fd, &config)
        .expect("a real Linux PTY must accept BOTHER + an arbitrary baud rate via TCSETS2");

    let mut t: libc::termios2 = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TCGETS2, &mut t) };
    assert_eq!(
        rc,
        0,
        "TCGETS2 read-back failed: {:?}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        t.c_ispeed, 74_880,
        "the real ioctl round-trip must preserve 74880 exactly, not round to a standard rate"
    );
    assert_eq!(
        t.c_cflag & libc::CBAUD,
        serialwardend::port_config::LINUX_BOTHER,
        "CBAUD must read back as the BOTHER selector value after a real TCSETS2 call"
    );
}

// ---- Port-open records: lease end, the open window, DTR/RTS apart from baud ----

/// Every event named `name` on `recorder`, in order.
fn events_named(
    recorder: &serialwardend::recorder::Recorder,
    name: &str,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    recorder
        .read_since(0, usize::MAX)
        .unwrap()
        .records
        .into_iter()
        .filter_map(|r| match r {
            Record::Event { event, extra, .. } if event == name => Some(extra),
            _ => None,
        })
        .collect()
}

/// Event names on `recorder`, in order.
fn event_names(recorder: &serialwardend::recorder::Recorder) -> Vec<String> {
    recorder
        .read_since(0, usize::MAX)
        .unwrap()
        .records
        .into_iter()
        .filter_map(|r| match r {
            Record::Event { event, .. } => Some(event),
            _ => None,
        })
        .collect()
}

fn wait_for(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !check() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
    true
}

/// A detector running on its own thread over one PTY mock device, plus
/// the device id. `detector` lets a test inject appliers first.
fn spawn_one_device(
    data_dir: &std::path::Path,
    device: &MockDevice,
    serial: &str,
    detector: impl FnOnce(HotplugDetector) -> HotplugDetector,
) -> (
    serialwardend::port::DetectorHandle,
    DeviceId,
    ScriptedEnumerator,
) {
    let usb = UsbMetadata {
        vid: 0x067b,
        pid: 0x2303,
        serial_number: Some(serial.to_string()),
    };
    let id = DeviceId::from_usb(&usb).expect("usb id");
    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device.slave_path().to_path_buf(),
        usb: Some(usb),
    });
    let handle = detector(HotplugDetector::new(
        Box::new(enumerator.clone()),
        data_dir.to_path_buf(),
        tiny_poll_config(),
    ))
    .spawn();
    (handle, id, enumerator)
}

/// Ending a lease reopens the port with the saved profile. That is recorded
/// like a connect's profile application (`config_change`, `old: null`), as
/// `system:lease_end`, after `lease_end`, saying whether it took. A change
/// saved during the lease was recorded "not applied"; this is the record
/// that says what then happened to it.
#[test]
fn ending_a_lease_records_the_profile_it_reopened_with_and_whether_it_took() {
    use serialwardend::port::LiveConfigApplier;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let fail = Arc::new(AtomicBool::new(false));
    let fail_in_applier = Arc::clone(&fail);
    let applier: LiveConfigApplier = Arc::new(move |_fd, _config| {
        if fail_in_applier.load(Ordering::SeqCst) {
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        } else {
            Ok(())
        }
    });
    let (handle, id, _enumerator) =
        spawn_one_device(&tmp.path().join("data"), &device, "LEASE-END", |d| {
            d.with_live_config_applier(applier)
        });
    let api = handle.port_config_api();
    assert!(
        wait_for(Duration::from_secs(5), || api
            .list_devices()
            .iter()
            .any(|d| d.id == id && d.connected)),
        "device never connected"
    );
    let recorder = Arc::clone(handle.recorders().lock().unwrap().get(&id).unwrap());

    // A lease the port reopens from cleanly.
    let lease = api
        .acquire_lease(&id, "true", std::process::id(), None)
        .expect("acquire");
    api.release_lease(&lease.token, 0).expect("release");
    let reopened = events_named(&recorder, "config_change")
        .into_iter()
        .filter(|e| e["changed_by"] == "system:lease_end")
        .collect::<Vec<_>>();
    assert_eq!(reopened.len(), 1, "{reopened:?}");
    assert!(reopened[0]["old"].is_null());
    assert_eq!(reopened[0]["new"]["baud"], 9600);
    assert_eq!(reopened[0]["applied"], true);
    assert_eq!(reopened[0]["apply"], "live");

    // A change saved during the next lease, and a port that refuses it.
    let lease = api
        .acquire_lease(&id, "true", std::process::id(), None)
        .expect("acquire again");
    let saved = api
        .set_port_config(
            &id,
            PortConfig {
                baud: 74_880,
                ..PortConfig::default()
            },
            "test:during-lease",
        )
        .expect("set during lease");
    assert_eq!(saved.port.apply, warden_proto::ConfigApply::NotConnected);
    fail.store(true, Ordering::SeqCst);
    api.release_lease(&lease.token, 0).expect("release again");

    let names = event_names(&recorder);
    let last_two = &names[names.len() - 2..];
    assert_eq!(
        last_two,
        ["lease_end", "config_change"],
        "the reopen is recorded after lease_end: {names:?}"
    );
    let last = events_named(&recorder, "config_change").pop().unwrap();
    assert_eq!(last["changed_by"], "system:lease_end");
    assert_eq!(last["new"]["baud"], 74_880);
    assert_eq!(last["applied"], false);
    assert_eq!(last["apply"], "failed");
    assert!(
        last["apply_error"]
            .as_str()
            .is_some_and(|e| e.contains("Invalid argument")),
        "{last:?}"
    );

    handle.stop();
}

/// A `set_config` arriving while the daemon is opening the port must not be
/// told "not connected" (and recorded that way) while the port comes up on
/// the previous profile. It waits for the open, then applies live.
#[test]
fn a_set_config_during_the_open_waits_for_it_and_applies_live() {
    use serialwardend::port::LiveConfigApplier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Mutex};

    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    // The first application is the open's: it reports that it has started,
    // then holds the open in progress until the test lets it go.
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in_applier = Arc::clone(&calls);
    let applier: LiveConfigApplier = Arc::new(move |_fd, _config| {
        if calls_in_applier.fetch_add(1, Ordering::SeqCst) == 0 {
            entered_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let (handle, id, _enumerator) =
        spawn_one_device(&tmp.path().join("data"), &device, "OPEN-WINDOW", |d| {
            d.with_live_config_applier(applier)
        });
    let api = handle.port_config_api();
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the open never applied the profile");

    // The port is open but the open has not finished: the window.
    let api_in_thread = api.clone();
    let id_in_thread = id.clone();
    let setter = thread::spawn(move || {
        api_in_thread.set_port_config(
            &id_in_thread,
            PortConfig {
                baud: 115_200,
                ..PortConfig::default()
            },
            "test:window",
        )
    });
    thread::sleep(Duration::from_millis(200));
    assert!(
        !setter.is_finished(),
        "set_config must wait for the open in progress, not report not_connected"
    );

    release_tx.send(()).unwrap();
    let outcome = setter.join().unwrap().expect("set_port_config");
    assert_eq!(outcome.port.apply, warden_proto::ConfigApply::Live);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "open, then the live apply");

    let recorder = Arc::clone(handle.recorders().lock().unwrap().get(&id).unwrap());
    let changes = events_named(&recorder, "config_change");
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert_eq!(changes[0]["changed_by"], "system:connect");
    assert_eq!(changes[0]["new"]["baud"], 9600);
    assert_eq!(changes[1]["changed_by"], "test:window");
    assert_eq!(changes[1]["applied"], true);

    handle.stop();
}

/// Opening in DTR/RTS `assert` mode on an adapter that refuses the lines
/// must not report the baud as failed: `applied` is about baud/framing, and
/// the refusal gets its own `control_line_error`.
#[test]
fn a_dtr_rts_refusal_on_open_is_recorded_apart_from_the_applied_baud() {
    use serialwardend::port::{ControlLineSetter, LiveConfigApplier};
    use serialwardend::port_config::OpenControlLines;

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut device = MockDevice::new().expect("open mock device");
    let old_path = device.slave_path().to_path_buf();
    let applier: LiveConfigApplier = Arc::new(|_fd, _config| Ok(()));
    let setter: ControlLineSetter =
        Arc::new(|_fd, _line, _level| Err(io::Error::from_raw_os_error(libc::ENOTTY)));
    let (handle, id, enumerator) =
        spawn_one_device(&tmp.path().join("data"), &device, "NO-MODEM-LINES", |d| {
            d.with_live_config_applier(applier)
                .with_open_control_line_setter(setter)
        });
    let api = handle.port_config_api();
    assert!(
        wait_for(Duration::from_secs(5), || api
            .list_devices()
            .iter()
            .any(|d| d.id == id && d.connected)),
        "device never connected"
    );
    let recorder = Arc::clone(handle.recorders().lock().unwrap().get(&id).unwrap());

    // Preserve mode touches no lines, so the first open has nothing to refuse.
    let first = events_named(&recorder, "config_change").pop().unwrap();
    assert_eq!(first["applied"], true);
    assert!(!first.contains_key("control_line_error"), "{first:?}");

    api.set_port_config(
        &id,
        PortConfig {
            baud: 74_880,
            open_control_lines: OpenControlLines::Assert {
                dtr: true,
                rts: true,
            },
            ..PortConfig::default()
        },
        "test:assert",
    )
    .expect("set assert mode");

    device.disconnect().expect("disconnect");
    assert!(
        wait_for(Duration::from_secs(5), || !events_named(
            &recorder,
            "disconnect"
        )
        .is_empty()),
        "expected a disconnect"
    );
    device.reconnect().expect("reconnect");
    enumerator.replace_path(&old_path, device.slave_path().to_path_buf());
    assert!(
        wait_for(Duration::from_secs(5), || events_named(
            &recorder, "connect"
        )
        .len()
            == 2),
        "expected a reconnect"
    );
    let reconnect = events_named(&recorder, "config_change").pop().unwrap();
    assert_eq!(reconnect["changed_by"], "system:connect");
    assert_eq!(reconnect["new"]["baud"], 74_880);
    assert_eq!(reconnect["applied"], true, "the baud took: {reconnect:?}");
    assert_eq!(reconnect["apply"], "live");
    assert!(!reconnect.contains_key("apply_error"), "{reconnect:?}");
    assert!(
        reconnect["control_line_error"]
            .as_str()
            .is_some_and(|e| e.contains("Inappropriate ioctl")),
        "{reconnect:?}"
    );

    // And the port is known to run the profile, so repeating it is a no-op.
    let again = api
        .set_port_config(
            &id,
            PortConfig {
                baud: 74_880,
                open_control_lines: OpenControlLines::Assert {
                    dtr: true,
                    rts: true,
                },
                ..PortConfig::default()
            },
            "test:assert",
        )
        .unwrap();
    assert_eq!(again.port.apply, warden_proto::ConfigApply::AlreadyApplied);

    handle.stop();
}

/// The apply lock is per device: a live apply stuck in a wedged adapter's
/// ioctl on device B (seconds, on Linux USB) must not hold up opening
/// device A — or hotplug, disconnects and leases for every other device.
#[test]
fn a_stalled_live_apply_on_one_device_does_not_block_opening_another() {
    use serialwardend::port::LiveConfigApplier;
    use std::sync::{mpsc, Mutex};

    let tmp = tempfile::tempdir().expect("tempdir");
    let device_a = MockDevice::new().expect("open mock device A");
    let device_b = MockDevice::new().expect("open mock device B");
    let usb = |serial: &str| UsbMetadata {
        vid: 0x067b,
        pid: 0x2303,
        serial_number: Some(serial.to_string()),
    };
    let (usb_a, usb_b) = (usb("STALL-A"), usb("STALL-B"));
    let id_a = DeviceId::from_usb(&usb_a).expect("usb id");
    let id_b = DeviceId::from_usb(&usb_b).expect("usb id");
    let enumerator = ScriptedEnumerator::new();
    enumerator.push(EnumeratedDevice {
        path: device_b.slave_path().to_path_buf(),
        usb: Some(usb_b),
    });

    // Only B's 74880 request stalls: it reports that it is stuck, then
    // waits until the test lets it go.
    let (stalled_tx, stalled_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let stalled_tx = Mutex::new(stalled_tx);
    let release_rx = Mutex::new(release_rx);
    let applier: LiveConfigApplier = Arc::new(move |_fd, config| {
        if config.baud == 74_880 {
            stalled_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let handle = HotplugDetector::new(
        Box::new(enumerator.clone()),
        tmp.path().join("data"),
        tiny_poll_config(),
    )
    .with_live_config_applier(applier)
    .spawn();
    let api = handle.port_config_api();
    let connected = |id: &DeviceId| {
        api.list_devices()
            .iter()
            .any(|d| &d.id == id && d.connected)
    };
    assert!(
        wait_for(Duration::from_secs(5), || connected(&id_b)),
        "device B never connected"
    );

    let api_in_thread = api.clone();
    let id_b_in_thread = id_b.clone();
    let stuck = thread::spawn(move || {
        api_in_thread.set_port_config(
            &id_b_in_thread,
            PortConfig {
                baud: 74_880,
                ..PortConfig::default()
            },
            "test:stalled",
        )
    });
    stalled_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B's live apply never started");

    // B's apply is stuck holding B's lock. A appears now and must open.
    enumerator.push(EnumeratedDevice {
        path: device_a.slave_path().to_path_buf(),
        usb: Some(usb_a),
    });
    let opened = wait_for(Duration::from_secs(5), || connected(&id_a));
    release_tx.send(()).unwrap();
    assert!(opened, "device A's open waited on device B's stalled apply");

    let outcome = stuck.join().unwrap().expect("B's set_port_config");
    assert_eq!(outcome.port.apply, warden_proto::ConfigApply::Live);
    handle.stop();
}

/// What `get_config` reports as the port's state, against the real detector:
/// after the port refuses a change, the saved config is not running, with
/// the error and what the port last accepted; once a change takes, it is.
#[test]
fn port_state_says_whether_the_port_runs_the_saved_config() {
    use serialwardend::port::LiveConfigApplier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use warden_proto::ConfigApply;

    let tmp = tempfile::tempdir().expect("tempdir");
    let device = MockDevice::new().expect("open mock device");
    let fail = Arc::new(AtomicBool::new(false));
    let fail_in_applier = Arc::clone(&fail);
    let applier: LiveConfigApplier = Arc::new(move |_fd, _config| {
        if fail_in_applier.load(Ordering::SeqCst) {
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        } else {
            Ok(())
        }
    });
    let (handle, id, _enumerator) =
        spawn_one_device(&tmp.path().join("data"), &device, "PORT-STATE", |d| {
            d.with_live_config_applier(applier)
        });
    let api = handle.port_config_api();
    assert!(
        wait_for(Duration::from_secs(5), || api
            .port_state(&id)
            .is_ok_and(|s| s.apply == ConfigApply::Live)),
        "the open port should be running the saved default"
    );

    fail.store(true, Ordering::SeqCst);
    let esp = PortConfig {
        baud: 74_880,
        ..PortConfig::default()
    };
    api.set_port_config(&id, esp.clone(), "test:state").unwrap();
    let state = api.port_state(&id).unwrap();
    assert!(!state.applied);
    assert_eq!(state.apply, ConfigApply::Failed);
    assert!(
        state
            .apply_error
            .as_deref()
            .is_some_and(|e| e.contains("Invalid argument")),
        "{state:?}"
    );
    assert_eq!(state.last_applied.unwrap()["baud"], 9600);

    fail.store(false, Ordering::SeqCst);
    api.set_port_config(&id, esp, "test:state").unwrap();
    let state = api.port_state(&id).unwrap();
    assert!(state.applied, "{state:?}");
    assert!(state.apply_error.is_none() && state.last_applied.is_none());

    handle.stop();
}
