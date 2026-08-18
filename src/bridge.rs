use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::{
    daemon_protocol::{
        DaemonHistoryMatch, DaemonRequest, DaemonResponse, DaemonSession, Frame, FrameKind,
        OpenStream, PROTOCOL_VERSION, Trigger, stream,
    },
    debug,
    model::{DirectoryListing, FileListing, FilePreview, Target, TaskProgress, Transport},
    talk::{
        DIRECT_CAPABILITY, TALK_CAPABILITY, TalkDeliver, TalkDraft, TalkFilter, TalkMessage,
        TalkPage, TalkState, TalkVector,
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
/// Keeping standing watches on session screens.
const TRIGGERS_CAPABILITY: &str = "triggers-v1";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Bytes the outbound frame queue may hold before producers wait. Large enough
/// that an upload keeps several 64 KiB chunks in flight, small enough that a
/// stalled link cannot buffer a whole file in memory.
const WRITE_QUEUE_LIMIT: usize = 4 * 1024 * 1024;
/// How long a producer waits for queue room before reporting the link as
/// backed up. Keystrokes and resizes come from the render loop, so they must
/// fail loudly instead of blocking the UI forever.
const WRITE_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const COMPANION_RELEASE_ROOT: &str =
    "https://github.com/MarsTechHAN/Muxloom/releases/latest/download";

#[derive(Debug, Clone)]
pub struct BridgeOptions {
    pub connect_timeout_secs: u64,
    pub command: String,
    pub reverse_tunnel: String,
    pub bootstrap_binary: String,
    pub download_environment: Vec<(String, String)>,
    /// The host's configured environment as the host itself sees it, for the
    /// work a target does on its own behalf — pulling its companion down from
    /// the release rather than waiting for us to push it.
    pub remote_environment: Vec<(String, String)>,
    /// Attention patterns sunk into the daemon right after the handshake, so
    /// waiting states surface at its refresh cadence rather than the
    /// controller's full scans.
    pub attention_patterns: Vec<String>,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 5,
            command: "muxloomd".into(),
            reverse_tunnel: String::new(),
            bootstrap_binary: String::new(),
            download_environment: Vec::new(),
            remote_environment: Vec::new(),
            attention_patterns: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct PendingRequest {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    data: Vec<u8>,
    sender: mpsc::Sender<Result<BridgeReply, String>>,
}

#[derive(Debug)]
struct BridgeReply {
    response: DaemonResponse,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct BridgeHistory {
    pub bytes: Vec<u8>,
    pub total_lines: usize,
    pub columns: u16,
    pub rows: u16,
    pub offset_from_bottom: usize,
    /// Whether the daemon answered in rendered rows. A daemon that only reads
    /// raw log lines leaves this false, whatever was asked for.
    pub rendered: bool,
    /// Whether the read reached the beginning of the log, so `total_lines`
    /// measures the session rather than how far this page happened to reach.
    /// A daemon that predates the field leaves this false.
    pub reached_start: bool,
}

struct ConnectionState {
    target: String,
    writer: Arc<FrameWriter>,
    child: Mutex<Option<Child>>,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    streams: Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>,
    next_request: AtomicU64,
    next_stream: AtomicU64,
    alive: AtomicBool,
    capabilities: Mutex<HashSet<String>>,
    /// The running daemon's version from its Hello, for the update indicator.
    daemon_version: Mutex<Option<String>>,
}

/// Outbound side of one bridge connection. Frames are queued here and written
/// by a dedicated thread, so a slow or stalled link never blocks the caller —
/// keystrokes, resizes, and stream teardown are all issued from the render
/// loop, where a blocking `write` would freeze the whole dashboard.
#[derive(Debug, Default)]
struct FrameWriter {
    queue: Mutex<FrameQueue>,
    /// Signals the writer thread that frames are waiting.
    pending: Condvar,
    /// Signals producers that the queue has room again.
    drained: Condvar,
}

#[derive(Debug, Default)]
struct FrameQueue {
    frames: VecDeque<Frame>,
    bytes: usize,
    closed: bool,
}

impl FrameWriter {
    /// Queue one frame. Returns once it is accepted for writing, not once it
    /// reaches the peer.
    fn send(&self, frame: Frame) -> Result<()> {
        let size = frame.payload.len() + crate::daemon_protocol::HEADER_LEN;
        let deadline = Instant::now() + WRITE_QUEUE_TIMEOUT;
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // A single oversized frame is always admitted once the queue is empty,
        // so an upload chunk larger than the limit cannot deadlock.
        while !queue.closed && queue.bytes > 0 && queue.bytes + size > WRITE_QUEUE_LIMIT {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("muxloomd bridge write queue is full");
            }
            let (next, _) = self
                .drained
                .wait_timeout(queue, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue = next;
        }
        if queue.closed {
            bail!("muxloomd bridge writer has stopped");
        }
        queue.bytes += size;
        queue.frames.push_back(frame);
        self.pending.notify_one();
        Ok(())
    }

    /// Stop accepting frames and wake everyone waiting on the queue.
    fn close(&self) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed = true;
        self.pending.notify_all();
        self.drained.notify_all();
    }

    fn next_frame(&self) -> Option<Frame> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(frame) = queue.frames.pop_front() {
                return Some(frame);
            }
            if queue.closed {
                return None;
            }
            queue = self
                .pending
                .wait(queue)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn release(&self, size: usize) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.bytes = queue.bytes.saturating_sub(size);
        self.drained.notify_all();
    }
}

fn spawn_writer(
    writer: Arc<FrameWriter>,
    state: Weak<ConnectionState>,
    mut sink: impl Write + Send + 'static,
) {
    thread::spawn(move || {
        while let Some(frame) = writer.next_frame() {
            let size = frame.payload.len() + crate::daemon_protocol::HEADER_LEN;
            let result = frame.write_to(&mut sink);
            writer.release(size);
            if let Err(error) = result {
                writer.close();
                if let Some(state) = state.upgrade() {
                    debug::log(
                        "bridge",
                        format!("target={} writer stopped: {error}", state.target),
                    );
                    state.fail_all(error.to_string());
                }
                return;
            }
        }
    });
}

#[derive(Debug)]
enum StreamEvent {
    Data(Vec<u8>),
    Closed,
    Error(String),
}

pub struct BridgeStream {
    state: Arc<ConnectionState>,
    stream_id: u32,
    events: mpsc::Receiver<StreamEvent>,
    closed: bool,
}

impl BridgeStream {
    pub fn try_read(&mut self) -> Option<Vec<u8>> {
        self.try_read_result().ok().flatten()
    }

    pub fn try_read_result(&mut self) -> Result<Option<Vec<u8>>> {
        match self.events.try_recv() {
            Ok(StreamEvent::Data(bytes)) => Ok(Some(bytes)),
            Ok(StreamEvent::Error(message)) => {
                self.closed = true;
                Err(anyhow!(message))
            }
            Ok(StreamEvent::Closed) | Err(mpsc::TryRecvError::Disconnected) => {
                self.closed = true;
                Ok(None)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
        }
    }

    pub fn read_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        match self.events.recv_timeout(timeout) {
            Ok(StreamEvent::Data(bytes)) => Ok(Some(bytes)),
            Ok(StreamEvent::Error(message)) => {
                self.closed = true;
                Err(anyhow!(message))
            }
            Ok(StreamEvent::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Ok(None)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("daemon stream timed out")
            }
        }
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.write_data(bytes, false)
    }

    fn write_data(&self, bytes: &[u8], compress: bool) -> Result<()> {
        if self.is_closed() {
            bail!("daemon terminal stream is closed");
        }
        self.state
            .writer
            .send(Frame::data(self.stream_id, 0, bytes, compress))
    }

    pub fn is_closed(&self) -> bool {
        self.closed || !self.state.alive.load(Ordering::Acquire)
    }
}

impl Drop for BridgeStream {
    fn drop(&mut self) {
        self.state
            .streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.stream_id);
        let frame = Frame::new(FrameKind::CloseStream, self.stream_id, 0, vec![]);
        let _ = self.state.writer.send(frame);
    }
}

impl ConnectionState {
    fn fail_all(&self, message: impl Into<String>) {
        self.alive.store(false, Ordering::Release);
        let message = message.into();
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, request) in pending {
            let _ = request.sender.send(Err(message.clone()));
        }
        let streams = std::mem::take(
            &mut *self
                .streams
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, stream) in streams {
            let _ = stream.send(StreamEvent::Closed);
        }
    }

    fn shutdown(&self) {
        self.fail_all("bridge connection closed");
        self.writer.close();
        if let Some(mut child) = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub struct BridgeConnection {
    state: Arc<ConnectionState>,
}

impl BridgeConnection {
    pub fn connect_ssh(
        alias: &str,
        options: &BridgeOptions,
    ) -> Result<(Arc<Self>, Option<String>)> {
        Self::connect_ssh_with_progress(alias, options, |_| {})
    }

    pub fn connect_ssh_with_progress(
        alias: &str,
        options: &BridgeOptions,
        mut progress: impl FnMut(TaskProgress),
    ) -> Result<(Arc<Self>, Option<String>)> {
        progress(TaskProgress::pending(format!("Connecting to {alias}")));
        let mut command = Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "RequestTTY=no",
            "-o",
            &format!("ConnectTimeout={}", options.connect_timeout_secs),
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ConnectionAttempts=3",
        ]);
        if !options.reverse_tunnel.trim().is_empty() {
            command.args([
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                options.reverse_tunnel.trim(),
            ]);
        }
        let bootstrap = remote_bootstrap_script(&options.command, &options.remote_environment);
        command
            .arg(alias)
            .arg(format!("sh -c {}", shell_quote(&bootstrap)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to open muxloomd bridge to {alias}"))?;
        let mut writer = child.stdin.take().context("ssh bridge has no stdin")?;
        let mut reader = BufReader::new(child.stdout.take().context("ssh bridge has no stdout")?);
        let stderr_lines = child
            .stderr
            .take()
            .map(|stderr| capture_bridge_stderr(stderr, alias));
        progress(TaskProgress::pending(format!("Checking {alias} companion")));
        let provision_notice = match negotiate_remote_companion(
            alias,
            options,
            &mut child,
            &mut reader,
            &mut writer,
            stderr_lines.as_ref(),
            &mut progress,
        ) {
            Ok(notice) => notice,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        progress(TaskProgress::pending(format!("Starting {alias} companion")));
        let connection = Self::from_parts(alias.to_string(), reader, writer, Some(child));
        Ok((Self::handshake(connection, alias)?, provision_notice))
    }

    pub fn connect_local(configured_command: &str) -> Result<Arc<Self>> {
        let executable = if configured_command == "muxloomd" {
            local_companion_command()
        } else {
            configured_command.into()
        };
        let mut child = Command::new(&executable)
            .arg("bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to open local muxloomd bridge via {executable}"))?;
        let writer = child.stdin.take().context("local bridge has no stdin")?;
        let reader = child.stdout.take().context("local bridge has no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    debug::log("bridge", format!("target=local daemon: {line}"));
                }
            });
        }
        let connection = Self::from_parts("local".into(), reader, writer, Some(child));
        Self::handshake(connection, "local")
    }

    fn handshake(connection: Arc<Self>, target: &str) -> Result<Arc<Self>> {
        match connection.request(DaemonRequest::Hello {
            client_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: PROTOCOL_VERSION,
        })? {
            BridgeReply {
                response:
                    DaemonResponse::Hello {
                        protocol_version,
                        capabilities,
                        daemon_version,
                        ..
                    },
                ..
            } if protocol_version == PROTOCOL_VERSION => {
                *connection
                    .state
                    .capabilities
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    capabilities.into_iter().collect();
                *connection
                    .state
                    .daemon_version
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(daemon_version);
                debug::log(
                    "bridge",
                    format!("connected target={target} via one persistent bridge"),
                );
                Ok(connection)
            }
            reply => {
                connection.state.shutdown();
                bail!(
                    "invalid muxloomd handshake from {target}: {:?}",
                    reply.response
                )
            }
        }
    }

    fn from_parts(
        target: String,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<Child>,
    ) -> Arc<Self> {
        let frames = Arc::new(FrameWriter::default());
        let state = Arc::new(ConnectionState {
            target,
            writer: Arc::clone(&frames),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            next_stream: AtomicU64::new(u64::from(stream::PTY_BASE)),
            alive: AtomicBool::new(true),
            capabilities: Mutex::new(HashSet::new()),
            daemon_version: Mutex::new(None),
        });
        spawn_writer(frames, Arc::downgrade(&state), writer);
        spawn_reader(Arc::clone(&state), reader);
        spawn_heartbeat(Arc::downgrade(&state));
        Arc::new(Self { state })
    }

    fn request(&self, request: DaemonRequest) -> Result<BridgeReply> {
        if !self.state.alive.load(Ordering::Acquire) {
            bail!("muxloomd bridge to {} is closed", self.state.target);
        }
        let request_id = self.state.next_request.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                request_id,
                PendingRequest {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    data: Vec::new(),
                    sender,
                },
            );
        let frame = Frame::json(FrameKind::Request, 0, request_id, &request)?;
        if let Err(error) = self.write_frame(frame) {
            self.state
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&request_id);
            self.state.fail_all(error.to_string());
            return Err(error);
        }
        match receiver.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.state
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&request_id);
                bail!("muxloomd request {request_id} timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("muxloomd request {request_id} was disconnected")
            }
        }
    }

    fn write_frame(&self, frame: Frame) -> Result<()> {
        self.state.writer.send(frame)
    }

    pub fn run_shell(&self, script: &str, environment: &[(String, String)]) -> Result<Output> {
        let reply = self.request(DaemonRequest::RunShell {
            script: script.into(),
            environment: environment.to_vec(),
        })?;
        match reply.response {
            DaemonResponse::ShellComplete { exit_code } => Ok(Output {
                status: exit_status(exit_code),
                stdout: reply.stdout,
                stderr: reply.stderr,
            }),
            DaemonResponse::Error { message } => bail!("muxloomd shell failed: {message}"),
            response => bail!("unexpected muxloomd shell response: {response:?}"),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<DaemonSession>> {
        match self.request(DaemonRequest::ListSessions)?.response {
            DaemonResponse::Sessions { sessions } => Ok(sessions),
            response => bail!("unexpected session-list response: {response:?}"),
        }
    }

    pub fn daemon_status(&self) -> Result<(u32, usize)> {
        match self.request(DaemonRequest::Status)?.response {
            DaemonResponse::Status { pid, clients, .. } => Ok((pid, clients)),
            response => bail!("unexpected status response: {response:?}"),
        }
    }

    pub fn read_history(
        &self,
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
        rendered: bool,
    ) -> Result<BridgeHistory> {
        let reply = self.request(DaemonRequest::ReadHistory {
            session_id,
            offset_from_bottom,
            lines,
            rendered,
        })?;
        match reply.response {
            DaemonResponse::HistoryComplete {
                total_lines,
                columns,
                rows,
                offset_from_bottom,
                rendered,
                reached_start,
            } => Ok(BridgeHistory {
                bytes: reply.data,
                total_lines,
                columns,
                rows,
                offset_from_bottom,
                rendered,
                reached_start,
            }),
            response => bail!("unexpected history response: {response:?}"),
        }
    }

    pub fn search_history(
        &self,
        session_id: String,
        query: String,
        max_matches: usize,
    ) -> Result<Vec<DaemonHistoryMatch>> {
        match self
            .request(DaemonRequest::SearchHistory {
                session_id,
                query,
                max_matches,
            })?
            .response
        {
            DaemonResponse::HistoryMatches { matches } => Ok(matches),
            response => bail!("unexpected history-search response: {response:?}"),
        }
    }

    pub fn list_directory(&self, path: String) -> Result<DirectoryListing> {
        match self
            .request(DaemonRequest::ListDirectory { path })?
            .response
        {
            DaemonResponse::Directory { listing } => Ok(listing),
            response => bail!("unexpected directory-list response: {response:?}"),
        }
    }

    pub fn list_files(&self, path: String) -> Result<FileListing> {
        match self.request(DaemonRequest::ListFiles { path })?.response {
            DaemonResponse::Files { listing } => Ok(listing),
            response => bail!("unexpected file-list response: {response:?}"),
        }
    }

    pub fn preview_file(&self, path: String, limit: usize) -> Result<FilePreview> {
        match self
            .request(DaemonRequest::PreviewFile { path, limit })?
            .response
        {
            DaemonResponse::Preview { preview } => Ok(preview),
            response => bail!("unexpected file-preview response: {response:?}"),
        }
    }

    pub fn probe_executables(&self, executables: Vec<String>) -> Result<Vec<String>> {
        match self
            .request(DaemonRequest::ProbeExecutables { executables })?
            .response
        {
            DaemonResponse::Executables { available } => Ok(available),
            response => bail!("unexpected executable-probe response: {response:?}"),
        }
    }

    pub fn tcp_listener_ports(&self) -> Result<Vec<u16>> {
        self.require_capability("tcp-listeners-v1")?;
        match self.request(DaemonRequest::ListTcpListeners)?.response {
            DaemonResponse::TcpListeners { ports } => Ok(ports),
            response => bail!("unexpected TCP-listener response: {response:?}"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &self,
        session_id: String,
        kind: String,
        path: String,
        label: String,
        temporary: bool,
        executable: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
    ) -> Result<DaemonSession> {
        match self
            .request(DaemonRequest::Launch {
                session_id,
                kind,
                path,
                label,
                temporary,
                executable,
                args,
                environment,
                created_at,
                columns: 120,
                rows: 40,
            })?
            .response
        {
            DaemonResponse::Launched { session } => Ok(session),
            response => bail!("unexpected launch response: {response:?}"),
        }
    }

    pub fn archive(&self, session_id: String) -> Result<()> {
        self.expect_ack(DaemonRequest::Archive { session_id })
    }

    pub fn send_input(&self, session_id: String, bytes: Vec<u8>) -> Result<()> {
        self.require_capability("send-input-v1")?;
        self.expect_ack(DaemonRequest::SendInput { session_id, bytes })
    }

    fn set_attention_patterns(&self, patterns: Vec<String>) -> Result<()> {
        self.expect_ack(DaemonRequest::SetAttentionPatterns { patterns })
    }

    pub fn set_trigger(&self, trigger: Trigger) -> Result<Trigger> {
        self.require_capability(TRIGGERS_CAPABILITY)?;
        match self
            .request(DaemonRequest::SetTrigger { trigger })?
            .response
        {
            DaemonResponse::Triggers { triggers } => triggers
                .into_iter()
                .next()
                .context("muxloomd stored the trigger but did not report it back"),
            response => bail!("unexpected trigger response: {response:?}"),
        }
    }

    pub fn list_triggers(&self, session_id: Option<String>) -> Result<Vec<Trigger>> {
        self.require_capability(TRIGGERS_CAPABILITY)?;
        match self
            .request(DaemonRequest::ListTriggers { session_id })?
            .response
        {
            DaemonResponse::Triggers { triggers } => Ok(triggers),
            response => bail!("unexpected trigger response: {response:?}"),
        }
    }

    pub fn delete_trigger(&self, id: String) -> Result<()> {
        self.require_capability(TRIGGERS_CAPABILITY)?;
        self.expect_ack(DaemonRequest::DeleteTrigger { id })
    }

    pub fn talk_post(&self, draft: TalkDraft) -> Result<TalkMessage> {
        self.require_capability(TALK_CAPABILITY)?;
        match self.request(DaemonRequest::TalkPost { draft })?.response {
            DaemonResponse::Talk { page } => page
                .messages
                .into_iter()
                .next()
                .context("muxloomd filed the message but did not report it back"),
            response => bail!("unexpected talk response: {response:?}"),
        }
    }

    pub fn talk_read(&self, filter: TalkFilter) -> Result<TalkPage> {
        self.require_capability(TALK_CAPABILITY)?;
        match self.request(DaemonRequest::TalkRead { filter })?.response {
            DaemonResponse::Talk { page } => Ok(page),
            response => bail!("unexpected talk response: {response:?}"),
        }
    }

    /// What the board on this machine holds, telling it the name a human uses
    /// for the machine while we are here: the controller is the only thing
    /// that knows it, and a message carries it for reading later.
    pub fn talk_status(&self, label: Option<String>) -> Result<TalkState> {
        self.require_capability(TALK_CAPABILITY)?;
        match self.request(DaemonRequest::TalkStatus { label })?.response {
            DaemonResponse::TalkBoard { state } => Ok(state),
            response => bail!("unexpected talk response: {response:?}"),
        }
    }

    pub fn talk_fetch(&self, from: TalkVector, limit: usize) -> Result<Vec<TalkMessage>> {
        self.require_capability(TALK_CAPABILITY)?;
        match self
            .request(DaemonRequest::TalkFetch { from, limit })?
            .response
        {
            DaemonResponse::TalkCarry { messages, .. } => Ok(messages),
            response => bail!("unexpected talk response: {response:?}"),
        }
    }

    pub fn talk_append(&self, messages: Vec<TalkMessage>) -> Result<usize> {
        self.require_capability(TALK_CAPABILITY)?;
        match self
            .request(DaemonRequest::TalkAppend { messages })?
            .response
        {
            DaemonResponse::TalkCarry { added, .. } => Ok(added),
            response => bail!("unexpected talk response: {response:?}"),
        }
    }

    /// Hand a direct message to the machine the target session lives on. That
    /// daemon renders the envelope and decides whether the session is free
    /// enough to be typed into; what comes back is the message as it was
    /// filed, and what became of it.
    pub fn talk_deliver(
        &self,
        draft: TalkDraft,
        deliver: TalkDeliver,
        reply_expected: bool,
    ) -> Result<(TalkMessage, String, Option<String>)> {
        self.require_capability(DIRECT_CAPABILITY)?;
        match self
            .request(DaemonRequest::TalkDeliver {
                draft,
                deliver,
                reply_expected,
            })?
            .response
        {
            DaemonResponse::TalkDelivery {
                message,
                delivery,
                reason,
            } => Ok((*message, delivery, reason)),
            response => bail!("unexpected talk response: {response:?}"),
        }
    }

    fn has_capability(&self, capability: &str) -> bool {
        self.state
            .capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(capability)
    }

    pub fn delete(&self, session_id: String) -> Result<()> {
        self.expect_ack(DaemonRequest::Delete { session_id })
    }

    pub fn resize(&self, session_id: String, columns: u16, rows: u16) -> Result<()> {
        self.expect_ack(DaemonRequest::Resize {
            session_id,
            columns,
            rows,
        })
    }

    /// Resize without waiting for the acknowledgement. Attached terminals are
    /// resized from the render loop, so waiting a round trip there would stall
    /// every pane whenever the layout changes on a slow link. The daemon
    /// ignores nothing: only our own reply handling is skipped.
    fn resize_detached(&self, session_id: String, columns: u16, rows: u16) -> Result<()> {
        let request_id = self.state.next_request.fetch_add(1, Ordering::Relaxed);
        self.write_frame(Frame::json(
            FrameKind::Request,
            0,
            request_id,
            &DaemonRequest::Resize {
                session_id,
                columns,
                rows,
            },
        )?)
    }

    fn expect_ack(&self, request: DaemonRequest) -> Result<()> {
        match self.request(request)?.response {
            DaemonResponse::Ack => Ok(()),
            response => bail!("unexpected daemon response: {response:?}"),
        }
    }

    pub fn open_pty(
        &self,
        session_id: String,
        columns: u16,
        rows: u16,
        scrollback_rows: usize,
    ) -> Result<BridgeStream> {
        self.open_stream(OpenStream::Pty {
            session_id,
            columns,
            rows,
            scrollback_rows,
        })
    }

    pub fn open_tcp(&self, host: String, port: u16) -> Result<BridgeStream> {
        self.require_capability("tcp-forward-v1")?;
        self.open_stream(OpenStream::Tcp { host, port })
    }

    fn require_capability(&self, capability: &str) -> Result<()> {
        if self
            .state
            .capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(capability)
        {
            Ok(())
        } else {
            // Reached only when the companion binary itself predates the
            // capability: a current companion serves forwarding from the
            // bridge process even when the daemon it talks to is older.
            bail!(
                "the muxloomd companion on this machine predates {capability}; update it to this client's generation"
            )
        }
    }

    pub fn open_file(
        &self,
        path: String,
        offset: u64,
        length: Option<u64>,
        media: bool,
    ) -> Result<BridgeStream> {
        self.open_stream(if media {
            OpenStream::Media {
                path,
                offset,
                length,
            }
        } else {
            OpenStream::File {
                path,
                offset,
                length,
            }
        })
    }

    fn open_stream(&self, open: OpenStream) -> Result<BridgeStream> {
        let stream_id = u32::try_from(self.state.next_stream.fetch_add(1, Ordering::Relaxed))
            .context("daemon stream id space exhausted")?;
        let (sender, events) = mpsc::channel();
        self.state
            .streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(stream_id, sender);
        let frame = Frame::json(FrameKind::OpenStream, stream_id, 0, &open)?;
        if let Err(error) = self.write_frame(frame) {
            self.state
                .streams
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&stream_id);
            return Err(error);
        }
        Ok(BridgeStream {
            state: Arc::clone(&self.state),
            stream_id,
            events,
            closed: false,
        })
    }

    pub fn upload_file(
        &self,
        local_path: &std::path::Path,
        remote_path: String,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        let parent = std::path::Path::new(&remote_path)
            .parent()
            .context("remote upload path has no parent")?
            .to_string_lossy()
            .into_owned();
        let mut file = std::fs::File::open(local_path)
            .with_context(|| format!("failed to open {}", local_path.display()))?;
        let size = file.metadata()?.len();
        let stream = self.open_stream(OpenStream::Upload {
            path: remote_path,
            size,
        })?;
        let mut buffer = vec![0; crate::daemon_protocol::DATA_CHUNK_SIZE];
        let mut sent = 0u64;
        progress(0, size);
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            stream.write_data(&buffer[..read], true)?;
            sent = sent.saturating_add(read as u64);
            progress(sent, size);
        }
        drop(stream);
        self.list_files(parent)?;
        Ok(())
    }

    pub fn is_alive(&self) -> bool {
        self.state.alive.load(Ordering::Acquire)
    }
}

impl Drop for BridgeConnection {
    fn drop(&mut self) {
        self.state.shutdown();
    }
}

fn spawn_reader(state: Arc<ConnectionState>, mut reader: impl Read + Send + 'static) {
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            while let Some(frame) = Frame::read_from(&mut reader)? {
                match frame.kind {
                    FrameKind::Data => {
                        let payload = frame.decoded_payload()?;
                        let consumed = u32::try_from(payload.len()).unwrap_or(u32::MAX);
                        if frame.request_id != 0
                            && let Some(request) = state
                                .pending
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .get_mut(&frame.request_id)
                        {
                            match frame.stream_id {
                                stream::STDOUT => request.stdout.extend(payload),
                                stream::STDERR => request.stderr.extend(payload),
                                _ => request.data.extend(payload),
                            }
                        } else if let Some(stream) = state
                            .streams
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .get(&frame.stream_id)
                        {
                            let _ = stream.send(StreamEvent::Data(payload));
                        }
                        if frame.request_id == 0 && consumed > 0 {
                            state
                                .writer
                                .send(Frame::window_update(frame.stream_id, consumed))?;
                        }
                    }
                    FrameKind::Error if frame.stream_id != 0 => {
                        let response = frame.decode_json::<DaemonResponse>()?;
                        let message = match response {
                            DaemonResponse::Error { message } => message,
                            response => format!("unexpected stream error: {response:?}"),
                        };
                        if let Some(stream) = state
                            .streams
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&frame.stream_id)
                        {
                            let _ = stream.send(StreamEvent::Error(message));
                        }
                    }
                    FrameKind::Response | FrameKind::Error => {
                        let response = frame.decode_json::<DaemonResponse>()?;
                        if let Some(request) = state
                            .pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&frame.request_id)
                        {
                            let result = match response {
                                DaemonResponse::Error { ref message } => Err(message.clone()),
                                _ => Ok(BridgeReply {
                                    response,
                                    stdout: request.stdout,
                                    stderr: request.stderr,
                                    data: request.data,
                                }),
                            };
                            let _ = request.sender.send(result);
                        }
                    }
                    FrameKind::Heartbeat => {}
                    FrameKind::CloseStream => {
                        if let Some(stream) = state
                            .streams
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&frame.stream_id)
                        {
                            let _ = stream.send(StreamEvent::Closed);
                        }
                    }
                    FrameKind::OpenStream | FrameKind::WindowUpdate | FrameKind::Request => {}
                }
            }
            bail!("bridge reached EOF")
        })();
        let message = result
            .err()
            .map_or_else(|| "bridge reader stopped".into(), |error| error.to_string());
        debug::log("bridge", format!("target={} {message}", state.target));
        state.fail_all(message);
    });
}

