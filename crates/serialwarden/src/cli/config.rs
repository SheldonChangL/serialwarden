//! `serialwarden config [device] [flags]` (issue #10 / `TASKS.md` T2.3): read
//! or update one device's shared port configuration.
//!
//! Read (no flags): prints the current configuration plus error counts,
//! `unavailable` rather than a misleading `0` on platforms (macOS) with no
//! way to measure them — see `serialwardend::error_counts`'s module docs.
//!
//! Write (any config flag given): merges only the given fields onto the
//! device's current configuration (`set_config`'s wire semantics — see
//! `warden_proto::Request::SetConfig`'s docs) and/or issues a manual DTR/RTS
//! toggle (`set_control_line`) — two distinct requests, since a manual
//! toggle and the device's *persisted, open-time* control-line policy are
//! different things (see `serialwardend::port_config::OpenControlLines`'s
//! docs). Every write goes through the daemon's already-implemented
//! `config_change`/`control_line_change` event semantics — this subcommand
//! adds no new daemon-side behavior, only the client side of requests that
//! already work end-to-end.
//!
//! A config write reports what the daemon says happened, not just that the
//! request succeeded: saved and applied, already in effect, saved for a
//! disconnected device, or saved but rejected by the port. The last one
//! exits non-zero, so `serialwarden config --baud N && next-step` does not go
//! on as if the port were running at `N` (see `warden_proto::ConfigApply`).

use std::io::{self, Write as _};

use clap::Args;
use serde_json::Value;

use serialwardend::port_config::{DataBits, FlowControl, OpenControlLines, Parity, StopBits};
use warden_proto::{ConfigApply, ConfigApplyOutcome, Request};

use super::client::{resolve_device, resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum ParityArg {
    None,
    Odd,
    Even,
}

impl From<ParityArg> for Parity {
    fn from(v: ParityArg) -> Self {
        match v {
            ParityArg::None => Parity::None,
            ParityArg::Odd => Parity::Odd,
            ParityArg::Even => Parity::Even,
        }
    }
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum FlowArg {
    None,
    Software,
    Hardware,
}

impl From<FlowArg> for FlowControl {
    fn from(v: FlowArg) -> Self {
        match v {
            FlowArg::None => FlowControl::None,
            FlowArg::Software => FlowControl::Software,
            FlowArg::Hardware => FlowControl::Hardware,
        }
    }
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum OnOff {
    On,
    Off,
}

impl From<OnOff> for bool {
    fn from(v: OnOff) -> Self {
        matches!(v, OnOff::On)
    }
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Device id (see `serialwarden devices`). Omit only when exactly one
    /// device is known to the daemon.
    pub device: Option<String>,

    #[arg(long)]
    pub baud: Option<u32>,

    #[arg(long, value_enum)]
    pub parity: Option<ParityArg>,

    /// Data bits: 5, 6, 7, or 8.
    #[arg(long)]
    pub data: Option<u8>,

    /// Stop bits: 1 or 2.
    #[arg(long)]
    pub stop: Option<u8>,

    #[arg(long, value_enum)]
    pub flow: Option<FlowArg>,

    #[arg(long, value_enum)]
    pub dtr: Option<OnOff>,

    #[arg(long, value_enum)]
    pub rts: Option<OnOff>,

    /// Set the device's open-time control-line policy back to "preserve"
    /// (touch neither DTR nor RTS on the next open/reconnect) — the safe
    /// default this project ships (see `serialwardend::port_config`'s docs on
    /// why boards reset when DTR toggles). Distinct from `--dtr`/`--rts`,
    /// which toggle the lines right now rather than changing that policy.
    #[arg(long)]
    pub no_touch_dtr_rts: bool,
}

fn data_bits_from(n: u8) -> io::Result<DataBits> {
    match n {
        5 => Ok(DataBits::Five),
        6 => Ok(DataBits::Six),
        7 => Ok(DataBits::Seven),
        8 => Ok(DataBits::Eight),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--data: must be 5, 6, 7, or 8 (got {n})"),
        )),
    }
}

fn stop_bits_from(n: u8) -> io::Result<StopBits> {
    match n {
        1 => Ok(StopBits::One),
        2 => Ok(StopBits::Two),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--stop: must be 1 or 2 (got {n})"),
        )),
    }
}

