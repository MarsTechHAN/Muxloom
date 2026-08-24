use std::{fs::File, io::Read, process::ExitCode};

use anyhow::{Result, bail};
use muxloom::{
    daemon::{DaemonPaths, bridge, request_status, serve, stop},
    daemon_protocol::{DaemonResponse, PROTOCOL_VERSION},
};
use sha2::{Digest, Sha256};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("muxloomd: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// One session's keeper: internal, launched by the daemon, never by hand.
#[cfg(unix)]
fn keeper_entry() -> Result<()> {
    muxloom::keeper::keeper_main()
}

#[cfg(not(unix))]
fn keeper_entry() -> Result<()> {
    bail!("muxloomd is currently supported on Unix targets")
}

fn run() -> Result<()> {
    let paths = DaemonPaths::discover()?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve(&paths),
        Some("bridge") => bridge(&paths),
        Some("keeper") => keeper_entry(),
        Some("stop") => stop(&paths),
        Some("mcp") => {
            let mut surface = muxloom::control::DaemonControl::new()?;
            muxloom::mcp::serve(
                &mut surface,
                "muxloomd",
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            )
        }
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
        Some("protocol-version") => {
            println!("{PROTOCOL_VERSION}");
            Ok(())
        }
        Some("binary-sha256") => {
            let mut executable = File::open(std::env::current_exe()?)?;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = executable.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
            println!("{:x}", digest.finalize());
            Ok(())
        }
        Some("--help" | "-h" | "help") | None => {
            println!(
                "muxloomd {}\n\nUSAGE:\n    muxloomd serve\n    muxloomd bridge\n    muxloomd mcp\n    muxloomd status\n    muxloomd stop            stop the running daemon; sessions keep running\n    muxloomd protocol-version\n    muxloomd binary-sha256",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(command) => bail!("unknown command {command:?}"),
    }
}
