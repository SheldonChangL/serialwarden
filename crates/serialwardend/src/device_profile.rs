//! Per-device configuration profile: persistence keyed by `DeviceId`, plus
//! the event-append helpers that make every change to this shared,
//! per-device state auditable (`TASKS.md` T1.3, issue #5).
//!
//! # Config is shared, per-device state
//!
//! A `PortConfig` belongs to a device, not to whichever client last
//! touched it — there are no clients yet (T1.4 owns the UDS protocol),
//! but the storage/event design here is already shaped for that: one
//! profile per [`crate::port::DeviceId`], every change recorded with an
//! explicit `changed_by` string a future client layer supplies, and a
//! live re-application path (`port.rs`'s `PortConfigApi`) so a change
//! while connected affects the one shared fd everyone reads from — not a
//! per-connection copy.
//!
//! # Storage location
//!
//! `<data_dir>/devices/<device_id>/profile.json` — right alongside
//! `Recorder`'s own `segments/`, `index.jsonl`, and `.lock` for the same
//! device (see `recorder.rs`'s "Storage layout" docs). Chosen over a
//! separate top-level `config/` directory because it reuses the exact
//! same sanitized-device-id-as-directory-name convention `Recorder::open`
//! already established, so a device's recording and its profile live,
//! back up, and get deleted together under one path, without introducing
//! a second key scheme.
//!
//! # Event naming
//!
//! Three distinct event kinds, on purpose, so a future rule engine or
//! human audit trail (T4.1) can tell these apart without inspecting
//! payload contents:
//!
//! - `config_change` — a [`crate::port_config::PortConfig`] change
//!   (baud/data bits/parity/stop bits/flow control), with full old/new
//!   values, `changed_by`, and whether the open port actually took it
//!   (`applied`/`apply`/`apply_error`, see [`PortApply`]). Never recorded
//!   for a request that changes nothing (issue #51). Every time the daemon
//!   opens the port it records the profile it applied the same way, with
//!   `old: null`: `changed_by: "system:connect"` on a (re)connect and
//!   `"system:lease_end"` when it takes the port back after a lease. Those
//!   open-time records also carry `control_line_error` when the profile
//!   asserts DTR/RTS and the driver refused; `applied` is about baud and
//!   framing only.
//! - `config_reapplied` — the saved configuration did not change, but the
//!   port was not known to be running it (an earlier live apply failed), so
//!   the request retried the live apply. Recorded because it touched the
//!   port, under its own name so it never reads as a change.
//! - `control_line_change` — a manual, single-line DTR or RTS
//!   assert/deassert.
//! - `dtr_pulse` — the independently-named reset-shaped operation the
//!   issue specifically calls out as *not* a `set_config` parameter, so it
//!   reads as "reset the board" rather than "changed a control line".
//!
//! # Old data is never reinterpreted
//!
//! None of the functions here ever read, rewrite, or otherwise touch
//! previously-recorded `rx`/`tx` records — `Recorder` only ever gets
//! `append_event` calls from this module. Changing baud does not, and
//! structurally cannot, alter how previously-stored bytes are interpreted
//! (see `recorder.rs`'s "Write semantics": it records bytes, not
//! characters).

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Map;
use warden_proto::{ConfigApply, ConfigApplyOutcome};

use crate::port_config::PortConfig;
use crate::query::LineTerminatorMode;
use crate::recorder::Recorder;

/// One device's persisted configuration. A `struct` (rather than a bare
/// type alias around just [`PortConfig`]) so per-device settings beyond the
/// port config itself have somewhere to go without changing every call
/// site — [`Self::line_terminator`] (issue #52) is the first to use that
/// room: "跟著裝置記住" applies to line-ending convention exactly the same
/// way it already does to baud.
///
/// `#[serde(default)]` on `line_terminator` keeps this forward-compatible
/// with a `profile.json` written before this field existed: it
/// deserializes as [`LineTerminatorMode::Auto`], never a load failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeviceProfile {
    pub config: PortConfig,
    #[serde(default)]
    pub line_terminator: LineTerminatorMode,
}

