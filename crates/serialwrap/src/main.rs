//! `serialwrap`: the single binary. Subcommands dispatch into the `cli`
//! module tree (or, for `daemon`/`mcp`, directly into `serialwrapd`/`mcp`).
//!
//! Dependency direction: `serialwrap` -> `serialwrapd` -> `wrap-proto`.

use clap::{Parser, Subcommand};

/// `devices`/`tail` (T1.5, issue #7) live in their own module tree rather
/// than inline here — see `cli`'s module docs for why.
mod cli;
/// `serialwrap mcp` (T3.1, issue #12) — see `mcp`'s module docs.
mod mcp;

#[derive(Parser)]
#[command(
    name = "serialwrap",
    // Not clap's bare `version` (which would print `CARGO_PKG_VERSION`
    // alone): this is the string an operator uses to answer "is the binary
    // on my PATH built from the code in front of me?". That question is
    // unavoidable once `serialwrap service install` points a login service
    // at a *copy* of this binary — rebuilding the repo doesn't touch the
    // copy, and `0.1.0` is identical either way. `SERVER_VERSION` carries
    // `git describe --always --dirty`, so `serialwrap --version` can be
    // compared against `git rev-parse --short HEAD` directly, with no
    // daemon running. See `serialwrapd::SERVER_VERSION`.
    version = serialwrapd::SERVER_VERSION,
    about = "Serial port broker: one daemon owns the port, everyone else is a client."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon that owns the serial ports and records them.
    Daemon,
    /// Run the MCP stdio bridge, for AI agents.
    Mcp,
    /// List known devices.
    Devices,
    /// Tail a device's record stream.
    Tail(cli::tail::TailArgs),
    /// Write bytes to a device, subject to the write gate.
    Write(cli::write::WriteArgs),
    /// Take a temporary lease and run an external command (e.g. a flashing
    /// tool) against the device.
    Run(cli::run::RunArgs),
    /// Read or update per-device configuration.
    Config(cli::config::ConfigArgs),
    /// List, kick, or demote connected clients.
    Clients(cli::clients::ClientsArgs),
    /// Export recorded data as jsonl/txt/bin.
    Export(cli::export::ExportArgs),
    /// Query the audit view over the record stream.
    Audit(cli::audit::AuditArgs),
    /// List, approve, or deny pending write approvals.
    Approvals(cli::approvals::ApprovalsArgs),
    /// Install or uninstall the launchd/systemd user service that runs
    /// `serialwrap daemon` in the background.
    Service(cli::service::ServiceArgs),
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => serialwrapd::run().await,
        Command::Mcp => cli::dispatch(mcp::run().await),
        Command::Devices => cli::dispatch(cli::devices::run().await),
        Command::Tail(args) => cli::dispatch(cli::tail::run(args).await),
        Command::Write(args) => cli::dispatch(cli::write::run(args).await),
        Command::Run(args) => cli::dispatch(cli::run::run(args).await),
        Command::Config(args) => cli::dispatch(cli::config::run(args).await),
        Command::Clients(args) => cli::dispatch(cli::clients::run(args).await),
        Command::Export(args) => cli::dispatch(cli::export::run(args).await),
        Command::Audit(args) => cli::dispatch(cli::audit::run(args).await),
        Command::Approvals(args) => cli::dispatch(cli::approvals::run(args).await),
        Command::Service(args) => cli::dispatch(cli::service::run(args).await),
    }
}