fn spawn_heartbeat(state: Weak<ConnectionState>) {
    thread::spawn(move || {
        loop {
            thread::sleep(HEARTBEAT_INTERVAL);
            let Some(state) = state.upgrade() else {
                return;
            };
            if !state.alive.load(Ordering::Acquire) {
                return;
            }
            let heartbeat = Frame::new(FrameKind::Heartbeat, 0, 0, vec![]);
            if let Err(error) = state.writer.send(heartbeat) {
                state.fail_all(error.to_string());
                return;
            }
        }
    });
}

fn local_companion_command() -> String {
    let executable_name = format!("muxloomd{}", std::env::consts::EXE_SUFFIX);
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        for candidate in [
            parent.join(&executable_name),
            parent.parent().map_or_else(
                || parent.join(&executable_name),
                |root| root.join(&executable_name),
            ),
        ] {
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    executable_name
}

const BOOTSTRAP_MARKER: &str = "__MUXLOOM_BOOTSTRAP__";

/// The environment a target reaches the release through, as `export` lines for
/// the bootstrap's pull step. This is the host's own configuration, unmapped:
/// a proxy the operator pointed at `127.0.0.1:<port>` is the reverse tunnel
/// this very connection opened, and from the target that address is correct.
fn environment_prelude(environment: &[(String, String)]) -> String {
    let mut script = String::new();
    for (name, value) in environment {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            continue;
        }
        script.push_str("        export ");
        script.push_str(name);
        script.push('=');
        script.push_str(&shell_quote(value));
        script.push('\n');
    }
    script
}