/// Persists [`DeviceProfile`]s under `<data_dir>/devices/<device_id>/profile.json`.
/// See the module docs for why this location.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    data_dir: PathBuf,
}

impl ProfileStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    fn profile_path(&self, device_id: &str) -> PathBuf {
        self.data_dir
            .join("devices")
            .join(device_id)
            .join("profile.json")
    }

    /// `Ok(None)` if no profile has ever been saved for this device — the
    /// caller should fall back to [`PortConfig::default`].
    pub fn load(&self, device_id: &str) -> io::Result<Option<DeviceProfile>> {
        let path = self.profile_path(device_id);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Persist `profile` for `device_id`, creating parent directories as
    /// needed. Writes to a temp file and renames over the real path — an
    /// atomic replace on the same filesystem — so a crash mid-write can
    /// never leave a torn `profile.json` as the only copy for the next
    /// `load` to choke on (same "never leave a half-written file behind"
    /// principle `recorder.rs` applies to its own segments).
    pub fn save(&self, device_id: &str, profile: &DeviceProfile) -> io::Result<()> {
        let path = self.profile_path(device_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(profile)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &bytes)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }
}

/// What happened at the open port when a configuration was applied to it.
///
/// A configuration is always saved before it is applied, so "saved" is
/// never in question once a request succeeds; this records the other half,
/// which can fail on its own (see [`ConfigApply`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortApply {
    pub apply: ConfigApply,
    /// The port's error, set exactly when `apply` is [`ConfigApply::Failed`].
    pub error: Option<String>,
    /// Only for a port being opened in `open_control_lines: assert` mode:
    /// the driver refused to set DTR/RTS. Kept apart from `apply`/`error`,
    /// which are about baud and framing — a port with no modem lines can
    /// run the requested line settings perfectly well (see
    /// `port_io::OpenedPort`).
    pub control_line_error: Option<String>,
}

impl PortApply {
    pub fn live() -> Self {
        Self {
            apply: ConfigApply::Live,
            error: None,
            control_line_error: None,
        }
    }

    pub fn already_applied() -> Self {
        Self {
            apply: ConfigApply::AlreadyApplied,
            error: None,
            control_line_error: None,
        }
    }

    pub fn not_connected() -> Self {
        Self {
            apply: ConfigApply::NotConnected,
            error: None,
            control_line_error: None,
        }
    }

    pub fn failed(error: impl std::fmt::Display) -> Self {
        Self {
            apply: ConfigApply::Failed,
            error: Some(error.to_string()),
            control_line_error: None,
        }
    }

    /// What opening a port did with `config`: the termios result decides
    /// `apply`, and a DTR/RTS failure is carried on its own.
    pub fn from_open(termios: &io::Result<()>, control_lines: Option<&io::Error>) -> Self {
        let mut port = match termios {
            Ok(()) => Self::live(),
            Err(e) => Self::failed(e),
        };
        port.control_line_error = control_lines.map(|e| e.to_string());
        port
    }

    pub fn applied(&self) -> bool {
        self.apply.applied()
    }

    /// Whether the request touched the port at all, as opposed to finding
    /// nothing to do or having no open port to touch.
    pub fn attempted(&self) -> bool {
        matches!(self.apply, ConfigApply::Live | ConfigApply::Failed)
    }

    fn insert_into(&self, extra: &mut Map<String, serde_json::Value>) {
        extra.insert("applied".to_string(), self.applied().into());
        extra.insert(
            "apply".to_string(),
            serde_json::to_value(self.apply).unwrap_or(serde_json::Value::Null),
        );
        if let Some(error) = &self.error {
            extra.insert("apply_error".to_string(), error.clone().into());
        }
        if let Some(error) = &self.control_line_error {
            extra.insert("control_line_error".to_string(), error.clone().into());
        }
    }
}

