use std::{path::PathBuf, sync::mpsc, thread, time::Instant};

use crate::{
    bridge::BridgePool,
    config::CommandConfig,
    debug,
    media::{MediaPlayback, MediaUpdate, decode_image_stream, decode_video_stream},
    model::{
        AgentKind, AgentSession, DirectoryListing, FileListing, FilePreview, FilePreviewKind,
        HistoryMatch, HistoryPage, LaunchRequest, Probe, RestoredTranscript, ResumeCandidate,
        SearchMatchKind, SearchResult, Target, TaskProgress,
    },
    runtime::{Runtime, is_temporary_session_id},
};

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub target: Target,
    pub codex_command: String,
    pub claude_command: String,
    pub environment: Vec<(String, String)>,
    pub attention_patterns: Vec<String>,
}

#[derive(Debug)]
pub enum Request {
    Scan(ScanRequest),
    RefreshActivity {
        target: Target,
    },
    /// Cycle the target's bridge so the reconnect deploys the current
    /// companion and, with keepers carrying the sessions, hands the daemon
    /// over to the new generation.
    RefreshDaemon {
        target: Target,
    },
    DetectPorts {
        target: Target,
    },
    Capture {
        target: Target,
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
        width: u16,
        height: u16,
    },
    Launch {
        request: LaunchRequest,
        command: CommandConfig,
        environment: Vec<(String, String)>,
        remove_archive_session_id: Option<String>,
    },
    Install {
        target: Target,
        kind: AgentKind,
        command: CommandConfig,
        environment: Vec<(String, String)>,
    },
    Kill {
        target: Target,
        session_id: String,
    },
    Archive {
        target: Target,
        session_id: String,
    },
    RemoveResumedArchive {
        target: Target,
        session_id: String,
    },
    Search {
        query: String,
        sessions: Vec<(Target, AgentSession)>,
    },
    ListDirectory {
        target: Target,
        path: String,
    },
    ScanResumes {
        target: Target,
        kind: AgentKind,
        path: String,
    },
    ListFiles {
        target: Target,
        path: String,
    },
    SearchFiles {
        target: Target,
        root: String,
        pattern: String,
        request_id: u64,
    },
    PreviewFile {
        target: Target,
        path: String,
    },
    OpenMedia {
        target: Target,
        path: String,
        kind: FilePreviewKind,
        width: u16,
        height: u16,
    },
    PreloadDirectory {
        target: Target,
        path: String,
    },
    PreloadPreview {
        target: Target,
        path: String,
    },
    DownloadFile {
        target: Target,
        remote_path: String,
        local_directory: PathBuf,
        total_size: u64,
    },
    UploadFiles {
        target: Target,
        local_paths: Vec<PathBuf>,
        remote_directory: String,
    },
    /// Mirror the given targets' session history into the local backup store.
    BackupSync {
        targets: Vec<Target>,
        include_ansi: bool,
        ansi_max_bytes: u64,
    },
    /// Push one backed-up transcript from the local store back onto a machine
    /// that lost it, so the agent's own resume can find it again. The record
    /// itself is read from the store on the worker thread, so only its key
    /// travels: the stable machine partition plus the session id.
    BackupRestore {
        target: Target,
        machine_key: String,
        session_id: String,
    },
}

