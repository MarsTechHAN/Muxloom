use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    channel::{ChannelReceipt, ChannelSet},
    model::{Composer, DirectoryListing, FileListing, FilePreview},
    relay::{RelayAnswer, RelayJob, RelayPeer},
    talk::{TalkDeliver, TalkDraft, TalkFilter, TalkMessage, TalkPage, TalkState, TalkVector},
};

pub const PROTOCOL_VERSION: u16 = 1;
/// A daemon that watches its sessions for the moment they fall under a
/// subagent's parent's notice, and will hand those moments to a controller
/// that asks. Additive: a daemon too old to have it simply never gets asked.
pub const PARENT_ALERT_CAPABILITY: &str = "parent-alerts-v1";
pub const HEADER_LEN: usize = 28;
pub const MAX_FRAME_PAYLOAD: usize = 8 * 1024 * 1024;
pub const DATA_CHUNK_SIZE: usize = 64 * 1024;
pub const INITIAL_STREAM_WINDOW: u32 = 1024 * 1024;
pub const COMPRESSION_THRESHOLD: usize = 4 * 1024;

const MAGIC: [u8; 4] = *b"MXL1";
const FLAG_COMPRESSED_LZ4: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Request = 1,
    Response = 2,
    OpenStream = 3,
    Data = 4,
    WindowUpdate = 5,
    CloseStream = 6,
    Heartbeat = 7,
    Error = 8,
}

