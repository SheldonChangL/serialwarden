//! Protocol and on-disk record types shared by `serialwardend`, the `serialwarden`
//! CLI, and the MCP bridge.
//!
//! This crate sits at the bottom of the dependency graph
//! (`serialwarden` -> `serialwardend` -> `warden-proto`) and must never depend on
//! either of the other two workspace crates. It only defines shapes — no
//! I/O, no daemon logic, no CLI logic.
//!
//! See `TASKS.md` ("0. 範圍與技術基線") for the authoritative record schema
//! this crate mirrors, and the [Architecture wiki
//! page](https://github.com/SheldonChangL/serialwarden/wiki/Architecture) for
//! how these types are used across the daemon/client boundary.

mod client;
mod config_apply;
mod error;
mod hello;
mod record;
mod request;
mod wire_error;

pub use client::ClientType;
pub use config_apply::{ConfigApply, ConfigApplyOutcome};
pub use error::ErrorCode;
pub use hello::{HelloAck, HelloRequest, Permission};
pub use record::{Kind, Record};
pub use request::{ExportBound, ExportFormat, Filter, LineEnding, Request};
pub use wire_error::WireError;