#[derive(Debug)]
pub enum Event {
    TaskProgress {
        target_id: String,
        operation: TaskKind,
        progress: TaskProgress,
    },
    Scanned {
        target_id: String,
        result: Result<(Probe, Vec<AgentSession>), String>,
    },
    ActivityRefreshed {
        target_id: String,
        result: Result<Vec<AgentSession>, String>,
    },
    /// The daemon version now serving the target after a bridge cycle, when
    /// the reconnect could read one.
    DaemonRefreshed {
        target_id: String,
        result: Result<Option<String>, String>,
    },
    PortsDetected {
        target_id: String,
        result: Result<Vec<u16>, String>,
    },
    Captured {
        target_id: String,
        session_id: String,
        result: Result<HistoryPage, String>,
    },
    Launched {
        target_id: String,
        notice: Option<String>,
        result: Result<String, String>,
        remove_archive_session_id: Option<String>,
    },
    Installed {
        target_id: String,
        kind: AgentKind,
        result: Result<String, String>,
    },
    Killed {
        target_id: String,
        result: Result<(), String>,
    },
    Archived {
        target_id: String,
        session_id: String,
        result: Result<(), String>,
    },
    ResumedArchiveRemoved {
        target_id: String,
        session_id: String,
        result: Result<(), String>,
    },
    Searched {
        query: String,
        results: Vec<SearchResult>,
        /// Machines whose history could not be read for this query. Without
        /// them an empty result list cannot tell "nothing matched" apart from
        /// "nothing could be looked at".
        unreachable: Vec<String>,
    },
    DirectoryListed {
        target_id: String,
        requested_path: String,
        result: Result<DirectoryListing, String>,
    },
    ResumesScanned {
        target_id: String,
        kind: AgentKind,
        path: String,
        result: Result<Vec<ResumeCandidate>, String>,
        /// Set when one runtime's history could be read and the other's could
        /// not, so an empty list is not reported as a definite "nothing here".
        warning: Option<String>,
    },
    FilesListed {
        target_id: String,
        requested_path: String,
        result: Result<FileListing, String>,
    },
    FilesSearched {
        target_id: String,
        root: String,
        pattern: String,
        request_id: u64,
        result: Result<FileListing, String>,
    },
    FilePreviewed {
        target_id: String,
        path: String,
        result: Result<FilePreview, String>,
    },
    MediaOpened {
        target_id: String,
        path: String,
        result: Result<MediaPlayback, String>,
    },
    DirectoryPreloaded {
        target_id: String,
        path: String,
        result: Result<FileListing, String>,
    },
    PreviewPreloaded {
        target_id: String,
        path: String,
        result: Result<FilePreview, String>,
    },
    FileDownloadProgress {
        remote_path: String,
        transferred: u64,
        total_size: u64,
        bytes_per_second: f64,
    },
    FileDownloaded {
        result: Result<PathBuf, String>,
    },
    FileUploadProgress {
        name: String,
        transferred: u64,
        total_size: u64,
        bytes_per_second: f64,
    },
    FilesUploaded {
        target_id: String,
        remote_directory: String,
        /// The names the files were stored under, which differ from the ones
        /// dropped when an upload had to step around an existing file.
        result: Result<Vec<String>, String>,
    },
    /// One backup pass finished; the payload is a human-readable summary.
    BackupSynced {
        result: Result<String, String>,
    },
    /// A backed-up transcript finished transferring back onto a machine. On
    /// success the payload carries the agent-native id to resume with.
    BackupRestored {
        target_id: String,
        session_id: String,
        result: Result<RestoredTranscript, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Connect,
    Install,
}

pub struct Worker {
    pub requests: mpsc::Sender<Request>,
    pub events: mpsc::Receiver<Event>,
    pub bridges: BridgePool,
}

impl Worker {
    pub fn start(runtime: Runtime) -> Self {
        let bridges = runtime.bridge_pool();
        let (request_tx, request_rx) = mpsc::channel::<Request>();
        let (event_tx, event_rx) = mpsc::channel::<Event>();

        thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let runtime = runtime.clone();
                let events = event_tx.clone();
                thread::spawn(move || match request {
                    Request::Scan(request) => {
                        let target_id = request.target.id.clone();
                        let progress_target = target_id.clone();
                        let progress_events = events.clone();
                        let mut result = runtime
                            .probe_and_discover_with_progress(
                                &request.target,
                                &request.codex_command,
                                &request.claude_command,
                                &request.environment,
                                move |progress| {
                                    let _ = progress_events.send(Event::TaskProgress {
                                        target_id: progress_target.clone(),
                                        operation: TaskKind::Connect,
                                        progress,
                                    });
                                },
                            )
                            .map_err(|error| error.to_string());
                        if let Ok((_, sessions)) = &mut result {
                            for session in sessions.iter_mut().filter(|session| {
                                !session.dead
                                    && session.kind != crate::model::AgentKind::Terminal
                                    && !crate::runtime::is_daemon_session_id(&session.id)
                            }) {
                                match runtime.inspect_agent(
                                    &request.target,
                                    &session.id,
                                    session.kind,
                                    &request.attention_patterns,
                                ) {
                                    Ok((working, attention, recap)) => {
                                        session.working = working;
                                        session.needs_attention = attention.is_some();
                                        session.attention_reason = attention;
                                        if recap.is_some() {
                                            session.recap = recap;
                                        }
                                    }
                                    Err(error) => debug::log(
                                        "worker",
                                        format!(
                                            "agent state check failed session={}: {error}",
                                            session.id
                                        ),
                                    ),
                                }
                            }
                        }
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("scan failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Scanned { target_id, result });
                    }
                    Request::RefreshActivity { target } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .daemon_sessions(&target)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("activity refresh failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::ActivityRefreshed { target_id, result });
                    }
                    Request::RefreshDaemon { target } => {
                        let target_id = target.id.clone();
                        let bridges = runtime.bridge_pool();
                        bridges.disconnect(&target.id);
                        let result = bridges
                            .list_sessions(&target)
                            .map(|_| bridges.daemon_version(&target.id))
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("daemon refresh failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::DaemonRefreshed { target_id, result });
                    }
                    Request::DetectPorts { target } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .tcp_listener_ports(&target)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "forward",
                                format!("port detection failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::PortsDetected { target_id, result });
                    }
                    Request::Capture {
                        target,
                        session_id,
                        offset_from_bottom,
                        lines,
                        width,
                        height,
                    } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .capture_page(
                                &target,
                                &session_id,
                                offset_from_bottom,
                                lines,
                                width,
                                height,
                            )
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("capture failed session={session_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Captured {
                            target_id,
                            session_id,
                            result,
                        });
                    }
                    Request::Launch {
                        request,
                        command,
                        environment,
                        remove_archive_session_id,
                    } => {
                        let target_id = request.target.id.clone();
                        let result = runtime
                            .launch(&request, &command, &environment)
                            .map_err(|error| error.to_string());
                        let notice = runtime.take_bridge_notice(&target_id);
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("launch failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Launched {
                            target_id,
                            notice,
                            result,
                            remove_archive_session_id,
                        });
                    }
                    Request::Install {
                        target,
                        kind,
                        command,
                        environment,
                    } => {
                        let target_id = target.id.clone();
                        let progress_target = target_id.clone();
                        let progress_events = events.clone();
                        let result = runtime
                            .install_runtime_with_progress(
                                &target,
                                kind,
                                &command,
                                &environment,
                                move |progress| {
                                    let _ = progress_events.send(Event::TaskProgress {
                                        target_id: progress_target.clone(),
                                        operation: TaskKind::Install,
                                        progress,
                                    });
                                },
                            )
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("install failed target={target_id} kind={kind}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Installed {
                            target_id,
                            kind,
                            result,
                        });
                    }
                    Request::Kill { target, session_id } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .kill(&target, &session_id)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("kill failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Killed { target_id, result });
                    }
                    Request::Archive { target, session_id } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .archive(&target, &session_id)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("archive failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::Archived {
                            target_id,
                            session_id,
                            result,
                        });
                    }
                    Request::RemoveResumedArchive { target, session_id } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .kill(&target, &session_id)
                            .map_err(|error| error.to_string());
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!(
                                    "resumed archive removal failed target={target_id} session={session_id}: {error}"
                                ),
                            );
                        }
                        let _ = events.send(Event::ResumedArchiveRemoved {
                            target_id,
                            session_id,
                            result,
                        });
                    }
                    Request::Search { query, sessions } => {
                        let mut results = Vec::new();
                        let mut history_jobs = Vec::new();
                        for (target, session) in sessions {
                            if is_temporary_session_id(&session.id) {
                                continue;
                            }
                            if let Some((score, snippet)) = best_name_match(&session, &query) {
                                results.push((
                                    search_result(&session, SearchMatchKind::Name, snippet, None),
                                    score,
                                ));
                            } else if let Some((score, snippet)) =
                                best_recap_match(&session, &query)
                            {
                                results.push((
                                    search_result(&session, SearchMatchKind::Recap, snippet, None),
                                    score,
                                ));
                            } else {
                                history_jobs.push((target, session));
                            }
                        }
                        // Multiplex a bounded number of SSH/tmux searches at once. This keeps
                        // large fleets responsive without opening an unbounded connection burst.
                        let mut unreachable: Vec<String> = Vec::new();
                        for jobs in history_jobs.chunks(8) {
                            let batch = thread::scope(|scope| {
                                let mut handles = Vec::new();
                                for (target, session) in jobs {
                                    let runtime = runtime.clone();
                                    let target = target.clone();
                                    let session = session.clone();
                                    let query = query.clone();
                                    handles.push(scope.spawn(move || {
                                        search_session_history(&runtime, target, session, &query)
                                    }));
                                }
                                handles
                                    .into_iter()
                                    .filter_map(|handle| handle.join().ok())
                                    .collect::<Vec<_>>()
                            });
                            for outcome in batch {
                                match outcome {
                                    Ok(Some(hit)) => results.push(hit),
                                    Ok(None) => {}
                                    Err(target_id) => {
                                        if !unreachable.contains(&target_id) {
                                            unreachable.push(target_id);
                                        }
                                    }
                                }
                            }
                        }
                        results.sort_by(|left, right| {
                            right
                                .0
                                .match_kind
                                .cmp(&left.0.match_kind)
                                .then_with(|| left.1.cmp(&right.1))
                                .then_with(|| right.0.created_at.cmp(&left.0.created_at))
                                .then_with(|| left.0.target_id.cmp(&right.0.target_id))
                        });
                        results.truncate(100);
                        let results: Vec<_> =
                            results.into_iter().map(|(result, _)| result).collect();
                        debug::log(
                            "search",
                            format!("query completed results={}", results.len()),
                        );
                        let _ = events.send(Event::Searched {
                            query,
                            results,
                            unreachable,
                        });
                    }
                    Request::ListDirectory { target, path } => {
                        let target_id = target.id.clone();
                        let requested_path = path.clone();
                        let result = runtime
                            .list_directory(&target, &path)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::DirectoryListed {
                            target_id,
                            requested_path,
                            result,
                        });
                    }
                    Request::ScanResumes { target, kind, path } => {
                        let target_id = target.id.clone();
                        let codex = runtime
                            .scan_resumes(&target, AgentKind::Codex, &path)
                            .map_err(|error| error.to_string());
                        let claude = runtime
                            .scan_resumes(&target, AgentKind::Claude, &path)
                            .map_err(|error| error.to_string());
                        let mut warning = None;
                        let result = match (codex, claude) {
                            (Ok(mut codex), Ok(claude)) => {
                                codex.extend(claude);
                                Ok(codex)
                            }
                            (Ok(candidates), Err(error)) => {
                                debug::log(
                                    "resume",
                                    format!(
                                        "Claude history scan failed target={target_id}: {error}"
                                    ),
                                );
                                warning =
                                    Some(format!("Could not scan claude history: {error}"));
                                Ok(candidates)
                            }
                            (Err(error), Ok(candidates)) => {
                                debug::log(
                                    "resume",
                                    format!(
                                        "Codex history scan failed target={target_id}: {error}"
                                    ),
                                );
                                warning = Some(format!("Could not scan codex history: {error}"));
                                Ok(candidates)
                            }
                            (Err(codex), Err(claude)) => Err(format!(
                                "Codex history scan failed: {codex}; Claude history scan failed: {claude}"
                            )),
                        }
                        .map(|mut candidates| {
                            candidates
                                .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
                            candidates.truncate(100);
                            candidates
                        });
                        let _ = events.send(Event::ResumesScanned {
                            target_id,
                            kind,
                            path,
                            result,
                            warning,
                        });
                    }
                    Request::ListFiles { target, path } => {
                        let target_id = target.id.clone();
                        let requested_path = path.clone();
                        let result = runtime
                            .list_files(&target, &path)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::FilesListed {
                            target_id,
                            requested_path,
                            result,
                        });
                    }
                    Request::SearchFiles {
                        target,
                        root,
                        pattern,
                        request_id,
                    } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .search_files(&target, &root, &pattern)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::FilesSearched {
                            target_id,
                            root,
                            pattern,
                            request_id,
                            result,
                        });
                    }
                    Request::PreviewFile { target, path } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .preview_file(&target, &path)
                            .map(normalize_legacy_image_preview)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::FilePreviewed {
                            target_id,
                            path,
                            result,
                        });
                    }
                    Request::OpenMedia {
                        target,
                        path,
                        kind,
                        width,
                        height,
                    } => {
                        let target_id = target.id.clone();
                        match runtime
                            .bridge_pool()
                            .open_media(&target, path.clone(), 0, None)
                        {
                            Ok(stream) => {
                                let (updates, playback) = MediaPlayback::channel();
                                if events
                                    .send(Event::MediaOpened {
                                        target_id,
                                        path: path.clone(),
                                        result: Ok(playback),
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                let result = match kind {
                                    FilePreviewKind::Image => {
                                        decode_image_stream(stream, width, height, &updates)
                                    }
                                    FilePreviewKind::Video => {
                                        decode_video_stream(stream, width, height, &updates)
                                    }
                                    _ => {
                                        Err(anyhow::anyhow!("{} is not a visual media type", kind))
                                    }
                                };
                                if let Err(error) = result {
                                    let _ = updates.send(MediaUpdate::Failed(error.to_string()));
                                }
                            }
                            Err(error) => {
                                let _ = events.send(Event::MediaOpened {
                                    target_id,
                                    path,
                                    result: Err(error.to_string()),
                                });
                            }
                        }
                    }
                    Request::PreloadDirectory { target, path } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .list_files(&target, &path)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::DirectoryPreloaded {
                            target_id,
                            path,
                            result,
                        });
                    }
                    Request::PreloadPreview { target, path } => {
                        let target_id = target.id.clone();
                        let result = runtime
                            .preview_file(&target, &path)
                            .map(normalize_legacy_image_preview)
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::PreviewPreloaded {
                            target_id,
                            path,
                            result,
                        });
                    }
                    Request::DownloadFile {
                        target,
                        remote_path,
                        local_directory,
                        total_size,
                    } => {
                        let progress_path = remote_path.clone();
                        let progress_events = events.clone();
                        let mut last_update = Instant::now();
                        let mut last_bytes = 0u64;
                        let result = runtime
                            .download_file_with_progress(
                                &target,
                                &remote_path,
                                &local_directory,
                                |transferred| {
                                    if last_update.elapsed().as_millis() < 100
                                        && transferred < total_size
                                    {
                                        return;
                                    }
                                    let elapsed = last_update.elapsed().as_secs_f64().max(0.001);
                                    let bytes_per_second =
                                        transferred.saturating_sub(last_bytes) as f64 / elapsed;
                                    let _ = progress_events.send(Event::FileDownloadProgress {
                                        remote_path: progress_path.clone(),
                                        transferred,
                                        total_size,
                                        bytes_per_second,
                                    });
                                    last_update = Instant::now();
                                    last_bytes = transferred;
                                },
                            )
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::FileDownloaded { result });
                    }
                    Request::UploadFiles {
                        target,
                        local_paths,
                        remote_directory,
                    } => {
                        let target_id = target.id.clone();
                        let progress_events = events.clone();
                        let mut last_update = Instant::now();
                        let mut last_bytes = 0u64;
                        let result = runtime
                            .upload_files_with_progress(
                                &target,
                                &local_paths,
                                &remote_directory,
                                |name, transferred, total_size| {
                                    if last_update.elapsed().as_millis() < 100
                                        && transferred < total_size
                                    {
                                        return;
                                    }
                                    let elapsed = last_update.elapsed().as_secs_f64().max(0.001);
                                    let bytes_per_second =
                                        transferred.saturating_sub(last_bytes) as f64 / elapsed;
                                    let _ = progress_events.send(Event::FileUploadProgress {
                                        name: name.to_string(),
                                        transferred,
                                        total_size,
                                        bytes_per_second,
                                    });
                                    last_update = Instant::now();
                                    // Each file starts its own count, so the
                                    // rate does not read as a huge negative
                                    // jump when the next one begins at zero.
                                    last_bytes = transferred;
                                },
                            )
                            .map_err(|error| error.to_string());
                        let _ = events.send(Event::FilesUploaded {
                            target_id,
                            remote_directory,
                            result,
                        });
                    }
                    Request::BackupSync {
                        targets,
                        include_ansi,
                        ansi_max_bytes,
                    } => {
                        #[cfg(feature = "controller")]
                        {
                            let result = crate::backup::run_sync(
                                &runtime,
                                &targets,
                                include_ansi,
                                ansi_max_bytes,
                            )
                            .map(|summary| summary.to_string())
                            .map_err(|error| error.to_string());
                            if let Err(error) = &result {
                                debug::log("worker", format!("backup sync failed: {error}"));
                            }
                            let _ = events.send(Event::BackupSynced { result });
                        }
                        #[cfg(not(feature = "controller"))]
                        {
                            let _ = (targets, include_ansi, ansi_max_bytes);
                        }
                    }
                    Request::BackupRestore {
                        target,
                        machine_key,
                        session_id,
                    } => {
                        let target_id = target.id.clone();
                        #[cfg(feature = "controller")]
                        {
                            let result = crate::backup::restore_session(
                                &runtime,
                                &target,
                                &machine_key,
                                &session_id,
                            )
                            .map_err(|error| format!("{error:#}"));
                            if let Err(error) = &result {
                                debug::log(
                                    "worker",
                                    format!("backup restore {session_id} failed: {error}"),
                                );
                            }
                            let _ = events.send(Event::BackupRestored {
                                target_id,
                                session_id,
                                result,
                            });
                        }
                        #[cfg(not(feature = "controller"))]
                        {
                            let _ = (machine_key, target_id, session_id);
                        }
                    }
                });
            }
        });

        Self {
            requests: request_tx,
            events: event_rx,
            bridges,
        }
    }
}

