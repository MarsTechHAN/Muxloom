use std::{
    collections::{HashMap, VecDeque},
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
        OpenStream, PROTOCOL_VERSION, stream,
    },
    debug,
    model::{DirectoryListing, FileListing, FilePreview, Target, TaskProgress, Transport},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
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
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 5,
            command: "muxloomd".into(),
            reverse_tunnel: String::new(),
            bootstrap_binary: String::new(),
            download_environment: Vec::new(),
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
}

pub struct BridgeStream {
    state: Arc<ConnectionState>,
    stream_id: u32,
    events: mpsc::Receiver<StreamEvent>,
    closed: bool,
}

impl BridgeStream {
    pub fn try_read(&mut self) -> Option<Vec<u8>> {
        match self.events.try_recv() {
            Ok(StreamEvent::Data(bytes)) => Some(bytes),
            Ok(StreamEvent::Closed) | Err(mpsc::TryRecvError::Disconnected) => {
                self.closed = true;
                None
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }

    pub fn read_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        match self.events.recv_timeout(timeout) {
            Ok(StreamEvent::Data(bytes)) => Ok(Some(bytes)),
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
        let bootstrap = remote_bootstrap_script(&options.command);
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
                        protocol_version, ..
                    },
                ..
            } if protocol_version == PROTOCOL_VERSION => {
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

    pub fn read_history(
        &self,
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
    ) -> Result<BridgeHistory> {
        let reply = self.request(DaemonRequest::ReadHistory {
            session_id,
            offset_from_bottom,
            lines,
        })?;
        match reply.response {
            DaemonResponse::HistoryComplete {
                total_lines,
                columns,
                rows,
                offset_from_bottom,
            } => Ok(BridgeHistory {
                bytes: reply.data,
                total_lines,
                columns,
                rows,
                offset_from_bottom,
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

    pub fn upload_file(&self, local_path: &std::path::Path, remote_path: String) -> Result<()> {
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
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            stream.write_data(&buffer[..read], true)?;
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

fn remote_bootstrap_script(configured_command: &str) -> String {
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
    printf '{marker} HAVE %s %s %s\n' "$os" "$arch" "$fingerprint"
else
    printf '{marker} NEED %s %s\n' "$os" "$arch"
fi
IFS= read -r muxloom_action
if [ "$muxloom_action" = USE ] && [ -n "$candidate" ]; then
    exec "$candidate" bridge
fi
case "$muxloom_action" in
    'INSTALL '*) muxloom_size=${{muxloom_action#INSTALL }} ;;
    *) printf 'invalid bootstrap action\n' >&2; exit 64 ;;
esac
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
exec "$installed" bridge"#,
        configured = shell_quote(configured_command),
        protocol_version = PROTOCOL_VERSION,
        marker = BOOTSTRAP_MARKER,
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
        [marker, "HAVE", os, arch, fingerprint] if *marker == BOOTSTRAP_MARKER => {
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
            if fingerprint.eq_ignore_ascii_case(&expected) {
                debug::log(
                    "bridge",
                    format!("target={alias} remote companion fingerprint is current"),
                );
                writeln!(writer, "USE")?;
                writer.flush()?;
                Ok(None)
            } else {
                deploy_remote_companion(alias, &asset, &notice, os, arch, reader, writer, progress)
            }
        }
        [marker, "NEED", os, arch] if *marker == BOOTSTRAP_MARKER => {
            let (asset, notice) = resolve_companion_asset(options, os, arch, progress)?;
            deploy_remote_companion(alias, &asset, &notice, os, arch, reader, writer, progress)
        }
        _ => bail!(
            "invalid muxloomd bootstrap response from {alias}: {}",
            status.trim()
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn deploy_remote_companion(
    alias: &str,
    asset: &Path,
    notice: &str,
    os: &str,
    arch: &str,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    progress: &mut impl FnMut(TaskProgress),
) -> Result<Option<String>> {
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
        debug::log(
            "bridge",
            format!("using bundled {triple} companion asset {}", path.display()),
        );
        return Ok((
            path,
            format!("deployed bundled {triple} muxloomd companion"),
        ));
    }
    debug::log(
        "bridge",
        format!(
            "no bundled {triple} companion asset; downloading the latest GitHub Release on the controller"
        ),
    );
    let (path, downloaded) =
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
    let source = if downloaded {
        "downloaded and checksum-verified from the latest GitHub Release"
    } else {
        "loaded from the checksum-verified controller cache"
    };
    Ok((path, format!("deployed {triple} muxloomd {source}")))
}

fn download_latest_companion(
    triple: &str,
    executable: &str,
    environment: &[(String, String)],
    progress: &mut impl FnMut(TaskProgress),
) -> Result<(PathBuf, bool)> {
    let asset_name = format!(
        "muxloomd-{triple}{}",
        executable_suffix_for_name(executable)
    );
    let cache = companion_cache_root().join(triple);
    fs::create_dir_all(&cache)
        .with_context(|| format!("failed to create companion cache {}", cache.display()))?;
    let destination = cache.join(executable);
    let checksum_url = format!("{COMPANION_RELEASE_ROOT}/{asset_name}.sha256");
    progress(TaskProgress::pending(format!(
        "Checking {triple} companion release"
    )));
    let expected = controller_fetch_text(&checksum_url, environment)
        .context("failed to fetch companion checksum")?;
    let expected = parse_sha256_checksum(&expected)?;
    if destination.is_file() && sha256_file(&destination).is_ok_and(|actual| actual == expected) {
        debug::log(
            "bridge",
            format!("using cached {triple} companion {}", destination.display()),
        );
        return Ok((destination, false));
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
    Ok((destination, true))
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
    ) -> Result<BridgeHistory> {
        self.connection_for_target(target)?
            .read_history(session_id, offset_from_bottom, lines)
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

    /// Read a whole remote file over the chunked, flow-controlled file stream.
    /// Used to finish a preview whose body does not fit in one response frame,
    /// so the transfer stays bounded per frame no matter how big the file is.
    pub fn read_file(&self, target: &Target, path: String) -> Result<Vec<u8>> {
        let connection = self.connection_for_target(target)?;
        let mut stream = connection.open_file(path, 0, None, false)?;
        let mut bytes = Vec::new();
        while !stream.is_closed() {
            if let Some(chunk) = stream.read_timeout(REQUEST_TIMEOUT)? {
                bytes.extend_from_slice(&chunk);
            }
        }
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
    ) -> Result<()> {
        self.connection_for_target(target)?
            .upload_file(local_path, remote_path)
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
        let script = remote_bootstrap_script("~/.local/bin/muxloomd");
        assert!(script.contains(BOOTSTRAP_MARKER));
        assert!(script.contains("uname -s"));
        assert!(script.contains("binary-sha256"));
        assert!(script.contains("HAVE %s %s %s"));
        assert!(script.contains("INSTALL "));
        assert!(script.contains("head -c \"$muxloom_size\""));
        assert!(script.contains("mv -f \"$temporary\" \"$installed\""));
        assert!(script.contains("exec \"$installed\" bridge"));
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
        let notice = deploy_remote_companion(
            "gpu",
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
