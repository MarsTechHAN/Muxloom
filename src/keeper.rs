//! The session keeper: the smallest process that can keep an agent running.
//!
//! `muxloomd` generations change — the daemon is replaced by handover, and it
//! can crash. Sessions must not care. Each managed session is therefore owned
//! by one keeper process whose only responsibilities are the PTY, the child
//! process, and appending raw output to the session's history file. The daemon
//! is just the keeper's current client: it connects to `keepers/<id>.sock`,
//! relays input and output, and can disconnect — or die — without the session
//! noticing. A new daemon adopts every running session by connecting to the
//! sockets it finds.
//!
//! # The protocol is frozen
//!
//! Keeper processes are launched once and then outlive arbitrarily many daemon
//! generations, so `KEEPER_PROTOCOL` version 1 is final: new daemons must keep
//! speaking it to old keepers forever. Do not add responsibilities here — the
//! whole point of the keeper is that it never needs to be updated. Unknown
//! frame kinds are ignored so a hypothetical v2 client degrades instead of
//! wedging a running session.
//!
//! Wire format: the keeper greets each client with the magic `MXK1` followed
//! by a `Hello` frame; after that both sides exchange `[kind u8][len u32 BE]
//! [payload]` frames.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

pub const KEEPER_PROTOCOL: u8 = 1;
const MAGIC: [u8; 4] = *b"MXK1";
const MAX_PAYLOAD: usize = 256 * 1024;
const DATA_CHUNK: usize = 64 * 1024;

pub mod frame {
    pub const HELLO: u8 = 1;
    pub const DATA: u8 = 2;
    pub const RESIZE: u8 = 3;
    pub const KILL: u8 = 4;
    pub const QUIT: u8 = 5;
    pub const PING: u8 = 6;
    pub const PONG: u8 = 7;
    pub const EXITED: u8 = 8;
}

/// Everything a keeper needs to own one session, handed over stdin as one
/// JSON line so environment values never appear on a command line.
///
/// `program` is the resolved absolute executable and `environment` is the
/// final ordered variable list (later entries win): the daemon does all
/// resolution so the keeper's behavior never has to change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeeperSpec {
    pub session_id: String,
    pub program: String,
    pub args: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub cwd: String,
    pub columns: u16,
    pub rows: u16,
    /// `None` for temporary sessions, which retain no transcript.
    pub history_path: Option<PathBuf>,
    pub socket_path: PathBuf,
}

/// The keeper's self-description, sent as `Hello` on connect and `Pong` on
/// demand. It names its session because the socket filename cannot: socket
/// paths are digests to stay under the Unix socket path limit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeeperStatus {
    pub keeper_protocol: u8,
    pub session_id: String,
    pub keeper_pid: u32,
    pub child_pid: Option<u32>,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub columns: u16,
    pub rows: u16,
}

/// The socket path for a session's keeper. Session ids are too long to embed
/// in a socket path — `sockaddr_un` caps the whole path around 104 bytes on
/// macOS — so the filename is a digest, deterministic from the id.
pub fn socket_path_for(keepers: &std::path::Path, session_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(session_id.as_bytes());
    let mut name = String::with_capacity(21);
    for byte in &digest[..8] {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".sock");
    keepers.join(name)
}

pub fn write_frame(writer: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Read one frame; `None` on a clean EOF at a frame boundary.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    let mut offset = 0;
    while offset < header.len() {
        match reader.read(&mut header[offset..])? {
            0 if offset == 0 => return Ok(None),
            0 => bail!("truncated keeper frame header"),
            read => offset += read,
        }
    }
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    if length > MAX_PAYLOAD {
        bail!("keeper frame payload is too large: {length}");
    }
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .context("truncated keeper frame payload")?;
    Ok(Some((header[0], payload)))
}

/// Read the greeting a keeper opens every connection with.
pub fn read_greeting(reader: &mut impl Read) -> Result<KeeperStatus> {
    let mut magic = [0u8; 4];
    reader
        .read_exact(&mut magic)
        .context("keeper closed before greeting")?;
    if magic != MAGIC {
        bail!("invalid keeper magic");
    }
    match read_frame(reader)? {
        Some((frame::HELLO, payload)) => decode_status(&payload),
        Some((kind, _)) => bail!("keeper greeted with unexpected frame kind {kind}"),
        None => bail!("keeper closed before hello"),
    }
}

