use std::process::ExitCode;

use anyhow::{Result, bail};
use muxloom::{
    daemon::{DaemonPaths, bridge, request_status, serve},
    daemon_protocol::DaemonResponse,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("muxloomd: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let paths = DaemonPaths::discover()?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve(&paths),
        Some("bridge") => bridge(&paths),
        Some("status") => {
            match request_status(&paths)? {
                DaemonResponse::Status {
                    pid,
                    uptime_ms,
                    clients,
                } => println!("pid={pid} uptime_ms={uptime_ms} clients={clients}"),
                response => bail!("unexpected status response: {response:?}"),
            }
            Ok(())
        }
        Some("--version" | "-V" | "version") => {
            println!("muxloomd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h" | "help") | None => {
            println!(
                "muxloomd {}\n\nUSAGE:\n    muxloomd serve\n    muxloomd bridge\n    muxloomd status",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(command) => bail!("unknown command {command:?}"),
    }
}
