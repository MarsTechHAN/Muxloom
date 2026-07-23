use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    daemon_protocol::{
        DaemonHistoryMatch, DaemonRequest, DaemonResponse, DaemonSession, Frame, FrameKind,
        OpenStream, PROTOCOL_VERSION, stream,
    },
    debug,
    model::{DirectoryListing, FileListing, FilePreview, Target, Transport},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct BridgeOptions {
    pub connect_timeout_secs: u64,
    pub command: String,
    pub reverse_tunnel: String,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 5,
            command: "muxloomd".into(),
            reverse_tunnel: String::new(),
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
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Option<Child>>,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    streams: Mutex<HashMap<u32, mpsc::Sender<StreamEvent>>>,
    next_request: AtomicU64,
    next_stream: AtomicU64,
    alive: AtomicBool,
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
        Frame::data(self.stream_id, 0, bytes, compress).write_to(
            &mut *self
                .state
                .writer
                .lock()
                .map_err(|_| anyhow!("bridge writer is poisoned"))?,
        )
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
        if let Ok(mut writer) = self.state.writer.lock() {
            let _ = frame.write_to(&mut *writer);
        }
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
    pub fn connect_ssh(alias: &str, options: &BridgeOptions) -> Result<Arc<Self>> {
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
        command
            .arg(alias)
            .arg(&options.command)
            .arg("bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to open muxloomd bridge to {alias}"))?;
        let writer = child.stdin.take().context("ssh bridge has no stdin")?;
        let reader = child.stdout.take().context("ssh bridge has no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            let target = alias.to_string();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    debug::log("bridge", format!("target={target} ssh: {line}"));
                }
            });
        }
        let connection = Self::from_parts(alias.to_string(), reader, writer, Some(child));
        Self::handshake(connection, alias)
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
        let state = Arc::new(ConnectionState {
            target,
            writer: Mutex::new(Box::new(writer)),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            next_stream: AtomicU64::new(u64::from(stream::PTY_BASE)),
            alive: AtomicBool::new(true),
        });
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
        if let Err(error) = self.write_frame(&frame) {
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

    fn write_frame(&self, frame: &Frame) -> Result<()> {
        frame.write_to(
            &mut *self
                .state
                .writer
                .lock()
                .map_err(|_| anyhow!("bridge writer is poisoned"))?,
        )
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

    fn expect_ack(&self, request: DaemonRequest) -> Result<()> {
        match self.request(request)?.response {
            DaemonResponse::Ack => Ok(()),
            response => bail!("unexpected daemon response: {response:?}"),
        }
    }

    pub fn open_pty(&self, session_id: String, columns: u16, rows: u16) -> Result<BridgeStream> {
        self.open_stream(OpenStream::Pty {
            session_id,
            columns,
            rows,
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
        if let Err(error) = self.write_frame(&frame) {
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
                            Frame::window_update(frame.stream_id, consumed).write_to(
                                &mut *state
                                    .writer
                                    .lock()
                                    .map_err(|_| anyhow!("bridge writer is poisoned"))?,
                            )?;
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
            let result = state
                .writer
                .lock()
                .map_err(|_| anyhow!("bridge writer is poisoned"))
                .and_then(|mut writer| heartbeat.write_to(&mut *writer));
            if let Err(error) = result {
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

#[derive(Clone, Default)]
pub struct BridgePool {
    connections: Arc<Mutex<HashMap<String, Arc<BridgeConnection>>>>,
    target_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
        self.connection_for_target(target)?
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

    pub fn open_pty(
        &self,
        target: &Target,
        session_id: String,
        columns: u16,
        rows: u16,
    ) -> Result<BridgeStream> {
        self.connection_for_target(target)?
            .open_pty(session_id, columns, rows)
    }

    pub fn download_file(
        &self,
        target: &Target,
        remote_path: String,
        destination: &std::path::Path,
    ) -> Result<()> {
        let connection = self.connection_for_target(target)?;
        let mut stream = connection.open_file(remote_path, 0, None, false)?;
        let mut file = std::fs::File::create(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        while !stream.is_closed() {
            if let Some(bytes) = stream.read_timeout(REQUEST_TIMEOUT)? {
                file.write_all(&bytes)?;
            }
        }
        file.flush()?;
        Ok(())
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
        match &target.transport {
            Transport::Local => self.connection(&target.id, None),
            Transport::Ssh { alias } => self.connection(&target.id, Some(alias)),
        }
    }

    fn connection(&self, target_id: &str, alias: Option<&str>) -> Result<Arc<BridgeConnection>> {
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
        let connection = match alias {
            Some(alias) => BridgeConnection::connect_ssh(alias, options)?,
            None => BridgeConnection::connect_local(&options.command)?,
        };
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target_id.into(), Arc::clone(&connection));
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

    pub fn is_connected(&self, target_id: &str) -> bool {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(target_id)
            .is_some_and(|connection| connection.is_alive())
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
}