fn remote_bootstrap_script(
    configured_command: &str,
    pull_environment: &[(String, String)],
) -> String {
    format!(
        r#"configured={configured}
case "$configured" in "~/"*) configured="$HOME/${{configured#~/}}" ;; esac
install_root="${{XDG_DATA_HOME:-$HOME/.local/share}}/muxloom/bin"
installed="$install_root/muxloomd"
expected_protocol='{protocol_version}'
os=$(uname -s 2>/dev/null || printf unknown)
arch=$(uname -m 2>/dev/null || printf unknown)
candidate=
if [ -x "$installed" ] && [ "$("$installed" protocol-version 2>/dev/null || true)" = "$expected_protocol" ]; then
    candidate="$installed"
elif command -v "$configured" >/dev/null 2>&1 && [ "$("$configured" protocol-version 2>/dev/null || true)" = "$expected_protocol" ]; then
    candidate="$configured"
elif [ -x "$configured" ] && [ "$("$configured" protocol-version 2>/dev/null || true)" = "$expected_protocol" ]; then
    candidate="$configured"
fi
if [ -n "$candidate" ]; then
    fingerprint=$("$candidate" binary-sha256 2>/dev/null || true)
    case "$fingerprint" in ''|*[!0-9a-fA-F]*) fingerprint=legacy ;; esac
    version=$("$candidate" --version 2>/dev/null || true)
    version=${{version##* }}
    case "$version" in ''|*[!0-9A-Za-z.+-]*) version=unknown ;; esac
    printf '{marker} HAVE %s %s %s %s\n' "$os" "$arch" "$fingerprint" "$version"
else
    printf '{marker} NEED %s %s\n' "$os" "$arch"
fi
# The controller answers with one action at a time. A pull that comes back
# empty-handed is reported and the loop waits for the next one, so a machine
# that cannot reach the release still gets the bytes pushed down this pipe.
while IFS= read -r muxloom_action; do
case "$muxloom_action" in
    USE)
        if [ -n "$candidate" ]; then
            exec "$candidate" bridge
        fi
        printf 'invalid bootstrap action\n' >&2
        exit 64 ;;
    'PULL '*)
        muxloom_rest=${{muxloom_action#PULL }}
        muxloom_url=${{muxloom_rest%% *}}
        muxloom_sum=${{muxloom_rest##* }}
        muxloom_failure=
        mkdir -p "$install_root"
        temporary="$installed.pull.$$"
        rm -f "$temporary"
        (
{pull_exports}        if command -v curl >/dev/null 2>&1; then
            curl -fsSL --connect-timeout 8 --max-time 120 --speed-limit 4096 --speed-time 20 -o "$temporary" "$muxloom_url"
        elif command -v wget >/dev/null 2>&1; then
            wget -q --timeout=20 --tries=1 -O "$temporary" "$muxloom_url"
        else
            exit 69
        fi
        ) >/dev/null 2>&1 || muxloom_failure='download failed'
        if [ -z "$muxloom_failure" ]; then
            if command -v sha256sum >/dev/null 2>&1; then
                muxloom_actual=$(sha256sum "$temporary" | cut -d' ' -f1)
            elif command -v shasum >/dev/null 2>&1; then
                muxloom_actual=$(shasum -a 256 "$temporary" | cut -d' ' -f1)
            elif command -v openssl >/dev/null 2>&1; then
                muxloom_actual=$(openssl dgst -sha256 "$temporary" | awk '{{print $NF}}')
            else
                muxloom_actual=
                muxloom_failure='no sha256 tool'
            fi
            if [ -z "$muxloom_failure" ] && [ "$muxloom_actual" != "$muxloom_sum" ]; then
                muxloom_failure='checksum mismatch'
            fi
        fi
        if [ -n "$muxloom_failure" ]; then
            rm -f "$temporary"
            printf '{marker} PULLFAILED %s\n' "$muxloom_failure"
            continue
        fi
        chmod 700 "$temporary"
        mv -f "$temporary" "$installed"
        printf '{marker} PULLED\n'
        exec "$installed" bridge ;;
    'INSTALL '*)
        muxloom_size=${{muxloom_action#INSTALL }}
        case "$muxloom_size" in ''|*[!0-9]*) printf 'invalid bootstrap size\n' >&2; exit 64 ;; esac
        mkdir -p "$install_root"
        temporary="$installed.tmp.$$"
        if head -c 0 </dev/null >/dev/null 2>&1; then
            head -c "$muxloom_size" > "$temporary"
        else
            dd bs=1 count="$muxloom_size" of="$temporary" 2>/dev/null
        fi
        chmod 700 "$temporary"
        mv -f "$temporary" "$installed"
        printf '{marker} INSTALLED\n'
        exec "$installed" bridge ;;
    *)
        printf 'invalid bootstrap action\n' >&2
        exit 64 ;;
esac
done
printf 'bootstrap stream closed before an action arrived\n' >&2
exit 64"#,
        configured = shell_quote(configured_command),
        protocol_version = PROTOCOL_VERSION,
        marker = BOOTSTRAP_MARKER,
        pull_exports = environment_prelude(pull_environment),
    )
}

fn capture_bridge_stderr(
    stderr: impl Read + Send + 'static,
    target: &str,
) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&lines);
    let target = target.to_string();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            debug::log("bridge", format!("target={target} ssh: {line}"));
            captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(line);
        }
    });
    lines
}