/// Result of a `set_config` request whose configuration was saved: the
/// resulting configuration, whether it differs from what was saved before,
/// and what happened at the port.
#[derive(Debug, Clone, PartialEq)]
pub struct SetConfigOutcome {
    pub config: PortConfig,
    pub changed: bool,
    pub port: PortApply,
}

impl SetConfigOutcome {
    pub fn wire(&self) -> ConfigApplyOutcome {
        ConfigApplyOutcome::new(self.changed, self.port.apply, self.port.error.clone())
    }

    /// The body every transport replies with for a successful `set_config`
    /// (UDS and web alike): `config` plus [`ConfigApplyOutcome`]'s fields.
    pub fn reply_body(&self) -> serde_json::Value {
        let mut body = Map::new();
        body.insert(
            "config".to_string(),
            serde_json::to_value(&self.config).unwrap_or(serde_json::Value::Null),
        );
        if let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(self.wire()) {
            body.extend(fields);
        }
        serde_json::Value::Object(body)
    }
}

/// Append a `config_change` event: full old/new [`PortConfig`] values, who
/// changed it, and whether the open port took it (`port`). `old: None` means
/// "no config has ever been applied to this device before" (its very first
/// connect, with no saved profile).
pub fn append_config_change_event(
    recorder: &Recorder,
    old: Option<&PortConfig>,
    new: &PortConfig,
    changed_by: &str,
    port: &PortApply,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert(
        "old".to_string(),
        old.map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null),
    );
    extra.insert(
        "new".to_string(),
        serde_json::to_value(new).unwrap_or(serde_json::Value::Null),
    );
    extra.insert("changed_by".to_string(), changed_by.into());
    port.insert_into(&mut extra);
    recorder.append_event("config_change", extra)?;
    Ok(())
}

/// Record what a saved `set_config` request did, and nothing it didn't:
///
/// - the configuration changed: one `config_change`, with the port outcome;
/// - it did not change, but the port was touched (a retry of a live apply
///   that had failed): one `config_reapplied`, with the port outcome;
/// - otherwise (nothing changed, port not touched): nothing (issue #51).
pub fn append_set_config_events(
    recorder: &Recorder,
    old: &PortConfig,
    outcome: &SetConfigOutcome,
    changed_by: &str,
) -> io::Result<()> {
    if outcome.changed {
        return append_config_change_event(
            recorder,
            Some(old),
            &outcome.config,
            changed_by,
            &outcome.port,
        );
    }
    if outcome.port.attempted() {
        let mut extra = Map::new();
        extra.insert(
            "config".to_string(),
            serde_json::to_value(&outcome.config).unwrap_or(serde_json::Value::Null),
        );
        extra.insert("changed_by".to_string(), changed_by.into());
        outcome.port.insert_into(&mut extra);
        recorder.append_event("config_reapplied", extra)?;
    }
    Ok(())
}

/// Append a `control_line_change` event: manual DTR/RTS assert/deassert —
/// distinct from `dtr_pulse` (see module docs).
pub fn append_control_line_change_event(
    recorder: &Recorder,
    line: &str,
    level: bool,
    changed_by: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("line".to_string(), line.into());
    extra.insert("level".to_string(), level.into());
    extra.insert("changed_by".to_string(), changed_by.into());
    recorder.append_event("control_line_change", extra)?;
    Ok(())
}

