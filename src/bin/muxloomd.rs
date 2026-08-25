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

/// Serve this MCP session from the controller instead. `exec` rather than a
/// child process: the agent is already talking to this process's stdin and
/// stdout, and replacing the image hands it the same pipes with nothing left in
/// the middle to keep alive or to lose an error through.
#[cfg(unix)]
fn hand_over(controller: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(controller).arg("mcp").exec();
    Err(anyhow::Error::new(error).context(format!("failed to run {controller} mcp")))
}

/// This machine's control surface, over stdio.
///
/// Every agent on this machine has the same entry, and it points here. A
/// moderator is the exception: its work is the fleet, so it is handed to the
/// controller beside this daemon before anything is served, taking over these
/// very pipes.
#[cfg(unix)]
fn mcp_entry(paths: &DaemonPaths) -> Result<()> {
    let session_path = std::env::var("MUXLOOM_SESSION_PATH").ok();
    let daemon = std::env::current_exe().unwrap_or_default();
    if let Some(controller) =
        muxloom::mcp_register::handover_to_controller(&paths.root, session_path.as_deref(), &daemon)
    {
        return hand_over(&controller);
    }
    let mut surface = muxloom::control::DaemonControl::new()?;
    muxloom::mcp::serve(
        &mut surface,
        "muxloomd",
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    )
}

/// Off Unix there is no daemon to serve a surface for, and no state directory
/// to read one out of: `DaemonPaths` there is a stub with nothing in it.
#[cfg(not(unix))]
fn mcp_entry(_: &DaemonPaths) -> Result<()> {
    bail!("muxloomd is currently supported on Unix targets")
}

fn run() -> Result<()> {
    let paths = DaemonPaths::discover()?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve(&paths),
        Some("bridge") => bridge(&paths),
        Some("keeper") => keeper_entry(),
        Some("stop") => stop(&paths),
        Some("mcp") => mcp_entry(&paths),
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