fn negotiate_remote_companion(
    alias: &str,
    options: &BridgeOptions,
    child: &mut Child,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    stderr_lines: Option<&Arc<Mutex<Vec<String>>>>,
    progress: &mut impl FnMut(TaskProgress),
) -> Result<Option<String>> {
    let mut status = String::new();
    if reader.read_line(&mut status)? == 0 {
        let _ = child.wait();
        let detail = stderr_lines
            .map(|lines| {
                lines
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .join("; ")
            })
            .filter(|detail| !detail.is_empty())
            .unwrap_or_else(|| "remote bootstrap exited before reporting status".into());
        bail!("failed to start muxloomd on {alias}: {detail}");
    }
    let fields: Vec<_> = status.split_whitespace().collect();
    match fields.as_slice() {
        [marker, "READY"] if *marker == BOOTSTRAP_MARKER => Ok(None),
        [marker, "HAVE", os, arch, fingerprint, version] if *marker == BOOTSTRAP_MARKER => {
            let (asset, notice) = match resolve_companion_asset(options, os, arch, progress) {
                Ok(asset) => asset,
                Err(error) => {
                    debug::log(
                        "bridge",
                        format!(
                            "target={alias} companion update unavailable; continuing compatible remote binary: {error:#}"
                        ),
                    );
                    writeln!(writer, "USE")?;
                    writer.flush()?;
                    return Ok(Some(format!(
                        "companion update unavailable; continuing compatible remote generation ({error:#})"
                    )));
                }
            };
            let expected = sha256_file(&asset)?;
            if !fingerprint.eq_ignore_ascii_case(&expected) {
                return deploy_remote_companion(
                    alias, options, &asset, &notice, os, arch, reader, writer, progress,
                );
            }
            writeln!(writer, "USE")?;
            writer.flush()?;
            // The remote already runs exactly this asset, which makes it
            // current only if the asset is this generation. A companion left
            // behind by an older build matches itself forever, so without the
            // version the remote stays pinned to it and every capability
            // added since goes silently missing.
            if *version == env!("CARGO_PKG_VERSION") {
                debug::log(
                    "bridge",
                    format!("target={alias} remote companion fingerprint is current"),
                );
                return Ok(None);
            }
            debug::log(
                "bridge",
                format!(
                    "target={alias} companion asset {} is muxloomd {version}, not {}; the remote cannot be updated from it",
                    asset.display(),
                    env!("CARGO_PKG_VERSION")
                ),
            );
            Ok(Some(format!(
                "remote muxloomd {version} is older than this client and the {} companion asset is the same build, so it cannot be updated; rebuild or replace that asset",
                companion_target_triple(os, arch).unwrap_or_else(|_| format!("{os}/{arch}"))
            )))
        }
        [marker, "NEED", os, arch] if *marker == BOOTSTRAP_MARKER => {
            let (asset, notice) = resolve_companion_asset(options, os, arch, progress)?;
            deploy_remote_companion(
                alias, options, &asset, &notice, os, arch, reader, writer, progress,
            )
        }
        _ => bail!(
            "invalid muxloomd bootstrap response from {alias}: {}",
            status.trim()
        ),
    }
}

/// Ask the target to fetch the companion itself, and only when what it would
/// fetch is byte-for-byte the asset we would otherwise push: the digest of the
/// local asset has to match the published release checksum. That keeps the
/// generation the controller decided on — a source build, a bundled asset — from
/// being quietly swapped for whatever the latest release happens to hold.
///
/// `Ok(None)` means the bytes still have to go down this pipe; the caller
/// pushes. An error means the bootstrap stream itself is no longer trustworthy,
/// and pushing into it would be worse than failing.
#[allow(clippy::too_many_arguments)]
fn pull_remote_companion(
    alias: &str,
    options: &BridgeOptions,
    asset: &Path,
    os: &str,
    arch: &str,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    progress: &mut impl FnMut(TaskProgress),
) -> Result<Option<String>> {
    let Some((url, digest, triple)) = companion_release_match(options, asset, os, arch) else {
        return Ok(None);
    };
    debug::log(
        "bridge",
        format!("target={alias} fetching its own {triple} companion from {url}"),
    );
    exchange_companion_pull(alias, &url, &digest, &triple, reader, writer, progress)
}

#[allow(clippy::too_many_arguments)]
fn exchange_companion_pull(
    alias: &str,
    url: &str,
    digest: &str,
    triple: &str,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    progress: &mut impl FnMut(TaskProgress),
) -> Result<Option<String>> {
    progress(TaskProgress::pending(format!(
        "{alias} is downloading its companion"
    )));
    writeln!(writer, "PULL {url} {digest}")?;
    writer.flush()?;
    let mut status = String::new();
    if reader.read_line(&mut status)? == 0 {
        bail!("the bootstrap on {alias} closed while fetching its companion");
    }
    let status = status.trim();
    if status == format!("{BOOTSTRAP_MARKER} PULLED") {
        return Ok(Some(format!(
            "{alias} downloaded the {triple} muxloomd companion itself"
        )));
    }
    if let Some(reason) = status.strip_prefix(&format!("{BOOTSTRAP_MARKER} PULLFAILED")) {
        debug::log(
            "bridge",
            format!(
                "target={alias} could not fetch its own companion ({}); pushing it",
                reason.trim()
            ),
        );
        return Ok(None);
    }
    bail!("invalid bootstrap pull response from {alias}: {status}")
}