/// Append a `dtr_pulse` event — independently named per this task's spec
/// (see module docs).
pub fn append_dtr_pulse_event(
    recorder: &Recorder,
    duration_ms: u64,
    changed_by: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("duration_ms".to_string(), duration_ms.into());
    extra.insert("changed_by".to_string(), changed_by.into());
    recorder.append_event("dtr_pulse", extra)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_config::{DataBits, FlowControl, OpenControlLines, Parity, StopBits};
    use crate::recorder::RecorderConfig;
    use warden_proto::Record;

    fn custom_config() -> PortConfig {
        PortConfig {
            baud: 74_880,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::Hardware,
            open_control_lines: OpenControlLines::Assert {
                dtr: true,
                rts: false,
            },
        }
    }

    // ---- Issue #52: per-device line-terminator override persistence ----

    #[test]
    fn line_terminator_defaults_to_auto_and_round_trips_through_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());

        assert_eq!(
            DeviceProfile::default().line_terminator,
            LineTerminatorMode::Auto,
            "a brand-new profile must default to auto-detection, not force a mode"
        );

        let profile = DeviceProfile {
            config: custom_config(),
            line_terminator: LineTerminatorMode::Cr,
        };
        store.save("dev-1", &profile).unwrap();
        let loaded = store.load("dev-1").unwrap().expect("profile must load");
        assert_eq!(loaded.line_terminator, LineTerminatorMode::Cr);
    }

    #[test]
    fn line_terminator_serializes_as_snake_case_matching_this_repos_wire_convention() {
        // `warden_proto::request::LineEnding` (the write-side equivalent) is
        // `#[serde(rename_all = "snake_case")]`; every other enum on this
        // wire follows the same rule. Pin it down explicitly rather than
        // relying on `#[derive(Serialize)]`'s PascalCase default, since
        // `profile.json` is hand-editable and this field is read by the
        // daemon on every device's first `get_or_spawn`.
        for (mode, expected) in [
            (LineTerminatorMode::Auto, "auto"),
            (LineTerminatorMode::Lf, "lf"),
            (LineTerminatorMode::Cr, "cr"),
        ] {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::Value::String(expected.to_string()),
                "{mode:?} must serialize as snake_case \"{expected}\""
            );
        }
    }

    #[test]
    fn a_profile_json_written_before_line_terminator_existed_still_loads_as_auto() {
        // Simulates a `profile.json` persisted by a pre-issue-#52 daemon
        // build: no `line_terminator` key at all. `#[serde(default)]` must
        // make this a normal, successful load (falling back to `Auto`), not
        // a `ProfileStore::load` error — an operator upgrading the daemon
        // must never have their existing saved profiles start failing to
        // load.
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        let dir = tmp.path().join("devices").join("dev-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("profile.json"),
            serde_json::to_vec_pretty(&serde_json::json!({ "config": PortConfig::default() }))
                .unwrap(),
        )
        .unwrap();

        let loaded = store
            .load("dev-1")
            .unwrap()
            .expect("a profile.json missing line_terminator must still load");
        assert_eq!(loaded.line_terminator, LineTerminatorMode::Auto);
    }

    // ---- Acceptance criterion 4: persistence + reconnect application ----

    #[test]
    fn profile_saved_then_reloaded_matches_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        assert!(store.load("dev-1").unwrap().is_none(), "nothing saved yet");

        let profile = DeviceProfile {
            config: custom_config(),
            ..Default::default()
        };
        store.save("dev-1", &profile).unwrap();

        let loaded = store
            .load("dev-1")
            .unwrap()
            .expect("profile must load back");
        assert_eq!(loaded, profile);
    }

    #[test]
    fn profile_is_stored_alongside_the_recorder_directory_for_the_same_device() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        store
            .save(
                "dev-1",
                &DeviceProfile {
                    config: custom_config(),
                    ..Default::default()
                },
            )
            .unwrap();

        let expected = tmp
            .path()
            .join("devices")
            .join("dev-1")
            .join("profile.json");
        assert!(expected.is_file(), "expected profile.json at {expected:?}");
    }

    #[test]
    fn saving_a_second_time_overwrites_the_first_no_stale_leftover() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        store
            .save(
                "dev-1",
                &DeviceProfile {
                    config: PortConfig::default(),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .save(
                "dev-1",
                &DeviceProfile {
                    config: custom_config(),
                    ..Default::default()
                },
            )
            .unwrap();

        let loaded = store.load("dev-1").unwrap().unwrap();
        assert_eq!(loaded.config.baud, 74_880);
        // No leftover temp file from the atomic rename.
        let dir = tmp.path().join("devices").join("dev-1");
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["profile.json".to_string()]);
    }

    // ---- Acceptance criterion 3: config_change carries old/new + changed_by ----

    #[test]
    fn config_change_event_carries_full_old_and_new_values_and_changed_by() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        let old = PortConfig::default();
        let new = custom_config();
        append_config_change_event(
            &recorder,
            Some(&old),
            &new,
            "cli:sheldon",
            &PortApply::live(),
        )
        .unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let (extra_old, extra_new, changed_by) = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "config_change" => Some((
                    extra.get("old").cloned().unwrap(),
                    extra.get("new").cloned().unwrap(),
                    extra
                        .get("changed_by")
                        .and_then(|v| v.as_str())
                        .unwrap()
                        .to_string(),
                )),
                _ => None,
            })
            .expect("expected a config_change event");

        assert_eq!(extra_old, serde_json::to_value(&old).unwrap());
        assert_eq!(extra_new, serde_json::to_value(&new).unwrap());
        assert_eq!(extra_new.get("baud").and_then(|v| v.as_u64()), Some(74_880));
        assert_eq!(changed_by, "cli:sheldon");
    }

    #[test]
    fn config_change_event_with_no_prior_config_records_old_as_null() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        append_config_change_event(
            &recorder,
            None,
            &PortConfig::default(),
            "system:connect",
            &PortApply::live(),
        )
        .unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let extra_old = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "config_change" => {
                    Some(extra.get("old").cloned().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert!(extra_old.is_null());
    }

    fn events(recorder: &Recorder) -> Vec<(String, Map<String, serde_json::Value>)> {
        recorder
            .read_since(0, usize::MAX)
            .unwrap()
            .records
            .into_iter()
            .filter_map(|r| match r {
                Record::Event { event, extra, .. } => Some((event, extra)),
                _ => None,
            })
            .collect()
    }

    /// A change the port rejected is still one `config_change` (it was
    /// saved), but it says so: `applied: false` plus the port's error.
    #[test]
    fn a_change_the_port_rejected_records_applied_false_and_the_error() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();
        let outcome = SetConfigOutcome {
            config: custom_config(),
            changed: true,
            port: PortApply::failed("Invalid argument (os error 22)"),
        };
        append_set_config_events(&recorder, &PortConfig::default(), &outcome, "gui").unwrap();

        let events = events(&recorder);
        assert_eq!(events.len(), 1, "{events:?}");
        let (name, extra) = &events[0];
        assert_eq!(name, "config_change");
        assert_eq!(extra["applied"], false);
        assert_eq!(extra["apply"], "failed");
        assert_eq!(extra["apply_error"], "Invalid argument (os error 22)");
        assert_eq!(extra["new"]["baud"], 74_880);
    }

    #[test]
    fn a_change_to_a_disconnected_device_is_saved_but_not_applied_and_has_no_error() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();
        let outcome = SetConfigOutcome {
            config: custom_config(),
            changed: true,
            port: PortApply::not_connected(),
        };
        append_set_config_events(&recorder, &PortConfig::default(), &outcome, "gui").unwrap();

        let events = events(&recorder);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1["applied"], false);
        assert_eq!(events[0].1["apply"], "not_connected");
        assert!(!events[0].1.contains_key("apply_error"));
    }

    /// Issue #51's no-op rule, plus the one case an unchanged request still
    /// touches the port: retrying a live apply that had failed. That retry
    /// is recorded, but never as a `config_change`.
    #[test]
    fn an_unchanged_request_records_nothing_unless_it_retried_the_port() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();
        let config = custom_config();
        for port in [PortApply::already_applied(), PortApply::not_connected()] {
            let outcome = SetConfigOutcome {
                config: config.clone(),
                changed: false,
                port,
            };
            append_set_config_events(&recorder, &config, &outcome, "gui").unwrap();
        }
        assert!(events(&recorder).is_empty(), "a no-op must record nothing");

        let retried = SetConfigOutcome {
            config: config.clone(),
            changed: false,
            port: PortApply::live(),
        };
        append_set_config_events(&recorder, &config, &retried, "gui").unwrap();
        let events = events(&recorder);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "config_reapplied");
        assert_eq!(events[0].1["applied"], true);
        assert_eq!(events[0].1["config"]["baud"], 74_880);
    }

    #[test]
    fn set_config_reply_body_carries_config_and_outcome_fields() {
        let outcome = SetConfigOutcome {
            config: custom_config(),
            changed: true,
            port: PortApply::failed("Invalid argument (os error 22)"),
        };
        let body = outcome.reply_body();
        assert_eq!(body["config"]["baud"], 74_880);
        assert_eq!(body["changed"], true);
        assert_eq!(body["applied"], false);
        assert_eq!(body["apply"], "failed");
        assert_eq!(body["apply_error"], "Invalid argument (os error 22)");
    }

    #[test]
    fn control_line_change_event_carries_line_level_and_changed_by() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        append_control_line_change_event(&recorder, "dtr", true, "agent:claude").unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let extra = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "control_line_change" => {
                    Some(extra.clone())
                }
                _ => None,
            })
            .expect("expected a control_line_change event");
        assert_eq!(extra.get("line").and_then(|v| v.as_str()), Some("dtr"));
        assert_eq!(extra.get("level").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            extra.get("changed_by").and_then(|v| v.as_str()),
            Some("agent:claude")
        );
    }

    #[test]
    fn dtr_pulse_event_is_distinct_from_control_line_change_and_config_change() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        append_dtr_pulse_event(&recorder, 50, "cli:sheldon").unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let kinds: Vec<&str> = records
            .iter()
            .filter_map(|r| match r {
                Record::Event { event, .. } => Some(event.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["dtr_pulse"]);

        let extra = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "dtr_pulse" => Some(extra.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(extra.get("duration_ms").and_then(|v| v.as_u64()), Some(50));
        assert_eq!(
            extra.get("changed_by").and_then(|v| v.as_str()),
            Some("cli:sheldon")
        );
    }

    // ---- Acceptance criterion 5: old data is never reinterpreted ----

    #[test]
    fn appending_config_change_events_never_alters_previously_recorded_rx_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        let before: Vec<Vec<u8>> = (0..10)
            .map(|i| format!("boot log line {i}\n").into_bytes())
            .collect();
        for line in &before {
            recorder.append_rx(line).unwrap();
        }
        let before_records = recorder.read_since(0, usize::MAX).unwrap().records;

        // Change baud (and every other setting) several times.
        append_config_change_event(
            &recorder,
            None,
            &PortConfig::default(),
            "system:connect",
            &PortApply::live(),
        )
        .unwrap();
        append_config_change_event(
            &recorder,
            Some(&PortConfig::default()),
            &custom_config(),
            "cli:sheldon",
            &PortApply::live(),
        )
        .unwrap();
        append_config_change_event(
            &recorder,
            Some(&custom_config()),
            &PortConfig {
                baud: 115_200,
                ..PortConfig::default()
            },
            "cli:sheldon",
            &PortApply::live(),
        )
        .unwrap();

        // Every previously-recorded rx record must be byte-for-byte
        // unchanged — same seq, same t_mono/t_wall, same data_b64.
        let after_records = recorder.read_since(0, usize::MAX).unwrap().records;
        for (idx, original) in before_records.iter().enumerate() {
            assert_eq!(
                &after_records[idx], original,
                "record at index {idx} must be untouched by config changes"
            );
        }

        // And the decoded bytes themselves still match what was written,
        // proving the *content*, not just the record wrapper, survives.
        for (i, expected) in before.iter().enumerate() {
            match &after_records[i] {
                Record::Rx { data_b64, .. } => {
                    use base64::engine::general_purpose::STANDARD as BASE64;
                    use base64::Engine as _;
                    let decoded = BASE64.decode(data_b64).unwrap();
                    assert_eq!(&decoded, expected);
                }
                other => panic!("expected an Rx record, got {other:?}"),
            }
        }
    }
}