fn normalize_legacy_image_preview(mut preview: FilePreview) -> FilePreview {
    if preview.kind == FilePreviewKind::Binary
        && (preview.mime.starts_with("image/") || image_extension(&preview.path))
    {
        preview.kind = FilePreviewKind::Image;
        if !preview.mime.starts_with("image/") {
            preview.mime = "image/*".into();
        }
    }
    preview
}

fn image_extension(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "ico"
                    | "tif"
                    | "tiff"
                    | "pnm"
                    | "pbm"
                    | "pgm"
                    | "ppm"
                    | "qoi"
            )
        })
}

fn best_name_match(session: &AgentSession, query: &str) -> Option<(usize, String)> {
    let mut candidates = Vec::new();
    if !session.label.trim().is_empty()
        && let Some(score) = search_match_score(&session.label, query)
    {
        candidates.push((score, session.label.clone()));
    }
    let display = session.display_label();
    if let Some(score) = search_match_score(display, query) {
        candidates.push((score.saturating_add(10), display.to_string()));
    }
    if let Some(score) = search_match_score(&session.path, query) {
        candidates.push((score.saturating_add(25), session.path.clone()));
    }
    candidates.into_iter().min_by_key(|(score, _)| *score)
}

fn best_recap_match(session: &AgentSession, query: &str) -> Option<(usize, String)> {
    let recap = session.recap.as_deref()?.trim();
    search_match_score(recap, query).map(|score| (score, recap.to_string()))
}