pub fn decode_status(payload: &[u8]) -> Result<KeeperStatus> {
    let status: KeeperStatus =
        serde_json::from_slice(payload).context("invalid keeper status payload")?;
    if status.keeper_protocol != KEEPER_PROTOCOL {
        bail!(
            "keeper speaks protocol {}, expected {KEEPER_PROTOCOL}",
            status.keeper_protocol
        );
    }
    Ok(status)
}

/// Entry point for the `muxloomd keeper` subcommand: one JSON spec line on
/// stdin, then serve until the child is gone and the client says quit.
pub fn keeper_main() -> Result<()> {
    use std::io::BufRead;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read the keeper spec")?;
    let spec: KeeperSpec = serde_json::from_str(line.trim()).context("invalid keeper spec")?;
    let listener = bind_socket(&spec.socket_path)?;
    run(spec, listener)
}

/// Bind the keeper's socket, replacing any stale file from a dead keeper.
pub fn bind_socket(socket_path: &std::path::Path) -> Result<UnixListener> {
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove stale {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

struct KeeperState {
    /// Writer half toward the one connected client, if any, tagged with its
    /// accept generation so a superseded client's thread cannot evict its
    /// replacement.
    client: Mutex<Option<(u64, UnixStream)>>,
    pty_writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    exit_code: Mutex<Option<i32>>,
    session_id: String,
    child_pid: Option<u32>,
    columns: Mutex<(u16, u16)>,
    /// Set when the keeper's work is over; the accept loop notices, removes
    /// the socket, and `run` returns. Tests run keepers on threads, so this
    /// must never be a process exit.
    done: AtomicBool,
}

impl KeeperState {
    fn status(&self) -> KeeperStatus {
        let exit_code = *self
            .exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (columns, rows) = *self
            .columns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        KeeperStatus {
            keeper_protocol: KEEPER_PROTOCOL,
            session_id: self.session_id.clone(),
            keeper_pid: std::process::id(),
            child_pid: self.child_pid,
            alive: exit_code.is_none(),
            exit_code,
            columns,
            rows,
        }
    }

    fn send_to_client(&self, kind: u8, payload: &[u8]) {
        let mut client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, stream)) = client.as_mut()
            && write_frame(stream, kind, payload).is_err()
        {
            // A client that cannot be written to is gone; the session
            // continues for the next one.
            *client = None;
        }
    }

    fn has_client(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
    }
}