/// The release URL a target can fetch this exact asset from, if the published
/// checksum says the release holds the same bytes.
fn companion_release_match(
    options: &BridgeOptions,
    asset: &Path,
    os: &str,
    arch: &str,
) -> Option<(String, String, String)> {
    let triple = companion_target_triple(os, arch).ok()?;
    let asset_name = format!("muxloomd-{triple}{}", executable_suffix(os));
    let digest = sha256_file(asset).ok()?;
    let published = controller_fetch_text(
        &format!("{COMPANION_RELEASE_ROOT}/{asset_name}.sha256"),
        &options.download_environment,
    )
    .ok()
    .and_then(|text| parse_sha256_checksum(&text).ok())?;
    if !published.eq_ignore_ascii_case(&digest) {
        debug::log(
            "bridge",
            format!(
                "companion asset {} is not the published {triple} release; it has to be pushed",
                asset.display()
            ),
        );
        return None;
    }
    Some((
        format!("{COMPANION_RELEASE_ROOT}/{asset_name}"),
        digest,
        triple,
    ))
}

#[allow(clippy::too_many_arguments)]
fn deploy_remote_companion(
    alias: &str,
    options: &BridgeOptions,
    asset: &Path,
    notice: &str,
    os: &str,
    arch: &str,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    progress: &mut impl FnMut(TaskProgress),
) -> Result<Option<String>> {
    if let Some(pulled) =
        pull_remote_companion(alias, options, asset, os, arch, reader, writer, progress)?
    {
        return Ok(Some(pulled));
    }
    let mut file = fs::File::open(asset)
        .with_context(|| format!("failed to open companion asset {}", asset.display()))?;
    let size = file.metadata()?.len();
    debug::log(
        "bridge",
        format!(
            "target={alias} deploying {} bytes from {} for {os}/{arch}",
            size,
            asset.display()
        ),
    );
    writeln!(writer, "INSTALL {size}")?;
    let label = format!("Uploading {alias} companion");
    progress(TaskProgress::bytes(&label, 0, Some(size)));
    let mut transferred = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        transferred = transferred.saturating_add(read as u64);
        progress(TaskProgress::bytes(&label, transferred, Some(size)));
    }
    writer.flush()?;
    let mut status = String::new();
    if reader.read_line(&mut status)? == 0
        || status.trim() != format!("{BOOTSTRAP_MARKER} INSTALLED")
    {
        bail!(
            "muxloomd deployment on {alias} did not complete: {}",
            status.trim()
        );
    }
    Ok(Some(notice.into()))
}

fn resolve_companion_asset(
    options: &BridgeOptions,
    os: &str,
    arch: &str,
    progress: &mut impl FnMut(TaskProgress),
) -> Result<(PathBuf, String)> {
    let triple = companion_target_triple(os, arch)?;
    if !options.bootstrap_binary.trim().is_empty() {
        let path = expand_local_tilde(options.bootstrap_binary.trim());
        if path.is_file() {
            debug::log(
                "bridge",
                format!(
                    "using configured {triple} companion asset {}",
                    path.display()
                ),
            );
            return Ok((
                path,
                format!("deployed configured {triple} muxloomd companion"),
            ));
        }
        debug::log(
            "bridge",
            format!(
                "configured {triple} companion asset is missing: {}; trying packaged assets",
                path.display()
            ),
        );
    }
    let executable = format!("muxloomd{}", executable_suffix(os));
    let current = env::current_exe().context("failed to locate the muxloom executable")?;
    let parent = current.parent().unwrap_or_else(|| Path::new("."));
    let mut candidates = vec![
        parent.join("companions").join(&triple).join(&executable),
        parent.join(format!("muxloomd-{triple}")),
    ];
    if current_target_triple().as_deref() == Some(triple.as_str()) {
        candidates.insert(0, parent.join(&executable));
    }
    if let Some(workspace) = parent.parent().and_then(Path::parent) {
        candidates.push(
            workspace
                .join("target")
                .join(&triple)
                .join("release")
                .join(&executable),
        );
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        // A bundle ships companions built together with the controller; a
        // source tree can hold a cross-build from weeks ago that would pin
        // every remote to that stale generation. Trust a found asset only
        // when it is not clearly older than the controller itself.
        if bundled_asset_is_stale(&path, &current) {
            debug::log(
                "bridge",
                format!(
                    "ignoring stale {triple} companion asset {}; falling back to the release download/cache",
                    path.display()
                ),
            );
        } else {
            debug::log(
                "bridge",
                format!("using bundled {triple} companion asset {}", path.display()),
            );
            return Ok((
                path,
                format!("deployed bundled {triple} muxloomd companion"),
            ));
        }
    }
    debug::log(
        "bridge",
        format!(
            "no bundled {triple} companion asset; downloading the latest GitHub Release on the controller"
        ),
    );
    let (path, source) =
        download_latest_companion(
            &triple,
            &executable,
            &options.download_environment,
            progress,
        )
        .with_context(|| {
                format!(
                    "no bundled {triple} muxloomd asset and the controller could not fetch the latest GitHub Release"
                )
            })?;
    Ok((path, format!("deployed {triple} muxloomd {source}")))
}

/// How a companion asset was obtained, for the visible deployment notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionSource {
    Downloaded,
    VerifiedCache,
    /// The release checksum could not be fetched, so the previously verified
    /// cache was used; it may lag the latest release.
    StaleCache,
}

impl CompanionSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Downloaded => "downloaded and checksum-verified from the latest GitHub Release",
            Self::VerifiedCache => "loaded from the checksum-verified controller cache",
            Self::StaleCache => {
                "loaded from the previously verified cache because GitHub was unreachable; \
                 it may be stale"
            }
        }
    }
}

/// Whether a companion asset found beside (or below) the controller is
/// clearly older than the controller executable itself. Bundles unpack with
/// their build-time stamps, so genuine siblings land within the slack; a
/// leftover cross-build from an earlier week does not.
fn bundled_asset_is_stale(asset: &Path, controller: &Path) -> bool {
    const SLACK: Duration = Duration::from_secs(60 * 60);
    let (Ok(asset_meta), Ok(controller_meta)) = (fs::metadata(asset), fs::metadata(controller))
    else {
        return false;
    };
    match (asset_meta.modified(), controller_meta.modified()) {
        (Ok(asset_time), Ok(controller_time)) => controller_time
            .duration_since(asset_time)
            .is_ok_and(|behind| behind > SLACK),
        _ => false,
    }
}

/// Refresh the controller-side companion cache from the latest release: the
/// local platform plus every triple already cached. This is what makes a
/// source-built controller able to update remote companions — and what the
/// unreachable-GitHub fallback later serves from. Returns a summary line.
pub fn refresh_companion_cache(environment: &[(String, String)]) -> Result<String> {
    let mut triples: Vec<String> = Vec::new();
    if let Some(triple) = current_target_triple() {
        triples.push(triple);
    }
    if let Ok(entries) = fs::read_dir(companion_cache_root()) {
        for entry in entries.flatten() {
            if let Some(triple) = entry.file_name().to_str()
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && !triples.iter().any(|known| known == triple)
            {
                triples.push(triple.to_string());
            }
        }
    }
    let mut refreshed = Vec::new();
    for triple in &triples {
        download_latest_companion(triple, "muxloomd", environment, &mut |_| {})
            .with_context(|| format!("failed to fetch the {triple} companion"))?;
        refreshed.push(triple.as_str());
    }
    Ok(format!(
        "companion cache refreshed for {}",
        refreshed.join(", ")
    ))
}

fn download_latest_companion(
    triple: &str,
    executable: &str,
    environment: &[(String, String)],
    progress: &mut impl FnMut(TaskProgress),
) -> Result<(PathBuf, &'static str)> {
    download_latest_companion_at(
        &companion_cache_root(),
        triple,
        executable,
        environment,
        progress,
    )
}

fn download_latest_companion_at(
    cache_root: &Path,
    triple: &str,
    executable: &str,
    environment: &[(String, String)],
    progress: &mut impl FnMut(TaskProgress),
) -> Result<(PathBuf, &'static str)> {
    let asset_name = format!(
        "muxloomd-{triple}{}",
        executable_suffix_for_name(executable)
    );
    let cache = cache_root.join(triple);
    fs::create_dir_all(&cache)
        .with_context(|| format!("failed to create companion cache {}", cache.display()))?;
    let destination = cache.join(executable);
    let checksum_url = format!("{COMPANION_RELEASE_ROOT}/{asset_name}.sha256");
    progress(TaskProgress::pending(format!(
        "Checking {triple} companion release"
    )));
    let expected = match controller_fetch_text(&checksum_url, environment) {
        Ok(expected) => expected,
        Err(error) if destination.is_file() => {
            // The release checksum is unreachable — a proxy-less network that
            // cannot see GitHub, most often. The cached asset was verified
            // when it was downloaded; a possibly-stale companion beats none,
            // and the miss is said out loud rather than read as up to date.
            debug::log(
                "bridge",
                format!(
                    "companion release unreachable; using verified cache {}: {error:#}",
                    destination.display()
                ),
            );
            return Ok((destination, CompanionSource::StaleCache.describe()));
        }
        Err(error) => return Err(error).context("failed to fetch companion checksum"),
    };
    let expected = parse_sha256_checksum(&expected)?;
    if destination.is_file() && sha256_file(&destination).is_ok_and(|actual| actual == expected) {
        debug::log(
            "bridge",
            format!("using cached {triple} companion {}", destination.display()),
        );
        return Ok((destination, CompanionSource::VerifiedCache.describe()));
    }

    let partial = cache.join(format!(".{executable}.partial-{}", std::process::id()));
    let asset_url = format!("{COMPANION_RELEASE_ROOT}/{asset_name}");
    let label = format!("Downloading {triple} companion");
    let result = controller_download(&asset_url, &partial, environment, |completed, total| {
        progress(TaskProgress::bytes(&label, completed, total));
    })
    .and_then(|_| {
        let actual = sha256_file(&partial)?;
        if actual != expected {
            bail!("companion checksum mismatch: expected {expected}, got {actual}");
        }
        if destination.exists() {
            fs::remove_file(&destination).with_context(|| {
                format!("failed to replace stale cache {}", destination.display())
            })?;
        }
        fs::rename(&partial, &destination).with_context(|| {
            format!(
                "failed to finalize companion download {}",
                destination.display()
            )
        })?;
        Ok(())
    });
    if let Err(error) = result {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    debug::log(
        "bridge",
        format!(
            "downloaded latest {triple} companion from GitHub to {}",
            destination.display()
        ),
    );
    Ok((destination, CompanionSource::Downloaded.describe()))
}

fn executable_suffix_for_name(executable: &str) -> &'static str {
    if executable.ends_with(".exe") {
        ".exe"
    } else {
        ""
    }
}

