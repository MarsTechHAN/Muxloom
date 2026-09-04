use std::{fs::File, io::Read, process::ExitCode};

use anyhow::{Result, bail};
use muxloom::{
    daemon::{DaemonPaths, bridge, current_generation, request_status, serve, stop},
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
    let surface = muxloom::control::DaemonControl::new()?;
    muxloom::mcp::serve(
        &surface,
        "muxloomd",
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
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
        Some("register") => register_this_machine(&paths),
        Some("--version" | "-V" | "version") => {
            println!("muxloomd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("protocol-version") => {
            println!("{PROTOCOL_VERSION}");
            Ok(())
        }
        // Which build this is, not merely which release it belongs to. A
        // controller asks a remote companion this to find out whether it is
        // behind: two builds of one version are only told apart by the height
        // in here, and a fleet between two releases that compares versions
        // alone reads as current however far back it has fallen.
        Some("generation") => {
            println!("{}", current_generation());
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
                "muxloomd {}\n\nUSAGE:\n    muxloomd serve\n    muxloomd bridge\n    muxloomd mcp\n    muxloomd register        write this machine's control surface into its agents\n    muxloomd status\n    muxloomd stop            stop the running daemon; sessions keep running\n    muxloomd protocol-version\n    muxloomd generation\n    muxloomd binary-sha256",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(command) => bail!("unknown command {command:?}"),
    }
}

/// Write this machine's control surface into every agent on it — the same
/// step the daemon takes when it starts serving. Running it again is safe:
/// entries that already point here are left alone, and a config that does not
/// parse is left untouched.
///
/// A controller uses this after provisioning a runtime on a target whose own
/// muxloomd is running as a `bridge` (never `serve`), because a bridge never
/// registers its agents for itself. This stand-alone path gives the target the
/// same fleet wiring its own daemon would have, pointing at the muxloomd that
/// is actually on that machine.
#[cfg(unix)]
fn register_this_machine(paths: &DaemonPaths) -> Result<()> {
    let written = muxloom::mcp_register::register_for_this_daemon(paths.is_the_machines_own())?;
    for path in written {
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Off Unix there is no daemon to write a surface for, and `DaemonPaths` there
/// is a stub with no ownership question to ask: the same answer as every other
/// subcommand here.
#[cfg(not(unix))]
fn register_this_machine(_: &DaemonPaths) -> Result<()> {
    bail!("muxloomd is currently supported on Unix targets")
}