/// Searches one session's history. `Err` carries the machine id, so a search
/// that could not reach a host can say so instead of reporting no matches.
fn search_session_history(
    runtime: &Runtime,
    target: Target,
    session: AgentSession,
    query: &str,
) -> Result<Option<(SearchResult, usize)>, String> {
    let matches = match runtime.search_history(&target, &session.id, query, 12) {
        Ok(matches) => matches,
        Err(error) => {
            debug::log(
                "search",
                format!("history search failed session={}: {error}", session.id),
            );
            return Err(target.id);
        }
    };
    let Some((item, score)) = best_history_match(&matches, query) else {
        return Ok(None);
    };
    let match_kind = if item.recap {
        SearchMatchKind::Recap
    } else {
        SearchMatchKind::History
    };
    Ok(Some((
        search_result(
            &session,
            match_kind,
            item.text.clone(),
            Some(item.line_number),
        ),
        score,
    )))
}

fn search_result(
    session: &AgentSession,
    match_kind: SearchMatchKind,
    snippet: String,
    line_number: Option<usize>,
) -> SearchResult {
    SearchResult {
        session_id: session.id.clone(),
        target_id: session.target_id.clone(),
        kind: session.kind,
        label: session.display_label().to_string(),
        path: session.path.clone(),
        match_kind,
        snippet,
        line_number,
        created_at: session.created_at,
        dead: session.dead,
    }
}

