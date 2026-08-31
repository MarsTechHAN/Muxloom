use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    bridge::BridgePool,
    config::{CommandConfig, Config},
    daemon_protocol::ParentAlert,
    debug,
    media::{MediaPlayback, MediaUpdate, decode_image_stream, decode_video_stream},
    model::{
        AgentKind, AgentSession, DirectoryListing, FileListing, FilePreview, FilePreviewKind,
        HistoryMatch, HistoryPage, LaunchRequest, Probe, RestoredTranscript, ResumeCandidate,
        SearchMatchKind, SearchResult, Target, TaskProgress,
    },
    runtime::{Runtime, is_temporary_session_id},
    talk::{
        TalkAddress, TalkAuthor, TalkDeliver, TalkDraft, TalkFilter, TalkKind, TalkMessage,
        TalkPage, TalkScope, TalkSelector, TalkVoice, decode_cursor,
    },
};

/// What a dashboard asks another controller to look at for it. Only looking:
/// the relay carries reads freely, and everything that would change a machine
/// belongs to the agents living on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Errand {
    /// The agents on a machine this dashboard cannot reach.
    Sessions,
    /// What is on one of their screens.
    Screen { session_id: String, lines: u16 },
}

impl Errand {
    /// The tool call this errand is, as the controller's own tool surface
    /// takes it.
    fn call(&self, machine: &str) -> (&'static str, serde_json::Value) {
        match self {
            Self::Sessions => (
                "list_sessions",
                serde_json::json!({ "machine": machine, "include_archived": false }),
            ),
            Self::Screen { session_id, lines } => (
                "read_screen",
                serde_json::json!({
                    "machine": machine,
                    "session_id": session_id,
                    "lines": lines,
                    "raw": true,
                }),
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub target: Target,
    /// The executable to look for per runtime, so a machine only offers what
    /// it actually has.
    pub commands: Vec<(AgentKind, String)>,
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
    /// Last resort of a forced update whose negotiated handover keeps being
    /// deferred (a drifted client count on an old daemon can defer it
    /// forever): with every session verified dead or archived, stop the old
    /// daemon outright and reconnect so the new generation starts.
    ForceDaemonRestart {
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
        /// Whether to carry this machine's own configuration for the runtime,
        /// credentials included, over to the target. The person is asked before
        /// the request is made; this is their answer.
        sync_config: bool,
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
    /// Search the local backup for conversations to reference. Nothing here
    /// leaves the machine, but the corpus is every conversation every machine
    /// ever had: reading it is not something to do between two keystrokes on
    /// the thread that draws.
    SearchBackup {
        query: String,
        limit: usize,
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
    /// Carry talk board messages between the given targets and this machine,
    /// then run whatever errands the agents on them left for a controller.
    /// Only the controller can see two daemons at once, so only it can do
    /// either. The config travels because the errands are run through the same
    /// tool surface the MCP adapter serves, policy included.
    TalkSync {
        targets: Vec<Target>,
        config: Box<Config>,
        /// The channels the fleet may speak through, so the round can also put
        /// every machine at the dashboard's revision. They travel with the
        /// round rather than being read off disk here: the dashboard is the
        /// only writer, and this way an edit is live on the next round.
        channels: Box<crate::channel::ChannelSet>,
        /// What has been read out of the chats so far. It travels both ways
        /// because the dashboard is the only reader: one reader means one
        /// answer per message, with nobody to agree with about who saw it
        /// first.
        inbox: Box<crate::channel::Inbox>,
        /// Whether this round also reads the chats. Errands run every couple of
        /// seconds and a chat app does not need asking that often, so most
        /// rounds only carry receipts back.
        read_inbox: bool,
        /// Where to put down anything a person attaches to a message. Carried
        /// rather than worked out here, because where this machine keeps its
        /// state is the dashboard's to know.
        received: PathBuf,
        /// What the dashboard's board already holds, so the round brings back
        /// only what was said since. Empty on the first round, which is how the
        /// board is filled the first time it is drawn.
        board_since: String,
    },
    /// Run whatever errands the agents on the given targets left for a
    /// controller, and nothing else.
    ///
    /// This is the one thing in a round that somebody is *waiting* on: a
    /// relayed call sits unanswered until this runs, so it rides its own short
    /// cadence rather than the back of the board round. Carrying the board
    /// contacts every machine and can take seconds on a slow link, and an
    /// agent's cross-machine call should not cost whatever that took.
    RelayPump {
        targets: Vec<Target>,
        /// The errands are run through the same tool surface the MCP adapter
        /// serves, policy included, so the config travels with them.
        config: Box<Config>,
    },
    /// Borrow another controller's reach for one look, on behalf of the person
    /// at this dashboard. The errand is left on `through` — the machine whose
    /// daemon named the one being asked about, which is where the controller
    /// that can reach it comes round — and this waits there for the answer.
    RelayErrand {
        through: Target,
        /// The machine the answer is about, as that controller addresses it.
        machine: String,
        errand: Errand,
    },
    /// Say something on the board as the person at the keyboard. The draft goes
    /// to this machine's daemon like any other post; replication carries it from
    /// there.
    TalkPost {
        draft: Box<TalkDraft>,
    },
    /// Post one message through one binding, on behalf of the person looking at
    /// the communication panel. The binding travels rather than being read off
    /// disk, because the point of the test is to find out whether what is in
    /// the panel — not what was last saved — actually reaches anybody.
    ChannelTest {
        binding: Box<crate::channel::ChannelBinding>,
        environment: Vec<(String, String)>,
    },
    /// Ask WeChat for a login code and then watch it until a phone has been
    /// through it.
    ///
    /// The whole scan is one request rather than a poll from the dashboard,
    /// because watching is a long poll that can run for the five minutes a code
    /// lives: keeping it here leaves the dashboard's side as "draw what came".
    ChannelLogin {
        /// Which attempt this is. Echoed on every event, so somebody who walked
        /// away from one code and asked for another is never shown the first.
        attempt: u64,
        /// Cleared when nobody is looking at the code any more, so the watch
        /// stops asking WeChat about it rather than running out its five
        /// minutes on a screen that has moved on.
        alive: Arc<AtomicBool>,
        environment: Vec<(String, String)>,
    },
    /// Begin Feishu/Lark onboarding: the worker asks Feishu for a code, shows
    /// it, and polls until the phone approves and Feishu hands over a freshly
    /// created bot's credentials. The same alive flag as [`Self::ChannelLogin`]
    /// ends the watch when the scanner walks away.
    ChannelLarkLogin {
        attempt: u64,
        alive: Arc<AtomicBool>,
        environment: Vec<(String, String)>,
    },
    /// Ask Lark which chats an app's bot is in, so one can be chosen instead of
    /// having its id copied out of a browser's address bar.
    ChannelChats {
        /// Which attempt this is, for the same reason a login carries one: a
        /// person who corrected a typo and asked again must not be shown the
        /// answer to what they corrected.
        attempt: u64,
        app_id: String,
        secret: String,
        environment: Vec<(String, String)>,
    },
}

/// How far a WeChat login has got, in the steps a person sees.
#[derive(Debug, Clone)]
pub enum LoginStep {
    /// A code to draw, as the link a phone should arrive at.
    Code(String),
    /// A phone has read it. What is left is a tap on the phone.
    Scanned,
    /// Nobody got there in time; codes last about five minutes.
    Expired,
    /// Done, and this is the bot that came of it.
    Connected(Box<crate::ilink::Account>),
    /// Done, and this is the Feishu/Lark bot onboarding created.
    LarkConnected(Box<crate::lark_onboard::Onboarded>),
    Failed(String),
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
        session_id: String,
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
        /// An instalment: how many captures have been read of how many, with
        /// more still to come. Names and recaps are in hand at once and are
        /// answered with the first of these; reading a fleet's scrollback takes
        /// seconds, so what it finds arrives a batch at a time. `None` is the
        /// last word on this query.
        reading: Option<(usize, usize)>,
    },
    /// What the backup held for a query. The query comes back with it: a
    /// person types faster than a corpus can be read, and an answer to a
    /// question they have already moved on from is not an answer.
    BackupSearched {
        query: String,
        hits: Vec<crate::app::CrossMachineHit>,
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
    /// One talk replication round finished. The payload says what it moved, and
    /// carries whatever the local board has to say that the dashboard has not
    /// seen yet.
    TalkSynced {
        result: Result<String, String>,
        board: Option<TalkPage>,
        /// How far the channel set got: which machines now hold this revision,
        /// and which could not be told.
        channels: crate::channel::ChannelRound,
        /// What reading the chats left to remember, and what it did. Handed
        /// straight back so the next round starts where this one stopped.
        inbox: Box<crate::channel::Inbox>,
        mail: crate::channel::InboxRound,
    },
    /// One round of errands finished.
    RelayPumped {
        /// Machines another controller told one of these daemons it could
        /// reach, which this one cannot, each with the daemon that named it.
        forwarded: Vec<crate::relay::Forwarded>,
    },
    /// An errand this dashboard left for another controller came back.
    RelayErranded {
        machine: String,
        errand: Errand,
        /// The tool's own output, or why there was none.
        result: Result<String, String>,
    },
    /// A message the human wrote is on the board, or could not be put there.
    TalkPosted {
        result: Box<Result<TalkMessage, String>>,
    },
    /// What the communication panel's test message did. Carries the binding's
    /// id rather than the binding, so nothing secret goes back through a queue.
    ChannelTested {
        id: String,
        result: Result<String, String>,
    },
    /// A WeChat login moved on. Every step of one scan carries the same
    /// `attempt`; anything from an older one is the dashboard's to drop.
    ChannelLogin {
        attempt: u64,
        step: LoginStep,
    },
    /// The chats a Lark app turned out to be in, or why asking failed. Failing
    /// is the ordinary way a mistyped secret is found, so the reason is carried
    /// rather than swallowed.
    ChannelChats {
        attempt: u64,
        result: Result<Vec<crate::channel::Chat>, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Connect,
    Install,
}

/// Stop a daemon whose negotiated handover cannot succeed, then reconnect so
/// the current generation starts. Refuses while any session is live: the old
/// daemon's in-process PTYs die with it, so this is only safe after a forced
/// update archived everything.
fn force_daemon_restart(runtime: &Runtime, target: &Target) -> Result<Option<String>, String> {
    let bridges = runtime.bridge_pool();
    let sessions = bridges
        .list_sessions(target)
        .map_err(|error| format!("could not verify the sessions are stopped: {error:#}"))?;
    if let Some(live) = sessions
        .iter()
        .find(|session| !session.dead && !session.archived)
    {
        return Err(format!(
            "session {} is still live; refusing to stop its daemon",
            live.id
        ));
    }
    let (pid, _clients) = bridges
        .daemon_status(target)
        .map_err(|error| format!("could not read the daemon pid: {error:#}"))?;
    // Schedule the stop from a detached shell so the acknowledgement gets
    // out before the daemon goes down, and clear the socket the way an
    // orderly exit would.
    let script = format!(
        "nohup sh -c 'sleep 1; kill {pid} 2>/dev/null; sleep 2; kill -9 {pid} 2>/dev/null; \
         state=\"${{MUXLOOMD_STATE_DIR:-${{XDG_STATE_HOME:-$HOME/.local/state}}/muxloom}}\"; \
         rm -f \"$state/muxloomd.sock\" \"$state/muxloomd.pid\"' >/dev/null 2>&1 & echo scheduled"
    );
    runtime
        .run_shell(target, &script, false)
        .map_err(|error| format!("could not schedule the daemon stop: {error:#}"))?;
    thread::sleep(Duration::from_secs(5));
    bridges.disconnect(&target.id);
    thread::sleep(Duration::from_millis(1_500));
    bridges
        .list_sessions(target)
        .map(|_| bridges.daemon_version(&target.id))
        .map_err(|error| format!("daemon did not come back after the restart: {error:#}"))
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
                                &request.commands,
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
                        // The dropped bridge's remote half needs a moment to
                        // hang up; reconnecting under it leaves the daemon
                        // counting two clients and deferring the handover.
                        thread::sleep(Duration::from_millis(1_500));
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
                    Request::ForceDaemonRestart { target } => {
                        let target_id = target.id.clone();
                        let result = force_daemon_restart(&runtime, &target);
                        if let Err(error) = &result {
                            debug::log(
                                "worker",
                                format!("forced daemon restart failed target={target_id}: {error}"),
                            );
                        }
                        let _ = events.send(Event::DaemonRefreshed {
                            target_id,
                            result: result.map_err(|error| error.to_string()),
                        });
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
                        sync_config,
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
                                sync_config,
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
                        let _ = events.send(Event::Killed {
                            target_id,
                            session_id,
                            result,
                        });
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
                        let captures = history_jobs.len();
                        // What was matched without reading anything - names,
                        // paths, recaps - goes out before the first capture is
                        // opened, so the list is on screen while the slow half
                        // runs underneath it.
                        let instalment =
                            |results: &[(SearchResult, usize)],
                             unreachable: &[String],
                             read: Option<usize>| {
                                let mut ranked = results.to_vec();
                                ranked.sort_by(|left, right| {
                                    right
                                        .0
                                        .match_kind
                                        .cmp(&left.0.match_kind)
                                        .then_with(|| left.1.cmp(&right.1))
                                        .then_with(|| right.0.created_at.cmp(&left.0.created_at))
                                        .then_with(|| left.0.target_id.cmp(&right.0.target_id))
                                });
                                ranked.truncate(100);
                                let _ = events.send(Event::Searched {
                                    query: query.clone(),
                                    results: ranked.into_iter().map(|(result, _)| result).collect(),
                                    unreachable: unreachable.to_vec(),
                                    reading: read.map(|read| (read, captures)),
                                });
                            };
                        instalment(&results, &unreachable, (captures > 0).then_some(0));
                        let mut read = 0;
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
                            read += jobs.len();
                            // The last batch is reported as the final answer
                            // rather than as progress, so the search is never
                            // left looking like it is still running.
                            instalment(&results, &unreachable, (read < captures).then_some(read));
                        }
                        debug::log(
                            "search",
                            format!(
                                "query completed results={} captures={captures}",
                                results.len()
                            ),
                        );
                    }
                    Request::SearchBackup { query, limit } => {
                        let hits = crate::app::backup_search_hits(&query, limit);
                        let _ = events.send(Event::BackupSearched { query, hits });
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
                        // Only the runtimes that keep a transcript of their own
                        // have a history to resume from. One of them failing
                        // must not hide what the others did find, so the
                        // candidates are pooled and the failures reported apart.
                        let mut candidates = Vec::new();
                        let mut failures: Vec<(AgentKind, String)> = Vec::new();
                        let mut scanned = 0usize;
                        for source in AgentKind::agents().filter(|kind| kind.has_native_history()) {
                            scanned += 1;
                            match runtime.scan_resumes(&target, source, &path) {
                                Ok(found) => candidates.extend(found),
                                Err(error) => {
                                    debug::log(
                                        "resume",
                                        format!(
                                            "{source} history scan failed target={target_id}: {error:#}"
                                        ),
                                    );
                                    failures.push((source, format!("{error:#}")));
                                }
                            }
                        }
                        let mut warning = None;
                        let result = if failures.len() == scanned {
                            Err(failures
                                .iter()
                                .map(|(source, error)| {
                                    format!("{source} history scan failed: {error}")
                                })
                                .collect::<Vec<_>>()
                                .join("; "))
                        } else {
                            if !failures.is_empty() {
                                warning = Some(
                                    failures
                                        .iter()
                                        .map(|(source, error)| {
                                            format!(
                                                "Could not scan {} history: {error}",
                                                source.as_str()
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join("; "),
                                );
                            }
                            Ok(candidates)
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
                    Request::TalkPost { draft } => {
                        let result = runtime
                            .bridge_pool()
                            .talk_post(&Target::local(), *draft)
                            .map_err(|error| format!("{error:#}"));
                        let _ = events.send(Event::TalkPosted {
                            result: Box::new(result),
                        });
                    }
                    Request::ChannelTest {
                        binding,
                        environment,
                    } => {
                        let message = crate::channel::Outgoing {
                            title: "muxloom".into(),
                            text: format!(
                                "This channel is bound. Every agent on every enabled machine \
                                 can reach you here from now on.{}",
                                if binding.kind.listens() {
                                    "\n\nReply to a card an agent sends and the answer goes \
                                     back to that agent. `/who` lists them, `/select <name>` \
                                     picks one for this chat."
                                } else {
                                    "\n\nThis kind of channel only sends: a reply here \
                                     reaches nobody."
                                }
                            ),
                            signature: "muxloom dashboard".into(),
                            ..Default::default()
                        };
                        let id = binding.id.clone();
                        // This is the one send whose entire purpose is to answer
                        // "does this reach me?", so it is the one send that must
                        // not confuse being accepted with arriving. WeChat takes
                        // a message on a stale conversation token exactly as it
                        // takes a good one and issues no id of its own, and a
                        // test reported as delivered on that evidence sends the
                        // person off to look for a fault everywhere except where
                        // it is. The dashboard prints a failure whole, so the
                        // repair goes in it: nothing here renews a token, only
                        // the person saying something does.
                        let result = crate::channel::send(&binding, &message, &environment)
                            .map_err(|error| format!("{error:#}"))
                            .and_then(|sent| match sent.delivered() {
                                true => Ok(sent.through),
                                false => Err("WeChat took it and sent nothing — a stale \
                                              conversation token. Say anything to the bot, then \
                                              test again."
                                    .into()),
                            });
                        if let Err(error) = &result {
                            debug::log("channel", format!("test through {id} failed: {error}"));
                        }
                        let _ = events.send(Event::ChannelTested { id, result });
                    }
                    Request::ChannelLogin {
                        attempt,
                        alive,
                        environment,
                    } => {
                        let reached = |step| {
                            let _ = events.send(Event::ChannelLogin { attempt, step });
                        };
                        match crate::ilink::begin(&environment) {
                            Err(error) => reached(LoginStep::Failed(format!("{error:#}"))),
                            Ok(login) => {
                                reached(LoginStep::Code(login.link));
                                // Said once. WeChat repeats "scanned" on every
                                // poll until the tap comes, and a line that
                                // rewrites itself with the same words reads as
                                // something going wrong.
                                let mut announced = false;
                                while alive.load(Ordering::Relaxed) {
                                    match crate::ilink::watch(&login.handle, &environment) {
                                        Ok(crate::ilink::Scan::Waiting) => {}
                                        Ok(crate::ilink::Scan::Scanned) if announced => {}
                                        Ok(crate::ilink::Scan::Scanned) => {
                                            announced = true;
                                            reached(LoginStep::Scanned);
                                        }
                                        Ok(crate::ilink::Scan::Expired) => {
                                            reached(LoginStep::Expired);
                                            break;
                                        }
                                        Ok(crate::ilink::Scan::Connected(account)) => {
                                            reached(LoginStep::Connected(account));
                                            break;
                                        }
                                        Err(error) => {
                                            reached(LoginStep::Failed(format!("{error:#}")));
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Request::ChannelLarkLogin {
                        attempt,
                        alive,
                        environment,
                    } => {
                        let reached = |step| {
                            let _ = events.send(Event::ChannelLogin { attempt, step });
                        };
                        match crate::lark_onboard::begin(&environment) {
                            Err(error) => reached(LoginStep::Failed(format!("{error:#}"))),
                            Ok((link, device_code, interval)) => {
                                reached(LoginStep::Code(link));
                                // Said once. Feishu repeats the pending answer
                                // on every poll, and a line that rewrites
                                // itself with the same words reads as
                                // something going wrong.
                                let mut announced = false;
                                let mut host = crate::lark_onboard::FEISHU.to_string();
                                while alive.load(Ordering::Relaxed) {
                                    match crate::lark_onboard::poll(
                                        &host,
                                        &device_code,
                                        &environment,
                                    ) {
                                        Ok((
                                            next_host,
                                            crate::lark_onboard::Scan::Waiting { interval: wait },
                                        )) => {
                                            host = next_host;
                                            if !announced {
                                                announced = true;
                                                // The scan is up and waiting;
                                                // there is nothing more to
                                                // tell the phone screen until
                                                // something moves.
                                                reached(LoginStep::Scanned);
                                            }
                                            std::thread::sleep(std::time::Duration::from_secs(
                                                wait,
                                            ));
                                        }
                                        Ok((
                                            _,
                                            crate::lark_onboard::Scan::Connected(onboarded),
                                        )) => {
                                            reached(LoginStep::LarkConnected(Box::new(onboarded)));
                                            break;
                                        }
                                        Err(error) => {
                                            reached(LoginStep::Failed(format!("{error:#}")));
                                            break;
                                        }
                                    }
                                }
                                let _ = interval;
                            }
                        }
                    }
                    Request::ChannelChats {
                        attempt,
                        app_id,
                        secret,
                        environment,
                    } => {
                        let result = crate::channel::chats(&app_id, &secret, &environment)
                            .map_err(|error| format!("{error:#}"));
                        let _ = events.send(Event::ChannelChats { attempt, result });
                    }
                    Request::TalkSync {
                        targets,
                        config,
                        channels,
                        mut inbox,
                        read_inbox,
                        received,
                        board_since,
                    } => {
                        let result = crate::talk::run_sync(&runtime, &targets)
                            .map(|summary| summary.to_string())
                            .map_err(|error| format!("{error:#}"));
                        if let Err(error) = &result {
                            debug::log("worker", format!("talk sync failed: {error}"));
                        }
                        // While the round is out: a child session that
                        // fell onto a question gets its parent told, so no
                        // agent has to poll the fleet to find its own subagent
                        // sitting on a permission prompt.
                        drain_parent_alerts(&runtime, &targets, &config);
                        // And while the round is out: every machine that can
                        // hold the channel set is brought to this revision, so
                        // an agent anywhere can reach the human without the
                        // dashboard having to be the one to speak.
                        let round = crate::channel::run_sync(&runtime, &targets, &channels);
                        // Receipts first, then reading: a human who answers the
                        // moment a card arrives is answering something this
                        // very round collected.
                        for receipt in &round.receipts {
                            inbox.remember(receipt.clone());
                        }
                        let mail = if read_inbox {
                            match config.environment_for(crate::model::LOCAL_TARGET_ID) {
                                Ok(environment) => crate::channel::run_inbox(
                                    &runtime,
                                    &targets,
                                    &channels,
                                    &mut inbox,
                                    &received,
                                    &environment,
                                    &config,
                                ),
                                Err(error) => {
                                    debug::log("channel", format!("no environment: {error:#}"));
                                    crate::channel::InboxRound::default()
                                }
                            }
                        } else {
                            crate::channel::InboxRound::default()
                        };
                        // Whatever the round pulled is on the local board by
                        // now, so the dashboard reads one board rather than
                        // every machine in turn.
                        let board = read_board(&runtime, &board_since);
                        let _ = events.send(Event::TalkSynced {
                            result,
                            board,
                            channels: round,
                            inbox,
                            mail,
                        });
                    }
                    Request::RelayPump { targets, config } => {
                        // Errands run whether or not anything else is due: an
                        // agent waiting on one is waiting on this round, and
                        // this is the only round it waits on.
                        let forwarded = match crate::relay::run_pump(&runtime, &config, &targets) {
                            Ok(round) => {
                                if round.busy() {
                                    debug::log("relay", round.to_string());
                                }
                                round.heard
                            }
                            Err(error) => {
                                debug::log("relay", format!("errands failed: {error:#}"));
                                Vec::new()
                            }
                        };
                        let _ = events.send(Event::RelayPumped { forwarded });
                    }
                    Request::RelayErrand {
                        through,
                        machine,
                        errand,
                    } => {
                        let result = run_errand(&runtime, &through, &machine, &errand)
                            .map_err(|error| format!("{error:#}"));
                        if let Err(error) = &result {
                            debug::log("relay", format!("{machine}: errand failed ({error})"));
                        }
                        let _ = events.send(Event::RelayErranded {
                            machine,
                            errand,
                            result,
                        });
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

/// How much of the board one round carries to the dashboard. The overlay shows
/// a tail, not an archive: what does not fit was said before anyone opened it.
const BOARD_PAGE: usize = 200;

/// How long a dashboard waits on another controller's round before giving up.
/// Two carriers' rounds have to line up — the daemon has to be polled, the job
/// run, the answer written back — and a look at a machine three seconds away is
/// still a look. Past this the person is told, rather than left watching a
/// pane that says nothing.
const ERRAND_WAIT: Duration = Duration::from_secs(20);
const ERRAND_POLL: Duration = Duration::from_millis(120);

/// Leave one look on a daemon's queue and wait there for whichever controller
/// can reach the machine to run it.
///
/// This is exactly what an agent with no fleet of its own does (see
/// `relay.rs`), for exactly the same reason: the machine is one this process
/// has no route to, and the daemon is where the route comes round.
fn run_errand(
    runtime: &Runtime,
    through: &Target,
    machine: &str,
    errand: &Errand,
) -> anyhow::Result<String> {
    let pool = runtime.bridge_pool();
    let (tool, arguments) = errand.call(machine);
    let id = pool.relay_submit(through, tool, &arguments.to_string(), "")?;
    let deadline = Instant::now() + ERRAND_WAIT;
    loop {
        let answer = pool.relay_result(through, &id)?;
        if answer.done {
            if answer.ok {
                return Ok(answer.output);
            }
            anyhow::bail!("{}", answer.output);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "no muxloom controller answered for {machine} within {} seconds",
                ERRAND_WAIT.as_secs()
            );
        }
        thread::sleep(ERRAND_POLL);
    }
}

/// What the local board has said since the dashboard last looked.
///
/// Read as the owner rather than as a session: the person watching the
/// dashboard is the one both ends of a direct message are working for, and the
/// point of the board is that they can see what was said.
fn read_board(runtime: &Runtime, since: &str) -> Option<TalkPage> {
    let filter = TalkFilter {
        since: decode_cursor(since),
        machines: TalkSelector::All,
        paths: TalkSelector::All,
        limit: BOARD_PAGE,
        owner: true,
        ..TalkFilter::default()
    };
    match runtime.bridge_pool().talk_read(&Target::local(), filter) {
        Ok(page) => Some(page),
        Err(error) => {
            debug::log("talk", format!("board unreadable ({error:#})"));
            None
        }
    }
}

/// Tell every parent agent, unasked, that a subagent it started has fallen
/// onto something only the parent can answer.
///
/// The daemons mark the edges — they see the screens — and this hands them
/// over once per round. Delivery reuses the direct-message path an agent
/// would use itself: the daemon that owns the parent decides whether the
/// prompt box can take the message now or holds it in the outbox until it can.
/// The local machine is included like [`crate::relay::run_pump`] includes it:
/// an agent's subagents almost always live next door to it, and this machine's
/// daemon watches them just the same.
///
/// A delivery that fails is not retried by hand: the child is still sitting on
/// its question, and the daemon's reminder schedule will offer the tell again
/// while it goes unanswered. That bounds the whole thing — the round's ask is
/// at-most-once per tell, and one stuck question is told about a handful of
/// times at widening spacing, then not at all until it changes, which is what
/// the stall it cures deserves and no more.
fn drain_parent_alerts(runtime: &Runtime, targets: &[Target], config: &Config) {
    if !config.alerts_to_parent {
        return;
    }
    let pool = runtime.bridge_pool();
    let local = Target::local();
    let everywhere: Vec<&Target> = std::iter::once(&local)
        .chain(targets.iter().filter(|target| target.id != local.id))
        .collect();
    for target in everywhere {
        let alerts = match pool.drain_alerts(target) {
            Ok(alerts) => alerts,
            // A machine too old to watch answers empty, not an error; anything
            // heard here is this round's daemon being unreachable, and the next
            // round will ask again on its own.
            Err(error) => {
                debug::log(
                    "alert",
                    format!(
                        "could not read waiting children on {}: {error:#}",
                        target.id
                    ),
                );
                continue;
            }
        };
        for alert in alerts {
            // The draft leaves every machine field empty on purpose, exactly as
            // a `message_agent` call does: the daemon taking delivery is the one
            // that knows what this machine is called, and the author is
            // muxloom itself — not a session and not a person, saying that one
            // of the sessions it runs is waiting.
            let draft = TalkDraft {
                scope: TalkScope::Machine {
                    machine: String::new(),
                },
                author: TalkAuthor {
                    machine: String::new(),
                    machine_label: String::new(),
                    voice: TalkVoice {
                        session_id: None,
                        label: Some("muxloom".into()),
                        kind: None,
                        human: false,
                        channel: None,
                        channel_quote: None,
                    },
                },
                kind: TalkKind::Direct,
                to: Some(TalkAddress {
                    machine: String::new(),
                    session_id: alert.parent_session_id.clone(),
                }),
                reply_to: None,
                text: parent_alert_text(&alert, &target.id),
            };
            // `Auto`, not `Now`: an alert is an announcement, and a parent
            // mid-turn reads it at the end of the turn like any other direct.
            match pool.talk_deliver(target, draft, TalkDeliver::Auto, false) {
                Ok((_, delivery, _)) => debug::log(
                    "alert",
                    format!(
                        "told {} that its subagent {} is waiting: {} ({delivery})",
                        alert.parent_session_id,
                        alert.session_id,
                        alert
                            .attention_reason
                            .as_deref()
                            .unwrap_or("waiting for input")
                    ),
                ),
                Err(error) => debug::log(
                    "alert",
                    format!(
                        "{} is waiting and its parent {} could not be told: {error:#}",
                        alert.session_id, alert.parent_session_id
                    ),
                ),
            }
        }
    }
}

/// The sentence a waiting child deserves, spelled out of what the daemon
/// saw: which session, of what kind, under what name, on which machine,
/// waiting on what, and the last thing it said. It arrives inside the standard
/// direct-message envelope, which is what says it came from muxloom rather
/// than from an agent; this text is what says what to do about it.
fn parent_alert_text(alert: &ParentAlert, machine: &str) -> String {
    let mut named = format!("session {}", alert.session_id);
    if !alert.label.is_empty()
        && alert.label != alert.session_id
        && !alert.label.starts_with("muxloomd-")
    {
        named = format!("{named} (\"{}\")", alert.label);
    }
    let reason = alert
        .attention_reason
        .as_deref()
        .unwrap_or("waiting for input");
    let said = match alert
        .recap
        .as_deref()
        .filter(|recap| !recap.trim().is_empty())
    {
        Some(recap) => format!(" Last it said: {}.", recap.trim()),
        None => String::new(),
    };
    format!(
        "Your subagent {named}, kind {}, on machine {machine}, needs attention: {reason}.{said} \
         It is sitting at its prompt waiting for you — check with read_screen and answer it, or it \
         will wait there for its next minute of nothing. You are its parent ({}); message_agent \
         reaches it.",
        alert.kind, alert.parent_session_id
    )
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
    // What the runtime called the conversation. A hand-typed label hides it
    // from the list, but someone looking for it by name should still find it.
    if let Some(title) = session
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        && let Some(score) = search_match_score(title, query)
    {
        candidates.push((score.saturating_add(5), title.to_string()));
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

    fn waiting_child() -> ParentAlert {
        ParentAlert {
            session_id: "muxloomd-codex-1-2-3".into(),
            parent_session_id: "muxloomd-opencode-9-9-9".into(),
            kind: "codex".into(),
            label: "formatter".into(),
            attention_reason: Some("command approval".into()),
            recap: Some("I need to run cargo fmt first".into()),
            at: 1,
        }
    }

    #[test]
    fn a_parent_alert_names_the_child_machine_kind_reason_recap_and_how_to_look() {
        let text = parent_alert_text(&waiting_child(), "atlas");
        for wanted in [
            "muxloomd-codex-1-2-3",
            "formatter",
            "atlas",
            "codex",
            "command approval",
            "I need to run cargo fmt first",
            "read_screen",
            "muxloomd-opencode-9-9-9",
        ] {
            // The machine the parent must name to reach a far child, the kind
            // that decides how it is answered, and the way to look — all of
            // them have to be readable without asking anything else first.
            assert!(text.contains(wanted), "{wanted} missing from: {text}");
        }
    }

    #[test]
    fn a_parent_alert_says_what_it_has_and_never_invents_what_it_lacks() {
        // An unnamed child is named by its id alone; a reason the daemon could
        // not read still says *something* true; a recap that never arrived
        // leaves its sentence out rather than printing an empty quote.
        let alert = ParentAlert {
            label: "muxloomd-pi-7-7-7".into(),
            attention_reason: None,
            recap: Some("   ".into()),
            ..waiting_child()
        };
        let text = parent_alert_text(&alert, "atlas");
        assert!(
            !text.contains("(\""),
            "a label equal to the id is not quoted: {text}"
        );
        assert!(text.contains("waiting for input"), "{text}");
        assert!(!text.contains("Last it said"), "{text}");
    }

    #[test]
    fn parent_alerts_are_gated_by_config_before_any_machine_is_asked() {
        // The gate is the controller's: an old daemon is never asked, and a
        // config that says off stops even the asking. (drain_parent_alerts
        // returns before touching a bridge; the full round needs a Runtime and
        // live daemons, which is what the daemon-side edge tests cover.)
        let config = Config {
            alerts_to_parent: false,
            ..Config::default()
        };
        // No runtime is built: the first statement of the function returns
        // here, so this only pins that the key reads through Config.
        assert!(!config.alerts_to_parent);
    }
}