/// Serve one session until its child is gone and no client claims it. This is
/// the whole keeper: everything else about a session — screens, status,
/// search, metadata — is the daemon's business.
pub fn run(spec: KeeperSpec, listener: UnixListener) -> Result<()> {
    let pair = native_pty_system().openpty(PtySize {
        rows: spec.rows.max(5),
        cols: spec.columns.max(20),
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(&spec.program);
    command.args(spec.args.iter());
    command.cwd(&spec.cwd);
    for (name, value) in &spec.environment {
        command.env(name, value);
    }
    let child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let child_pid = child.process_id();
    let mut pty_reader = pair.master.try_clone_reader()?;
    let pty_writer = pair.master.take_writer()?;

    let state = Arc::new(KeeperState {
        client: Mutex::new(None),
        pty_writer: Mutex::new(pty_writer),
        master: Mutex::new(pair.master),
        child: Mutex::new(child),
        exit_code: Mutex::new(None),
        session_id: spec.session_id.clone(),
        child_pid,
        columns: Mutex::new((spec.columns.max(20), spec.rows.max(5))),
        done: AtomicBool::new(false),
    });

    let reader_state = Arc::clone(&state);
    let history_path = spec.history_path.clone();
    thread::spawn(move || {
        let mut history = history_path.as_ref().and_then(|path| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|error| eprintln!("keeper history open failed: {error}"))
                .ok()
        });
        let mut buffer = vec![0u8; DATA_CHUNK];
        loop {
            match pty_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let bytes = &buffer[..read];
                    if let Some(history) = history.as_mut() {
                        let _ = history.write_all(bytes);
                    }
                    reader_state.send_to_client(frame::DATA, bytes);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if let Some(history) = history.as_mut() {
            let _ = history.flush();
        }
        let code = reader_state
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .wait()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(-1);
        *reader_state
            .exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(code);
        if reader_state.has_client() {
            // The connected daemon hears the exit and answers with QUIT once
            // it has recorded the death.
            reader_state.send_to_client(frame::EXITED, &code.to_be_bytes());
        } else {
            // Nobody to tell: leave the record on disk and go. The next
            // daemon finds no socket and retires the session from metadata.
            reader_state.finish();
        }
    });

    listener.set_nonblocking(true)?;
    let mut next_client = 0u64;
    loop {
        if state.done.load(Ordering::Acquire) {
            break;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => break,
        };
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let mut reader = match stream.try_clone() {
            Ok(reader) => reader,
            Err(_) => continue,
        };
        next_client += 1;
        let client_id = next_client;
        {
            // One client at a time: a newer daemon adopting the session
            // replaces whatever generation held it before.
            let mut client = state
                .client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((_, previous)) = client.take() {
                let _ = previous.shutdown(std::net::Shutdown::Both);
            }
            let mut greeting = stream;
            if greeting.write_all(&MAGIC).is_err() {
                continue;
            }
            let hello = serde_json::to_vec(&state.status()).unwrap_or_default();
            if write_frame(&mut greeting, frame::HELLO, &hello).is_err() {
                continue;
            }
            *client = Some((client_id, greeting));
        }
        let client_state = Arc::clone(&state);
        thread::spawn(move || {
            while let Ok(Some((kind, payload))) = read_frame(&mut reader) {
                match kind {
                    frame::DATA => {
                        let mut writer = client_state
                            .pty_writer
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if writer
                            .write_all(&payload)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                    frame::RESIZE if payload.len() == 4 => {
                        let columns = u16::from_be_bytes([payload[0], payload[1]]).max(20);
                        let rows = u16::from_be_bytes([payload[2], payload[3]]).max(5);
                        let resized = client_state
                            .master
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .resize(PtySize {
                                rows,
                                cols: columns,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        if resized.is_ok() {
                            *client_state
                                .columns
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = (columns, rows);
                        }
                    }
                    frame::KILL => {
                        let mut child = client_state
                            .child
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let _ = child.kill();
                    }
                    frame::QUIT => {
                        if client_state
                            .exit_code
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .is_some()
                        {
                            client_state.finish();
                            break;
                        }
                        // A quit for a living child is ignored: killing the
                        // session is what KILL is for.
                    }
                    frame::PING => {
                        let pong = serde_json::to_vec(&client_state.status()).unwrap_or_default();
                        client_state.send_to_client(frame::PONG, &pong);
                    }
                    _ => {}
                }
            }
            // A client that disconnects after the child died is done with the
            // session even if it never said QUIT; do not park forever.
            let this_client_left = {
                let mut client = client_state
                    .client
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Only clear the slot while it still belongs to this
                // connection; a replacement already took it otherwise.
                if client.as_ref().is_some_and(|(id, _)| *id == client_id) {
                    *client = None;
                    true
                } else {
                    false
                }
            };
            if this_client_left
                && client_state
                    .exit_code
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_some()
            {
                client_state.finish();
            }
        });
    }
    let _ = fs::remove_file(&spec.socket_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_reject_oversized_payloads() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame::DATA, b"hello").unwrap();
        write_frame(&mut bytes, frame::PING, b"").unwrap();
        let mut cursor = bytes.as_slice();
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((frame::DATA, b"hello".to_vec()))
        );
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((frame::PING, vec![]))
        );
        assert_eq!(read_frame(&mut cursor).unwrap(), None);

        let mut oversized = vec![frame::DATA];
        oversized.extend(((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        assert!(read_frame(&mut oversized.as_slice()).is_err());
    }

    #[test]
    fn status_decoding_enforces_the_frozen_protocol_version() {
        let status = KeeperStatus {
            keeper_protocol: KEEPER_PROTOCOL,
            session_id: "muxloomd-terminal-1-2-3".into(),
            keeper_pid: 1,
            child_pid: Some(2),
            alive: true,
            exit_code: None,
            columns: 80,
            rows: 24,
        };
        let payload = serde_json::to_vec(&status).unwrap();
        assert_eq!(decode_status(&payload).unwrap(), status);

        let mut wrong = status.clone();
        wrong.keeper_protocol = 2;
        let payload = serde_json::to_vec(&wrong).unwrap();
        assert!(decode_status(&payload).is_err());
    }
}
