use std::{fs::File, io::Read, process::ExitCode};

use anyhow::{Result, bail};
use muxloom::{
    daemon::{DaemonPaths, bridge, request_status, serve},
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
                "muxloomd {}\n\nUSAGE:\n    muxloomd serve\n    muxloomd bridge\n    muxloomd status\n    muxloomd protocol-version\n    muxloomd binary-sha256",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(command) => bail!("unknown command {command:?}"),
    }
}
