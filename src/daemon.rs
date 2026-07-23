#[cfg(unix)]
mod platform {
    use std::{
        collections::HashMap,
        fs::{self, File, OpenOptions},
        io::{self, BufRead, BufReader, Read, Write},
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
            process::ExitStatusExt,
        },
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result, anyhow, bail};
    use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

    use crate::{
        daemon_protocol::{
            DATA_CHUNK_SIZE, DaemonHistoryMatch, DaemonRequest, DaemonResponse, DaemonSession,
            Frame, FrameKind, INITIAL_STREAM_WINDOW, OpenStream, PROTOCOL_VERSION, StreamOpened,
            stream,
        },
        model::{
            AgentKind, DirectoryListing, FileEntry, FileEntryKind, FileListing, FilePreview,
            FilePreviewKind,
        },
        recap::extract_recap,
        runtime::{agent_is_working, attention_reason},
        terminal_session::resize_parser,
    };

    const RECENT_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

    #[derive(Debug, Clone)]
    pub struct DaemonPaths {
        pub root: PathBuf,
        pub socket: PathBuf,
        pub pid: PathBuf,
        pub log: PathBuf,
        pub history: PathBuf,
        pub sessions: PathBuf,
    }

    impl DaemonPaths {
        pub fn discover() -> Result<Self> {
            if let Some(path) = std::env::var_os("MUXLOOMD_STATE_DIR") {
                return Ok(Self::under(PathBuf::from(path)));
            }
            if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
                return Ok(Self::under(PathBuf::from(path).join("muxloom")));
            }
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(Self::under(
                PathBuf::from(home).join(".local/state/muxloom"),
            ))
        }

        pub fn under(root: PathBuf) -> Self {
            Self {
                socket: root.join("muxloomd.sock"),
                pid: root.join("muxloomd.pid"),
                log: root.join("muxloomd.log"),
                history: root.join("history"),
                sessions: root.join("sessions"),
                root,
            }
        }

        fn prepare(&self) -> Result<()> {
            fs::create_dir_all(&self.root)
                .with_context(|| format!("failed to create {}", self.root.display()))?;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
            fs::create_dir_all(&self.history)?;
            fs::set_permissions(&self.history, fs::Permissions::from_mode(0o700))?;
            fs::create_dir_all(&self.sessions)?;
            fs::set_permissions(&self.sessions, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
    }

    struct DaemonState {
        started: Instant,
        clients: AtomicUsize,
        next_subscriber: AtomicU64,
        sessions: Mutex<HashMap<String, Arc<ManagedSession>>>,
        paths: DaemonPaths,
    }

    struct ManagedSession {
        metadata: Mutex<DaemonSession>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        writer: Mutex<Box<dyn Write + Send>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        subscribers: Mutex<HashMap<u64, Subscriber>>,
        screen: Mutex<vt100::Parser>,
        recent_output: Mutex<Vec<u8>>,
        history_path: PathBuf,
        metadata_path: PathBuf,
        archived: AtomicBool,
        line_count: AtomicUsize,
        columns: AtomicU16,
        rows: AtomicU16,
    }

    #[derive(Clone)]
    struct Subscriber {
        stream_id: u32,
        writer: Arc<Mutex<UnixStream>>,
    }

    enum ClientStream {
        Pty {
            session: Arc<ManagedSession>,
            subscriber_id: u64,
        },
        Upload {
            file: File,
            temporary_path: PathBuf,
            destination: PathBuf,
            remaining: u64,
        },
    }

    #[derive(Default)]
    struct StreamFlow {
        credits: Mutex<HashMap<u32, u64>>,
        changed: Condvar,
        closed: AtomicBool,
    }

    impl StreamFlow {
        fn open(&self, stream_id: u32) {
            self.credits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(stream_id, u64::from(INITIAL_STREAM_WINDOW));
        }

        fn add(&self, stream_id: u32, credit: u32) {
            if let Some(current) = self
                .credits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_mut(&stream_id)
            {
                *current = current.saturating_add(u64::from(credit));
                self.changed.notify_all();
            }
        }

        fn consume(&self, stream_id: u32, bytes: usize) -> Result<()> {
            let mut credits = self
                .credits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while credits.get(&stream_id).copied().unwrap_or(0) < bytes as u64 {
                if self.closed.load(Ordering::Acquire) {
                    bail!("stream connection closed");
                }
                credits = self
                    .changed
                    .wait(credits)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if let Some(current) = credits.get_mut(&stream_id) {
                *current -= bytes as u64;
            }
            Ok(())
        }

        fn close(&self, stream_id: u32) {
            self.credits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&stream_id);
        }

        fn disconnect(&self) {
            self.closed.store(true, Ordering::Release);
            self.changed.notify_all();
        }
    }

    impl DaemonState {
        fn new(paths: DaemonPaths) -> Self {
            Self {
                started: Instant::now(),
                clients: AtomicUsize::new(0),
                next_subscriber: AtomicU64::new(1),
                sessions: Mutex::new(HashMap::new()),
                paths,
            }
        }
    }

    pub fn serve(paths: &DaemonPaths) -> Result<()> {
        paths.prepare()?;
        if paths.socket.exists() {
            if UnixStream::connect(&paths.socket).is_ok() {
                bail!("muxloomd is already running");
            }
            if daemon_process_alive(paths) {
                bail!("muxloomd is running but its socket is not accessible");
            }
            fs::remove_file(&paths.socket).with_context(|| {
                format!("failed to remove stale socket {}", paths.socket.display())
            })?;
        }
        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("failed to bind {}", paths.socket.display()))?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        fs::write(&paths.pid, format!("{}\n", std::process::id()))?;
        let _guard = SocketGuard {
            socket: paths.socket.clone(),
            pid: paths.pid.clone(),
        };
        let state = Arc::new(DaemonState::new(paths.clone()));
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    let state = Arc::clone(&state);
                    thread::spawn(move || {
                        if let Err(error) = serve_client(stream, state) {
                            eprintln!("muxloomd client closed: {error:#}");
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("muxloomd accept failed"),
            }
        }
        Ok(())
    }

    struct SocketGuard {
        socket: PathBuf,
        pid: PathBuf,
    }

    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.socket);
            let _ = fs::remove_file(&self.pid);
        }
    }

    struct ClientGuard(Arc<DaemonState>);

    impl Drop for ClientGuard {
        fn drop(&mut self) {
            self.0.clients.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn serve_client(mut stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
        state.clients.fetch_add(1, Ordering::Relaxed);
        let _client_guard = ClientGuard(Arc::clone(&state));
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        let flow = Arc::new(StreamFlow::default());
        let mut subscriptions: HashMap<u32, ClientStream> = HashMap::new();
        let result = (|| -> Result<()> {
            while let Some(frame) = Frame::read_from(&mut stream)? {
                match frame.kind {
                    FrameKind::Heartbeat => {
                        write_frame(
                            &writer,
                            &Frame::new(FrameKind::Heartbeat, 0, frame.request_id, vec![]),
                        )?;
                    }
                    FrameKind::WindowUpdate => {
                        flow.add(frame.stream_id, frame.window_credit()?);
                    }
                    FrameKind::Request => {
                        let request = match frame.decode_json::<DaemonRequest>() {
                            Ok(request) => request,
                            Err(error) => {
                                write_response(
                                    &writer,
                                    frame.request_id,
                                    &DaemonResponse::Error {
                                        message: error.to_string(),
                                    },
                                )?;
                                continue;
                            }
                        };
                        let writer = Arc::clone(&writer);
                        let state = Arc::clone(&state);
                        thread::spawn(move || {
                            if let Err(error) =
                                handle_request(&writer, &state, frame.request_id, request)
                            {
                                let _ = write_response(
                                    &writer,
                                    frame.request_id,
                                    &DaemonResponse::Error {
                                        message: error.to_string(),
                                    },
                                );
                            }
                        });
                    }
                    FrameKind::OpenStream => match frame.decode_json::<OpenStream>()? {
                        OpenStream::Pty {
                            session_id,
                            columns,
                            rows,
                        } => {
                            let session = daemon_session(&state, &session_id)?;
                            session.resize(columns, rows)?;
                            let subscriber_id =
                                state.next_subscriber.fetch_add(1, Ordering::Relaxed);
                            session
                                .subscribers
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .insert(
                                    subscriber_id,
                                    Subscriber {
                                        stream_id: frame.stream_id,
                                        writer: Arc::clone(&writer),
                                    },
                                );
                            subscriptions.insert(
                                frame.stream_id,
                                ClientStream::Pty {
                                    session: Arc::clone(&session),
                                    subscriber_id,
                                },
                            );
                            write_stream_opened(&writer, &frame, None)?;
                            let recent = session
                                .recent_output
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            for chunk in recent.chunks(DATA_CHUNK_SIZE) {
                                write_frame(
                                    &writer,
                                    &Frame::data(frame.stream_id, 0, chunk, true),
                                )?;
                            }
                        }
                        OpenStream::File {
                            path,
                            offset,
                            length,
                        } => {
                            open_download_stream(
                                &writer, &flow, &frame, path, offset, length, true,
                            )?;
                        }
                        OpenStream::Media {
                            path,
                            offset,
                            length,
                        } => {
                            open_download_stream(
                                &writer, &flow, &frame, path, offset, length, false,
                            )?;
                        }
                        OpenStream::Upload { path, size } => {
                            let destination = PathBuf::from(path);
                            let parent = destination
                                .parent()
                                .context("upload destination has no parent")?;
                            if !parent.is_dir() {
                                bail!("upload destination directory does not exist");
                            }
                            let nonce = state.next_subscriber.fetch_add(1, Ordering::Relaxed);
                            let temporary_path = parent
                                .join(format!(".muxloom-upload-{}-{nonce}", std::process::id()));
                            let file = OpenOptions::new()
                                .create_new(true)
                                .write(true)
                                .open(&temporary_path)?;
                            subscriptions.insert(
                                frame.stream_id,
                                ClientStream::Upload {
                                    file,
                                    temporary_path,
                                    destination,
                                    remaining: size,
                                },
                            );
                            write_stream_opened(&writer, &frame, Some(size))?;
                        }
                    },
                    FrameKind::Data => {
                        if let Some(stream) = subscriptions.get_mut(&frame.stream_id) {
                            let payload = frame.decoded_payload()?;
                            match stream {
                                ClientStream::Pty { session, .. } => {
                                    session.write_input(&payload)?
                                }
                                ClientStream::Upload {
                                    file, remaining, ..
                                } => {
                                    if payload.len() as u64 > *remaining {
                                        bail!("upload sent more bytes than declared");
                                    }
                                    file.write_all(&payload)?;
                                    *remaining -= payload.len() as u64;
                                }
                            }
                        }
                    }
                    FrameKind::CloseStream => {
                        if let Some(stream) = subscriptions.remove(&frame.stream_id) {
                            close_client_stream(stream)?;
                        }
                    }
                    FrameKind::Response | FrameKind::Error => {
                        write_response(
                            &writer,
                            frame.request_id,
                            &DaemonResponse::Error {
                                message: format!("unexpected client frame {:?}", frame.kind),
                            },
                        )?;
                    }
                }
            }
            Ok(())
        })();
        for (_, stream) in subscriptions {
            cleanup_client_stream(stream);
        }
        flow.disconnect();
        result
    }

    fn write_stream_opened(
        writer: &Arc<Mutex<UnixStream>>,
        frame: &Frame,
        total_bytes: Option<u64>,
    ) -> Result<()> {
        write_frame(
            writer,
            &Frame::json(
                FrameKind::OpenStream,
                frame.stream_id,
                frame.request_id,
                &StreamOpened {
                    initial_window: INITIAL_STREAM_WINDOW,
                    total_bytes,
                },
            )?,
        )
    }

    fn open_download_stream(
        writer: &Arc<Mutex<UnixStream>>,
        flow: &Arc<StreamFlow>,
        frame: &Frame,
        path: String,
        offset: u64,
        length: Option<u64>,
        compress: bool,
    ) -> Result<()> {
        use std::io::{Seek, SeekFrom};

        let mut file = File::open(&path).with_context(|| format!("failed to open {path}"))?;
        let total = file.metadata()?.len();
        if offset > total {
            bail!("stream offset is past end of file");
        }
        file.seek(SeekFrom::Start(offset))?;
        let remaining = length.unwrap_or(total - offset).min(total - offset);
        write_stream_opened(writer, frame, Some(total))?;
        flow.open(frame.stream_id);
        let writer = Arc::clone(writer);
        let flow = Arc::clone(flow);
        let stream_id = frame.stream_id;
        thread::spawn(move || {
            if let Err(error) = stream_file(&writer, &flow, stream_id, file, remaining, compress) {
                eprintln!("muxloomd file stream failed: {error:#}");
            }
            flow.close(stream_id);
        });
        Ok(())
    }

    fn stream_file(
        writer: &Arc<Mutex<UnixStream>>,
        flow: &StreamFlow,
        stream_id: u32,
        mut file: File,
        mut remaining: u64,
        compress: bool,
    ) -> Result<()> {
        let mut buffer = vec![0; DATA_CHUNK_SIZE];
        while remaining > 0 {
            let capacity = remaining.min(DATA_CHUNK_SIZE as u64) as usize;
            flow.consume(stream_id, capacity)?;
            let read = file.read(&mut buffer[..capacity])?;
            if read == 0 {
                break;
            }
            write_frame(
                writer,
                &Frame::data(stream_id, 0, &buffer[..read], compress),
            )?;
            remaining -= read as u64;
        }
        write_frame(
            writer,
            &Frame::new(FrameKind::CloseStream, stream_id, 0, vec![]),
        )
    }

    fn close_client_stream(stream: ClientStream) -> Result<()> {
        match stream {
            ClientStream::Pty {
                session,
                subscriber_id,
            } => {
                session
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&subscriber_id);
                Ok(())
            }
            ClientStream::Upload {
                mut file,
                temporary_path,
                destination,
                remaining,
            } => {
                file.flush()?;
                if remaining != 0 {
                    drop(file);
                    let _ = fs::remove_file(&temporary_path);
                    bail!("upload closed with {remaining} bytes missing");
                }
                file.sync_all()?;
                drop(file);
                fs::rename(&temporary_path, &destination).with_context(|| {
                    format!(
                        "failed to activate upload {}",
                        destination.to_string_lossy()
                    )
                })
            }
        }
    }

    fn cleanup_client_stream(stream: ClientStream) {
        match stream {
            ClientStream::Pty {
                session,
                subscriber_id,
            } => {
                session
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&subscriber_id);
            }
            ClientStream::Upload { temporary_path, .. } => {
                let _ = fs::remove_file(temporary_path);
            }
        }
    }

    fn handle_request(
        writer: &Arc<Mutex<UnixStream>>,
        state: &DaemonState,
        request_id: u64,
        request: DaemonRequest,
    ) -> Result<()> {
        match request {
            DaemonRequest::Hello {
                protocol_version, ..
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return write_response(
                        writer,
                        request_id,
                        &DaemonResponse::Error {
                            message: format!(
                                "protocol mismatch: client={protocol_version} daemon={PROTOCOL_VERSION}"
                            ),
                        },
                    );
                }
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Hello {
                        daemon_version: env!("CARGO_PKG_VERSION").into(),
                        protocol_version: PROTOCOL_VERSION,
                        pid: std::process::id(),
                        capabilities: vec![
                            "multiplex-v1".into(),
                            "compression-lz4-v1".into(),
                            "shell-compat-v1".into(),
                            "pty-v1".into(),
                            "files-v1".into(),
                            "history-v1".into(),
                            "media-v1".into(),
                        ],
                    },
                )
            }
            DaemonRequest::Ping => write_response(
                writer,
                request_id,
                &DaemonResponse::Pong {
                    unix_time_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                },
            ),
            DaemonRequest::Status => write_response(
                writer,
                request_id,
                &DaemonResponse::Status {
                    pid: std::process::id(),
                    uptime_ms: state
                        .started
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                    clients: state.clients.load(Ordering::Relaxed),
                },
            ),
            DaemonRequest::ProbeExecutables { executables } => {
                let available = executables
                    .into_iter()
                    .filter(|executable| executable_available(executable))
                    .collect();
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Executables { available },
                )
            }
            DaemonRequest::ListSessions => {
                let sessions = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .map(|session| session.snapshot())
                    .collect();
                write_response(writer, request_id, &DaemonResponse::Sessions { sessions })
            }
            DaemonRequest::Launch {
                session_id,
                kind,
                path,
                label,
                executable,
                args,
                environment,
                created_at,
                columns,
                rows,
            } => {
                let session = launch_session(
                    state,
                    session_id,
                    kind,
                    path,
                    label,
                    executable,
                    args,
                    environment,
                    created_at,
                    columns,
                    rows,
                )?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Launched {
                        session: session.snapshot(),
                    },
                )
            }
            DaemonRequest::Resize {
                session_id,
                columns,
                rows,
            } => {
                daemon_session(state, &session_id)?.resize(columns, rows)?;
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::ReadHistory {
                session_id,
                offset_from_bottom,
                lines,
            } => {
                let session = daemon_session(state, &session_id)?;
                let (history, total_lines, actual_offset) =
                    session.read_history(offset_from_bottom, lines)?;
                write_chunks(writer, stream::HISTORY, request_id, &history)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::HistoryComplete {
                        total_lines,
                        columns: session.columns.load(Ordering::Relaxed),
                        rows: session.rows.load(Ordering::Relaxed),
                        offset_from_bottom: actual_offset,
                    },
                )
            }
            DaemonRequest::SearchHistory {
                session_id,
                query,
                max_matches,
            } => {
                let matches = daemon_session(state, &session_id)?
                    .search_history(&query, max_matches.clamp(1, 50))?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::HistoryMatches { matches },
                )
            }
            DaemonRequest::ListDirectory { path } => write_response(
                writer,
                request_id,
                &DaemonResponse::Directory {
                    listing: native_list_directory(&path)?,
                },
            ),
            DaemonRequest::ListFiles { path } => write_response(
                writer,
                request_id,
                &DaemonResponse::Files {
                    listing: native_list_files(&path)?,
                },
            ),
            DaemonRequest::PreviewFile { path, limit } => write_response(
                writer,
                request_id,
                &DaemonResponse::Preview {
                    preview: native_preview_file(&path, limit.min(1024 * 1024))?,
                },
            ),
            DaemonRequest::Archive { session_id } => {
                daemon_session(state, &session_id)?.archive()?;
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::Delete { session_id } => {
                let session = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&session_id)
                    .with_context(|| format!("unknown daemon session {session_id}"))?;
                session.archive()?;
                let _ = fs::remove_file(&session.history_path);
                let _ = fs::remove_file(&session.metadata_path);
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::RunShell {
                script,
                environment,
            } => {
                let mut command = Command::new("sh");
                command.args(["-lc", &script]).envs(environment);
                let output = command
                    .output()
                    .context("failed to execute compatibility shell")?;
                write_chunks(writer, stream::STDOUT, request_id, &output.stdout)?;
                write_chunks(writer, stream::STDERR, request_id, &output.stderr)?;
                let exit_code = output
                    .status
                    .code()
                    .unwrap_or_else(|| output.status.signal().map_or(255, |signal| 128 + signal));
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::ShellComplete { exit_code },
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn launch_session(
        state: &DaemonState,
        session_id: String,
        kind: String,
        path: String,
        label: String,
        executable: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
        columns: u16,
        rows: u16,
    ) -> Result<Arc<ManagedSession>> {
        validate_session_id(&session_id)?;
        if state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&session_id)
        {
            bail!("daemon session already exists: {session_id}");
        }
        if !Path::new(&path).is_dir() {
            bail!("working directory does not exist: {path}");
        }
        let executable = if executable.trim().is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        } else {
            executable
        };
        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(5),
            cols: columns.max(20),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(executable);
        command.args(args);
        command.cwd(path.clone());
        let path_overridden = environment.iter().any(|(name, _)| name == "PATH");
        for (name, value) in environment {
            command.env(name, value);
        }
        if !path_overridden
            && let (Some(home), Some(path)) = (std::env::var_os("HOME"), std::env::var_os("PATH"))
        {
            let mut paths = vec![PathBuf::from(home).join(".local/bin")];
            paths.extend(std::env::split_paths(&path));
            if let Ok(path) = std::env::join_paths(paths) {
                command.env("PATH", path);
            }
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "muxloom");
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let pid = child.process_id();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let history_path = state.paths.history.join(format!("{session_id}.ansi"));
        let metadata_path = state.paths.sessions.join(format!("{session_id}.json"));
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_path)?;
        let metadata = DaemonSession {
            id: session_id.clone(),
            kind,
            path,
            label,
            created_at,
            pid,
            dead: false,
            archived: false,
            recap: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
        };
        let session = Arc::new(ManagedSession {
            metadata: Mutex::new(metadata),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            subscribers: Mutex::new(HashMap::new()),
            screen: Mutex::new(vt100::Parser::new(rows.max(5), columns.max(20), 0)),
            recent_output: Mutex::new(Vec::new()),
            history_path,
            metadata_path,
            archived: AtomicBool::new(false),
            line_count: AtomicUsize::new(0),
            columns: AtomicU16::new(columns.max(20)),
            rows: AtomicU16::new(rows.max(5)),
        });
        session.persist_metadata()?;
        state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, Arc::clone(&session));
        let managed = Arc::clone(&session);
        thread::spawn(move || {
            let mut history = match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&managed.history_path)
            {
                Ok(history) => history,
                Err(error) => {
                    eprintln!("muxloomd history open failed: {error}");
                    managed.mark_dead();
                    return;
                }
            };
            let mut buffer = vec![0; DATA_CHUNK_SIZE];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let bytes = &buffer[..read];
                        let _ = history.write_all(bytes);
                        managed.record_output(bytes);
                        managed.broadcast(bytes);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = history.flush();
            managed.mark_dead();
        });
        Ok(session)
    }

    fn daemon_session(state: &DaemonState, session_id: &str) -> Result<Arc<ManagedSession>> {
        state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
            .with_context(|| format!("unknown daemon session {session_id}"))
    }

    fn validate_session_id(session_id: &str) -> Result<()> {
        if session_id.is_empty()
            || session_id.len() > 160
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("invalid daemon session id");
        }
        Ok(())
    }

    fn executable_available(executable: &str) -> bool {
        if executable.contains('/') {
            return is_executable(Path::new(executable));
        }
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(executable))
            .any(|path| is_executable(&path))
            || std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/bin").join(executable))
                .is_some_and(|path| is_executable(&path))
    }

    fn native_list_directory(path: &str) -> Result<DirectoryListing> {
        let path = canonical_directory(path)?;
        let mut directories = fs::read_dir(&path)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.metadata().is_ok_and(|metadata| metadata.is_dir()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        directories.sort_by_key(|value| value.to_lowercase());
        Ok(DirectoryListing {
            path: path.to_string_lossy().into_owned(),
            directories,
        })
    }

    fn native_list_files(path: &str) -> Result<FileListing> {
        let path = canonical_directory(path)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let metadata = entry.metadata().ok();
            let kind = if file_type.is_symlink() {
                FileEntryKind::Symlink
            } else if file_type.is_dir() {
                FileEntryKind::Directory
            } else if file_type.is_file() {
                FileEntryKind::File
            } else {
                FileEntryKind::Other
            };
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path().to_string_lossy().into_owned(),
                kind,
                size: metadata
                    .filter(|metadata| metadata.is_file())
                    .map_or(0, |metadata| metadata.len()),
            });
        }
        entries.sort_by(|left, right| {
            file_kind_order(left.kind)
                .cmp(&file_kind_order(right.kind))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(FileListing {
            path: path.to_string_lossy().into_owned(),
            entries,
        })
    }

    fn canonical_directory(path: &str) -> Result<PathBuf> {
        let path = if path.trim().is_empty() { "." } else { path };
        let path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve directory {path}"))?;
        if !path.is_dir() {
            bail!("not a directory: {}", path.display());
        }
        Ok(path)
    }

    fn file_kind_order(kind: FileEntryKind) -> u8 {
        match kind {
            FileEntryKind::Directory => 0,
            FileEntryKind::Symlink => 1,
            FileEntryKind::File => 2,
            FileEntryKind::Other => 3,
        }
    }

    fn native_preview_file(path: &str, limit: usize) -> Result<FilePreview> {
        let metadata = fs::metadata(path).with_context(|| format!("failed to stat {path}"))?;
        if !metadata.is_file() {
            bail!("not a regular file: {path}");
        }
        let limit = limit.max(1);
        let mut bytes = Vec::new();
        File::open(path)?
            .take(limit as u64)
            .read_to_end(&mut bytes)?;
        let lower = path.to_lowercase();
        let kind = if matches_extension(&lower, &["md", "markdown", "mdown", "mkd"]) {
            FilePreviewKind::Markdown
        } else if matches_extension(&lower, &["mp3", "wav", "flac", "aac", "m4a", "ogg", "opus"]) {
            FilePreviewKind::Audio
        } else if matches_extension(
            &lower,
            &["mp4", "m4v", "mov", "mkv", "webm", "avi", "mpeg", "mpg"],
        ) {
            FilePreviewKind::Video
        } else if looks_like_text(&bytes) {
            FilePreviewKind::Text
        } else {
            FilePreviewKind::Binary
        };
        let mime = match kind {
            FilePreviewKind::Text => "text/plain",
            FilePreviewKind::Markdown => "text/markdown",
            FilePreviewKind::Audio => "audio/*",
            FilePreviewKind::Video => "video/*",
            FilePreviewKind::Binary => "application/octet-stream",
        };
        let content = if matches!(kind, FilePreviewKind::Text | FilePreviewKind::Markdown) {
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            String::new()
        };
        Ok(FilePreview {
            path: path.into(),
            mime: mime.into(),
            kind,
            size: metadata.len(),
            content,
            truncated: metadata.len() > limit as u64,
        })
    }

    fn matches_extension(path: &str, extensions: &[&str]) -> bool {
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extensions.contains(&extension))
    }

    fn looks_like_text(bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if bytes.iter().take(8192).any(|&byte| byte == 0) {
            return false;
        }
        if std::str::from_utf8(bytes).is_ok() {
            return true;
        }
        let controls = bytes
            .iter()
            .filter(|&&byte| byte < 0x20 && !matches!(byte, b'\n' | b'\r' | b'\t' | 0x0c))
            .count();
        controls.saturating_mul(100) < bytes.len()
    }

    fn is_executable(path: &Path) -> bool {
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }

    impl ManagedSession {
        fn snapshot(&self) -> DaemonSession {
            let mut snapshot = self
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            snapshot.archived = self.archived.load(Ordering::Relaxed);
            let visible_screen = self
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .screen()
                .contents();
            let recent = self
                .recent_output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Ok(kind) = snapshot.kind.parse::<AgentKind>() {
                let output = String::from_utf8_lossy(&recent);
                snapshot.recap = extract_recap(kind, &output);
                snapshot.attention_reason = attention_reason(kind, &visible_screen, &[]);
                snapshot.needs_attention = snapshot.attention_reason.is_some();
                snapshot.working =
                    !snapshot.needs_attention && agent_is_working(kind, &visible_screen);
            }
            snapshot
        }

        fn persist_metadata(&self) -> Result<()> {
            fs::write(
                &self.metadata_path,
                serde_json::to_vec_pretty(&self.snapshot())?,
            )?;
            Ok(())
        }

        fn resize(&self, columns: u16, rows: u16) -> Result<()> {
            self.columns.store(columns.max(20), Ordering::Relaxed);
            self.rows.store(rows.max(5), Ordering::Relaxed);
            resize_parser(
                &mut self
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                rows.max(5),
                columns.max(20),
            );
            self.master
                .lock()
                .map_err(|_| anyhow!("session PTY is poisoned"))?
                .resize(PtySize {
                    rows: rows.max(5),
                    cols: columns.max(20),
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
            Ok(())
        }

        fn write_input(&self, bytes: &[u8]) -> Result<()> {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow!("session input is poisoned"))?;
            writer.write_all(bytes)?;
            writer.flush()?;
            Ok(())
        }

        fn archive(&self) -> Result<()> {
            self.archived.store(true, Ordering::Relaxed);
            let mut child = self
                .child
                .lock()
                .map_err(|_| anyhow!("session child is poisoned"))?;
            let _ = child.kill();
            let _ = child.wait();
            drop(child);
            self.mark_dead();
            Ok(())
        }

        fn record_output(&self, bytes: &[u8]) {
            self.line_count.fetch_add(
                bytes.iter().filter(|&&byte| byte == b'\n').count(),
                Ordering::Relaxed,
            );
            self.screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .process(bytes);
            let mut recent = self
                .recent_output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            recent.extend_from_slice(bytes);
            if recent.len() > RECENT_OUTPUT_LIMIT {
                let remove = recent.len() - RECENT_OUTPUT_LIMIT;
                recent.drain(..remove);
            }
        }

        fn broadcast(&self, bytes: &[u8]) {
            let subscribers = self
                .subscribers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let mut failed = Vec::new();
            for (subscriber_id, subscriber) in subscribers {
                if write_frame(
                    &subscriber.writer,
                    &Frame::data(subscriber.stream_id, 0, bytes, true),
                )
                .is_err()
                {
                    failed.push(subscriber_id);
                }
            }
            if !failed.is_empty() {
                let mut subscribers = self
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for subscriber_id in failed {
                    subscribers.remove(&subscriber_id);
                }
            }
        }

        fn mark_dead(&self) {
            {
                let mut metadata = self
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                metadata.dead = true;
                metadata.pid = None;
            }
            let _ = self.persist_metadata();
        }

        fn read_history(
            &self,
            offset_from_bottom: usize,
            lines: usize,
        ) -> Result<(Vec<u8>, usize, usize)> {
            let total_lines = self.line_count.load(Ordering::Relaxed);
            let scrollback =
                total_lines.saturating_sub(usize::from(self.rows.load(Ordering::Relaxed)));
            let actual_offset = offset_from_bottom.min(scrollback);
            let end = total_lines.saturating_sub(actual_offset);
            let start = end.saturating_sub(lines.max(1));
            let file = File::open(&self.history_path).with_context(|| {
                format!("failed to open history {}", self.history_path.display())
            })?;
            let mut reader = BufReader::new(file);
            let mut output = Vec::new();
            let mut buffer = Vec::new();
            let mut line = 0usize;
            while line < end {
                buffer.clear();
                if reader.read_until(b'\n', &mut buffer)? == 0 {
                    break;
                }
                if line >= start {
                    output.extend_from_slice(&buffer);
                }
                line += 1;
            }
            Ok((output, total_lines, actual_offset))
        }

        fn search_history(
            &self,
            query: &str,
            max_matches: usize,
        ) -> Result<Vec<DaemonHistoryMatch>> {
            let query = query.trim().to_lowercase();
            if query.is_empty() {
                return Ok(Vec::new());
            }
            let file = File::open(&self.history_path)?;
            let mut reader = BufReader::new(file);
            let mut buffer = Vec::new();
            let mut line_number = 0usize;
            let mut matches = Vec::new();
            loop {
                buffer.clear();
                if reader.read_until(b'\n', &mut buffer)? == 0 {
                    break;
                }
                line_number += 1;
                let text = String::from_utf8_lossy(&buffer);
                if !text.to_lowercase().contains(&query) {
                    continue;
                }
                let text = text
                    .trim()
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(500)
                    .collect::<String>();
                if text.is_empty() {
                    continue;
                }
                let lower = text.to_lowercase();
                matches.push(DaemonHistoryMatch {
                    recap: lower.contains("※ recap:")
                        || lower.contains("※ recap：")
                        || lower.starts_with("recap:"),
                    line_number,
                    text,
                });
                if matches.len() > max_matches {
                    matches.remove(0);
                }
            }
            Ok(matches)
        }
    }

    fn write_chunks(
        writer: &Arc<Mutex<UnixStream>>,
        stream_id: u32,
        request_id: u64,
        bytes: &[u8],
    ) -> Result<()> {
        for chunk in bytes.chunks(DATA_CHUNK_SIZE) {
            write_frame(writer, &Frame::data(stream_id, request_id, chunk, true))?;
        }
        Ok(())
    }

    fn write_response(
        writer: &Arc<Mutex<UnixStream>>,
        request_id: u64,
        response: &DaemonResponse,
    ) -> Result<()> {
        write_frame(
            writer,
            &Frame::json(FrameKind::Response, 0, request_id, response)?,
        )
    }

    fn write_frame(writer: &Arc<Mutex<UnixStream>>, frame: &Frame) -> Result<()> {
        frame.write_to(
            &mut *writer
                .lock()
                .map_err(|_| anyhow!("daemon connection writer is poisoned"))?,
        )
    }

    pub fn bridge(paths: &DaemonPaths) -> Result<()> {
        let mut stream = connect_or_start(paths)?;
        let mut outbound = stream.try_clone()?;
        let input = thread::spawn(move || -> io::Result<()> {
            io::copy(&mut io::stdin().lock(), &mut outbound)?;
            outbound.shutdown(std::net::Shutdown::Write)
        });
        let mut stdout = io::stdout().lock();
        let mut buffer = vec![0; DATA_CHUNK_SIZE];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            stdout.write_all(&buffer[..read])?;
            stdout.flush()?;
        }
        input
            .join()
            .map_err(|_| anyhow!("muxloomd bridge input thread panicked"))??;
        Ok(())
    }

    pub fn connect_or_start(paths: &DaemonPaths) -> Result<UnixStream> {
        if let Ok(stream) = UnixStream::connect(&paths.socket) {
            return Ok(stream);
        }
        if daemon_process_alive(paths) {
            bail!("muxloomd is running but its socket is not accessible");
        }
        paths.prepare()?;
        spawn_background(paths)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match UnixStream::connect(&paths.socket) {
                Ok(stream) => return Ok(stream),
                Err(error) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                    let _ = error;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("muxloomd did not start at {}", paths.socket.display())
                    });
                }
            }
        }
    }

    fn spawn_background(paths: &DaemonPaths) -> Result<()> {
        let executable = std::env::current_exe().context("failed to find muxloomd executable")?;
        let log = open_log(&paths.log)?;
        let error_log = log.try_clone()?;
        let mut command = Command::new(executable);
        command
            .arg("serve")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().context("failed to start muxloomd")?;
        Ok(())
    }

    fn daemon_process_alive(paths: &DaemonPaths) -> bool {
        let Ok(pid) = fs::read_to_string(&paths.pid) else {
            return false;
        };
        let Ok(pid) = pid.trim().parse::<i32>() else {
            return false;
        };
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn open_log(path: &Path) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))
    }

    pub fn request_status(paths: &DaemonPaths) -> Result<DaemonResponse> {
        let mut stream = connect_or_start(paths)?;
        Frame::json(FrameKind::Request, 0, 1, &DaemonRequest::Status)?.write_to(&mut stream)?;
        loop {
            let frame = Frame::read_from(&mut stream)?.context("muxloomd closed before status")?;
            if frame.kind == FrameKind::Response && frame.request_id == 1 {
                return frame.decode_json();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn test_state(name: &str) -> Arc<DaemonState> {
            let root = std::env::temp_dir().join(format!(
                "muxloomd-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            let paths = DaemonPaths::under(root);
            paths.prepare().unwrap();
            Arc::new(DaemonState::new(paths))
        }

        #[test]
        fn one_socket_multiplexes_out_of_order_requests_and_chunked_shell_output() {
            let (mut client, server) = UnixStream::pair().unwrap();
            let state = test_state("multiplex");
            let handle = thread::spawn(move || serve_client(server, state));

            Frame::json(FrameKind::Request, 0, 10, &DaemonRequest::Ping)
                .unwrap()
                .write_to(&mut client)
                .unwrap();
            Frame::json(
                FrameKind::Request,
                0,
                11,
                &DaemonRequest::RunShell {
                    script: "printf shell-output; printf shell-error >&2; exit 7".into(),
                    environment: vec![],
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut ping = false;
            let mut exit = None;
            while !ping || exit.is_none() {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Data && frame.request_id == 11 {
                    match frame.stream_id {
                        stream::STDOUT => stdout.extend(frame.decoded_payload().unwrap()),
                        stream::STDERR => stderr.extend(frame.decoded_payload().unwrap()),
                        _ => panic!("unexpected stream"),
                    }
                } else if frame.kind == FrameKind::Response {
                    match frame.decode_json::<DaemonResponse>().unwrap() {
                        DaemonResponse::Pong { .. } => ping = true,
                        DaemonResponse::ShellComplete { exit_code } => exit = Some(exit_code),
                        response => panic!("unexpected response {response:?}"),
                    }
                }
            }
            assert_eq!(stdout, b"shell-output");
            assert_eq!(stderr, b"shell-error");
            assert_eq!(exit, Some(7));
            drop(client);
            handle.join().unwrap().unwrap();
        }

        #[test]
        fn visible_pty_screen_drives_agent_working_state() {
            let state = test_state("visible-working");
            let session = launch_session(
                &state,
                "muxloomd-codex-visible-working".into(),
                "codex".into(),
                "/tmp".into(),
                "visible working state".into(),
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '\\033[2J\\033[H• Working (2s • esc to interrupt)'; sleep 1".into(),
                ],
                vec![],
                1,
                80,
                24,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while !session.snapshot().working && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(session.snapshot().working);
            session.archive().unwrap();
        }

        #[test]
        fn daemon_owns_pty_process_and_streams_input_output_without_tmux() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let state = test_state("pty");
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-terminal-test";
            Frame::json(
                FrameKind::Request,
                0,
                20,
                &DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: "terminal".into(),
                    path: "/tmp".into(),
                    label: "cat".into(),
                    executable: "/bin/cat".into(),
                    args: vec![],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 20 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    break;
                }
            }
            Frame::json(
                FrameKind::OpenStream,
                stream::PTY_BASE,
                21,
                &OpenStream::Pty {
                    session_id: session_id.into(),
                    columns: 80,
                    rows: 24,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::OpenStream {
                    break;
                }
            }
            Frame::data(stream::PTY_BASE, 0, b"daemon-pty-ok\n", false)
                .write_to(&mut client)
                .unwrap();
            let mut output = Vec::new();
            while !String::from_utf8_lossy(&output).contains("daemon-pty-ok") {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE {
                    output.extend(frame.decoded_payload().unwrap());
                }
            }
            Frame::json(
                FrameKind::Request,
                0,
                22,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 22 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
        }

        #[test]
        fn file_streams_are_compressed_flow_controlled_and_bidirectional() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let state = test_state("files");
            let root = state.paths.root.clone();
            let source = root.join("source.bin");
            let source_bytes = vec![b'z'; 2 * 1024 * 1024];
            fs::write(&source, &source_bytes).unwrap();
            let handle = thread::spawn(move || serve_client(server, state));

            Frame::json(
                FrameKind::OpenStream,
                stream::FILE_BASE,
                30,
                &OpenStream::File {
                    path: source.to_string_lossy().into_owned(),
                    offset: 0,
                    length: None,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            let mut downloaded = Vec::new();
            let mut saw_compressed = false;
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                match frame.kind {
                    FrameKind::Data if frame.stream_id == stream::FILE_BASE => {
                        saw_compressed |= frame.flags != 0;
                        let payload = frame.decoded_payload().unwrap();
                        downloaded.extend_from_slice(&payload);
                        Frame::window_update(frame.stream_id, payload.len() as u32)
                            .write_to(&mut client)
                            .unwrap();
                    }
                    FrameKind::CloseStream if frame.stream_id == stream::FILE_BASE => break,
                    _ => {}
                }
            }
            assert!(saw_compressed);
            assert_eq!(downloaded, source_bytes);

            let destination = root.join("uploaded.txt");
            let upload = vec![b'u'; 128 * 1024];
            Frame::json(
                FrameKind::OpenStream,
                stream::FILE_BASE + 1,
                31,
                &OpenStream::Upload {
                    path: destination.to_string_lossy().into_owned(),
                    size: upload.len() as u64,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            Frame::data(stream::FILE_BASE + 1, 0, &upload, true)
                .write_to(&mut client)
                .unwrap();
            Frame::new(FrameKind::CloseStream, stream::FILE_BASE + 1, 0, vec![])
                .write_to(&mut client)
                .unwrap();
            Frame::json(
                FrameKind::Request,
                0,
                32,
                &DaemonRequest::ListFiles {
                    path: root.to_string_lossy().into_owned(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 32 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Files { .. }
                    ));
                    break;
                }
            }
            assert_eq!(fs::read(destination).unwrap(), upload);
            drop(client);
            handle.join().unwrap().unwrap();
        }
    }
}

#[cfg(unix)]
pub use platform::*;

#[cfg(not(unix))]
mod unsupported {
    use anyhow::{Result, bail};

    #[derive(Debug, Clone)]
    pub struct DaemonPaths;

    impl DaemonPaths {
        pub fn discover() -> Result<Self> {
            bail!("muxloomd is currently supported on Unix targets")
        }
    }

    pub fn serve(_: &DaemonPaths) -> Result<()> {
        bail!("muxloomd is currently supported on Unix targets")
    }

    pub fn bridge(_: &DaemonPaths) -> Result<()> {
        bail!("muxloomd is currently supported on Unix targets")
    }

    pub fn request_status(_: &DaemonPaths) -> Result<crate::daemon_protocol::DaemonResponse> {
        bail!("muxloomd is currently supported on Unix targets")
    }
}

#[cfg(not(unix))]
pub use unsupported::*;