fn best_history_match<'a>(
    matches: &'a [HistoryMatch],
    query: &str,
) -> Option<(&'a HistoryMatch, usize)> {
    best_history_kind(matches, query, true).or_else(|| best_history_kind(matches, query, false))
}

fn best_history_kind<'a>(
    matches: &'a [HistoryMatch],
    query: &str,
    recap: bool,
) -> Option<(&'a HistoryMatch, usize)> {
    matches
        .iter()
        .filter(|item| item.recap == recap)
        .filter_map(|item| search_match_score(&item.text, query).map(|score| (item, score)))
        .min_by(|(left, left_score), (right, right_score)| {
            left_score
                .cmp(right_score)
                .then_with(|| right.line_number.cmp(&left.line_number))
        })
}

fn search_match_score(value: &str, query: &str) -> Option<usize> {
    let value = value.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    if value == query {
        return Some(0);
    }
    if value.starts_with(&query) {
        return Some(1 + value.len().saturating_sub(query.len()));
    }
    if let Some(position) = value.find(&query) {
        let word_boundary = position == 0
            || value[..position]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_alphanumeric());
        return Some(if word_boundary { 20 } else { 100 } + position);
    }

    let mut positions = Vec::new();
    for term in query.split_whitespace() {
        positions.push(value.find(term)?);
    }
    let first = positions.iter().copied().min().unwrap_or_default();
    let last = positions.iter().copied().max().unwrap_or_default();
    Some(500 + first + last.saturating_sub(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_ranking_prefers_exact_prefix_and_compact_multi_term_matches() {
        assert!(
            search_match_score("renderer", "renderer")
                < search_match_score("renderer work", "render")
        );
        assert!(search_match_score("fix remote renderer", "remote fix").is_some());
        assert!(search_match_score("unrelated", "remote fix").is_none());
    }

    #[test]
    fn recap_is_preferred_and_newer_equal_history_wins() {
        let matches = vec![
            HistoryMatch {
                recap: false,
                line_number: 10,
                text: "fix renderer".into(),
            },
            HistoryMatch {
                recap: true,
                line_number: 2,
                text: "fix renderer".into(),
            },
        ];
        assert!(best_history_match(&matches, "renderer").unwrap().0.recap);
    }

    #[test]
    fn legacy_binary_image_previews_are_normalized_on_the_controller() {
        let preview = normalize_legacy_image_preview(FilePreview {
            path: "/tmp/frame.PNG".into(),
            mime: "application/octet-stream".into(),
            kind: FilePreviewKind::Binary,
            size: 12,
            content: String::new(),
            truncated: false,
        });
        assert_eq!(preview.kind, FilePreviewKind::Image);
        assert_eq!(preview.mime, "image/*");
    }
}
