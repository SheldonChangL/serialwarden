use serde::{Deserialize, Serialize};

/// Whether a configuration reached the open port, or only the saved
/// profile. Carried by the `set_config` reply and by the `config_change`/
/// `config_reapplied` events, so a client and the timeline both see the same
/// answer.
///
/// "Saved" and "running on the port" are different facts. A config is always
/// persisted first, so the next open applies it, but the live port can refuse
/// it: a macOS PL2303 driver was seen returning `EINVAL` for every live
/// change while a freshly opened fd accepted the same settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigApply {
    /// The open port accepted the configuration during this request.
    Live,
    /// Nothing changed and the open port was already running this
    /// configuration, so the port was not touched.
    AlreadyApplied,
    /// The device has no open port (disconnected, or handed out on a lease).
    /// The configuration is saved and applied the next time it opens.
    NotConnected,
    /// The open port rejected the configuration (see `apply_error`). It is
    /// saved and applied the next time the device opens; until then the port
    /// keeps whatever it was running before.
    Failed,
}

impl ConfigApply {
    /// Whether the open port is now running the requested configuration.
    pub fn applied(self) -> bool {
        matches!(self, ConfigApply::Live | ConfigApply::AlreadyApplied)
    }
}

/// The outcome fields of a successful `set_config` reply, next to its
/// `config` (the saved configuration). A request only fails outright when
/// nothing could be saved; a port that refuses the change is a success with
/// `applied: false`, because the change *was* saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigApplyOutcome {
    /// `false` when the request matched the configuration already saved. No
    /// `config_change` is recorded for it.
    pub changed: bool,
    /// Shorthand for `apply.applied()`: is the port running `config` now?
    pub applied: bool,
    pub apply: ConfigApply,
    /// The port's error, present exactly when `apply` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_error: Option<String>,
}

impl ConfigApplyOutcome {
    pub fn new(changed: bool, apply: ConfigApply, apply_error: Option<String>) -> Self {
        Self {
            changed,
            applied: apply.applied(),
            apply,
            apply_error,
        }
    }
}

/// Whether the open port is running the saved configuration: the fields a
/// `get_config` reply (UDS, web, MCP) carries next to `config`, which is
/// always the *saved* one. Without these a client showing `config` shows
/// what the daemon will apply next time, not necessarily what the port runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortApplyState {
    /// The port is open and known to be running the saved `config`.
    pub applied: bool,
    /// `live` (running the saved config), `not_connected` (no open port),
    /// or `failed` (the last application to the open port failed, so it is
    /// not known to run the saved config). Never `already_applied`.
    pub apply: ConfigApply,
    /// The error from the last failed application, when `apply` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_error: Option<String>,
    /// When `apply` is `failed`: the last configuration the open port
    /// accepted, if any (a port config object, same shape as `config`; it
    /// can equal `config` when a later apply of it failed). After a failed
    /// change this is what the port was left on — the driver may have taken
    /// part of the failed change, so "last accepted", not a read-back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_applied: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_rejected_live_apply_serializes_with_its_error() {
        let outcome = ConfigApplyOutcome::new(
            true,
            ConfigApply::Failed,
            Some("Invalid argument (os error 22)".to_string()),
        );
        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            json!({
                "changed": true,
                "applied": false,
                "apply": "failed",
                "apply_error": "Invalid argument (os error 22)",
            })
        );
    }

    #[test]
    fn an_applied_outcome_omits_apply_error_and_round_trips() {
        let outcome = ConfigApplyOutcome::new(false, ConfigApply::AlreadyApplied, None);
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            value,
            json!({"changed": false, "applied": true, "apply": "already_applied"})
        );
        let back: ConfigApplyOutcome = serde_json::from_value(value).unwrap();
        assert_eq!(back, outcome);
    }

    #[test]
    fn only_live_and_already_applied_count_as_applied() {
        assert!(ConfigApply::Live.applied());
        assert!(ConfigApply::AlreadyApplied.applied());
        assert!(!ConfigApply::NotConnected.applied());
        assert!(!ConfigApply::Failed.applied());
    }
}