pub async fn run(args: ConfigArgs) -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwarden-config", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let device = resolve_device(&mut client, args.device.as_deref()).await?;

    let mut patch = serde_json::Map::new();
    if let Some(baud) = args.baud {
        patch.insert("baud".to_string(), baud.into());
    }
    if let Some(parity) = args.parity {
        patch.insert(
            "parity".to_string(),
            serde_json::to_value(Parity::from(parity)).expect("Parity always serializes"),
        );
    }
    if let Some(data) = args.data {
        let db = data_bits_from(data)?;
        patch.insert(
            "data_bits".to_string(),
            serde_json::to_value(db).expect("DataBits always serializes"),
        );
    }
    if let Some(stop) = args.stop {
        let sb = stop_bits_from(stop)?;
        patch.insert(
            "stop_bits".to_string(),
            serde_json::to_value(sb).expect("StopBits always serializes"),
        );
    }
    if let Some(flow) = args.flow {
        patch.insert(
            "flow_control".to_string(),
            serde_json::to_value(FlowControl::from(flow)).expect("FlowControl always serializes"),
        );
    }
    if args.no_touch_dtr_rts {
        patch.insert(
            "open_control_lines".to_string(),
            serde_json::to_value(OpenControlLines::Preserve)
                .expect("OpenControlLines always serializes"),
        );
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let wrote_config = !patch.is_empty();
    let mut config_outcome = None;
    let mut legacy_daemon = false;
    if wrote_config {
        let reply = client
            .call(Request::SetConfig {
                device: device.clone(),
                config: patch,
            })
            .await?;
        check_ok(&reply, &device)?;
        // A daemon from before these fields existed replies with just
        // `config`. The change was still made; say what is not known rather
        // than fail a request that succeeded.
        config_outcome = serde_json::from_value::<ConfigApplyOutcome>(reply).ok();
        legacy_daemon = config_outcome.is_none();
    }

    let wrote_control_line = args.dtr.is_some() || args.rts.is_some();
    if wrote_control_line {
        let reply = client
            .call(Request::SetControlLine {
                device: device.clone(),
                dtr: args.dtr.map(bool::from),
                rts: args.rts.map(bool::from),
            })
            .await?;
        check_ok(&reply, &device)?;
    }

    let reply = client
        .call(Request::GetConfig {
            device: device.clone(),
        })
        .await?;
    check_ok(&reply, &device)?;
    if let Some(outcome) = &config_outcome {
        writeln!(out, "{}", describe_outcome(&device, outcome))?;
    } else if legacy_daemon {
        writeln!(
            out,
            "{device}: config saved (this daemon does not report whether the port applied it)"
        )?;
    } else if wrote_control_line {
        writeln!(out, "{device}: control lines updated")?;
    }
    print_config(&mut out, &device, &reply)?;

    match config_outcome {
        Some(ConfigApplyOutcome {
            apply: ConfigApply::Failed,
            apply_error,
            ..
        }) => Err(io::Error::other(format!(
            "{device}: the port rejected the new configuration ({}); it is saved but not in \
             effect",
            apply_error.as_deref().unwrap_or("no error given")
        ))),
        _ => Ok(()),
    }
}

/// One line saying what a `set_config` did, from the daemon's own outcome
/// fields. The printed configuration below it is always the *saved* one,
/// which is only what the port is running when this line says so.
fn describe_outcome(device: &str, outcome: &ConfigApplyOutcome) -> String {
    match (outcome.changed, outcome.apply) {
        (true, ConfigApply::Live) => format!("{device}: config updated and applied to the port"),
        (false, ConfigApply::Live) => {
            format!(
                "{device}: config unchanged; re-applied to the port (its last apply had failed)"
            )
        }
        (_, ConfigApply::AlreadyApplied) => {
            format!("{device}: config unchanged (already in effect); nothing recorded")
        }
        (true, ConfigApply::NotConnected) => format!(
            "{device}: config saved; the port is not open (device disconnected, or leased), so \
             it will be applied when the port next opens"
        ),
        (false, ConfigApply::NotConnected) => {
            format!("{device}: config unchanged; the port is not open (disconnected or leased)")
        }
        (_, ConfigApply::Failed) => format!(
            "{device}: config saved but NOT applied: the port rejected it ({}). The port keeps \
             its previous settings until the device reconnects or is reopened (unplug/replug, or \
             `serialwarden run {device} -- true`; reopening may toggle DTR/RTS and reset the \
             board).",
            outcome.apply_error.as_deref().unwrap_or("no error given")
        ),
    }
}

fn check_ok(reply: &Value, device: &str) -> io::Result<()> {
    if reply["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    Err(io::Error::other(describe_wire_error(
        &reply["error"],
        Some(device),
    )))
}

fn print_config(out: &mut impl io::Write, device: &str, reply: &Value) -> io::Result<()> {
    let config = &reply["config"];
    let baud = config["baud"]
        .as_u64()
        .map(|b| b.to_string())
        .unwrap_or_else(|| "-".to_string());
    let data_bits = config["data_bits"].as_str().unwrap_or("-");
    let parity = config["parity"].as_str().unwrap_or("-");
    let stop_bits = config["stop_bits"].as_str().unwrap_or("-");
    let flow = config["flow_control"].as_str().unwrap_or("-");
    let open_lines = &config["open_control_lines"];
    let open_lines_desc = match open_lines["mode"].as_str() {
        Some("assert") => format!(
            "assert(dtr={}, rts={})",
            open_lines["dtr"].as_bool().unwrap_or(false),
            open_lines["rts"].as_bool().unwrap_or(false)
        ),
        _ => "preserve".to_string(),
    };

    writeln!(
        out,
        "{device}\tbaud={baud} data_bits={data_bits} parity={parity} stop_bits={stop_bits} \
         flow={flow} open_control_lines={open_lines_desc}"
    )?;

    // The line above is the *saved* config; say when the port isn't running
    // it. Absent from a daemon that predates these fields: print nothing.
    match reply["apply"].as_str() {
        Some("failed") => {
            let error = reply["apply_error"].as_str().unwrap_or("no error given");
            let last = match reply["last_applied"]["baud"].as_u64() {
                Some(baud) => format!("; the port last accepted baud={baud}"),
                None => String::new(),
            };
            writeln!(
                out,
                "port: NOT running the saved config (its last apply failed: {error}){last}"
            )?;
        }
        Some("not_connected") => writeln!(
            out,
            "port: not open; the saved config applies when it next opens"
        )?,
        _ => {}
    }

    let error_counts = &reply["error_counts"];
    match error_counts["status"].as_str() {
        Some("available") => writeln!(
            out,
            "error counts: framing={} overrun={} parity={}",
            error_counts["framing"].as_u64().unwrap_or(0),
            error_counts["overrun"].as_u64().unwrap_or(0),
            error_counts["parity"].as_u64().unwrap_or(0),
        )?,
        _ => writeln!(out, "error counts: unavailable")?,
    }
    out.flush()
}