impl TryFrom<u8> for FrameKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            1 => Self::Request,
            2 => Self::Response,
            3 => Self::OpenStream,
            4 => Self::Data,
            5 => Self::WindowUpdate,
            6 => Self::CloseStream,
            7 => Self::Heartbeat,
            8 => Self::Error,
            other => bail!("unsupported muxloom frame kind {other}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub flags: u8,
    pub stream_id: u32,
    pub request_id: u64,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameKind, stream_id: u32, request_id: u64, payload: Vec<u8>) -> Self {
        Self {
            kind,
            flags: 0,
            stream_id,
            request_id,
            payload,
        }
    }

    pub fn data(stream_id: u32, request_id: u64, payload: &[u8], compress: bool) -> Self {
        if compress && payload.len() >= COMPRESSION_THRESHOLD {
            let compressed = lz4_flex::compress_prepend_size(payload);
            if compressed.len() < payload.len() {
                return Self {
                    kind: FrameKind::Data,
                    flags: FLAG_COMPRESSED_LZ4,
                    stream_id,
                    request_id,
                    payload: compressed,
                };
            }
        }
        Self::new(FrameKind::Data, stream_id, request_id, payload.to_vec())
    }

    pub fn decoded_payload(&self) -> Result<Vec<u8>> {
        if self.flags & !FLAG_COMPRESSED_LZ4 != 0 {
            bail!("muxloom frame has unsupported flags {:#x}", self.flags);
        }
        if self.flags & FLAG_COMPRESSED_LZ4 == 0 {
            return Ok(self.payload.clone());
        }
        lz4_flex::decompress_size_prepended(&self.payload)
            .context("failed to decompress muxloom LZ4 frame")
    }

    pub fn window_update(stream_id: u32, credit: u32) -> Self {
        Self::new(
            FrameKind::WindowUpdate,
            stream_id,
            0,
            credit.to_be_bytes().to_vec(),
        )
    }

    pub fn window_credit(&self) -> Result<u32> {
        if self.kind != FrameKind::WindowUpdate || self.payload.len() != 4 {
            bail!("invalid stream window update");
        }
        Ok(u32::from_be_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]))
    }

    pub fn json<T: Serialize>(
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        value: &T,
    ) -> Result<Self> {
        let payload = serde_json::to_vec(value).context("failed to encode daemon message")?;
        if payload.len() > MAX_FRAME_PAYLOAD {
            bail!("daemon message exceeds maximum frame size");
        }
        Ok(Self::new(kind, stream_id, request_id, payload))
    }

    pub fn decode_json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.payload).context("failed to decode daemon message")
    }

    pub fn read_from(reader: &mut impl Read) -> Result<Option<Self>> {
        let mut header = [0_u8; HEADER_LEN];
        if !read_exact_or_eof(reader, &mut header)? {
            return Ok(None);
        }
        if header[..4] != MAGIC {
            bail!("invalid muxloom frame magic");
        }
        let version = u16::from_be_bytes([header[4], header[5]]);
        if version != PROTOCOL_VERSION {
            bail!("unsupported muxloom protocol version {version}");
        }
        let kind = FrameKind::try_from(header[6])?;
        let flags = header[7];
        let stream_id = u32::from_be_bytes(header[8..12].try_into().unwrap());
        let request_id = u64::from_be_bytes(header[12..20].try_into().unwrap());
        let payload_len = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
        if header[24..28] != [0, 0, 0, 0] {
            bail!("muxloom frame has non-zero reserved bytes");
        }
        if payload_len > MAX_FRAME_PAYLOAD {
            bail!("muxloom frame payload is too large: {payload_len}");
        }
        let mut payload = vec![0; payload_len];
        reader
            .read_exact(&mut payload)
            .context("truncated muxloom frame payload")?;
        Ok(Some(Self {
            kind,
            flags,
            stream_id,
            request_id,
            payload,
        }))
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<()> {
        if self.payload.len() > MAX_FRAME_PAYLOAD {
            bail!("muxloom frame payload is too large: {}", self.payload.len());
        }
        let mut header = [0_u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        header[6] = self.kind as u8;
        header[7] = self.flags;
        header[8..12].copy_from_slice(&self.stream_id.to_be_bytes());
        header[12..20].copy_from_slice(&self.request_id.to_be_bytes());
        header[20..24].copy_from_slice(&(self.payload.len() as u32).to_be_bytes());
        writer
            .write_all(&header)
            .context("failed to write muxloom frame header")?;
        writer
            .write_all(&self.payload)
            .context("failed to write muxloom frame payload")?;
        writer.flush().context("failed to flush muxloom frame")
    }
}

fn read_exact_or_eof(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<bool> {
    let mut offset = 0;
    while offset < buffer.len() {
        match reader.read(&mut buffer[offset..])? {
            0 if offset == 0 => return Ok(false),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated muxloom frame header",
                ));
            }
            read => offset += read,
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum DaemonRequest {
    Hello {
        client_version: String,
        protocol_version: u16,
    },
    Ping,
    Status,
    PrepareHandover,
    ProbeExecutables {
        executables: Vec<String>,
    },
    ListTcpListeners,
    ListSessions {
        /// Leave out the archive: the records this daemon loaded off disk when
        /// it started, which nothing running can change.
        ///
        /// A dashboard asks what the sessions are doing three times a second,
        /// and every one of those answers used to carry the machine's whole
        /// history with it — a record per conversation ever held there, none of
        /// which has moved since the daemon read it, all of them serialized,
        /// written down a socket (an ssh one, for another machine) and parsed
        /// again to be thrown away. What changes on that cadence is what is
        /// running, and that is all this asks for.
        ///
        /// Absent from a client too old to send one, and absent means the whole
        /// list — which is also what a daemon too old to read it will answer.
        #[serde(default)]
        live_only: bool,
        /// One session, by id, when the round is about one session.
        ///
        /// Leaving the archive out saved carrying records nothing can change;
        /// what is left is the expensive half. Answering for a running session
        /// means drawing its screen and laying the grid out as text, which is
        /// most of what reading one costs, and a round that wants a single id —
        /// a wait polling for it once a second, a message asking what the
        /// session sending it is called — paid that for every session on the
        /// machine to throw all but one away.
        ///
        /// Absent means all of them, so a client too old to send this and a
        /// daemon too old to read it both still get the list they expect and
        /// find their id in it.
        #[serde(default)]
        only: Option<String>,
    },
    Launch {
        session_id: String,
        kind: String,
        path: String,
        label: String,
        #[serde(default)]
        temporary: bool,
        executable: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
        columns: u16,
        rows: u16,
        /// The session that asked for this one, when an agent started it
        /// rather than a person. Absent from a client too old to send one, and
        /// from every launch a human makes.
        #[serde(default)]
        parent: Option<String>,
        /// What the agent starting this session is handing it: how far it may
        /// write, what it may start, whether it may reach the person. Already
        /// narrowed against what the caller itself holds by the time it gets
        /// here — this is the grant, not the request. Absent from every launch
        /// a person makes and from a client too old to send one, and absent is
        /// what full powers look like.
        #[serde(default)]
        powers: Option<crate::model::Powers>,
        /// A prompt the runtime left out of the command line because that CLI
        /// reads a positional argument as something else — OpenCode reads it as
        /// the project directory and dies on the spot. The daemon types it in
        /// once the session shows a ready prompt box, on the same path a queued
        /// direct message takes. Absent from a client too old to send one.
        #[serde(default)]
        initial_prompt: Option<String>,
    },
    Resize {
        session_id: String,
        columns: u16,
        rows: u16,
    },
    /// Write bytes to the session's PTY without attaching a stream. An attach
    /// resizes the PTY to the subscriber's geometry, so a client that only
    /// wants to type — the MCP surface driving an agent — must not open one.
    SendInput {
        session_id: String,
        bytes: Vec<u8>,
    },
    /// Attention patterns the daemon applies when classifying sessions, so
    /// waiting states surface at the daemon's own refresh cadence instead of
    /// the controller's slower full scans.
    SetAttentionPatterns {
        patterns: Vec<String>,
    },
    ReadHistory {
        session_id: String,
        offset_from_bottom: usize,
        lines: usize,
        /// Ask for the rows a terminal would have shown instead of raw log
        /// lines, so `offset_from_bottom` and `lines` count rendered rows. A
        /// daemon that predates this ignores it and answers in log lines,
        /// which [`DaemonResponse::HistoryComplete::rendered`] reports back.
        #[serde(default)]
        rendered: bool,
    },
    SearchHistory {
        session_id: String,
        query: String,
        max_matches: usize,
    },
    ListDirectory {
        path: String,
    },
    ListFiles {
        path: String,
    },
    PreviewFile {
        path: String,
        limit: usize,
    },
    Archive {
        session_id: String,
    },
    Delete {
        session_id: String,
    },
    /// Set the head name (the dashboard row's label) for a session. Additive:
    /// older daemons simply never receive this and ignore it.
    SetLabel {
        session_id: String,
        label: String,
    },
    RunShell {
        script: String,
        environment: Vec<(String, String)>,
    },
    /// Arm a standing watch on a session's screen. The daemon keeps it after
    /// the client that asked for it is gone, which is the whole point: an
    /// agent that has finished its turn is not there to look.
    SetTrigger {
        trigger: Trigger,
    },
    ListTriggers {
        #[serde(default)]
        session_id: Option<String>,
    },
    DeleteTrigger {
        id: String,
    },
    /// Write one message to this machine's talk board.
    TalkPost {
        draft: TalkDraft,
    },
    /// Read the board as someone standing on this machine sees it.
    TalkRead {
        filter: TalkFilter,
    },
    /// What this machine's board holds, so a peer can work out the difference.
    /// A controller sends down the name a human uses for the machine at the
    /// same time: it is the only thing that knows it.
    TalkStatus {
        #[serde(default)]
        label: Option<String>,
    },
    /// Everything held past a peer's version vector.
    TalkFetch {
        from: TalkVector,
        limit: usize,
    },
    /// File messages minted elsewhere. Idempotent, so a carrier never has to
    /// remember what it has already delivered.
    TalkAppend {
        messages: Vec<TalkMessage>,
    },
    /// Put a message in front of an agent session on this machine. The draft
    /// says who is speaking and `to` says which session; the daemon renders
    /// the envelope, decides whether the session is free enough to be typed
    /// into, and files the message on its board either way.
    TalkDeliver {
        draft: TalkDraft,
        #[serde(default)]
        deliver: TalkDeliver,
        /// Whether the envelope should say the sender is waiting on an answer.
        #[serde(default)]
        reply_expected: bool,
    },
    /// Ask whichever controller is watching this machine to run one tool
    /// somewhere this daemon cannot reach. Refused on the spot if no
    /// controller is asking for work, or if the tool is not one a controller
    /// runs on another machine's behalf.
    RelaySubmit {
        tool: String,
        arguments: String,
        /// The session that asked, when the daemon surface had one in
        /// context. Reaches the controller so a "always for this
        /// conversation" is scoped to the asking agent.
        #[serde(default)]
        session: String,
    },
    /// A controller asking what it can carry, and saying where it can reach
    /// while it is here. Answering it is also how a daemon learns a controller
    /// is there at all, and the peer list is the only way it ever learns that
    /// another machine exists — it never looks for one itself.
    ///
    /// Every field is an addition: a daemon from before them reads this as the
    /// bare ask it always was, and simply goes on knowing only itself.
    RelayPoll {
        #[serde(default)]
        peers: Vec<RelayPeer>,
        /// What the controller calls itself, for saying which way a machine is
        /// reached. Two controllers on one host say the same thing here, and
        /// should: it is the name of the way, not of the traveller.
        #[serde(default)]
        via: String,
        /// Which controller this is, as distinct from what it is called.
        ///
        /// A daemon keeps what each controller reaches under its own name, so
        /// that one of them naming its fleet does not erase another's. `via` is
        /// a host, and two controllers on one host share it — so keyed by
        /// `via`, the second one to come round overwrites the first, and every
        /// machine only the first could reach stops being reachable at all.
        /// Empty from a controller too old to say, which is then keyed the way
        /// it always was.
        #[serde(default)]
        who: String,
    },
    /// A controller handing back what a job produced.
    RelayComplete {
        id: String,
        ok: bool,
        output: String,
    },
    /// The submitter asking whether its job has come back.
    RelayResult {
        id: String,
    },
    /// An agent on this machine asking which other machines it can reach, which
    /// is whatever the last controller round named.
    RelayPeers,
    /// A controller handing this machine the channels the fleet may speak
    /// through, secrets included: an agent here sends its own messages, so the
    /// credentials have to be here. Kept in a `0600` file beside the sessions.
    ChannelsPut {
        set: ChannelSet,
    },
    /// What this machine holds, so a controller can tell whether it is in step
    /// without pushing the set again. The answer carries no secret.
    ChannelsGet,
    /// An agent here reporting that it put a message in front of the human, so
    /// that a reply to that message comes back to it rather than to the board.
    /// The MCP surface is its own process, so the daemon is where this rests
    /// until a dashboard comes round for it.
    ChannelSent {
        receipt: ChannelReceipt,
    },
    /// A controller taking the attention edges this daemon has marked but not
    /// yet handed over: child sessions that fell under their parent's notice
    /// since the last ask. Answering hands them over and forgets them, the way
    /// `ChannelsGet` hands over receipts; a controller that never delivers one
    /// will hear about the same session again on the daemon's reminder
    /// schedule, because the child is still sitting on its question.
    DrainAlerts,
}

/// What a [`Trigger`] does when its pattern reaches a session's screen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerAction {
    /// Type back into the session that matched.
    SendInput {
        text: String,
        #[serde(default)]
        submit: bool,
    },
    /// Flag the session as waiting for someone, with this as the reason. It
    /// reads exactly like the built-in classification, so it reaches every
    /// dashboard and every `list_sessions` without another channel.
    Notify { text: String },
}

/// A pattern the daemon watches one session's screen for, and what it does
/// when it appears. Triggers are persisted, so they outlive the daemon that
/// took them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Trigger {
    /// Assigned by the daemon when a client leaves it empty.
    #[serde(default)]
    pub id: String,
    pub session_id: String,
    pub pattern: String,
    pub action: TriggerAction,
    /// Whether the trigger is removed once it has fired.
    #[serde(default)]
    pub once: bool,
    /// The shortest gap between two firings of the same trigger.
    #[serde(default)]
    pub cooldown_ms: u64,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_fired_at: Option<u64>,
    #[serde(default)]
    pub fires: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DaemonResponse {
    Hello {
        daemon_version: String,
        protocol_version: u16,
        pid: u32,
        capabilities: Vec<String>,
        /// The full generation stamp of the running daemon, which says which
        /// build it is and not merely which version. Empty from a daemon old
        /// enough not to send one — and one of those is exactly what a fleet
        /// stuck between two releases is made of, so the reader must cope.
        #[serde(default)]
        daemon_generation: String,
    },
    Pong {
        unix_time_ms: u64,
    },
    Status {
        pid: u32,
        uptime_ms: u64,
        clients: usize,
    },
    HandoverReady,
    HandoverDeferred,
    Executables {
        available: Vec<String>,
    },
    TcpListeners {
        ports: Vec<u16>,
    },
    Sessions {
        sessions: Vec<DaemonSession>,
    },
    Launched {
        session: Box<DaemonSession>,
    },
    Ack,
    HistoryComplete {
        total_lines: usize,
        columns: u16,
        rows: u16,
        offset_from_bottom: usize,
        /// Whether the page holds rendered rows. Absent from daemons that only
        /// read raw log lines, so it defaults to false and a client can tell
        /// the two apart.
        #[serde(default)]
        rendered: bool,
        /// Whether the read started at the beginning of the log, which is what
        /// makes `total_lines` the whole history rather than the reach of this
        /// one page. Absent from daemons that predate it, where it defaults to
        /// false and a page is read as possibly having older rows above it.
        #[serde(default)]
        reached_start: bool,
    },
    HistoryMatches {
        matches: Vec<DaemonHistoryMatch>,
    },
    Directory {
        listing: DirectoryListing,
    },
    Files {
        listing: FileListing,
    },
    Preview {
        preview: FilePreview,
    },
    ShellComplete {
        exit_code: i32,
    },
    /// Answers both `SetTrigger` (the one stored, with the id it was given)
    /// and `ListTriggers`.
    Triggers {
        triggers: Vec<Trigger>,
    },
    /// Answers `TalkRead`, and `TalkPost` with the one message it minted.
    Talk {
        page: TalkPage,
    },
    /// Answers `TalkStatus`.
    TalkBoard {
        state: TalkState,
    },
    /// Answers `TalkFetch`, and `TalkAppend` with how many were new.
    TalkCarry {
        #[serde(default)]
        messages: Vec<TalkMessage>,
        #[serde(default)]
        added: usize,
    },
    /// Answers `TalkDeliver`: the message as it was filed, and whether it went
    /// into the session or is waiting for it to be free.
    TalkDelivery {
        /// Boxed only to keep one message from setting the size of every
        /// response that crosses the socket.
        message: Box<TalkMessage>,
        /// `delivered` or `queued`.
        delivery: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Answers `RelaySubmit`: what to ask about later.
    RelayTicket {
        id: String,
    },
    /// Answers `RelayPoll`.
    RelayWork {
        #[serde(default)]
        jobs: Vec<RelayJob>,
        /// Machines this daemon has been told about, each stamped with the
        /// controller offering it. A controller reads back its own reach here
        /// and ignores it; what is left are the machines somebody else can
        /// carry a call to, which is how a dashboard learns the fleet is
        /// bigger than the machines it can open itself.
        #[serde(default)]
        known: Vec<RelayPeer>,
    },
    /// Answers `RelayResult`.
    Relayed {
        answer: RelayAnswer,
    },
    /// Answers `RelayPeers`: every machine a controller has come round and
    /// said it could reach, each stamped with the way there, and whether a
    /// controller is here at all right now.
    RelayReach {
        #[serde(default)]
        peers: Vec<RelayPeer>,
        #[serde(default)]
        attached: bool,
    },
    /// Answers `ChannelsGet`: the revision this machine holds and what the
    /// bindings are for, with every secret blanked. The file on the machine is
    /// the only place a secret rests.
    Channels {
        #[serde(default)]
        set: ChannelSet,
        /// What agents here have said to the human since a dashboard last
        /// asked, handed over and forgotten in the same breath. Empty from a
        /// daemon that has none, which includes every daemon too old to keep
        /// them.
        #[serde(default)]
        receipts: Vec<ChannelReceipt>,
    },
    /// Answers `DrainAlerts`. Empty from a daemon that has watched and seen
    /// nothing, which is the usual answer and the same one a daemon too old
    /// to watch would give.
    Alerts {
        #[serde(default)]
        alerts: Vec<ParentAlert>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonSession {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub label: String,
    #[serde(default)]
    pub temporary: bool,
    pub created_at: u64,
    /// When this session stopped being live, in seconds, whether it was
    /// archived by hand or its process ended on its own. An archive is read
    /// newest-put-down first — the conversation somebody just closed is the
    /// one they come back for — and that is a different order from when each
    /// of them began. `None` on a record archived before this was written
    /// down, and on every live session.
    #[serde(default)]
    pub archived_at: Option<u64>,
    pub pid: Option<u32>,
    pub dead: bool,
    pub archived: bool,
    pub recap: Option<String>,
    /// The name the runtime gave the conversation, read out of the transcript
    /// it keeps. Absent from a daemon too old to read one, and from a session
    /// whose runtime writes no transcript.
    #[serde(default)]
    pub title: Option<String>,
    /// The transcript this session was matched to, so a restarted daemon can
    /// go on reading the same one instead of matching it again from scratch.
    #[serde(default)]
    pub thread: Option<String>,
    /// The conversation the launch was told to reopen, taken off the command
    /// line. It is the only durable account of what a `--resume` meant: the
    /// command line belongs to a keeper the next daemon did not spawn, and a
    /// resumed conversation began long before the session did, so a daemon
    /// that restarted before the two were matched could never match them by
    /// when each started.
    #[serde(default)]
    pub seed: Option<String>,
    /// The first substantial text the daemon typed into this session, in its
    /// own hearing. The runtime's transcript records the same words as the
    /// first thing the person said in the conversation, so this is what lets
    /// a session check its claim against content instead of timing - and it
    /// has to outlive the daemon that heard it, because the round that
    /// matches can come after a restart.
    #[serde(default)]
    pub first_prompt: Option<String>,
    pub working: bool,
    pub needs_attention: bool,
    pub attention_reason: Option<String>,
    /// Where the runtime's prompt box is and what is in it, which is what
    /// decides whether a message can be put in front of this session right now.
    /// Absent from a daemon too old to read one.
    #[serde(default)]
    pub composer: Option<Composer>,
    /// The session that started this one, when an agent did. Work an agent
    /// hands off is still that agent's work, and this is the only record of
    /// which agent it was: the child's own transcript never mentions it.
    ///
    /// It names a session id and nothing else. A child started on another
    /// machine keeps the id of the agent that asked for it even though the two
    /// are not on the same daemon; the id is enough to say they belong to
    /// the same piece of work, which is what a reader wants to know.
    #[serde(default)]
    pub parent: Option<String>,
    /// What the agent that started it handed over, kept so a resume can hand
    /// the same thing again. The grant is stamped into the session's
    /// environment at launch and read from there for the session's life, but a
    /// resumed session is launched afresh with an environment built from the
    /// config — so without this on the record, coming back would quietly
    /// restore a subagent to full powers. Absent from every session nobody
    /// started, and from records a daemon too old to keep one wrote.
    #[serde(default)]
    pub powers: Option<crate::model::Powers>,
    /// The archived session this one resumes, when a conversation came back
    /// under a new muxloom id rather than its own. The alias is recorded on
    /// both sides - `resumed_to` on the record it moved out of, `resumed_from`
    /// on the session that carries it now - so late rewrites and board
    /// archaeology still resolve either direction. Absent from every session
    /// that never moved, and from daemons too old to record it.
    #[serde(default)]
    pub resumed_from: Option<String>,
    /// The session this archived record's conversation moved to, when the
    /// resume could not come back on this id. What still points at this id
    /// - children above all - is repointed at the successor as the alias is
    ///   written, so a split master never strands its fleet.
    #[serde(default)]
    pub resumed_to: Option<String>,
}

/// One attention edge the daemon saw but has not told anyone about yet: a
/// session with a parent that started needing someone. It carries just enough
/// for a controller to say what happened in its own words; the daemon has
/// already decided *that* it happened, which is the part only this machine
/// could know.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParentAlert {
    pub session_id: String,
    /// The session to tell. Always one this daemon also runs: a child is
    /// launched on its parent's machine.
    pub parent_session_id: String,
    pub kind: String,
    pub label: String,
    pub attention_reason: Option<String>,
    /// The last thing the child was seen to say, for the recap line.
    #[serde(default)]
    pub recap: Option<String>,
    /// When the daemon marked the edge (epoch ms).
    pub at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum OpenStream {
    Pty {
        session_id: String,
        columns: u16,
        rows: u16,
        /// Rows of rendered scrollback to replay ahead of the retained raw
        /// output. Absent from older clients, which get the raw output alone.
        #[serde(default)]
        scrollback_rows: usize,
    },
    File {
        path: String,
        offset: u64,
        length: Option<u64>,
    },
    Media {
        path: String,
        offset: u64,
        length: Option<u64>,
    },
    Upload {
        path: String,
        size: u64,
    },
    Tcp {
        host: String,
        port: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamOpened {
    pub initial_window: u32,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonHistoryMatch {
    pub recap: bool,
    pub line_number: usize,
    pub text: String,
}

pub mod stream {
    pub const STDOUT: u32 = 1;
    pub const STDERR: u32 = 2;
    pub const HISTORY: u32 = 3;
    pub const PTY_BASE: u32 = 1024;
    pub const FILE_BASE: u32 = 1 << 20;
    pub const MEDIA_BASE: u32 = 1 << 24;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_preserves_routing_fields_and_binary_payload() {
        let frame = Frame {
            kind: FrameKind::Data,
            flags: 3,
            stream_id: 42,
            request_id: 9001,
            payload: vec![0, 1, 2, 255],
        };
        let mut bytes = Vec::new();
        frame.write_to(&mut bytes).unwrap();
        let decoded = Frame::read_from(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn rejects_oversized_frames_before_allocating_payload() {
        let mut bytes = vec![0; HEADER_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[6] = FrameKind::Data as u8;
        bytes[20..24].copy_from_slice(&((MAX_FRAME_PAYLOAD + 1) as u32).to_be_bytes());
        assert!(Frame::read_from(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn json_messages_are_versioned_independently_from_frames() {
        let request = DaemonRequest::Hello {
            client_version: "0.3.0".into(),
            protocol_version: PROTOCOL_VERSION,
        };
        let frame = Frame::json(FrameKind::Request, 0, 7, &request).unwrap();
        assert_eq!(frame.decode_json::<DaemonRequest>().unwrap(), request);
    }

    #[test]
    fn repetitive_data_is_compressed_but_small_interactive_data_is_not() {
        let large = vec![b'x'; COMPRESSION_THRESHOLD * 4];
        let frame = Frame::data(stream::FILE_BASE, 4, &large, true);
        assert_ne!(frame.flags & FLAG_COMPRESSED_LZ4, 0);
        assert!(frame.payload.len() < large.len());
        assert_eq!(frame.decoded_payload().unwrap(), large);

        let input = Frame::data(stream::PTY_BASE, 5, b"ls\r", true);
        assert_eq!(input.flags, 0);
        assert_eq!(input.decoded_payload().unwrap(), b"ls\r");
    }
}
