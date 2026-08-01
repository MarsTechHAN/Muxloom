#[cfg(unix)]
mod platform {
    use std::{
        collections::{BTreeSet, HashMap},
        fs::{self, File, OpenOptions},
        io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
        net::{Shutdown, TcpStream},
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
            process::ExitStatusExt,
        },
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::{
            Arc, Condvar, Mutex, OnceLock,
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
        terminal_session::{
            CodexActivity, render_history_rows, render_scrollback_seed, resize_parser,
        },
    };

    const RECENT_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
    /// The least of a session's log to render when seeding a client's
    /// scrollback. Enough on its own for an agent that writes its transcript
    /// out plainly, and cheap enough to read whether or not it is.
    const SCROLLBACK_SEED_BYTES_MIN: u64 = 16 * 1024 * 1024;
    /// The most of it to render. How much output a session spends per finished
    /// line varies by three orders of magnitude between agents, so the window is
    /// measured out in rows below and this only stops a log that has been
    /// growing for days from being read back to its beginning: the render costs
    /// roughly two seconds here, and an attach waits for it.
    const SCROLLBACK_SEED_BYTES_MAX: u64 = 128 * 1024 * 1024;
    static METADATA_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Clone)]
    pub struct DaemonPaths {
        pub root: PathBuf,
        pub socket: PathBuf,
        pub pid: PathBuf,
        pub log: PathBuf,
        pub generation: PathBuf,
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
                generation: root.join("muxloomd.generation"),
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
        client_gate: Mutex<()>,
        draining: AtomicBool,
        shutdown: AtomicBool,
        next_subscriber: AtomicU64,
        sessions: Mutex<HashMap<String, Arc<ManagedSession>>>,
        persisted_sessions: Mutex<HashMap<String, Arc<PersistedSession>>>,
        paths: DaemonPaths,
    }

    struct PersistedSession {
        metadata: Mutex<DaemonSession>,
        history_path: PathBuf,
        metadata_path: PathBuf,
        line_count: OnceLock<usize>,
        columns: u16,
        rows: u16,
    }

    struct ManagedSession {
        metadata: Mutex<DaemonSession>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        writer: Mutex<Box<dyn Write + Send>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        subscribers: Mutex<HashMap<u64, Subscriber>>,
        screen: Mutex<vt100::Parser>,
        codex_activity: Mutex<CodexActivity>,
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
        Tcp {
            socket: TcpStream,
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
            let persisted_sessions = load_persisted_sessions(&paths);
            Self {
                started: Instant::now(),
                clients: AtomicUsize::new(0),
                client_gate: Mutex::new(()),
                draining: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                next_subscriber: AtomicU64::new(1),
                sessions: Mutex::new(HashMap::new()),
                persisted_sessions: Mutex::new(persisted_sessions),
                paths,
            }
        }
    }

    fn load_persisted_sessions(paths: &DaemonPaths) -> HashMap<String, Arc<PersistedSession>> {
        let mut sessions = HashMap::new();
        let entries = match fs::read_dir(&paths.sessions) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!(
                    "muxloomd could not read persisted sessions {}: {error}",
                    paths.sessions.display()
                );
                return sessions;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            let loaded = (|| -> Result<(String, Arc<PersistedSession>)> {
                let mut metadata: DaemonSession = serde_json::from_slice(&fs::read(&path)?)?;
                validate_session_id(&metadata.id)?;
                if path.file_stem().and_then(|value| value.to_str()) != Some(metadata.id.as_str()) {
                    bail!(
                        "metadata filename does not match session id {}",
                        metadata.id
                    );
                }
                if metadata.temporary {
                    let _ = fs::remove_file(paths.history.join(format!("{}.ansi", metadata.id)));
                    let _ = fs::remove_file(&path);
                    bail!("discarded stale temporary session");
                }
                if !metadata.dead && !metadata.archived {
                    bail!("metadata still describes a live PTY");
                }
                metadata.dead = true;
                metadata.pid = None;
                metadata.working = false;
                metadata.needs_attention = false;
                metadata.attention_reason = None;
                let id = metadata.id.clone();
                Ok((
                    id.clone(),
                    Arc::new(PersistedSession {
                        metadata: Mutex::new(metadata),
                        history_path: paths.history.join(format!("{id}.ansi")),
                        metadata_path: path.clone(),
                        line_count: OnceLock::new(),
                        columns: 80,
                        rows: 24,
                    }),
                ))
            })();
            match loaded {
                Ok((id, session)) => {
                    sessions.insert(id, session);
                }
                Err(error) => eprintln!(
                    "muxloomd ignored persisted session {}: {error:#}",
                    path.display()
                ),
            }
        }
        sessions
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
        fs::write(&paths.generation, format!("{}\n", current_generation()))?;
        let _guard = SocketGuard {
            socket: paths.socket.clone(),
            pid: paths.pid.clone(),
        };
        let state = Arc::new(DaemonState::new(paths.clone()));
        listener.set_nonblocking(true)?;
        while !state.shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    let state = Arc::clone(&state);
                    thread::spawn(move || {
                        if let Err(error) = serve_client(stream, state) {
                            eprintln!("muxloomd client closed: {error:#}");
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
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
        {
            let _registration = state
                .client_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.draining.load(Ordering::Acquire) {
                return Ok(());
            }
            state.clients.fetch_add(1, Ordering::Relaxed);
        }
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
                            scrollback_rows,
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
                            // The retained output only repaints the screen. The
                            // history above it is rendered here so the client
                            // starts with scrollback the raw ring never held.
                            // Rendering takes long enough that the session can
                            // write during it, so the retained output is taken
                            // afterwards: it then covers everything the render
                            // was too early to see.
                            let seed = session
                                .scrollback_seed(
                                    columns,
                                    rows,
                                    RECENT_OUTPUT_LIMIT,
                                    scrollback_rows,
                                )
                                .unwrap_or_else(|error| {
                                    eprintln!("muxloomd scrollback seed failed: {error}");
                                    Vec::new()
                                });
                            let recent = session
                                .recent_output
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            for chunk in seed.chunks(DATA_CHUNK_SIZE) {
                                write_frame(
                                    &writer,
                                    &Frame::data(frame.stream_id, 0, chunk, true),
                                )?;
                            }
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
                        OpenStream::Tcp { host, port } => {
                            match TcpStream::connect((host.as_str(), port)) {
                                Ok(socket) => {
                                    socket.set_nodelay(true)?;
                                    let reader = socket.try_clone()?;
                                    subscriptions
                                        .insert(frame.stream_id, ClientStream::Tcp { socket });
                                    write_stream_opened(&writer, &frame, None)?;
                                    flow.open(frame.stream_id);
                                    let writer = Arc::clone(&writer);
                                    let flow = Arc::clone(&flow);
                                    let stream_id = frame.stream_id;
                                    thread::spawn(move || {
                                        if let Err(error) =
                                            stream_tcp(&writer, &flow, stream_id, reader)
                                        {
                                            eprintln!(
                                                "muxloomd TCP stream {stream_id} failed: {error:#}"
                                            );
                                        }
                                        flow.close(stream_id);
                                    });
                                }
                                Err(error) => write_stream_error(
                                    &writer,
                                    &frame,
                                    format!("cannot connect to {host}:{port}: {error}"),
                                )?,
                            }
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
                                ClientStream::Tcp { socket } => socket.write_all(&payload)?,
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

    fn write_stream_error(
        writer: &Arc<Mutex<UnixStream>>,
        frame: &Frame,
        message: String,
    ) -> Result<()> {
        write_frame(
            writer,
            &Frame::json(
                FrameKind::Error,
                frame.stream_id,
                frame.request_id,
                &DaemonResponse::Error { message },
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

    fn stream_tcp(
        writer: &Arc<Mutex<UnixStream>>,
        flow: &StreamFlow,
        stream_id: u32,
        mut socket: TcpStream,
    ) -> Result<()> {
        let mut buffer = vec![0; DATA_CHUNK_SIZE];
        loop {
            let read = socket.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            flow.consume(stream_id, read)?;
            write_frame(writer, &Frame::data(stream_id, 0, &buffer[..read], false))?;
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
            ClientStream::Tcp { socket } => {
                let _ = socket.shutdown(Shutdown::Both);
                Ok(())
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
            ClientStream::Tcp { socket } => {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }

    fn handle_request(
        writer: &Arc<Mutex<UnixStream>>,
        state: &Arc<DaemonState>,
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
                            "tcp-forward-v1".into(),
                            "tcp-listeners-v1".into(),
                            "handover-drain-v1".into(),
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
            DaemonRequest::PrepareHandover => {
                let ready = prepare_handover(state);
                write_response(
                    writer,
                    request_id,
                    if ready {
                        &DaemonResponse::HandoverReady
                    } else {
                        &DaemonResponse::HandoverDeferred
                    },
                )?;
                if ready {
                    state.shutdown.store(true, Ordering::Release);
                }
                Ok(())
            }
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
            DaemonRequest::ListTcpListeners => write_response(
                writer,
                request_id,
                &DaemonResponse::TcpListeners {
                    ports: tcp_listener_ports()?,
                },
            ),
            DaemonRequest::ListSessions => {
                let mut sessions: Vec<_> = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .map(|session| session.snapshot())
                    .collect();
                sessions.extend(
                    state
                        .persisted_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .values()
                        .map(|session| session.snapshot()),
                );
                write_response(writer, request_id, &DaemonResponse::Sessions { sessions })
            }
            DaemonRequest::Launch {
                session_id,
                kind,
                path,
                label,
                temporary,
                executable,
                args,
                environment,
                created_at,
                columns,
                rows,
            } => {
                let _launch_guard = state
                    .client_gate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.draining.load(Ordering::Acquire) {
                    bail!("muxloomd is draining for a generation handover");
                }
                let session = launch_session(
                    state,
                    session_id,
                    kind,
                    path,
                    label,
                    temporary,
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
                rendered,
            } => {
                let (history, total_lines, actual_offset, columns, rows) =
                    if let Ok(session) = daemon_session(state, &session_id) {
                        let (history, total_lines, actual_offset) =
                            session.read_history(offset_from_bottom, lines, rendered)?;
                        (
                            history,
                            total_lines,
                            actual_offset,
                            session.columns.load(Ordering::Relaxed),
                            session.rows.load(Ordering::Relaxed),
                        )
                    } else {
                        let session = persisted_session(state, &session_id)?;
                        let (history, total_lines, actual_offset) =
                            session.read_history(offset_from_bottom, lines, rendered)?;
                        (
                            history,
                            total_lines,
                            actual_offset,
                            session.columns,
                            session.rows,
                        )
                    };
                write_chunks(writer, stream::HISTORY, request_id, &history)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::HistoryComplete {
                        total_lines,
                        columns,
                        rows,
                        offset_from_bottom: actual_offset,
                        rendered,
                    },
                )
            }
            DaemonRequest::SearchHistory {
                session_id,
                query,
                max_matches,
            } => {
                let matches = if let Ok(session) = daemon_session(state, &session_id) {
                    session.search_history(&query, max_matches.clamp(1, 50))?
                } else {
                    persisted_session(state, &session_id)?
                        .search_history(&query, max_matches.clamp(1, 50))?
                };
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
                if let Ok(session) = daemon_session(state, &session_id) {
                    session.archive()?;
                } else {
                    persisted_session(state, &session_id)?.archive()?;
                }
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::Delete { session_id } => {
                let live = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&session_id);
                if let Some(session) = live {
                    session.stop()?;
                    let _ = fs::remove_file(&session.history_path);
                    let _ = fs::remove_file(&session.metadata_path);
                } else {
                    let session = state
                        .persisted_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session_id)
                        .with_context(|| format!("unknown daemon session {session_id}"))?;
                    let _ = fs::remove_file(&session.history_path);
                    let _ = fs::remove_file(&session.metadata_path);
                }
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

    fn prepare_handover(state: &DaemonState) -> bool {
        let _registration = state
            .client_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.draining.load(Ordering::Acquire) || state.clients.load(Ordering::Acquire) != 1 {
            return false;
        }
        let no_live_sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .all(|session| {
                let snapshot = session.snapshot();
                snapshot.dead || snapshot.archived
            });
        if no_live_sessions {
            state.draining.store(true, Ordering::Release);
        }
        no_live_sessions
    }

    /// Resolve a program name to an absolute path using `path_env`.
    ///
    /// The agent binary must come from the *launch* environment (PATH), never
    /// from the working directory being opened. portable-pty resolves a
    /// *relative* command against the spawn cwd before PATH, and accepts any
    /// existing filesystem entry — including a directory or non-executable file
    /// — as the target, so a `claude` entry inside the working directory would
    /// otherwise shadow the real CLI. Resolving to an absolute path here forces
    /// the intended binary regardless of the working directory contents.
    ///
    /// Returns:
    /// - the name unchanged if it already contains a path separator (an explicit
    ///   absolute/relative path the caller asked for), or
    /// - the absolute path of the first executable match on `path_env`, or
    /// - `None` for a bare name that is not found on PATH — the caller must then
    ///   refuse to launch rather than let portable-pty fall back to the cwd.
    fn resolve_executable_on_path(
        executable: &str,
        path_env: Option<&std::ffi::OsStr>,
    ) -> Option<std::ffi::OsString> {
        if executable.contains('/') {
            return Some(executable.into());
        }
        let path_env = path_env?;
        for dir in std::env::split_paths(path_env) {
            // An empty PATH entry means "current directory"; skipping it is what
            // keeps the working directory from shadowing the real executable.
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(executable);
            if is_executable_file(&candidate) {
                return Some(candidate.into_os_string());
            }
        }
        None
    }

    /// True if `path` resolves (following symlinks) to a regular file with at
    /// least one execute bit set.
    fn is_executable_file(path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn launch_session(
        state: &Arc<DaemonState>,
        session_id: String,
        kind: String,
        path: String,
        label: String,
        temporary: bool,
        executable: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
        columns: u16,
        rows: u16,
    ) -> Result<Arc<ManagedSession>> {
        validate_session_id(&session_id)?;
        let path = if path == "~" {
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        } else {
            path
        };
        if state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&session_id)
            || state
                .persisted_sessions
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
        // Work out the PATH the child will run with. portable-pty resolves a
        // *relative* program name (a bare `claude`) against the spawn cwd
        // *before* consulting PATH, and treats any existing entry there — even a
        // directory or a non-executable file — as a match. So a `claude` entry
        // inside the working directory would shadow the real CLI on PATH.
        // Resolve to an absolute path up front so the launch always uses the
        // intended binary regardless of what the working directory contains.
        let path_overridden = environment.iter().any(|(name, _)| name == "PATH");
        let prepended_path = if path_overridden {
            None
        } else if let (Some(home), Some(path)) =
            (std::env::var_os("HOME"), std::env::var_os("PATH"))
        {
            let mut paths = vec![PathBuf::from(home).join(".local/bin")];
            paths.extend(std::env::split_paths(&path));
            std::env::join_paths(paths).ok()
        } else {
            None
        };
        let child_path = if path_overridden {
            environment
                .iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| std::ffi::OsString::from(value))
        } else {
            prepended_path.clone()
        };
        let program =
            resolve_executable_on_path(&executable, child_path.as_deref()).with_context(|| {
                format!(
                    "cannot launch '{executable}': not found on PATH; \
                     refusing to fall back to a same-named entry inside {path}"
                )
            })?;
        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(5),
            cols: columns.max(20),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(&program);
        command.args(args);
        command.cwd(path.clone());
        for (name, value) in environment {
            command.env(name, value);
        }
        if let Some(prepended_path) = prepended_path {
            command.env("PATH", prepended_path);
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
        if !temporary {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&history_path)?;
        }
        let metadata = DaemonSession {
            id: session_id.clone(),
            kind,
            path,
            label,
            temporary,
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
            codex_activity: Mutex::new(CodexActivity::default()),
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
        let reader_state = Arc::clone(state);
        thread::spawn(move || {
            let mut history = if managed.temporary() {
                None
            } else {
                match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&managed.history_path)
                {
                    Ok(history) => Some(history),
                    Err(error) => {
                        eprintln!("muxloomd history open failed: {error}");
                        managed.mark_dead();
                        return;
                    }
                }
            };
            let mut buffer = vec![0; DATA_CHUNK_SIZE];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let bytes = &buffer[..read];
                        if let Some(history) = history.as_mut() {
                            let _ = history.write_all(bytes);
                        }
                        managed.record_output(bytes);
                        managed.broadcast(bytes);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if let Some(history) = history.as_mut() {
                let _ = history.flush();
            }
            if managed.temporary() {
                reader_state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&managed.session_id());
                let _ = fs::remove_file(&managed.history_path);
                let _ = fs::remove_file(&managed.metadata_path);
            } else {
                managed.mark_dead();
            }
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

    fn persisted_session(state: &DaemonState, session_id: &str) -> Result<Arc<PersistedSession>> {
        state
            .persisted_sessions
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
            .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
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
            let kind = if file_type.is_symlink() {
                FileEntryKind::Symlink
            } else if file_type.is_dir() {
                FileEntryKind::Directory
            } else if file_type.is_file() {
                FileEntryKind::File
            } else {
                FileEntryKind::Other
            };
            let metadata = entry.metadata().ok();
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path().to_string_lossy().into_owned(),
                kind,
                size: if file_type.is_file() {
                    metadata.as_ref().map_or(0, |metadata| metadata.len())
                } else {
                    0
                },
                mtime: metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |since_epoch| since_epoch.as_secs()),
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
        } else if matches_extension(
            &lower,
            &[
                "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "pnm", "pbm",
                "pgm", "ppm", "qoi",
            ],
        ) || looks_like_image(&bytes)
        {
            FilePreviewKind::Image
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
            FilePreviewKind::Image => "image/*",
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

    fn looks_like_image(bytes: &[u8]) -> bool {
        bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            || bytes.starts_with(b"\xff\xd8\xff")
            || bytes.starts_with(b"GIF87a")
            || bytes.starts_with(b"GIF89a")
            || bytes.starts_with(b"BM")
            || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
            || bytes.starts_with(b"II*\0")
            || bytes.starts_with(b"MM\0*")
            || bytes.starts_with(b"qoif")
            || matches!(
                bytes.get(..2),
                Some(b"P1") | Some(b"P2") | Some(b"P3") | Some(b"P4") | Some(b"P5") | Some(b"P6")
            )
    }

    fn is_executable(path: &Path) -> bool {
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }

    impl PersistedSession {
        fn snapshot(&self) -> DaemonSession {
            self.metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn archive(&self) -> Result<()> {
            let metadata = {
                let mut metadata = self
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if metadata.temporary {
                    bail!("temporary sessions cannot be archived");
                }
                metadata.archived = true;
                metadata.dead = true;
                metadata.pid = None;
                metadata.working = false;
                metadata.needs_attention = false;
                metadata.attention_reason = None;
                metadata.clone()
            };
            persist_session_metadata(&self.metadata_path, &metadata)
        }

        fn line_count(&self) -> Result<usize> {
            if let Some(count) = self.line_count.get() {
                return Ok(*count);
            }
            let count = count_history_lines(&self.history_path)?;
            let _ = self.line_count.set(count);
            Ok(count)
        }

        fn read_history(
            &self,
            offset_from_bottom: usize,
            lines: usize,
            rendered: bool,
        ) -> Result<(Vec<u8>, usize, usize)> {
            if self.snapshot().temporary {
                return Ok((Vec::new(), 0, 0));
            }
            if rendered {
                return render_history_file(
                    &self.history_path,
                    self.columns,
                    self.rows,
                    offset_from_bottom,
                    lines,
                    SCROLLBACK_SEED_BYTES_MIN,
                    SCROLLBACK_SEED_BYTES_MAX,
                );
            }
            read_history_file(
                &self.history_path,
                self.line_count()?,
                self.rows,
                offset_from_bottom,
                lines,
            )
        }

        fn search_history(
            &self,
            query: &str,
            max_matches: usize,
        ) -> Result<Vec<DaemonHistoryMatch>> {
            if self.snapshot().temporary {
                return Ok(Vec::new());
            }
            search_history_file(&self.history_path, query, max_matches)
        }
    }

    impl ManagedSession {
        fn session_id(&self) -> String {
            self.metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .id
                .clone()
        }

        fn temporary(&self) -> bool {
            self.metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .temporary
        }

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
            let recent = {
                let recent = self
                    .recent_output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                String::from_utf8_lossy(&recent).into_owned()
            };
            if let Ok(kind) = snapshot.kind.parse::<AgentKind>() {
                snapshot.recap = extract_recap(kind, &recent);
                if snapshot.dead || snapshot.archived {
                    snapshot.pid = None;
                    snapshot.working = false;
                    snapshot.needs_attention = false;
                    snapshot.attention_reason = None;
                } else {
                    snapshot.attention_reason = attention_reason(kind, &visible_screen, &[]);
                    snapshot.needs_attention = snapshot.attention_reason.is_some();
                    let working_hint = self
                        .codex_activity
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .working();
                    snapshot.working = !snapshot.needs_attention
                        && if kind == AgentKind::Codex {
                            working_hint.unwrap_or_else(|| agent_is_working(kind, &visible_screen))
                        } else {
                            agent_is_working(kind, &visible_screen)
                        };
                }
            }
            snapshot
        }

        fn persist_metadata(&self) -> Result<()> {
            persist_session_metadata(&self.metadata_path, &self.snapshot())
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
            if self.temporary() {
                bail!("temporary sessions cannot be archived");
            }
            self.archived.store(true, Ordering::Relaxed);
            self.stop()?;
            self.mark_dead();
            Ok(())
        }

        fn stop(&self) -> Result<()> {
            let mut child = self
                .child
                .lock()
                .map_err(|_| anyhow!("session child is poisoned"))?;
            let _ = child.kill();
            let _ = child.wait();
            Ok(())
        }

        /// Render the history that sits above the retained output into rows an
        /// attaching client can replay into its scrollback.
        ///
        /// `retained` is how many trailing bytes the client replays for itself;
        /// rendering stops that far short of the log's end so the rows meet that
        /// replay instead of repeating what it is about to redraw.
        /// Only the tail of the log is read, and how much of it is measured out
        /// in the rows asked for rather than in bytes: a redraw-heavy agent
        /// spends tens of kilobytes on the frames around one finished line, so a
        /// window that hands one agent its whole session leaves another with a
        /// screenful.
        fn scrollback_seed(
            &self,
            columns: u16,
            rows: u16,
            retained: usize,
            keep: usize,
        ) -> Result<Vec<u8>> {
            if keep == 0 || self.temporary() {
                return Ok(Vec::new());
            }
            let mut file = match File::open(&self.history_path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to open history {}", self.history_path.display())
                    });
                }
            };
            let end = file
                .metadata()?
                .len()
                .saturating_sub(retained.try_into().unwrap_or(u64::MAX));
            let start = seed_start(
                &mut file,
                end,
                keep,
                SCROLLBACK_SEED_BYTES_MIN,
                SCROLLBACK_SEED_BYTES_MAX,
            )
            .with_context(|| format!("failed to scan history {}", self.history_path.display()))?;
            if end == start {
                return Ok(Vec::new());
            }
            file.seek(SeekFrom::Start(start))?;
            render_scrollback_seed(BufReader::new(file).take(end - start), columns, rows, keep)
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
            self.codex_activity
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
            rendered: bool,
        ) -> Result<(Vec<u8>, usize, usize)> {
            if self.temporary() {
                return Ok((Vec::new(), 0, 0));
            }
            let rows = self.rows.load(Ordering::Relaxed);
            if rendered {
                return render_history_file(
                    &self.history_path,
                    self.columns.load(Ordering::Relaxed),
                    rows,
                    offset_from_bottom,
                    lines,
                    SCROLLBACK_SEED_BYTES_MIN,
                    SCROLLBACK_SEED_BYTES_MAX,
                );
            }
            read_history_file(
                &self.history_path,
                self.line_count.load(Ordering::Relaxed),
                rows,
                offset_from_bottom,
                lines,
            )
        }

        fn search_history(
            &self,
            query: &str,
            max_matches: usize,
        ) -> Result<Vec<DaemonHistoryMatch>> {
            if self.temporary() {
                return Ok(Vec::new());
            }
            search_history_file(&self.history_path, query, max_matches)
        }
    }

    fn persist_session_metadata(path: &Path, metadata: &DaemonSession) -> Result<()> {
        let nonce = METADATA_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));
        let result = (|| -> Result<()> {
            fs::write(&temporary, serde_json::to_vec_pretty(metadata)?)?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn count_history_lines(path: &Path) -> Result<usize> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open history {}", path.display()))?;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut lines = 0usize;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            lines =
                lines.saturating_add(buffer[..read].iter().filter(|&&byte| byte == b'\n').count());
        }
        Ok(lines)
    }

    /// Where in a session's log to start rendering to reach `keep` rows of
    /// scrollback, given that the render stops at `end`.
    ///
    /// A row only leaves the screen when something scrolls it off, and almost
    /// everything that does costs a newline byte, so counting them backwards
    /// says where the rows cannot be any further back than. It is a floor and
    /// not an answer: an agent redrawing a pane writes newlines that scroll
    /// nothing, so the count is taken with room to spare and clamped to a window
    /// that is neither pointlessly small (`least`) nor unbounded (`most`).
    fn seed_start(file: &mut File, end: u64, keep: usize, least: u64, most: u64) -> Result<u64> {
        let floor = end.saturating_sub(most);
        let ceiling = end.saturating_sub(least);
        if ceiling == 0 {
            return Ok(0);
        }
        let wanted = keep.saturating_mul(3) / 2;
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut cursor = end;
        let mut newlines = 0usize;
        while cursor > floor && newlines < wanted {
            let step = (cursor - floor).min(buffer.len() as u64);
            cursor -= step;
            file.seek(SeekFrom::Start(cursor))?;
            let window = &mut buffer[..step as usize];
            file.read_exact(window)?;
            newlines =
                newlines.saturating_add(window.iter().filter(|&&byte| byte == b'\n').count());
        }
        if newlines < wanted && cursor > 0 {
            eprintln!(
                "muxloomd scrollback seed stopped {} MiB back with {newlines} lines of the {wanted} it looks for",
                most / (1024 * 1024)
            );
        }
        Ok(cursor.min(ceiling))
    }

    /// Read a session's history back as rendered rows rather than raw log
    /// lines, by replaying the tail of the log through an emulator.
    ///
    /// How far back a window of bytes reaches in rows depends entirely on what
    /// the agent writes — a full-screen redraw costs a screenful of bytes and
    /// moves the history along by nothing — so the render starts at a window of
    /// `least` bytes and widens until it reaches the rows that were asked for,
    /// it has read the log from the start, or it has read `most`.
    fn render_history_file(
        path: &Path,
        columns: u16,
        rows: u16,
        offset_from_bottom: usize,
        lines: usize,
        least: u64,
        most: u64,
    ) -> Result<(Vec<u8>, usize, usize)> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open history {}", path.display()))?;
        let end = file.metadata()?.len();
        let mut window = least.max(1);
        loop {
            let start = end.saturating_sub(window);
            file.seek(SeekFrom::Start(start))?;
            let page = render_history_rows(
                BufReader::new(&mut file).take(end - start),
                columns,
                rows,
                offset_from_bottom,
                lines,
            )?;
            if page.2 >= offset_from_bottom || start == 0 || window >= most {
                return Ok(page);
            }
            window = window.saturating_mul(4).min(most);
        }
    }

    fn read_history_file(
        path: &Path,
        total_lines: usize,
        rows: u16,
        offset_from_bottom: usize,
        lines: usize,
    ) -> Result<(Vec<u8>, usize, usize)> {
        let scrollback = total_lines.saturating_sub(usize::from(rows));
        let actual_offset = offset_from_bottom.min(scrollback);
        let end = total_lines.saturating_sub(actual_offset);
        let start = end.saturating_sub(lines.max(1));
        let file = File::open(path)
            .with_context(|| format!("failed to open history {}", path.display()))?;
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

    fn search_history_file(
        path: &Path,
        query: &str,
        max_matches: usize,
    ) -> Result<Vec<DaemonHistoryMatch>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let file = File::open(path)?;
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

    fn tcp_listener_ports() -> Result<Vec<u16>> {
        let ports = BTreeSet::new();
        #[cfg(target_os = "linux")]
        let mut ports = ports;
        #[cfg(target_os = "linux")]
        for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let table = match fs::read_to_string(path) {
                Ok(table) => table,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to read TCP listeners from {path}"));
                }
            };
            collect_linux_tcp_listeners(&table, &mut ports);
        }
        Ok(ports.into_iter().collect())
    }

    #[cfg(target_os = "linux")]
    fn collect_linux_tcp_listeners(table: &str, ports: &mut BTreeSet<u16>) {
        for line in table.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let _slot = fields.next();
            let Some(local_address) = fields.next() else {
                continue;
            };
            let _remote_address = fields.next();
            if fields.next() != Some("0A") {
                continue;
            }
            let Some((_, port)) = local_address.rsplit_once(':') else {
                continue;
            };
            if let Ok(port) = u16::from_str_radix(port, 16)
                && port >= 1024
            {
                ports.insert(port);
            }
        }
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
        if let Ok(mut stream) = UnixStream::connect(&paths.socket) {
            if running_generation_is_current(paths) {
                return Ok(stream);
            }
            match prepare_atomic_handover(&mut stream)? {
                Some(false) => return Ok(stream),
                Some(true) => {
                    drop(stream);
                    wait_for_daemon_stop(paths)?;
                }
                None => {
                    if !daemon_is_idle_for_handover(&mut stream).unwrap_or(false) {
                        return Ok(stream);
                    }
                    drop(stream);
                    stop_idle_daemon(paths)?;
                }
            }
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

    fn current_generation() -> String {
        format!(
            "{}:protocol-{}:{}",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION,
            option_env!("MUXLOOM_BUILD_ID").unwrap_or("local")
        )
    }

    fn running_generation_is_current(paths: &DaemonPaths) -> bool {
        fs::read_to_string(&paths.generation)
            .is_ok_and(|generation| generation.trim() == current_generation())
    }

    fn prepare_atomic_handover(stream: &mut UnixStream) -> Result<Option<bool>> {
        const REQUEST_ID: u64 = u64::MAX - 3;
        Frame::json(
            FrameKind::Request,
            0,
            REQUEST_ID,
            &DaemonRequest::PrepareHandover,
        )?
        .write_to(stream)?;
        loop {
            let frame =
                Frame::read_from(stream)?.context("daemon closed during handover request")?;
            if frame.kind != FrameKind::Response || frame.request_id != REQUEST_ID {
                continue;
            }
            return Ok(match frame.decode_json::<DaemonResponse>()? {
                DaemonResponse::HandoverReady => Some(true),
                DaemonResponse::HandoverDeferred => Some(false),
                DaemonResponse::Error { .. } => None,
                response => bail!("unexpected handover response: {response:?}"),
            });
        }
    }

    fn daemon_is_idle_for_handover(stream: &mut UnixStream) -> Result<bool> {
        const STATUS_REQUEST: u64 = u64::MAX - 1;
        const SESSIONS_REQUEST: u64 = u64::MAX - 2;
        Frame::json(
            FrameKind::Request,
            0,
            STATUS_REQUEST,
            &DaemonRequest::Status,
        )?
        .write_to(stream)?;
        Frame::json(
            FrameKind::Request,
            0,
            SESSIONS_REQUEST,
            &DaemonRequest::ListSessions,
        )?
        .write_to(stream)?;
        let mut sole_client = None;
        let mut no_live_sessions = None;
        while sole_client.is_none() || no_live_sessions.is_none() {
            let frame = Frame::read_from(stream)?.context("daemon closed during handover probe")?;
            if frame.kind != FrameKind::Response {
                continue;
            }
            match frame.request_id {
                STATUS_REQUEST => match frame.decode_json::<DaemonResponse>()? {
                    DaemonResponse::Status { clients, .. } => sole_client = Some(clients <= 1),
                    DaemonResponse::Error { message } => bail!(message),
                    response => bail!("unexpected handover status response: {response:?}"),
                },
                SESSIONS_REQUEST => match frame.decode_json::<DaemonResponse>()? {
                    DaemonResponse::Sessions { sessions } => {
                        no_live_sessions = Some(
                            sessions
                                .iter()
                                .all(|session| session.dead || session.archived),
                        )
                    }
                    DaemonResponse::Error { message } => bail!(message),
                    response => bail!("unexpected handover sessions response: {response:?}"),
                },
                _ => {}
            }
        }
        Ok(sole_client == Some(true) && no_live_sessions == Some(true))
    }

    fn stop_idle_daemon(paths: &DaemonPaths) -> Result<()> {
        let pid = fs::read_to_string(&paths.pid)
            .context("stale muxloomd generation has no pid file")?
            .trim()
            .parse::<i32>()
            .context("stale muxloomd pid is invalid")?;
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result != 0 {
            return Err(io::Error::last_os_error()).context("failed to stop idle muxloomd");
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while daemon_process_alive(paths) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if daemon_process_alive(paths) {
            bail!("idle muxloomd did not stop during generation handover");
        }
        let _ = fs::remove_file(&paths.socket);
        let _ = fs::remove_file(&paths.pid);
        Ok(())
    }

    fn wait_for_daemon_stop(paths: &DaemonPaths) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while daemon_process_alive(paths) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if daemon_process_alive(paths) {
            bail!("muxloomd did not stop after accepting generation handover");
        }
        let _ = fs::remove_file(&paths.socket);
        let _ = fs::remove_file(&paths.pid);
        Ok(())
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

        /// Writes a log whose every `spacing`-th byte is a newline.
        fn log_with_lines(name: &str, len: usize, spacing: usize) -> File {
            let path = test_state(name).paths.history.join("log.ansi");
            let mut bytes = vec![b'x'; len];
            for offset in (spacing..len).step_by(spacing) {
                bytes[offset] = b'\n';
            }
            fs::write(&path, &bytes).unwrap();
            File::open(&path).unwrap()
        }

        #[test]
        fn a_seed_reads_back_as_far_as_the_rows_it_was_asked_for() {
            const MIB: u64 = 1024 * 1024;
            // A line every 4 KiB: 256 of them per mebibyte read.
            let mut file = log_with_lines("seed-start", 6 * MIB as usize, 4096);
            let end = 6 * MIB;

            // 400 rows are looked for with room to spare, so the scan stops
            // three mebibytes back, where the 600th line from the end is.
            assert_eq!(
                seed_start(&mut file, end, 400, MIB, 4 * MIB).unwrap(),
                end - 3 * MIB
            );
            // Asking for more rows than the log can show reads back as far as
            // it is allowed to and no further.
            assert_eq!(
                seed_start(&mut file, end, 100_000, MIB, 4 * MIB).unwrap(),
                end - 4 * MIB
            );
            // A handful of rows still reads a whole window: the scan only says
            // where the rows cannot be further back than, and starting a render
            // that late would leave a client with almost nothing.
            assert_eq!(
                seed_start(&mut file, end, 1, MIB, 4 * MIB).unwrap(),
                end - MIB
            );
            // A log shorter than one window is rendered from its beginning.
            assert_eq!(
                seed_start(&mut file, MIB / 2, 400, MIB, 4 * MIB).unwrap(),
                0
            );
        }

        #[test]
        fn a_log_of_redraws_is_read_back_the_same_distance_as_one_of_lines() {
            // Nothing in the window ever committed a line, which is what an
            // agent that repaints a full-screen pane looks like. The scan must
            // not answer with the whole log for want of newlines.
            const MIB: u64 = 1024 * 1024;
            let mut file = log_with_lines("seed-start-redraw", 6 * MIB as usize, usize::MAX);
            assert_eq!(
                seed_start(&mut file, 6 * MIB, 400, MIB, 4 * MIB).unwrap(),
                6 * MIB - 4 * MIB
            );
        }

        #[test]
        fn a_history_page_widens_its_window_until_it_reaches_the_rows_asked_for() {
            // A log of full-screen redraws: every frame repaints the same two
            // rows, so a window of it holds far fewer rows of history than its
            // size suggests. The render has to keep reading back until the row
            // that was asked for is inside the window.
            let path = test_state("render-history").paths.history.join("log.ansi");
            let mut log = String::new();
            for line in 1..=400 {
                log.push_str(&format!("\x1b[1;1H\x1b[Kline{line}"));
                for row in 2..=5 {
                    log.push_str(&format!("\x1b[{row};1H\x1b[Kpaint{row}"));
                }
                log.push_str("\x1b[5;1H\r\n");
            }
            fs::write(&path, &log).unwrap();
            // Small enough that reaching 300 rows back takes several widenings.
            let window = (log.len() / 8) as u64;

            let (page, total, offset) =
                render_history_file(&path, 20, 5, 300, 40, window, log.len() as u64).unwrap();
            let page = String::from_utf8_lossy(&page).into_owned();

            assert_eq!(offset, 300, "the row that was asked for");
            assert!(total > 300, "rows in all: {total}");
            assert!(page.contains("line9"), "rows from back then: {page:?}");
            assert!(
                !page.contains("line400"),
                "and not the newest ones: {page:?}"
            );

            // A window that may not widen answers with what it could reach.
            let (_, _, shallow) =
                render_history_file(&path, 20, 5, 300, 40, window, window).unwrap();
            assert!(
                shallow < 300,
                "as far back as one window reaches: {shallow}"
            );
        }

        #[test]
        fn resolve_executable_prefers_path_over_a_cwd_named_entry() {
            // Simulate `~/Works`, which contains a `claude` *directory* that
            // portable-pty would otherwise exec (and abort on) instead of the
            // real CLI. Resolving against PATH must ignore the cwd entirely.
            let root = test_state("resolve-exe").paths.root.clone();
            let bin = root.join("bin");
            let cwd = root.join("cwd");
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(cwd.join("claude")).unwrap(); // a `claude` directory in cwd
            let real = bin.join("claude");
            fs::write(&real, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

            let path_env = std::ffi::OsString::from(bin.to_str().unwrap());
            let resolved = resolve_executable_on_path("claude", Some(path_env.as_os_str()));
            assert_eq!(resolved, Some(real.into_os_string()));
        }

        #[test]
        fn resolve_executable_honours_explicit_paths_and_refuses_unresolved_names() {
            // An explicit path is honoured verbatim.
            assert_eq!(
                resolve_executable_on_path("/usr/bin/env", None),
                Some(std::ffi::OsString::from("/usr/bin/env"))
            );
            // A non-executable match on PATH is skipped; with nothing else to
            // find, resolution returns None so the caller refuses to launch
            // instead of falling back to a binary in the working directory.
            let root = test_state("resolve-missing").paths.root.clone();
            fs::write(root.join("claude"), b"data").unwrap(); // exists but not +x
            let path_env = std::ffi::OsString::from(root.to_str().unwrap());
            assert_eq!(
                resolve_executable_on_path("claude", Some(path_env.as_os_str())),
                None
            );
            // A bare name with no PATH to search is likewise refused.
            assert_eq!(resolve_executable_on_path("claude", None), None);
        }

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
        fn generation_handover_requires_an_idle_daemon() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let state = test_state("handover");
            let server_state = Arc::clone(&state);
            let handle = thread::spawn(move || serve_client(server, server_state));

            assert!(daemon_is_idle_for_handover(&mut client).unwrap());
            Frame::json(
                FrameKind::Request,
                0,
                70,
                &DaemonRequest::Launch {
                    session_id: "muxloomd-terminal-handover".into(),
                    kind: "terminal".into(),
                    path: "/tmp".into(),
                    label: "handover guard".into(),
                    temporary: false,
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
                if frame.kind == FrameKind::Response && frame.request_id == 70 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    break;
                }
            }
            assert!(!daemon_is_idle_for_handover(&mut client).unwrap());
            assert!(!prepare_handover(&state));

            Frame::json(
                FrameKind::Request,
                0,
                71,
                &DaemonRequest::Archive {
                    session_id: "muxloomd-terminal-handover".into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 71 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            assert!(daemon_is_idle_for_handover(&mut client).unwrap());
            assert!(prepare_handover(&state));

            let (mut rejected, server) = UnixStream::pair().unwrap();
            let draining_state = Arc::clone(&state);
            let rejected_handle = thread::spawn(move || serve_client(server, draining_state));
            let mut byte = [0_u8; 1];
            assert_eq!(rejected.read(&mut byte).unwrap(), 0);
            rejected_handle.join().unwrap().unwrap();
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
                false,
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
            let archived = session.snapshot();
            assert!(archived.archived && archived.dead);
            assert!(!archived.working);
            assert!(!archived.needs_attention);
        }

        #[test]
        fn codex_title_spinner_survives_partial_visible_redraws() {
            let state = test_state("title-working");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-codex-title-working".into(),
                "codex".into(),
                "/tmp".into(),
                "title working state".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
            )
            .unwrap();

            session.record_output("\x1b]0;⠋ project\x07\x1b[2J\x1b[HWork".as_bytes());
            assert!(session.snapshot().working);
            session.record_output(b"\x1b[2K\r");
            assert!(
                session.snapshot().working,
                "erasing the visible status must not erase the title signal"
            );
            session.record_output(b"\x1b]0;project\x07");
            assert!(!session.snapshot().working);

            session.archive().unwrap();
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn temporary_session_never_creates_history_or_becomes_archived() {
            let state = test_state("temporary");
            let paths = state.paths.clone();
            let session_id = "muxloomd-temporal-codex-test";
            let session = launch_session(
                &state,
                session_id.into(),
                "codex".into(),
                "/tmp".into(),
                "Temporal Chat".into(),
                true,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
            )
            .unwrap();

            assert!(session.snapshot().temporary);
            assert!(!paths.history.join(format!("{session_id}.ansi")).exists());
            assert_eq!(
                session.read_history(0, 100, false).unwrap(),
                (Vec::new(), 0, 0)
            );
            assert!(session.search_history("anything", 10).unwrap().is_empty());
            assert!(session.archive().is_err());

            session.stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while state.sessions.lock().unwrap().contains_key(session_id)
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(!state.sessions.lock().unwrap().contains_key(session_id));
            assert!(!paths.sessions.join(format!("{session_id}.json")).exists());
            assert!(!paths.history.join(format!("{session_id}.ansi")).exists());
            fs::remove_dir_all(paths.root).unwrap();
        }

        #[test]
        fn archived_sessions_reload_with_searchable_history_after_restart() {
            let initial = test_state("persisted-archive");
            let paths = initial.paths.clone();
            drop(initial);
            let session_id = "muxloomd-claude-persisted-archive";
            persist_session_metadata(
                &paths.sessions.join(format!("{session_id}.json")),
                &DaemonSession {
                    id: session_id.into(),
                    kind: "claude".into(),
                    path: "/tmp/project".into(),
                    label: "persisted archive".into(),
                    temporary: false,
                    created_at: 42,
                    pid: None,
                    dead: true,
                    archived: true,
                    recap: Some("completed the persistent work".into()),
                    working: false,
                    needs_attention: false,
                    attention_reason: None,
                },
            )
            .unwrap();
            fs::write(
                paths.history.join(format!("{session_id}.ansi")),
                b"first line\npersistent result\nlast line\n",
            )
            .unwrap();

            for _ in 0..2 {
                let restarted = DaemonState::new(paths.clone());
                assert!(restarted.sessions.lock().unwrap().is_empty());
                let persisted = persisted_session(&restarted, session_id).unwrap();
                let snapshot = persisted.snapshot();
                assert!(snapshot.archived && snapshot.dead);
                assert_eq!(
                    snapshot.recap.as_deref(),
                    Some("completed the persistent work")
                );
                let (history, total_lines, offset) = persisted.read_history(0, 10, false).unwrap();
                assert_eq!(total_lines, 3);
                assert_eq!(offset, 0);
                assert_eq!(
                    String::from_utf8_lossy(&history),
                    "first line\npersistent result\nlast line\n"
                );
                let matches = persisted.search_history("PERSISTENT", 10).unwrap();
                assert_eq!(matches.len(), 1);
                assert_eq!(matches[0].line_number, 2);
            }

            fs::remove_dir_all(paths.root).unwrap();
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
                    temporary: false,
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
                    scrollback_rows: 0,
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
        fn tcp_stream_forwards_bytes_over_the_existing_daemon_connection() {
            use std::net::TcpListener;

            let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let upstream_port = upstream.local_addr().unwrap().port();
            let upstream_handle = thread::spawn(move || {
                let (mut socket, _) = upstream.accept().unwrap();
                let mut request = [0_u8; 4];
                socket.read_exact(&mut request).unwrap();
                assert_eq!(&request, b"ping");
                socket.write_all(b"pong").unwrap();
            });

            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let state = test_state("tcp-forward");
            let root = state.paths.root.clone();
            let daemon_handle = thread::spawn(move || serve_client(server, state));
            let stream_id = stream::MEDIA_BASE + 7;
            Frame::json(
                FrameKind::OpenStream,
                stream_id,
                0,
                &OpenStream::Tcp {
                    host: "127.0.0.1".into(),
                    port: upstream_port,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::OpenStream && frame.stream_id == stream_id {
                    break;
                }
            }
            Frame::data(stream_id, 0, b"ping", false)
                .write_to(&mut client)
                .unwrap();
            let mut response = Vec::new();
            while response.len() < 4 {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Data && frame.stream_id == stream_id {
                    let payload = frame.decoded_payload().unwrap();
                    response.extend_from_slice(&payload);
                    Frame::window_update(stream_id, payload.len() as u32)
                        .write_to(&mut client)
                        .unwrap();
                }
            }
            assert_eq!(response, b"pong");
            Frame::new(FrameKind::CloseStream, stream_id, 0, vec![])
                .write_to(&mut client)
                .unwrap();
            upstream_handle.join().unwrap();

            let refused_stream = stream_id + 1;
            Frame::json(
                FrameKind::OpenStream,
                refused_stream,
                0,
                &OpenStream::Tcp {
                    host: "127.0.0.1".into(),
                    port: upstream_port,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Error && frame.stream_id == refused_stream {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Error { message } if message.contains("cannot connect")
                    ));
                    break;
                }
            }
            Frame::json(FrameKind::Request, 0, 99, &DaemonRequest::Ping)
                .unwrap()
                .write_to(&mut client)
                .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 99 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Pong { .. }
                    ));
                    break;
                }
            }
            drop(client);
            daemon_handle.join().unwrap().unwrap();
            fs::remove_dir_all(root).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_tcp_listener_parser_returns_only_unprivileged_listeners() {
            let table = "  sl  local_address rem_address   st\n\
                         0: 0100007F:0016 00000000:0000 0A\n\
                         1: 00000000:0BB8 00000000:0000 0A\n\
                         2: 00000000:1435 00000000:0000 01\n";
            let mut ports = BTreeSet::new();
            collect_linux_tcp_listeners(table, &mut ports);
            assert_eq!(ports.into_iter().collect::<Vec<_>>(), [3000]);
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

            Frame::json(
                FrameKind::OpenStream,
                stream::FILE_BASE + 2,
                33,
                &OpenStream::Media {
                    path: source.to_string_lossy().into_owned(),
                    offset: 128,
                    length: Some(4096),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            let mut media = Vec::new();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                match frame.kind {
                    FrameKind::Data if frame.stream_id == stream::FILE_BASE + 2 => {
                        assert_eq!(frame.flags, 0, "encoded media must not be recompressed");
                        let payload = frame.decoded_payload().unwrap();
                        media.extend_from_slice(&payload);
                        Frame::window_update(frame.stream_id, payload.len() as u32)
                            .write_to(&mut client)
                            .unwrap();
                    }
                    FrameKind::CloseStream if frame.stream_id == stream::FILE_BASE + 2 => break,
                    _ => {}
                }
            }
            assert_eq!(media, source_bytes[128..128 + 4096]);

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

            let extensionless_image = root.join("image-data");
            fs::write(
                &extensionless_image,
                b"\x89PNG\r\n\x1a\nnot-a-complete-image",
            )
            .unwrap();
            assert_eq!(
                native_preview_file(extensionless_image.to_str().unwrap(), 1024)
                    .unwrap()
                    .kind,
                FilePreviewKind::Image
            );
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