fn companion_cache_root() -> PathBuf {
    if let Some(path) = env::var_os("MUXLOOM_CACHE_DIR") {
        return PathBuf::from(path).join("companions");
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("muxloom/companions");
    }
    if let Some(path) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("Muxloom/cache/companions");
    }
    if let Some(path) = env::var_os("HOME") {
        return PathBuf::from(path).join(".cache/muxloom/companions");
    }
    env::temp_dir().join("muxloom-cache/companions")
}

fn controller_fetch_text(url: &str, environment: &[(String, String)]) -> Result<String> {
    #[cfg(feature = "controller")]
    {
        crate::http::fetch_text(url, environment)
    }
    #[cfg(not(feature = "controller"))]
    {
        let _ = (url, environment);
        bail!("companion downloads require the controller feature")
    }
}

fn controller_download<F>(
    url: &str,
    destination: &Path,
    environment: &[(String, String)],
    on_progress: F,
) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    #[cfg(feature = "controller")]
    {
        crate::http::download(url, destination, environment, on_progress)
    }
    #[cfg(not(feature = "controller"))]
    {
        let _ = (url, destination, environment, on_progress);
        bail!("companion downloads require the controller feature")
    }
}

fn parse_sha256_checksum(value: &str) -> Result<String> {
    let checksum = value
        .split_whitespace()
        .next()
        .context("companion checksum file was empty")?;
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("companion checksum file did not contain a SHA-256 digest");
    }
    Ok(checksum.to_ascii_lowercase())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn companion_target_triple(os: &str, arch: &str) -> Result<String> {
    match (
        os.to_ascii_lowercase().as_str(),
        arch.to_ascii_lowercase().as_str(),
    ) {
        ("linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-musl".into()),
        ("linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-musl".into()),
        ("darwin", "arm64" | "aarch64") => Ok("aarch64-apple-darwin".into()),
        ("darwin", "x86_64" | "amd64") => Ok("x86_64-apple-darwin".into()),
        _ => bail!("unsupported muxloomd target platform {os}/{arch}"),
    }
}

fn current_target_triple() -> Option<String> {
    let os = match env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    companion_target_triple(os, env::consts::ARCH).ok()
}

fn executable_suffix(os: &str) -> &'static str {
    if os.eq_ignore_ascii_case("windows") {
        ".exe"
    } else {
        ""
    }
}

fn expand_local_tilde(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Clone, Default)]
pub struct BridgePool {
    connections: Arc<Mutex<HashMap<String, Arc<BridgeConnection>>>>,
    target_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    notices: Arc<Mutex<HashMap<String, String>>>,
    options: Arc<HashMap<String, BridgeOptions>>,
    default_options: BridgeOptions,
}

impl std::fmt::Debug for BridgePool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BridgePool")
            .field("connected_targets", &self.connected_targets())
            .field("configured_targets", &self.options.len())
            .finish()
    }
}

impl BridgePool {
    pub fn new(default_options: BridgeOptions, options: HashMap<String, BridgeOptions>) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            target_locks: Arc::new(Mutex::new(HashMap::new())),
            notices: Arc::new(Mutex::new(HashMap::new())),
            options: Arc::new(options),
            default_options,
        }
    }

    pub fn run_shell(
        &self,
        target_id: &str,
        alias: &str,
        script: &str,
        environment: &[(String, String)],
    ) -> Result<Output> {
        let connection = self.connection(target_id, Some(alias))?;
        match connection.run_shell(script, environment) {
            Ok(output) => Ok(output),
            Err(error) => {
                self.invalidate(target_id, &connection);
                Err(error)
            }
        }
    }

    pub fn list_sessions(&self, target: &Target) -> Result<Vec<DaemonSession>> {
        self.connection_for_target(target)?.list_sessions()
    }

    pub fn probe_executables(
        &self,
        target: &Target,
        executables: Vec<String>,
    ) -> Result<Vec<String>> {
        self.probe_executables_with_progress(target, executables, |_| {})
    }

    pub fn tcp_listener_ports(&self, target: &Target) -> Result<Vec<u16>> {
        self.connection_for_target(target)?.tcp_listener_ports()
    }

    pub fn probe_executables_with_progress(
        &self,
        target: &Target,
        executables: Vec<String>,
        progress: impl FnMut(TaskProgress),
    ) -> Result<Vec<String>> {
        self.connection_for_target_with_progress(target, progress)?
            .probe_executables(executables)
    }

    pub fn read_history(
        &self,
        target: &Target,
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
        rendered: bool,
    ) -> Result<BridgeHistory> {
        self.connection_for_target(target)?.read_history(
            session_id,
            offset_from_bottom,
            lines,
            rendered,
        )
    }

    pub fn search_history(
        &self,
        target: &Target,
        session_id: String,
        query: String,
        max_matches: usize,
    ) -> Result<Vec<DaemonHistoryMatch>> {
        self.connection_for_target(target)?
            .search_history(session_id, query, max_matches)
    }

    pub fn list_directory(&self, target: &Target, path: String) -> Result<DirectoryListing> {
        self.connection_for_target(target)?.list_directory(path)
    }

    pub fn list_files(&self, target: &Target, path: String) -> Result<FileListing> {
        self.connection_for_target(target)?.list_files(path)
    }

    pub fn preview_file(&self, target: &Target, path: String, limit: usize) -> Result<FilePreview> {
        self.connection_for_target(target)?
            .preview_file(path, limit)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &self,
        target: &Target,
        session_id: String,
        kind: String,
        path: String,
        label: String,
        temporary: bool,
        executable: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
    ) -> Result<DaemonSession> {
        self.connection_for_target(target)?.launch(
            session_id,
            kind,
            path,
            label,
            temporary,
            executable,
            args,
            environment,
            created_at,
        )
    }

    pub fn archive(&self, target: &Target, session_id: String) -> Result<()> {
        self.connection_for_target(target)?.archive(session_id)
    }

    pub fn send_input(&self, target: &Target, session_id: String, bytes: Vec<u8>) -> Result<()> {
        self.connection_for_target(target)?
            .send_input(session_id, bytes)
    }

    pub fn set_trigger(&self, target: &Target, trigger: Trigger) -> Result<Trigger> {
        self.connection_for_target(target)?.set_trigger(trigger)
    }

    pub fn list_triggers(
        &self,
        target: &Target,
        session_id: Option<String>,
    ) -> Result<Vec<Trigger>> {
        self.connection_for_target(target)?
            .list_triggers(session_id)
    }

    pub fn delete_trigger(&self, target: &Target, id: String) -> Result<()> {
        self.connection_for_target(target)?.delete_trigger(id)
    }

    pub fn talk_post(&self, target: &Target, draft: TalkDraft) -> Result<TalkMessage> {
        self.connection_for_target(target)?.talk_post(draft)
    }

    pub fn talk_read(&self, target: &Target, filter: TalkFilter) -> Result<TalkPage> {
        self.connection_for_target(target)?.talk_read(filter)
    }

    pub fn talk_status(&self, target: &Target, label: Option<String>) -> Result<TalkState> {
        self.connection_for_target(target)?.talk_status(label)
    }

    pub fn talk_fetch(
        &self,
        target: &Target,
        from: TalkVector,
        limit: usize,
    ) -> Result<Vec<TalkMessage>> {
        self.connection_for_target(target)?.talk_fetch(from, limit)
    }

    pub fn talk_deliver(
        &self,
        target: &Target,
        draft: TalkDraft,
        deliver: TalkDeliver,
        reply_expected: bool,
    ) -> Result<(TalkMessage, String, Option<String>)> {
        self.connection_for_target(target)?
            .talk_deliver(draft, deliver, reply_expected)
    }

    pub fn talk_append(&self, target: &Target, messages: Vec<TalkMessage>) -> Result<usize> {
        self.connection_for_target(target)?.talk_append(messages)
    }

    /// The serving daemon's pid and client count.
    pub fn daemon_status(&self, target: &Target) -> Result<(u32, usize)> {
        self.connection_for_target(target)?.daemon_status()
    }

    pub fn delete(&self, target: &Target, session_id: String) -> Result<()> {
        self.connection_for_target(target)?.delete(session_id)
    }

    pub fn resize(
        &self,
        target: &Target,
        session_id: String,
        columns: u16,
        rows: u16,
    ) -> Result<()> {
        self.connection_for_target(target)?
            .resize(session_id, columns, rows)
    }

    /// Resize over the bridge this target already has, without waiting for the
    /// acknowledgement and without ever opening a connection. A target with no
    /// live bridge has no attached terminal to resize, and re-attaching sends
    /// the current size anyway, so that case is a no-op rather than a stall.
    pub fn resize_detached(
        &self,
        target: &Target,
        session_id: String,
        columns: u16,
        rows: u16,
    ) -> Result<()> {
        let Some(connection) = self.live_connection(&target.id) else {
            return Ok(());
        };
        connection.resize_detached(session_id, columns, rows)
    }

    pub fn open_pty(
        &self,
        target: &Target,
        session_id: String,
        columns: u16,
        rows: u16,
        scrollback_rows: usize,
    ) -> Result<BridgeStream> {
        self.connection_for_target(target)?
            .open_pty(session_id, columns, rows, scrollback_rows)
    }

    pub fn open_tcp(&self, target: &Target, host: String, port: u16) -> Result<BridgeStream> {
        self.connection_for_target(target)?.open_tcp(host, port)
    }

    pub fn ensure_tcp_forward(&self, target: &Target) -> Result<()> {
        self.connection_for_target(target)?
            .require_capability("tcp-forward-v1")
    }

    pub fn download_file(
        &self,
        target: &Target,
        remote_path: String,
        destination: &std::path::Path,
        mut progress: impl FnMut(u64),
    ) -> Result<()> {
        let connection = self.connection_for_target(target)?;
        let mut stream = connection.open_file(remote_path, 0, None, false)?;
        let mut file = std::fs::File::create(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        let mut transferred = 0u64;
        while !stream.is_closed() {
            if let Some(bytes) = stream.read_timeout(REQUEST_TIMEOUT)? {
                file.write_all(&bytes)?;
                transferred = transferred.saturating_add(bytes.len() as u64);
                progress(transferred);
            }
        }
        file.flush()?;
        Ok(())
    }

    /// Read up to `limit` bytes of a remote file over the chunked,
    /// flow-controlled file stream. Used to finish a preview whose body does
    /// not fit in one response frame, so the transfer stays bounded per frame
    /// no matter how big the file is — and bounded overall, because a preview
    /// nobody can scroll to the end of is not worth the wait or the memory.
    pub fn read_file(&self, target: &Target, path: String, limit: u64) -> Result<Vec<u8>> {
        let connection = self.connection_for_target(target)?;
        let mut stream = connection.open_file(path, 0, Some(limit), false)?;
        let mut bytes = Vec::new();
        while !stream.is_closed() {
            if let Some(chunk) = stream.read_timeout(REQUEST_TIMEOUT)? {
                bytes.extend_from_slice(&chunk);
            }
        }
        bytes.truncate(limit as usize);
        Ok(bytes)
    }

    pub fn open_media(
        &self,
        target: &Target,
        path: String,
        offset: u64,
        length: Option<u64>,
    ) -> Result<BridgeStream> {
        self.connection_for_target(target)?
            .open_file(path, offset, length, true)
    }

    pub fn upload_file(
        &self,
        target: &Target,
        local_path: &std::path::Path,
        remote_path: String,
        progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        self.connection_for_target(target)?
            .upload_file(local_path, remote_path, progress)
    }

    fn connection_for_target(&self, target: &Target) -> Result<Arc<BridgeConnection>> {
        self.connection_for_target_with_progress(target, |_| {})
    }

    fn connection_for_target_with_progress(
        &self,
        target: &Target,
        progress: impl FnMut(TaskProgress),
    ) -> Result<Arc<BridgeConnection>> {
        match &target.transport {
            Transport::Local => self.connection_with_progress(&target.id, None, progress),
            Transport::Ssh { alias } => {
                self.connection_with_progress(&target.id, Some(alias), progress)
            }
        }
    }

    fn connection(&self, target_id: &str, alias: Option<&str>) -> Result<Arc<BridgeConnection>> {
        self.connection_with_progress(target_id, alias, |_| {})
    }

    fn connection_with_progress(
        &self,
        target_id: &str,
        alias: Option<&str>,
        mut progress: impl FnMut(TaskProgress),
    ) -> Result<Arc<BridgeConnection>> {
        let target_lock = self
            .target_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(target_id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _connecting = target_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let mut connections = self
                .connections
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(connection) = connections.get(target_id)
                && connection.is_alive()
            {
                return Ok(Arc::clone(connection));
            }
            connections.remove(target_id);
        }
        let options = self.options.get(target_id).unwrap_or(&self.default_options);
        let (connection, notice) = match alias {
            Some(alias) => {
                BridgeConnection::connect_ssh_with_progress(alias, options, &mut progress)?
            }
            None => {
                progress(TaskProgress::pending("Starting local companion"));
                (BridgeConnection::connect_local(&options.command)?, None)
            }
        };
        // Sink the machine's attention patterns into the daemon so its own
        // snapshots classify waiting states without waiting on a full scan.
        // Best-effort: an older daemon simply keeps using its built-ins.
        if !options.attention_patterns.is_empty()
            && connection.has_capability("attention-patterns-v1")
            && let Err(error) =
                connection.set_attention_patterns(options.attention_patterns.clone())
        {
            debug::log(
                "bridge",
                format!("target={target_id} could not sink attention patterns: {error:#}"),
            );
        }
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target_id.into(), Arc::clone(&connection));
        if let Some(notice) = notice {
            self.notices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(target_id.into(), notice);
        }
        Ok(connection)
    }

    fn invalidate(&self, target_id: &str, connection: &Arc<BridgeConnection>) {
        let removed = {
            let mut connections = self
                .connections
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if connections
                .get(target_id)
                .is_some_and(|current| Arc::ptr_eq(current, connection))
            {
                connections.remove(target_id)
            } else {
                None
            }
        };
        if let Some(connection) = removed {
            connection.state.shutdown();
        }
    }

    pub fn connected_targets(&self) -> usize {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|connection| connection.is_alive())
            .count()
    }

    pub fn take_notice(&self, target_id: &str) -> Option<String> {
        self.notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(target_id)
    }

    pub fn record_notice(&self, target_id: &str, notice: impl Into<String>) {
        self.notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target_id.into(), notice.into());
    }

    pub fn is_connected(&self, target_id: &str) -> bool {
        self.live_connection(target_id).is_some()
    }

    /// Drop the target's bridge so the next operation reconnects, re-running
    /// bootstrap — and with it the companion update and generation handover.
    pub fn disconnect(&self, target_id: &str) {
        let removed = self
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(target_id);
        if let Some(connection) = removed {
            connection.state.shutdown();
        }
    }

    /// The version of the daemon the target's live bridge talks to. `None`
    /// means no live bridge rather than "current"; safe on the render loop.
    pub fn daemon_version(&self, target_id: &str) -> Option<String> {
        self.live_connection(target_id)?
            .state
            .daemon_version
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The target's established bridge, if it has one. Unlike
    /// [`Self::connection_for_target`] this never connects and never waits on
    /// the per-target connect lock, so callers on the render loop stay
    /// responsive while another thread is dialling.
    fn live_connection(&self, target_id: &str) -> Option<Arc<BridgeConnection>> {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(target_id)
            .filter(|connection| connection.is_alive())
            .map(Arc::clone)
    }
}

