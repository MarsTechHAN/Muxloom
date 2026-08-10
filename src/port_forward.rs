use std::{
    collections::BTreeMap,
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{bridge::BridgePool, debug, model::Target};

const ACCEPT_POLL: Duration = Duration::from_millis(40);
const SOCKET_POLL: Duration = Duration::from_millis(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortForwardState {
    Starting,
    Active,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForwardSummary {
    pub id: u64,
    pub target_id: String,
    pub session_id: String,
    pub folder: String,
    pub remote_host: String,
    pub remote_port: u16,
    pub local_port: u16,
    pub state: PortForwardState,
}

struct PortForwardEntry {
    summary: PortForwardSummary,
    state: Arc<Mutex<PortForwardState>>,
    stop: Arc<AtomicBool>,
}

pub struct PortForwardManager {
    entries: Arc<Mutex<BTreeMap<u64, PortForwardEntry>>>,
    next_id: AtomicU64,
}

impl Default for PortForwardManager {
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            next_id: AtomicU64::new(1),
        }
    }
}

impl PortForwardManager {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        bridges: BridgePool,
        target: Target,
        session_id: String,
        folder: String,
        remote_host: String,
        remote_port: u16,
        requested_local_port: u16,
    ) -> Result<PortForwardSummary> {
        if remote_port == 0 {
            bail!("remote port must be greater than zero");
        }
        let listener =
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, requested_local_port))
                .with_context(|| {
                    if requested_local_port == 0 {
                        "failed to allocate a local forwarding port".to_string()
                    } else {
                        format!("local port {requested_local_port} is unavailable")
                    }
                })?;
        listener.set_nonblocking(true)?;
        let local_port = listener.local_addr()?.port();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(Mutex::new(PortForwardState::Starting));
        let stop = Arc::new(AtomicBool::new(false));
        let summary = PortForwardSummary {
            id,
            target_id: target.id.clone(),
            session_id,
            folder,
            remote_host: remote_host.clone(),
            remote_port,
            local_port,
            state: PortForwardState::Starting,
        };
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                id,
                PortForwardEntry {
                    summary: summary.clone(),
                    state: Arc::clone(&state),
                    stop: Arc::clone(&stop),
                },
            );
        thread::spawn(move || {
            if let Err(error) = bridges.ensure_tcp_forward(&target) {
                set_state(&state, PortForwardState::Error(error.to_string()));
                return;
            }
            set_state(&state, PortForwardState::Active);
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let bridges = bridges.clone();
                        let target = target.clone();
                        let remote_host = remote_host.clone();
                        let state = Arc::clone(&state);
                        thread::spawn(move || {
                            set_state(&state, PortForwardState::Active);
                            if let Err(error) = proxy_connection(
                                socket,
                                &bridges,
                                &target,
                                remote_host,
                                remote_port,
                            ) {
                                debug::log(
                                    "forward",
                                    format!(
                                        "target={} remote_port={remote_port} connection failed: {error:#}",
                                        target.id
                                    ),
                                );
                                set_state(&state, PortForwardState::Error(error.to_string()));
                            }
                        });
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                    }
                    Err(error) => {
                        set_state(
                            &state,
                            PortForwardState::Error(format!("local listener failed: {error}")),
                        );
                        return;
                    }
                }
            }
        });
        Ok(summary)
    }

    pub fn stop(&self, id: u64) -> bool {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        if let Some(entry) = entry {
            entry.stop.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Every live forward, with the state its worker thread last published.
    pub fn summaries(&self) -> Vec<PortForwardSummary> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|entry| {
                let mut summary = entry.summary.clone();
                summary.state = entry
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                summary
            })
            .collect()
    }

    pub fn summaries_for(&self, target_id: &str) -> Vec<PortForwardSummary> {
        self.summaries()
            .into_iter()
            .filter(|summary| summary.target_id == target_id)
            .collect()
    }
}

impl Drop for PortForwardManager {
    fn drop(&mut self) {
        for entry in self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
        {
            entry.stop.store(true, Ordering::Release);
        }
    }
}

fn set_state(state: &Mutex<PortForwardState>, value: PortForwardState) {
    *state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

fn proxy_connection(
    mut local: TcpStream,
    bridges: &BridgePool,
    target: &Target,
    remote_host: String,
    remote_port: u16,
) -> Result<()> {
    local.set_read_timeout(Some(SOCKET_POLL))?;
    local.set_nodelay(true)?;
    let mut remote = bridges.open_tcp(target, remote_host, remote_port)?;
    let mut buffer = vec![0; crate::daemon_protocol::DATA_CHUNK_SIZE];
    loop {
        match local.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => remote.write(&buffer[..read])?,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
        while let Some(bytes) = remote.try_read_result()? {
            local.write_all(&bytes)?;
        }
        if remote.is_closed() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_port_zero_allocates_a_loopback_listener() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), 0);
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }
}