#[cfg(unix)]
fn exit_status(exit_code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(exit_code << 8)
}

#[cfg(windows)]
fn exit_status(exit_code: i32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(exit_code as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(unix)]
    #[test]
    fn tcp_streams_are_not_sent_to_an_older_companion_generation() {
        let (client, server) = UnixStream::pair().unwrap();
        let reader = client.try_clone().unwrap();
        let connection = BridgeConnection::from_parts("old-daemon".into(), reader, client, None);
        let error = connection
            .open_tcp("127.0.0.1".into(), 3000)
            .err()
            .expect("missing capability must reject the stream");
        assert!(error.to_string().contains("predates tcp-forward-v1"));
        connection.state.shutdown();
        drop(server);
    }

    /// A cross-build left in the workspace weeks ago must not pin every
    /// remote to its stale generation: it only counts as a sibling of the
    /// controller when their build stamps roughly agree.
    #[test]
    fn a_stale_workspace_companion_is_ignored_in_favour_of_the_release() {
        let root = std::env::temp_dir().join(format!(
            "muxloom-stale-asset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let controller = root.join("muxloom");
        let asset = root.join("muxloomd-cross");
        fs::write(&controller, b"controller").unwrap();
        fs::write(&asset, b"asset").unwrap();

        // Same build round: trusted.
        assert!(!bundled_asset_is_stale(&asset, &controller));

        // An asset from a much earlier build round: ignored.
        let old = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        let file = fs::File::options().write(true).open(&asset).unwrap();
        file.set_modified(old).unwrap();
        drop(file);
        assert!(bundled_asset_is_stale(&asset, &controller));
        fs::remove_dir_all(root).unwrap();
    }

    /// GitHub being unreachable must degrade to the previously verified cache
    /// with a visible "may be stale" source, not to no companion at all — and
    /// with nothing cached it must still be an error.
    #[test]
    fn an_unreachable_release_falls_back_to_the_verified_companion_cache() {
        let root = std::env::temp_dir().join(format!(
            "muxloom-companion-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let triple = "x86_64-unknown-linux-musl";
        fs::create_dir_all(root.join(triple)).unwrap();
        fs::write(root.join(triple).join("muxloomd"), b"cached-companion").unwrap();
        // A proxy nothing listens on makes every fetch fail immediately.
        let unreachable = vec![
            ("HTTPS_PROXY".to_string(), "http://127.0.0.1:1".to_string()),
            ("HTTP_PROXY".to_string(), "http://127.0.0.1:1".to_string()),
        ];

        let (path, source) =
            download_latest_companion_at(&root, triple, "muxloomd", &unreachable, &mut |_| {})
                .unwrap();
        assert_eq!(path, root.join(triple).join("muxloomd"));
        assert!(source.contains("may be stale"), "{source}");

        let error = download_latest_companion_at(
            &root,
            "aarch64-unknown-linux-musl",
            "muxloomd",
            &unreachable,
            &mut |_| {},
        )
        .expect_err("no cache and no network must stay an error");
        assert!(error.to_string().contains("checksum"), "{error:#}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn handshake_records_the_running_daemon_version_for_the_update_indicator() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let reader = client.try_clone().unwrap();
        let connection = BridgeConnection::from_parts("test".into(), reader, client, None);
        let server_thread = thread::spawn(move || {
            let hello = Frame::read_from(&mut server).unwrap().unwrap();
            Frame::json(
                FrameKind::Response,
                0,
                hello.request_id,
                &DaemonResponse::Hello {
                    daemon_version: "0.3.0".into(),
                    protocol_version: PROTOCOL_VERSION,
                    pid: 1,
                    capabilities: vec!["pty-v1".into()],
                },
            )
            .unwrap()
            .write_to(&mut server)
            .unwrap();
            server
        });
        let connection = BridgeConnection::handshake(connection, "test").unwrap();
        assert_eq!(
            connection.state.daemon_version.lock().unwrap().as_deref(),
            Some("0.3.0")
        );
        connection.state.shutdown();
        drop(server_thread.join().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn one_connection_correlates_parallel_shell_responses() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let reader = client.try_clone().unwrap();
        let connection = BridgeConnection::from_parts("test".into(), reader, client, None);
        let server_thread = thread::spawn(move || {
            let first = Frame::read_from(&mut server).unwrap().unwrap();
            let second = Frame::read_from(&mut server).unwrap().unwrap();
            for frame in [second, first] {
                Frame::data(
                    stream::STDOUT,
                    frame.request_id,
                    &frame.request_id.to_be_bytes(),
                    true,
                )
                .write_to(&mut server)
                .unwrap();
                Frame::json(
                    FrameKind::Response,
                    0,
                    frame.request_id,
                    &DaemonResponse::ShellComplete { exit_code: 0 },
                )
                .unwrap()
                .write_to(&mut server)
                .unwrap();
            }
        });
        let (left, right) = thread::scope(|scope| {
            let left = scope.spawn(|| connection.run_shell("left", &[]));
            let right = scope.spawn(|| connection.run_shell("right", &[]));
            (
                left.join().unwrap().unwrap(),
                right.join().unwrap().unwrap(),
            )
        });
        assert_ne!(left.stdout, right.stdout);
        server_thread.join().unwrap();
        connection.state.shutdown();
    }

    /// The render loop writes keystrokes and resizes itself, so a peer that has
    /// stopped reading must never stall the caller — the writer thread absorbs
    /// the backlog up to the queue budget.
    #[cfg(unix)]
    #[test]
    fn writes_do_not_block_the_caller_when_the_peer_stops_reading() {
        let (client, server) = UnixStream::pair().unwrap();
        let reader = client.try_clone().unwrap();
        let connection = BridgeConnection::from_parts("test".into(), reader, client, None);
        // Nothing ever reads `server`, so the socket buffer fills within a few
        // frames; a synchronous write would wedge here.
        let payload = vec![7u8; 64 * 1024];
        let started = Instant::now();
        for _ in 0..16 {
            connection
                .write_frame(Frame::data(stream::PTY_BASE, 0, &payload, false))
                .unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        connection.state.shutdown();
        drop(server);
    }

    /// Resizes are issued from the render loop on every layout change, so they
    /// are fire-and-forget: the frame goes out but no reply is awaited.
    #[cfg(unix)]
    #[test]
    fn detached_resize_sends_a_frame_without_awaiting_a_reply() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let reader = client.try_clone().unwrap();
        let connection = BridgeConnection::from_parts("test".into(), reader, client, None);
        connection
            .resize_detached("muxloom-demo".into(), 120, 40)
            .unwrap();
        let frame = Frame::read_from(&mut server).unwrap().unwrap();
        assert_eq!(frame.kind, FrameKind::Request);
        match frame.decode_json::<DaemonRequest>().unwrap() {
            DaemonRequest::Resize {
                session_id,
                columns,
                rows,
            } => {
                assert_eq!(session_id, "muxloom-demo");
                assert_eq!((columns, rows), (120, 40));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        // No pending entry was registered, so nothing is waiting on a response.
        assert!(connection.state.pending.lock().unwrap().is_empty());
        connection.state.shutdown();
    }

    #[test]
    fn bootstrap_maps_remote_platforms_and_accepts_an_explicit_asset() {
        assert_eq!(
            companion_target_triple("Linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            companion_target_triple("Darwin", "arm64").unwrap(),
            "aarch64-apple-darwin"
        );
        let asset = env::temp_dir().join(format!("muxloomd-bootstrap-{}", std::process::id()));
        fs::write(&asset, b"companion").unwrap();
        let options = BridgeOptions {
            bootstrap_binary: asset.display().to_string(),
            ..BridgeOptions::default()
        };
        assert_eq!(
            resolve_companion_asset(&options, "Linux", "x86_64", &mut |_| {})
                .unwrap()
                .0,
            asset
        );
        fs::remove_file(asset).unwrap();
    }

    #[test]
    fn bootstrap_script_updates_missing_or_stale_companions_in_place() {
        let script = remote_bootstrap_script("~/.local/bin/muxloomd", &[]);
        assert!(script.contains(BOOTSTRAP_MARKER));
        assert!(script.contains("uname -s"));
        assert!(script.contains("binary-sha256"));
        assert!(script.contains("HAVE %s %s %s %s"));
        // The reported version is what keeps a companion left behind by an
        // older build from matching its own fingerprint forever.
        assert!(script.contains("--version"));
        assert!(script.contains("version=${version##* }"));
        assert!(script.contains("INSTALL "));
        assert!(script.contains("head -c \"$muxloom_size\""));
        assert!(script.contains("mv -f \"$temporary\" \"$installed\""));
        assert!(script.contains("exec \"$installed\" bridge"));
        // A pull the target cannot complete is reported rather than fatal, so
        // the loop is still there to receive the pushed bytes.
        assert!(script.contains("PULLFAILED %s"));
        assert!(script.contains("while IFS= read -r muxloom_action"));

        // The host's own environment reaches the fetch, and nothing else.
        let script = remote_bootstrap_script(
            "muxloomd",
            &[
                ("HTTPS_PROXY".into(), "http://box:8118".into()),
                ("not a name".into(), "ignored".into()),
            ],
        );
        assert!(script.contains("export HTTPS_PROXY='http://box:8118'"));
        assert!(!script.contains("ignored"));
    }

    #[cfg(unix)]
    #[test]
    fn the_bootstrap_installs_what_it_pulled_and_falls_back_to_the_push() {
        use std::io::BufReader as StdBufReader;

        let has_curl = Command::new("sh")
            .args(["-c", "command -v curl >/dev/null 2>&1"])
            .status()
            .is_ok_and(|status| status.success());
        if !has_curl {
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "muxloom-bootstrap-pull-{}-{nonce}",
            std::process::id()
        ));
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let release = root.join("muxloomd-release");
        fs::write(&release, b"#!/bin/sh\nprintf 'companion %s\\n' \"$1\"\n").unwrap();
        let digest = sha256_file(&release).unwrap();
        let url = format!("file://{}", release.display());
        let script = remote_bootstrap_script("muxloomd-absent", &[]);

        let start = |actions: &str| -> Vec<String> {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(&script)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(actions.as_bytes())
                .unwrap();
            let output = StdBufReader::new(child.stdout.take().unwrap())
                .lines()
                .map_while(Result::ok)
                .collect();
            let _ = child.wait();
            output
        };

        // A digest that does not match what landed leaves nothing installed and
        // hands the turn back to the controller.
        let lines = start(&format!("PULL {url} {}\n", "ab".repeat(32)));
        assert_eq!(
            lines.first().unwrap().split(' ').next(),
            Some(BOOTSTRAP_MARKER)
        );
        assert!(lines[0].contains("NEED"), "{lines:?}");
        assert_eq!(
            lines.get(1).map(String::as_str),
            Some(format!("{BOOTSTRAP_MARKER} PULLFAILED checksum mismatch").as_str()),
            "{lines:?}"
        );
        assert!(!home.join(".local/share/muxloom/bin/muxloomd").exists());

        // The same failed pull, then the push it falls back to: one connection,
        // both attempts, and the companion running at the end of it.
        let payload = b"#!/bin/sh\nprintf 'pushed %s\\n' \"$1\"\n";
        let lines = start(&format!(
            "PULL {url} {}\nINSTALL {}\n{}",
            "ab".repeat(32),
            payload.len(),
            String::from_utf8_lossy(payload)
        ));
        assert_eq!(
            lines.get(2).map(String::as_str),
            Some(format!("{BOOTSTRAP_MARKER} INSTALLED").as_str()),
            "{lines:?}"
        );
        assert_eq!(
            lines.get(3).map(String::as_str),
            Some("pushed bridge"),
            "{lines:?}"
        );

        // And the pull that does match: the target installs it itself and execs
        // straight into the companion.
        let lines = start(&format!("PULL {url} {digest}\n"));
        assert_eq!(
            lines.get(1).map(String::as_str),
            Some(format!("{BOOTSTRAP_MARKER} PULLED").as_str()),
            "{lines:?}"
        );
        assert_eq!(
            lines.get(2).map(String::as_str),
            Some("companion bridge"),
            "{lines:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_controller_reads_every_answer_a_pull_can_come_back_with() {
        let digest = "ab".repeat(32);
        let exchange = |answer: &str| {
            let mut reader = std::io::Cursor::new(answer.to_string());
            let mut writer = Vec::new();
            let result = exchange_companion_pull(
                "gpu",
                "https://example.invalid/muxloomd-linux",
                &digest,
                "x86_64-unknown-linux-musl",
                &mut reader,
                &mut writer,
                &mut |_| {},
            );
            (result, String::from_utf8(writer).unwrap())
        };

        let (pulled, sent) = exchange(&format!("{BOOTSTRAP_MARKER} PULLED\n"));
        assert_eq!(
            sent,
            format!("PULL https://example.invalid/muxloomd-linux {digest}\n")
        );
        assert!(pulled.unwrap().is_some_and(|notice| notice.contains("gpu")));

        // A target with no route says so, and the caller pushes instead.
        let (refused, _) = exchange(&format!("{BOOTSTRAP_MARKER} PULLFAILED download failed\n"));
        assert_eq!(refused.unwrap(), None);

        // Anything else means the stream no longer says what we think it says,
        // and pushing megabytes into it would be worse than stopping.
        let (confused, _) = exchange("something else\n");
        assert!(
            confused
                .unwrap_err()
                .to_string()
                .contains("invalid bootstrap pull response")
        );
        let (closed, _) = exchange("");
        assert!(closed.unwrap_err().to_string().contains("closed while"));
    }

    #[test]
    fn github_companion_checksums_are_strictly_validated() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_sha256_checksum(&format!("{digest}  muxloomd-linux\n")).unwrap(),
            digest
        );
        assert!(parse_sha256_checksum("not-a-checksum").is_err());
        assert!(parse_sha256_checksum("").is_err());
    }

    #[test]
    fn companion_deployment_reports_uploaded_bytes() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let asset = env::temp_dir().join(format!(
            "muxloomd-progress-{}-{}",
            std::process::id(),
            nonce
        ));
        let contents = vec![7u8; 128 * 1024 + 3];
        fs::write(&asset, &contents).unwrap();
        let mut reader = std::io::Cursor::new(format!("{BOOTSTRAP_MARKER} INSTALLED\n"));
        let mut writer = Vec::new();
        let mut updates = Vec::new();
        // No route to the release: the checksum lookup fails, so the bytes go
        // down the pipe exactly as they always have.
        let options = BridgeOptions {
            download_environment: vec![("HTTPS_PROXY".into(), "http://127.0.0.1:1".into())],
            ..BridgeOptions::default()
        };
        let notice = deploy_remote_companion(
            "gpu",
            &options,
            &asset,
            "deployed test companion",
            "Linux",
            "x86_64",
            &mut reader,
            &mut writer,
            &mut |progress| updates.push(progress),
        )
        .unwrap();
        fs::remove_file(asset).unwrap();

        assert_eq!(notice.as_deref(), Some("deployed test companion"));
        assert!(writer.starts_with(format!("INSTALL {}\n", contents.len()).as_bytes()));
        assert!(writer.ends_with(&contents));
        assert_eq!(updates.first().unwrap().completed, 0);
        assert_eq!(updates.last().unwrap().completed, contents.len() as u64);
        assert_eq!(updates.last().unwrap().total, Some(contents.len() as u64));
    }
}
