use std::{fs, path::Path, time::UNIX_EPOCH};

use crate::daemon_protocol::PROTOCOL_VERSION;

/// What this build stamps into the state directory while it is serving, so the
/// next client can tell whether it is talking to its own build.
///
/// The fourth field is what orders two of them. CI stamps in the number of
/// commits behind the build, which is the only thing that tells two nightlies
/// carrying the same package version apart; a build made by hand says `local`
/// and ranks above every numbered build of its version, because somebody made
/// it deliberately and means it to be in front.
///
/// Ordering is not identity, though, and only CI fills those fields in. Every
/// `cargo build` of one version stamped the same `local:local`, so a daemon
/// compiled from yesterday's source read as this build's own and went on
/// serving: the fix was in the binary, the running code was the old one, and
/// nothing short of stopping the daemon by hand changed that. So the file
/// itself ends the stamp — what it is called, and then the same size and write
/// time [`stash_executable`] has always used to tell one build's copy from
/// another's.
///
/// Outside the Unix-only module because the controller asks it too, to see
/// whether the daemon on a machine it is watching is behind this build. The
/// controller and the daemon are two files, so that comparison reads the rank
/// and never the whole stamp.
pub fn current_generation() -> String {
    format!(
        "{}:protocol-{}:{}:{}:{}",
        env!("CARGO_PKG_VERSION"),
        PROTOCOL_VERSION,
        option_env!("MUXLOOM_BUILD_ID").unwrap_or("local"),
        option_env!("MUXLOOM_BUILD_HEIGHT").unwrap_or("local"),
        running_executable_identity(),
    )
}

/// Which file this process is running, as the stamp spells it: what the
/// executable is called, then which copy of it.
///
/// The name is there so two stamps can tell whether they describe the same file
/// at all. Only copies of one file can be read as one being the other's
/// rebuild; a controller and the companion beside it are two files from one
/// commit, and reading those as rebuilds of each other is how a machine comes
/// to hand its daemon back and forth all day.
///
/// `unknown` when the executable cannot be read at all — a package manager that
/// replaced it mid-life is the ordinary reason. It names no file, so two builds
/// that both say `unknown` are not each other's rebuild either: they compare by
/// rank alone and settle, rather than handing over on every connection.
fn running_executable_identity() -> String {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(|running| {
            let copy = file_identity(running)?;
            let name = running.file_name()?.to_string_lossy();
            Some(format!("{name}{IDENTITY_SEPARATOR}{copy}"))
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Between what the executable is called and which copy of it is running. Not
/// a character the stamp's own fields are cut on, and not one a file that gets
/// this far is named with.
const IDENTITY_SEPARATOR: char = '@';

/// What a stamp calls the executable it was made by, or `None` for one that
/// cannot say: written before the name was part of it, or written by a process
/// whose own file had already been replaced. Neither can claim to be a copy of
/// anything, so neither is ever read as the same file as another.
pub fn stamped_executable_name(stamp: &str) -> Option<&str> {
    stamp
        .trim()
        .split(':')
        .nth(4)?
        .split_once(IDENTITY_SEPARATOR)
        .map(|(name, _copy)| name)
        .filter(|name| !name.is_empty())
}

/// What tells one file apart from another that has taken its place: how big it
/// is and when it was written.
fn file_identity(path: &Path) -> Option<String> {
    let file = fs::metadata(path).ok()?;
    let written = file
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(format!("{}-{written}", file.len()))
}

#[cfg(unix)]
mod platform {
    use super::{current_generation, stamped_executable_name};
    use std::{
        cmp::Reverse,
        collections::{BTreeSet, HashMap},
        fs::{self, File, OpenOptions},
        io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
        net::{Shutdown, TcpStream, ToSocketAddrs},
        os::unix::{
            fs::PermissionsExt,
            io::AsRawFd,
            net::{UnixListener, UnixStream},
            process::CommandExt,
            process::ExitStatusExt,
        },
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::{
            Arc, Condvar, Mutex, OnceLock, RwLock,
            atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result, anyhow, bail};

    use crate::{
        channel::{CHANNELS_CAPABILITY, ChannelSet},
        daemon_protocol::{
            DATA_CHUNK_SIZE, DaemonHistoryMatch, DaemonHistorySearchHit, DaemonRequest,
            DaemonResponse, DaemonSession, Frame, FrameKind, HISTORY_SEARCH_CAPABILITY,
            INITIAL_STREAM_WINDOW, LINEAGE_CAPABILITY, OpenStream, PARENT_ALERT_CAPABILITY,
            PROTOCOL_VERSION, ParentAlert, StreamOpened, Trigger, TriggerAction, stream,
        },
        keeper,
        model::{
            AgentKind, Composer, DirectoryListing, FileEntry, FileEntryKind, FileListing,
            FilePreview, FilePreviewKind, generation_rank,
        },
        native_history::SessionFacts as NativeFacts,
        recap::extract_recap,
        relay::{RELAY_CAPABILITY, RelayQueue},
        runtime::{
            DAEMON_SESSION_PREFIX, agent_is_working, attention_reason, composer, composer_text,
            is_temporary_session_id, working_marker_is_held,
        },
        talk::{
            DIRECT_CAPABILITY, TALK_CAPABILITY, TalkAddress, TalkAuthor, TalkDeliver, TalkDraft,
            TalkKind, TalkMessage, TalkPage, TalkQueued, TalkScope, TalkStore, TalkUndelivered,
            TalkVoice, folded, paste_bytes, render_bounce, render_delivery,
        },
        terminal_session::{CodexActivity, InlineScrollback, render_history_rows, resize_parser},
    };

    const RECENT_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
    /// How recently the PTY must have produced output for a session whose
    /// screen shows a *ticking* working marker (a spinner, an elapsed counter)
    /// to count as working. A CLI redraws those about once a second for the
    /// whole of a turn, so one that has gone this quiet is not drawing them.
    const WORKING_TICKING_QUIET_MS: u64 = 15_000;
    /// The same, for a marker the CLI paints once and holds: an interrupt hint
    /// on a status bar. A turn that shells out to a build, or waits on a model
    /// that streams nothing back, sits silent for minutes with that hint on
    /// screen the whole time, and calling that quiet "stopped" is what makes a
    /// working agent report itself finished. What the bound still catches is
    /// the frame frozen by a process that died or wedged mid-turn.
    const WORKING_HELD_QUIET_MS: u64 = 10 * 60_000;
    /// The least of a session's log to render for a page, however few rows the
    /// page holds. Small enough that the shallowest ask pays about what it is
    /// worth, and large enough that a plainly written transcript fills a screen
    /// from it without reading again.
    const SCROLLBACK_SEED_BYTES_MIN: u64 = 256 * 1024;
    /// What one row of a page is guessed to have cost in the log, per column.
    /// Rows are stored as what was painted, escapes and all, so this is a
    /// starting guess and the widening below is what makes it right; it is set
    /// high enough that a page of ordinary output is reached on the first read.
    const SCROLLBACK_SEED_BYTES_PER_CELL: u64 = 16;
    /// The most of it to render. How much output a session spends per finished
    /// line varies by three orders of magnitude between agents, so the window is
    /// measured out in rows below and this only stops a log that has been
    /// growing for days from being read back to its beginning: the render costs
    /// roughly two seconds here, and an attach waits for it.
    const SCROLLBACK_SEED_BYTES_MAX: u64 = 128 * 1024 * 1024;
    /// How much of one log line a search holds at a time. A capture is not
    /// really made of lines: an agent that paints its screen puts the cursor
    /// back where it wants it instead of ending a line, so a whole session can
    /// arrive as a single line hundreds of megabytes long. Reading such a line
    /// whole to look for a word in it means holding all of it at once, and case
    /// folding it means holding it twice over again.
    const SEARCH_LINE_CHUNK: usize = 256 * 1024;
    static METADATA_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
    /// Opening a TCP stream to a host the far end can reach. Served by the
    /// daemon, or by the bridge when it talks to a daemon too old to have it.
    const FORWARD_CAPABILITY: &str = "tcp-forward-v1";
    /// Reporting which TCP ports the far end is listening on.
    const LISTENERS_CAPABILITY: &str = "tcp-listeners-v1";
    /// The most triggers one daemon keeps at a time. Each one costs a screen
    /// render per output frame on the session it watches; a set this long is a
    /// client that stopped cleaning up after itself.
    const TRIGGER_LIMIT: usize = 64;
    /// The shortest gap a trigger may ask for between two firings.
    const TRIGGER_MIN_COOLDOWN_MS: u64 = 250;
    static TRIGGER_COUNTER: AtomicU64 = AtomicU64::new(0);
    /// How long one sender waits before it may interrupt the same session
    /// again. Typing into a session costs whoever works there their attention,
    /// and two agents answering each other would spend it in a loop.
    const DIRECT_INTERVAL_MS: u64 = 10_000;
    /// When a child that stays under its parent's notice may ask again. The
    /// first ask is an edge and fires as the classification turns; this is the
    /// reminder schedule after it - 60 seconds, then 5 minutes, then 15 - and
    /// the whole length of the leash: a fourth tell, and then silence until
    /// the child is waiting on something actually *different*. The schedule
    /// exists because one stuck screen must not become a minute-ticker at its
    /// parent: an hour of an unanswered question costs four tells, not sixty.
    const PARENT_ALERT_REMINDERS_MS: [u64; 3] = [60_000, 300_000, 900_000];
    /// How long a child has to be read as *not* waiting before whatever it is
    /// waiting on afterwards counts as a different question. A leash that any
    /// changed reading can unclip is not a leash: what a session is waiting on
    /// is read off a screen and out of a transcript, and those two disagree
    /// with each other for reasons that have nothing to do with the child -
    /// a claim dropped and retaken, a modal over the last thing said. The one
    /// thing that cannot be a misreading is the child having stopped waiting,
    /// long enough that no blink is that long.
    const PARENT_ALERT_SETTLE_MS: u64 = PARENT_ALERT_REMINDERS_MS[0];
    /// The most messages that may be waiting for sessions to be free. Past
    /// this something is queueing faster than the machine can read, and the
    /// oldest of them are already stale.
    const OUTBOX_LIMIT: usize = 128;
    /// How often a queued message looks at whether its session has finished.
    const OUTBOX_POLL: Duration = Duration::from_secs(1);
    /// How long an unsent draft may sit unchanged before a queued message
    /// stops waiting for it: a box still changing has somebody typing into it,
    /// and typing behind them would fold our message into theirs; a box that
    /// has stopped changing is a draft nobody is coming back to, and holding a
    /// waiting message for it any longer is stranger than delivering.
    const DELIVER_STALE_DRAFT_MS: u64 = 15_000;
    /// How often a session's runtime is asked what it has been writing about
    /// itself. Soon enough that a finished turn shows up under the session
    /// while whoever asked for it is still looking, rarely enough that the
    /// reading costs nothing.
    const NATIVE_POLL: Duration = Duration::from_secs(5);
    /// How long a session must have been quiet, after answering something it
    /// was asked and writing none of it down, before muxloom takes it to be
    /// writing somewhere else. Clearing a conversation does exactly that: the
    /// old transcript is closed where it stands and a new one begins. The wait
    /// is what lets a transcript that is slow to be flushed catch up.
    const NATIVE_CLAIM_STALE_MS: u64 = 60_000;
    /// How many folder re-scans one claim may ask for while it is still a
    /// timing guess - a claim never weighed against the transcript's own
    /// first words. A crossed pair needs one round where both transcripts
    /// have said their first thing and both sessions know what they were
    /// asked; a transcript that says nothing at all is answered by the
    /// screen, not by asking the folder forever.
    const NATIVE_CLAIM_CHECK_LOOKS: u8 = 4;
    /// How long a write to a client may make no progress at all before that
    /// client is taken to be gone.
    ///
    /// A session's frames are written by the thread reading its keeper, and that
    /// thread is the one draining the PTY. A dashboard that stops reading -
    /// suspended, or on a link that died without saying so - fills its socket,
    /// which stops that thread, which fills the keeper's, which stops the keeper
    /// reading the PTY, which blocks the agent on its own output. So the agent
    /// freezes because somebody stopped watching it, and nothing times out.
    ///
    /// The window has to be wide enough for a real link carrying a real frame -
    /// a screen snapshot, a page of history, a file - and it only ever elapses
    /// where not one byte moved in all of it.
    const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(20);

    #[derive(Debug, Clone)]
    pub struct DaemonPaths {
        pub root: PathBuf,
        pub socket: PathBuf,
        pub pid: PathBuf,
        /// Held open and locked for as long as a daemon serves this directory,
        /// and never removed. See [`hold_the_serving_lock`].
        pub lock: PathBuf,
        pub log: PathBuf,
        pub generation: PathBuf,
        /// Which generation a newer build has asked to replace, and when it
        /// first asked. See [`handover_ask_age`].
        pub handover: PathBuf,
        pub history: PathBuf,
        pub sessions: PathBuf,
        pub keepers: PathBuf,
        /// One working directory per temporary session, made when it launches
        /// and removed with it. A Temporal Chat is a scratch pad, so it gets a
        /// scratch folder rather than moving into whichever project happened to
        /// be selected when it was started.
        pub scratch: PathBuf,
        pub triggers: PathBuf,
        pub talk: PathBuf,
        pub outbox: PathBuf,
        /// The channels an agent here may speak to a human through, as the last
        /// controller round left them. Secrets, so `0600` and never on the
        /// board.
        pub channels: PathBuf,
        /// Where a serving daemon keeps the copy of itself it starts keepers
        /// from, so a package manager cannot take that ability away from it
        /// mid-life. See `keeper_executable_for`.
        pub bin: PathBuf,
    }

    impl DaemonPaths {
        pub fn discover() -> Result<Self> {
            if let Some(path) = std::env::var_os("MUXLOOMD_STATE_DIR") {
                return Ok(Self::under(PathBuf::from(path)));
            }
            Self::the_machines_own()
        }

        /// Where a muxloom on this machine looks when it was not told
        /// otherwise: the one state directory every other muxloom here finds.
        pub fn the_machines_own() -> Result<Self> {
            if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
                return Ok(Self::under(PathBuf::from(path).join("muxloom")));
            }
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            Ok(Self::under(
                PathBuf::from(home).join(".local/state/muxloom"),
            ))
        }

        /// Whether this daemon is serving that directory. One serving anywhere
        /// else was handed somewhere to run — a test, an experiment — and the
        /// sessions in it are nobody else's business, however it was started.
        pub fn is_the_machines_own(&self) -> bool {
            let Ok(machines) = Self::the_machines_own() else {
                return false;
            };
            let settle =
                |path: &PathBuf| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            settle(&machines.root) == settle(&self.root)
        }

        pub fn under(root: PathBuf) -> Self {
            Self {
                socket: root.join("muxloomd.sock"),
                pid: root.join("muxloomd.pid"),
                lock: root.join("muxloomd.lock"),
                log: root.join("muxloomd.log"),
                generation: root.join("muxloomd.generation"),
                handover: root.join("muxloomd.handover"),
                history: root.join("history"),
                sessions: root.join("sessions"),
                keepers: root.join("keepers"),
                scratch: root.join("scratch"),
                triggers: root.join("triggers.json"),
                outbox: root.join("talk/outbox.json"),
                talk: root.join("talk"),
                channels: crate::channel::path_in(&root),
                bin: root.join("bin"),
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
            fs::create_dir_all(&self.keepers)?;
            fs::set_permissions(&self.keepers, fs::Permissions::from_mode(0o700))?;
            fs::create_dir_all(&self.scratch)?;
            fs::set_permissions(&self.scratch, fs::Permissions::from_mode(0o700))?;
            fs::create_dir_all(&self.bin)?;
            fs::set_permissions(&self.bin, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
    }

    struct DaemonState {
        started: Instant,
        clients: AtomicUsize,
        /// Held for reading by everything that starts work the drain must not
        /// cut in half -- taking a client, launching a session -- and for
        /// writing by the decision to drain. Readers do not exclude each
        /// other: a launch is slow, and it used to keep every connection
        /// arriving behind it waiting on the fork, the seeding and the
        /// metadata sync, which on a machine full of agents is every MCP call
        /// made while somebody starts a session.
        client_gate: RwLock<()>,
        /// One launch at a time. A session id is checked for being free and
        /// then taken, and two launches naming the same one both have to be
        /// able to see the other's answer.
        launch_gate: Mutex<()>,
        draining: AtomicBool,
        /// Whether a newer build has asked for this daemon's place and is
        /// waiting for a moment when taking it costs nothing.
        retiring: AtomicBool,
        /// How many requests are being answered right now. A daemon that stops
        /// mid-answer leaves whoever asked with an error, so retirement waits
        /// for this to reach zero.
        in_flight: AtomicUsize,
        shutdown: AtomicBool,
        next_subscriber: AtomicU64,
        sessions: Mutex<HashMap<String, Arc<ManagedSession>>>,
        persisted_sessions: Mutex<HashMap<String, Arc<PersistedSession>>>,
        paths: DaemonPaths,
        keeper_mode: KeeperMode,
        /// The file a keeper is started from, settled once while this daemon
        /// still knows where its own binary is.
        keeper_executable: PathBuf,
        /// Extra attention patterns a controller sank down, applied alongside
        /// the built-in classification on every snapshot. Shared with every
        /// session so an update reaches sessions launched before it arrived.
        attention_patterns: Arc<Mutex<Vec<String>>>,
        /// Standing watches on session screens, kept for whoever armed them.
        triggers: Mutex<Vec<ArmedTrigger>>,
        /// How many triggers exist at all. Session readers see every byte a
        /// PTY produces, so the common case — a daemon with no triggers —
        /// must cost one relaxed load and nothing else.
        armed: AtomicUsize,
        /// This machine's talk board, opened the first time anything asks for
        /// it: a daemon nobody collaborates through never writes one.
        talk: OnceLock<Arc<TalkStore>>,
        /// Direct messages waiting for the sessions they are addressed to to
        /// be free, oldest first.
        outbox: Mutex<Vec<TalkQueued>>,
        /// How many of those there are, so the drainer knows whether it still
        /// has anything to do without taking the lock.
        pending: AtomicUsize,
        /// Whether a thread is already watching the outbox.
        draining_outbox: AtomicBool,
        /// When each sender last reached each session, for the rate limit that
        /// keeps one agent from typing into another in a loop.
        directs: Mutex<HashMap<String, u64>>,
        /// Work this machine's agents have asked an attached controller to do
        /// for them, because it reaches machines this daemon cannot. Held in
        /// memory only: a job outlives neither the daemon nor the minute it
        /// was asked in.
        relay: Mutex<RelayQueue>,
        /// The channels an agent on this machine may reach a human through,
        /// pushed here by a controller and kept on disk so a restart does not
        /// leave the machine mute until the next round.
        channels: Mutex<ChannelSet>,
        /// What agents here have put in front of the human, waiting for a
        /// dashboard to come round and take them. In memory only, and bounded:
        /// a receipt nobody collected is a reply nobody is going to send.
        receipts: Mutex<Vec<crate::channel::ChannelReceipt>>,
    }

    /// A stored trigger plus the edge state that decides when it fires.
    struct ArmedTrigger {
        spec: Trigger,
        /// Whether the pattern was on screen the last time this was looked at.
        /// A match fires on the way in, so a pattern that simply stays there —
        /// a prompt nobody answered — fires once rather than forever.
        matched: bool,
    }

    /// How a launch obtains its keeper. Real daemons spawn the detached
    /// `muxloomd keeper` process that outlives them; tests run the identical
    /// keeper loop on a thread so `cargo test` never needs to exec a binary
    /// that is not the one under test.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum KeeperMode {
        Process,
        /// Only tests construct this; a serving daemon always spawns the real
        /// detached keeper.
        #[cfg_attr(not(test), allow(dead_code))]
        InProcess,
    }

    struct PersistedSession {
        metadata: Mutex<DaemonSession>,
        history_path: PathBuf,
        metadata_path: PathBuf,
        /// Whether this record's file has been deleted on purpose. See
        /// [`PersistedSession::persist`].
        discarded: Mutex<bool>,
        line_count: OnceLock<usize>,
        columns: u16,
        rows: u16,
    }

    struct ManagedSession {
        metadata: Mutex<DaemonSession>,
        /// Writer half toward the keeper process that owns the PTY, the child,
        /// and the history append. The daemon is only this session's current
        /// client; it can disconnect — or die — without ending the session.
        keeper: Mutex<UnixStream>,
        /// When the PTY last produced output (epoch ms), as *this daemon*
        /// heard it. A screen says what a session is doing only while
        /// something vouches that it is the screen the session has now, and
        /// that something is having heard it. A launch has, from the child's
        /// first byte; an adoption has not, and starts at `0`, because the
        /// screen it was handed was replayed out of the capture and may be
        /// drawing a turn that ended an hour ago.
        last_output: AtomicU64,
        /// When something was last typed into the session (epoch ms), by
        /// anyone - a person at a dashboard, another agent, a trigger. Unlike
        /// output, this only moves when a turn is actually asked for, which is
        /// what tells a session that is thinking apart from one that has been
        /// started over somewhere else.
        last_input: AtomicU64,
        /// Shared with [`DaemonState::attention_patterns`].
        attention_patterns: Arc<Mutex<Vec<String>>>,
        subscribers: Mutex<HashMap<u64, Subscriber>>,
        screen: Mutex<vt100::Parser>,
        /// How many times the grid has been changed. Bumped under [`Self::screen`]
        /// by the only two things that can change it -- output arriving and a
        /// resize -- so a count read while holding that lock names the picture
        /// the lock is holding.
        screen_seq: AtomicU64,
        /// The last laying-out of the grid as text, and the count it was taken
        /// at. See [`ManagedSession::visible_screen`].
        screen_text: Mutex<Option<(u64, Arc<str>)>>,
        /// Tracks the scroll region the session has, so an attach snapshot can
        /// hand the client the same `DECSTBM` the app left behind.
        inline: Mutex<InlineScrollback>,
        codex_activity: Mutex<CodexActivity>,
        /// The text last read out of this session's composer box, and the
        /// epoch-ms when it last changed. The outbox path reads it on every
        /// pass to age an unsent draft: a box that keeps changing has someone
        /// typing, and a box that has stopped is a draft we may deliver over.
        draft_watch: Mutex<Option<(String, u64)>>,
        /// The first substantial submission typed into this session, as the
        /// daemon itself heard it. What a runtime records as the first thing
        /// the person said in a transcript is the same text, so a session
        /// carrying this can check its claim against content rather than
        /// timing. Recorded once, while it is still `None`: the opening words
        /// are what identify a conversation, and later input is somebody
        /// else's sentence.
        first_prompt: Mutex<Option<String>>,
        /// Whether this session can still hear its own opening at all. Only
        /// a session this daemon spawned has its first input ahead of it; an
        /// adopted session's opening lies in the past whatever its metadata
        /// says, and a payload the new daemon hears must never pose as it.
        first_prompt_armed: AtomicBool,
        /// Set on adoption only: this daemon rebuilt the screen by replaying a
        /// bounded tail of history into a fresh parser, so what it holds is a
        /// partial frame for an app that differential-renders. The first
        /// attach must force the app to repaint instead of shipping the
        /// snapshot; the attach consumes the flag.
        screen_rebuilt: AtomicBool,
        /// The last thing the agent was seen to say, for a runtime that keeps
        /// no transcript of its own. Its answer scrolls off the screen long
        /// before it stops being the last word on the session, so the reading
        /// is kept rather than taken again from whatever is on screen now.
        screen_recap: Mutex<Option<String>>,
        /// Whether the final reading has been taken off a stopped session's
        /// screen. Nothing paints a session that has died or been archived, so
        /// the picture is finished and one reading of it is every reading of
        /// it — but the record stays in the map for as long as the daemon
        /// lives, and every round that lists it used to lay that same grid out
        /// as text again.
        screen_settled: AtomicBool,
        /// What a `notify` trigger left for whoever looks next. It reads as an
        /// attention reason until someone types into the session, which is the
        /// one signal that the message was seen.
        notice: Mutex<Option<String>>,
        /// An attention edge for this session's parent that no controller has
        /// carried yet. Set when `note_parent_alert` claims a tell, cleared
        /// when that tell is handed over.
        alert_pending: AtomicBool,
        /// The edge currently nagging about, if this session ever had one:
        /// *which* question it is (the reason and the last thing said - the
        /// pair that says whether it changed), how many tells that pair has
        /// had, and when the last one was claimed. Replaced the moment either
        /// half changes - the ceiling is silence *until something new*, not
        /// silence forever.
        alert_edge: Mutex<Option<AlertEdge>>,
        /// The transcript the runtime in this session is writing about itself,
        /// once the folder has been looked at.
        native: Mutex<NativeLink>,
        history_path: PathBuf,
        metadata_path: PathBuf,
        /// Whether this session's record has been deleted on purpose. See
        /// [`ManagedSession::persist_metadata`].
        discarded: Mutex<bool>,
        archived: AtomicBool,
        line_count: AtomicUsize,
        columns: AtomicU16,
        rows: AtomicU16,
    }

    /// One waiting state a parent has been told about, as far as the alerting
    /// machine remembers it. While both keys hold, the child sits on the same
    /// question, and tells about it run out: the first, then the reminders of
    /// `PARENT_ALERT_REMINDERS_MS`, then nothing until a key changes over a
    /// child that has been seen free in between.
    struct AlertEdge {
        reason_key: String,
        recap_key: String,
        told: u32,
        last_claimed_at: u64,
        /// When the stretch of *not* waiting this child is in began, or 0 while
        /// it is waiting. A stretch that reaches `PARENT_ALERT_SETTLE_MS` is
        /// what makes the next question a new one.
        free_since: u64,
    }

    /// What a session knows about the transcript its runtime keeps.
    ///
    /// The runtime names the conversation and records every turn of it, which
    /// is a far better account of the session than anything scraped off the
    /// screen. Finding out *which* transcript is a matching problem - several
    /// agents can be running in one folder - so a session holds on to its
    /// answer rather than deciding again every round.
    #[derive(Default)]
    struct NativeLink {
        /// The thread the launch was told to reopen, taken from the command
        /// line the daemon ran.
        seed: Option<String>,
        /// The transcript this session was matched to.
        claim: Option<NativeClaim>,
        /// Transcripts it has been moved off - a conversation cleared with
        /// `/clear` leaves one behind - and must not drift back onto.
        abandoned: Vec<String>,
        /// When the folder was last looked through on this session's behalf.
        /// Listing a directory is the expensive half of this, and a session
        /// that has said nothing since cannot have begun writing anything.
        scanned_at: u64,
        /// Whether the current claim has been weighed against the first
        /// words both accounts keep - what this session was asked and what
        /// the transcript says the person said first. A claim that has not
        /// is a timing guess, and two siblings started together are exactly
        /// what timing gets wrong: each can end up named by the other's
        /// conversation, invisibly from inside a claimed session, because a
        /// folder where every session is claimed is never looked through
        /// again on its own.
        claim_checked: bool,
        /// Re-scans spent checking it. A guess stops asking after the bound
        /// whether the transcript ever said its first words; the cost of
        /// being wrong there is a title read off the screen, and the cost of
        /// asking forever is listing the folder every round.
        claim_looks: u8,
    }

    struct NativeClaim {
        id: String,
        path: PathBuf,
        /// The file's modification time when it was last read, so a transcript
        /// that has not grown is not read again.
        read_at: u64,
        title: Option<String>,
        recap: Option<String>,
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
        fn new(paths: DaemonPaths, keeper_mode: KeeperMode) -> Self {
            let persisted_sessions = recover_persisted_sessions(&paths);
            let triggers = load_triggers(&paths.triggers);
            let armed = AtomicUsize::new(triggers.len());
            let outbox = load_outbox(&paths.outbox);
            let pending = AtomicUsize::new(outbox.len());
            let channels = ChannelSet::read(&paths.channels);
            let keeper_executable = keeper_executable_for(&paths, keeper_mode);
            Self {
                started: Instant::now(),
                clients: AtomicUsize::new(0),
                client_gate: RwLock::new(()),
                launch_gate: Mutex::new(()),
                draining: AtomicBool::new(false),
                retiring: AtomicBool::new(false),
                in_flight: AtomicUsize::new(0),
                shutdown: AtomicBool::new(false),
                next_subscriber: AtomicU64::new(1),
                sessions: Mutex::new(HashMap::new()),
                persisted_sessions: Mutex::new(persisted_sessions),
                paths,
                keeper_mode,
                keeper_executable,
                attention_patterns: Arc::new(Mutex::new(Vec::new())),
                triggers: Mutex::new(triggers),
                armed,
                talk: OnceLock::new(),
                outbox: Mutex::new(outbox),
                pending,
                draining_outbox: AtomicBool::new(false),
                directs: Mutex::new(HashMap::new()),
                relay: Mutex::new(RelayQueue::default()),
                channels: Mutex::new(channels),
                receipts: Mutex::new(Vec::new()),
            }
        }

        /// The talk board, opening it if this is the first time. A failure is
        /// worth reporting to whoever asked rather than at startup: everything
        /// else the daemon does works without a board.
        fn talk(&self) -> Result<Arc<TalkStore>> {
            if let Some(store) = self.talk.get() {
                return Ok(store.clone());
            }
            let store = Arc::new(TalkStore::open(self.paths.talk.clone())?);
            Ok(self.talk.get_or_init(|| store).clone())
        }

        /// Write the outbox out. Called while holding the outbox lock, for the
        /// same reason the triggers are: a message the file still claims is
        /// waiting would be delivered twice by the next generation.
        fn save_outbox(&self, queue: &[TalkQueued]) {
            self.pending.store(queue.len(), Ordering::Relaxed);
            if let Err(error) = write_outbox(&self.paths.outbox, queue) {
                eprintln!("muxloomd could not persist its message queue: {error:#}");
            }
        }

        /// Write the triggers out. Called while holding the trigger lock, so
        /// the file never records a set the daemon does not hold.
        fn save_triggers(&self, triggers: &[ArmedTrigger]) {
            let specs: Vec<&Trigger> = triggers.iter().map(|armed| &armed.spec).collect();
            if let Err(error) = write_triggers(&self.paths.triggers, &specs) {
                eprintln!("muxloomd could not persist its triggers: {error:#}");
            }
        }

        /// Drop every trigger armed on a session that no longer exists. A
        /// pattern can never reach a screen that is gone, so keeping them
        /// would only leave the file growing across restarts.
        fn drop_triggers_for(&self, session_id: &str) {
            let mut triggers = self
                .triggers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = triggers.len();
            triggers.retain(|armed| armed.spec.session_id != session_id);
            if triggers.len() == before {
                return;
            }
            self.armed.store(triggers.len(), Ordering::Relaxed);
            self.save_triggers(&triggers);
        }
    }

    /// The triggers a previous daemon left behind. A file that cannot be read
    /// is not worth refusing to serve over: the sessions matter more, and the
    /// next write replaces it.
    fn load_triggers(path: &Path) -> Vec<ArmedTrigger> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return Vec::new(),
        };
        match serde_json::from_str::<Vec<Trigger>>(&text) {
            Ok(triggers) => triggers
                .into_iter()
                .map(|spec| ArmedTrigger {
                    spec,
                    // Nothing has been looked at yet, and the screens are
                    // whatever the sessions were left showing. Starting
                    // unmatched would fire every trigger whose pattern is
                    // still on screen the moment output resumes; starting
                    // matched waits for the pattern to arrive again.
                    matched: true,
                })
                .collect(),
            Err(error) => {
                eprintln!("muxloomd could not read {}: {error}", path.display());
                Vec::new()
            }
        }
    }

    fn write_triggers(path: &Path, triggers: &[&Trigger]) -> Result<()> {
        let text = serde_json::to_string_pretty(triggers)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }

    /// Arm a trigger, replacing one with the same id. The session has to
    /// exist: a watch on a name nothing answers to would sit in the file
    /// forever, and the client that asked would never learn why.
    fn set_trigger(state: &DaemonState, mut trigger: Trigger) -> Result<Trigger> {
        if trigger.pattern.trim().is_empty() {
            bail!("a trigger needs a pattern to watch for");
        }
        let session = daemon_session(state, &trigger.session_id)?;
        if trigger.id.trim().is_empty() {
            trigger.id = format!(
                "trg-{:x}-{:x}",
                now_ms(),
                TRIGGER_COUNTER.fetch_add(1, Ordering::Relaxed)
            );
        }
        trigger.created_at = now_ms();
        trigger.cooldown_ms = trigger.cooldown_ms.max(TRIGGER_MIN_COOLDOWN_MS);
        trigger.last_fired_at = None;
        trigger.fires = 0;
        // What is on screen already is what the client just read. A trigger
        // waits for its pattern to arrive, so text that is there when it is
        // armed counts as seen rather than as an immediate match.
        let matched = session
            .visible_screen()
            .to_lowercase()
            .contains(&trigger.pattern.to_lowercase());
        let mut triggers = state
            .triggers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let armed = ArmedTrigger {
            spec: trigger.clone(),
            matched,
        };
        match triggers
            .iter_mut()
            .find(|existing| existing.spec.id == trigger.id)
        {
            Some(existing) => *existing = armed,
            None => {
                if triggers.len() >= TRIGGER_LIMIT {
                    bail!(
                        "this machine already holds {TRIGGER_LIMIT} triggers; delete one before \
                         arming another"
                    );
                }
                triggers.push(armed);
            }
        }
        state.armed.store(triggers.len(), Ordering::Relaxed);
        state.save_triggers(&triggers);
        Ok(trigger)
    }

    /// Whether an armed trigger fires on a frame whose screen does (`hit`) or
    /// does not carry its pattern.
    ///
    /// A trigger fires on the way *into* a match: text already on the screen
    /// when it was armed never counts, and a screen that goes on showing the
    /// match does not fire it again. The cooldown then debounces a pattern
    /// that flickers in and out — a shell prompt scrolling past, say.
    fn trigger_fires(armed: &ArmedTrigger, hit: bool, now: u64) -> bool {
        hit && !armed.matched
            && !armed
                .spec
                .last_fired_at
                .is_some_and(|last| now.saturating_sub(last) < armed.spec.cooldown_ms)
    }

    /// Look at one session's screen for the triggers armed on it, and run what
    /// matched. Runs on that session's reader thread, after the bytes have
    /// reached the screen, so a trigger sees exactly the text the attention
    /// classification and `read_screen` see.
    fn fire_triggers(state: &DaemonState, session: &ManagedSession, session_id: &str) {
        // Whether anything watches this session at all is the cheap question,
        // and it is asked on the thread draining that session's PTY, for every
        // piece of output it produces.
        let watched = {
            let triggers = state
                .triggers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            triggers
                .iter()
                .any(|armed| armed.spec.session_id == session_id)
        };
        if !watched {
            return;
        }
        // Only now, with a trigger known to be watching, is rendering the
        // screen worth it - and it is rendered with the trigger list let go
        // of. That list is one list for the whole machine: every watched
        // session's reader thread walks it, and so does every arm, list and
        // delete. Laying one session's screen out as text while holding it is
        // how one busy session's output comes to wait on another's.
        let screen = session.visible_screen().to_lowercase();
        let rendered_at = now_ms();
        let mut fired = Vec::new();
        {
            let mut triggers = state
                .triggers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = now_ms();
            let mut changed = false;
            triggers.retain_mut(|armed| {
                if armed.spec.session_id != session_id {
                    return true;
                }
                // Armed while this render was being taken, or after it: it
                // was primed against a picture of the screen at least as new
                // as this one, and judging it against an older picture is how
                // text that was already there when it was armed would read as
                // having just arrived - and fire. It waits for the next piece
                // of output, which is a frame away.
                if armed.spec.created_at >= rendered_at {
                    return true;
                }
                let hit = screen.contains(&armed.spec.pattern.to_lowercase());
                let fires = trigger_fires(armed, hit, now);
                armed.matched = hit;
                if !fires {
                    return true;
                }
                armed.spec.last_fired_at = Some(now);
                armed.spec.fires += 1;
                changed = true;
                fired.push(armed.spec.clone());
                !armed.spec.once
            });
            if changed {
                state.armed.store(triggers.len(), Ordering::Relaxed);
                state.save_triggers(&triggers);
            }
        }
        // Outside the lock: an action writes to the keeper, and a trigger set
        // must never be held across a write to a session.
        for spec in fired {
            match spec.action {
                TriggerAction::SendInput { text, submit } => {
                    let mut bytes = text.into_bytes();
                    if submit {
                        bytes.push(b'\r');
                    }
                    if let Err(error) = session.write_input(&bytes) {
                        eprintln!("muxloomd trigger {} could not type: {error:#}", spec.id);
                    }
                }
                TriggerAction::Notify { text } => session.set_notice(text),
            }
        }
    }

    /// Put a direct message in front of a session on this machine, or queue it
    /// until that session is free enough to read one.
    ///
    /// The message is filed on the board whichever way it goes, and before it
    /// is typed anywhere: a direct message is something that was said, and the
    /// board is where the other agents — and the person watching them — find
    /// out what they have been telling each other.
    fn deliver_direct(
        state: &Arc<DaemonState>,
        mut draft: TalkDraft,
        deliver: TalkDeliver,
        reply_expected: bool,
    ) -> Result<(TalkMessage, String, Option<String>)> {
        let to = draft
            .to
            .clone()
            .context("a direct message needs the session it is for")?;
        let session = daemon_session(state, &to.session_id)?;
        let snapshot = session.snapshot();
        let kind = snapshot.kind.parse::<AgentKind>().ok();
        let Some(kind) = kind.filter(|kind| *kind != AgentKind::Terminal) else {
            bail!(
                "session {} is a terminal, and there is nobody in it to read a message: type into \
                 a shell with send_input",
                to.session_id
            );
        };
        if snapshot.dead || snapshot.archived {
            bail!(
                "session {} has ended, so nothing there can be told",
                to.session_id
            );
        }
        rate_limit_direct(state, &draft, &to.session_id)?;
        draft.kind = TalkKind::Direct;
        let talk = state.talk()?;
        let here = talk.origin();
        let message = talk.post(draft)?;
        let body = render_delivery(&message, reply_expected, &here);
        // Whether this can go in is a question about the prompt box, not about
        // whether a turn is running: an empty box takes a paste whole and the
        // agent reads it when the turn it is in the middle of ends. What cannot
        // take one is a box holding a sentence nobody has sent, and a screen
        // with no box on it at all.
        let composer = snapshot.composer.unwrap_or(Composer::Ready);
        let queued = TalkQueued {
            message_id: message.id.clone(),
            session_id: to.session_id.clone(),
            body,
            queued_at: now_ms(),
            deliver,
            // Only a sender that is itself a session can be written back to.
            from: message
                .author
                .voice
                .session_id
                .clone()
                .map(|session_id| TalkAddress {
                    machine: message.author.machine.clone(),
                    session_id,
                }),
            text: message.text.clone(),
            // A sender that is not a session is a person on a chat app, and the
            // binding they wrote from is the only way back to them.
            channel: message.author.voice.channel.clone(),
            session_label: Some(snapshot.label.trim())
                .filter(|label| !label.is_empty())
                .map(str::to_string),
        };
        let draft_age = (composer == Composer::Occupied)
            .then(|| draft_age_ms(&session, &kind, queued.queued_at))
            .flatten();
        if deliver == TalkDeliver::Now
            || queued.due(
                composer,
                snapshot.working,
                snapshot.needs_attention,
                queued.queued_at,
            )
            || stale_draft_due(
                &queued,
                composer,
                snapshot.working,
                snapshot.needs_attention,
                draft_age,
            )
        {
            return Ok(match type_message(&session, kind, &queued.body) {
                Ok(()) => (message, "delivered".into(), None),
                Err(error) => (message, "failed".into(), Some(format!("{error:#}"))),
            });
        }
        {
            let mut outbox = state
                .outbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if outbox.len() >= OUTBOX_LIMIT {
                bail!(
                    "this machine already holds {OUTBOX_LIMIT} undelivered messages; the sessions \
                     here are not reading them"
                );
            }
            outbox.push(queued);
            state.save_outbox(&outbox);
        }
        spawn_outbox_drainer(state);
        let reason = queued_reason(
            composer,
            snapshot.needs_attention,
            snapshot.attention_reason.as_deref(),
        );
        Ok((message, "queued".into(), Some(reason)))
    }

    /// Hold the prompt a launch carried for its own first message until the
    /// session shows a box ready to take it.
    ///
    /// OpenCode reads a positional argument as the project directory rather
    /// than a prompt and dies on the spot, so the runtime leaves the prompt
    /// out of the command line and sends it here instead. It waits in the same
    /// outbox a queued direct message waits in, and goes in the same way: typed
    /// once the prompt box is there, bounced to whoever started the session if
    /// it never appears. It is not put on the board — this is not one agent
    /// telling another something; it is the prompt the session was started to
    /// work on.
    fn queue_seed_prompt(
        state: &Arc<DaemonState>,
        session_id: &str,
        parent: Option<String>,
        prompt: &str,
    ) {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return;
        }
        let from = parent.map(|session_id| TalkAddress {
            machine: state
                .talk()
                .map(|talk| talk.origin())
                .unwrap_or_else(|_| String::new()),
            session_id,
        });
        let queued = TalkQueued {
            message_id: format!("seed-{session_id}"),
            session_id: session_id.into(),
            body: prompt.into(),
            queued_at: now_ms(),
            deliver: TalkDeliver::Auto,
            from,
            text: prompt.into(),
            // A launch prompt comes from whoever asked for the session, and
            // that is a session or a dashboard, never a chat.
            channel: None,
            session_label: None,
        };
        {
            let mut outbox = state
                .outbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if outbox.len() >= OUTBOX_LIMIT {
                eprintln!(
                    "muxloomd could not queue the initial prompt for session {session_id}: the \
                     outbox is full"
                );
                return;
            }
            outbox.push(queued);
            state.save_outbox(&outbox);
        }
        spawn_outbox_drainer(state);
    }

    /// Refuse a sender that just reached this session.
    ///
    /// The key is the sender rather than its machine: two agents on one
    /// machine are two voices, and one of them being noisy is no reason to
    /// silence the other.
    fn rate_limit_direct(state: &DaemonState, draft: &TalkDraft, session_id: &str) -> Result<()> {
        let sender = format!(
            "{}/{} -> {session_id}",
            draft.author.machine,
            draft.author.voice.session_id.as_deref().unwrap_or("-")
        );
        let now = now_ms();
        let mut directs = state
            .directs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(last) = directs.get(&sender) {
            let waited = now.saturating_sub(*last);
            if waited < DIRECT_INTERVAL_MS {
                bail!(
                    "you sent this session a message {}s ago: wait {}s, or say the rest of it in \
                     one message — every one of them interrupts whatever it is doing",
                    waited / 1000,
                    (DIRECT_INTERVAL_MS - waited).div_ceil(1000)
                );
            }
        }
        // Nothing here is worth remembering once it can no longer refuse
        // anything, and a daemon that runs for weeks would otherwise hold a
        // row per pair forever.
        directs.retain(|_, last| now.saturating_sub(*last) < DIRECT_INTERVAL_MS * 60);
        directs.insert(sender, now);
        Ok(())
    }

    /// The exact bytes a delivered message is typed with, per runtime.
    ///
    /// Codex and Claude Code both understand bracketed paste, which is what
    /// gets a multi-line envelope into the prompt whole; typed as bytes, every
    /// newline in it would submit what came before. The other runtimes are not
    /// known to, so they are told the same thing folded onto one line — and it
    /// ends in a plain `\r`: the Enter keystroke is what OpenCode and pi bind
    /// their submit to. Anything else (a `\n`, an escape sequence) would type
    /// the draft in and leave it sitting in the box, which is what the
    /// "message never got through" reports were.
    fn message_bytes(kind: AgentKind, body: &str) -> Vec<u8> {
        if matches!(kind, AgentKind::Codex | AgentKind::Claude) {
            paste_bytes(body, true)
        } else {
            let mut folded = folded(body).into_bytes();
            folded.push(b'\r');
            folded
        }
    }

    /// Type a rendered message into a session as one submission.
    fn type_message(session: &ManagedSession, kind: AgentKind, body: &str) -> Result<()> {
        session.write_input(&message_bytes(kind, body))
    }

    /// How long this session's composer-box text has gone without changing,
    /// read on this pass: the first sighting of a draft is age zero, and an
    /// unchanged draft ages up one poll per pass. `None` when there is no box
    /// text to read (an absent or empty composer is the patience constants' to
    /// wait on, not the draft's).
    fn draft_age_ms(session: &ManagedSession, kind: &AgentKind, now: u64) -> Option<u64> {
        let visible_screen = session.visible_screen();
        let draft = composer_text(*kind, &visible_screen)?;
        let mut watch = session
            .draft_watch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match watch.as_ref() {
            Some((last, changed_at)) if *last == draft => Some(now.saturating_sub(*changed_at)),
            _ => {
                *watch = Some((draft, now));
                Some(0)
            }
        }
    }

    /// What to tell a sender whose message [`TalkQueued::due`] has just held
    /// back, in terms of what they can do about it.
    ///
    /// A session sitting on a question is the case worth saying out loud, and
    /// the one an empty prompt used to be lumped in with. `due` will not type
    /// into a dialog for as long as one is up, so such a message is not waiting
    /// on a turn to end or on a draft to clear — it is waiting on a person.
    /// Saying it goes in "as soon as a prompt appears" reads as a promise that
    /// waiting will fix it, and waiting fixes nothing: the message sits for the
    /// full `DELIVER_EXPIRY_MS` and then comes back undelivered. Name the
    /// question instead, so whoever sent this knows there is something for them
    /// to go and answer.
    ///
    /// An empty prompt otherwise only queues for a `when_idle` sender, who
    /// asked for exactly this.
    fn queued_reason(composer: Composer, attention: bool, question: Option<&str>) -> String {
        match (composer, attention) {
            // A ready prompt is delivered into whether or not a question is up,
            // so reaching here with one means a `when_idle` sender mid-turn.
            (Composer::Ready, _) => {
                "that session is mid-turn; the message goes in when it finishes".to_string()
            }
            (_, true) => {
                let question = question
                    .map(|question| format!(" ({question})"))
                    .unwrap_or_default();
                format!(
                    "that session is waiting on a question{question}, and whatever is typed there \
                     next answers it — so this is held rather than sent, and no amount of waiting \
                     will release it. Answer the question on that session and the message \
                     follows; leave it and this comes back undelivered."
                )
            }
            (Composer::Occupied, false) => {
                "that session's prompt already holds something unsent, and this would be submitted \
                 together with it; it goes in as soon as the prompt clears, or in a few minutes \
                 anyway"
                    .to_string()
            }
            (Composer::Absent, false) => {
                "that session is not showing a prompt to type into — it is starting up, or no \
                 longer running its agent; the message goes in as soon as one appears"
                    .to_string()
            }
        }
    }

    /// Whether a queued message should go in now although [`TalkQueued::due`]
    /// still holds it: the unsent draft it would land behind has gone quiet
    /// (nobody has changed it in `DELIVER_STALE_DRAFT_MS`), or the session has
    /// gone idle and can read it. A `now` delivery never waits on a draft.
    fn stale_draft_due(
        queued: &TalkQueued,
        composer: Composer,
        working: bool,
        attention: bool,
        draft_age: Option<u64>,
    ) -> bool {
        if queued.deliver == TalkDeliver::Now {
            return true;
        }
        if composer != Composer::Occupied || attention || queued.deliver == TalkDeliver::WhenIdle {
            return false;
        }
        !working || draft_age.is_some_and(|age| age >= DELIVER_STALE_DRAFT_MS)
    }

    /// Deliver what the outbox holds to the sessions that have become free
    /// enough to read it, and drop what has waited too long to be worth
    /// delivering at all.
    fn drain_outbox(state: &DaemonState) {
        if state.pending.load(Ordering::Relaxed) == 0 {
            return;
        }
        // The session map is taken first and let go of again: the outbox lock
        // is held while messages are picked, and a lock over both in one order
        // here and the other in `deliver_direct` is how a daemon stops.
        let sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let now = now_ms();
        let mut due = Vec::new();
        let mut lost = Vec::new();
        {
            let mut outbox = state
                .outbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = outbox.len();
            outbox.retain(|queued| {
                let Some(session) = sessions.get(&queued.session_id) else {
                    lost.push((queued.clone(), TalkUndelivered::Gone));
                    return false;
                };
                let snapshot = session.snapshot();
                let kind = snapshot.kind.parse::<AgentKind>().ok();
                let Some(kind) = kind.filter(|_| !snapshot.dead && !snapshot.archived) else {
                    lost.push((queued.clone(), TalkUndelivered::Ended));
                    return false;
                };
                let composer = snapshot.composer.unwrap_or(Composer::Ready);
                if queued.due(composer, snapshot.working, snapshot.needs_attention, now) {
                    due.push((Arc::clone(session), kind, queued.clone()));
                    return false;
                }
                // A draft that has gone quiet is no reason to keep holding:
                // nobody is finishing it, and a message waiting behind it is
                // stranger than one delivered.
                let draft_age = (composer == Composer::Occupied)
                    .then(|| draft_age_ms(session, &kind, now))
                    .flatten();
                if stale_draft_due(
                    queued,
                    composer,
                    snapshot.working,
                    snapshot.needs_attention,
                    draft_age,
                ) {
                    due.push((Arc::clone(session), kind, queued.clone()));
                    return false;
                }
                if queued.expired(now) {
                    lost.push((queued.clone(), TalkUndelivered::NoPrompt));
                    return false;
                }
                true
            });
            if outbox.len() != before {
                state.save_outbox(&outbox);
            }
        }
        // Outside the lock: writing to a session reaches its keeper, and the
        // queue must never be held across that.
        for (session, kind, queued) in due {
            if let Err(error) = type_message(&session, kind, &queued.body) {
                lost.push((queued, TalkUndelivered::Failed(format!("{error:#}"))));
            }
        }
        for (queued, why) in lost {
            bounce(state, &queued, &why);
        }
    }

    /// Tell the sender that a message never arrived.
    ///
    /// This is the only end an undelivered message has that the sender can act
    /// on. A line on stderr is kept too, because the person running the daemon
    /// is entitled to see it, but the line that matters goes on the board.
    fn bounce(state: &DaemonState, queued: &TalkQueued, why: &TalkUndelivered) {
        eprintln!(
            "muxloomd could not deliver message {} to session {}: {}",
            queued.message_id,
            queued.session_id,
            why.reason()
        );
        let Some(from) = queued.from.clone() else {
            // No session to write back to. A person who sent this from a chat
            // app is still waiting on it, and the board is not somewhere they
            // read — so the chat, or nowhere.
            bounce_to_channel(state, queued, why);
            return;
        };
        let posted = state.talk().and_then(|talk| {
            talk.post(TalkDraft {
                // The bounce belongs to the machine that failed to deliver it,
                // and the sender reads it as a direct wherever they are.
                scope: TalkScope::Machine {
                    machine: String::new(),
                },
                author: TalkAuthor {
                    machine: String::new(),
                    machine_label: String::new(),
                    // Not a session and not a person: muxloom itself, saying
                    // what happened to something it was handed.
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
                to: Some(from),
                // The id is what lets a sender's wait end on this rather than
                // on the next thing anyone says.
                reply_to: Some(queued.message_id.clone()),
                text: render_bounce(queued, why),
            })
        });
        if let Err(error) = posted {
            eprintln!(
                "muxloomd could not tell the sender of {} that it never arrived: {error:#}",
                queued.message_id
            );
        }
    }

    /// Tell a person on a chat app that what they sent from it never landed.
    ///
    /// This machine dials the chat itself, out of the bindings a controller
    /// sank down, for the same reason `send_channel_message` does: the message
    /// failed here, and a machine nobody is watching is exactly the machine a
    /// person is most likely to be left waiting on. Proxy settings come from
    /// this daemon's own environment — it has no config of its own to read one
    /// out of, and it was started from a shell that had whatever the machine
    /// uses to reach the outside.
    ///
    /// On its own thread: a chat API is a network round trip with a minute of
    /// timeout behind it, and the outbox drain that called this has other
    /// sessions' messages to deliver.
    fn bounce_to_channel(state: &DaemonState, queued: &TalkQueued, why: &TalkUndelivered) {
        let Some(channel) = queued.channel.clone() else {
            return;
        };
        let set = state
            .channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let binding = match set.pick(Some(&channel)) {
            Ok(binding) => binding.clone(),
            Err(error) => {
                eprintln!(
                    "muxloomd could not tell the person who sent {} that it never arrived: \
                     {error:#}",
                    queued.message_id
                );
                return;
            }
        };
        let machine = state.talk().map(|talk| talk.label()).unwrap_or_default();
        let message = crate::channel::Outgoing {
            title: "Message not delivered".into(),
            text: crate::talk::render_channel_bounce(queued, why),
            // Not an agent speaking. Saying so is the difference between a
            // person reading this as muxloom's own bookkeeping and reading it
            // as the agent they wrote to finally answering.
            signature: [String::from("muxloom"), machine]
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            ..crate::channel::Outgoing::default()
        };
        let message_id = queued.message_id.clone();
        thread::spawn(move || {
            // Bad news that is itself swallowed leaves the person waiting on an
            // answer to a message nobody has, and this log is the only place
            // that would ever say so. WeChat drops a send on a stale
            // conversation token while answering as though it had taken it, so a
            // bounce is one of the sends most likely to go that way: the person
            // has not written to the bot recently, which is the whole reason
            // there is something to bounce.
            match crate::channel::send(&binding, &message, &[]) {
                Ok(sent) if !sent.delivered() => eprintln!(
                    "muxloomd told nobody that {message_id} never arrived: the chat accepted the \
                     notice without a delivery id, which is what a stale conversation token does \
                     to one"
                ),
                Ok(_) => {}
                Err(error) => eprintln!(
                    "muxloomd could not tell the person who sent {message_id} that it never \
                     arrived: {error:#}"
                ),
            }
        });
    }

    /// Keep an eye on the outbox for as long as it holds anything.
    ///
    /// A queued message waits for a session to stop working, and that is not
    /// something anything here can be woken by: the reader thread sees output,
    /// and an agent finishing is precisely the absence of it. So the wait is a
    /// poll — one that exists only while a message is actually waiting.
    fn spawn_outbox_drainer(state: &Arc<DaemonState>) {
        if state.pending.load(Ordering::Relaxed) == 0 {
            return;
        }
        if state.draining_outbox.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = Arc::clone(state);
        thread::spawn(move || {
            while state.pending.load(Ordering::Relaxed) > 0
                && !state.shutdown.load(Ordering::Acquire)
                && !state.draining.load(Ordering::Acquire)
            {
                thread::sleep(OUTBOX_POLL);
                drain_outbox(&state);
            }
            state.draining_outbox.store(false, Ordering::Release);
            // Something may have queued a message between the last look and
            // the flag coming down, and seen a drainer that was already on its
            // way out.
            if state.pending.load(Ordering::Relaxed) > 0
                && !state.shutdown.load(Ordering::Acquire)
                && !state.draining.load(Ordering::Acquire)
            {
                spawn_outbox_drainer(&state);
            }
        });
    }

    /// The messages a previous daemon was still holding. Like the triggers, a
    /// file that cannot be read is not worth refusing to serve over.
    fn load_outbox(path: &Path) -> Vec<TalkQueued> {
        let Ok(text) = fs::read_to_string(path) else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<TalkQueued>>(&text).unwrap_or_else(|error| {
            eprintln!("muxloomd could not read {}: {error}", path.display());
            Vec::new()
        })
    }

    fn write_outbox(path: &Path, queue: &[TalkQueued]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(queue)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }

    /// Load what a previous daemon left in the state directory, recovering the
    /// sessions it was killed before it could finish with.
    ///
    /// A daemon that stops without warning — SIGKILL, an OOM kill, a lost
    /// machine — leaves metadata that still claims a live PTY. That claim can
    /// never be honoured: a PTY master does not outlive the process that opened
    /// it, so there is nothing left to re-attach to. What does survive is the
    /// record and the log beside it, so each interrupted session is retired
    /// into the archive with its history intact. Dropping them, which is what
    /// this did before, lost every running agent of a killed daemon: the
    /// sessions did not come back and could not even be reached in the archive.
    fn recover_persisted_sessions(paths: &DaemonPaths) -> HashMap<String, Arc<PersistedSession>> {
        let mut sessions = HashMap::new();
        match fs::read_dir(&paths.sessions) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|value| value.to_str()) != Some("json")
                        || !entry.file_type().is_ok_and(|kind| kind.is_file())
                    {
                        continue;
                    }
                    // A session whose keeper socket is still on disk is not a
                    // leftover to retire: adoption decides whether it is alive.
                    if path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|id| keeper::socket_path_for(&paths.keepers, id).exists())
                    {
                        continue;
                    }
                    match load_persisted_session(paths, &path) {
                        Ok(Some((id, session))) => {
                            sessions.insert(id, session);
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!(
                            "muxloomd ignored persisted session {}: {error:#}",
                            path.display()
                        ),
                    }
                }
            }
            Err(error) => eprintln!(
                "muxloomd could not read persisted sessions {}: {error}",
                paths.sessions.display()
            ),
        }
        adopt_orphaned_histories(paths, &mut sessions);
        sessions
    }

    /// Read one session's metadata back, repairing what an interrupted daemon
    /// left behind. `Ok(None)` means the record was discarded rather than
    /// loaded, which only a stale temporary session is.
    fn load_persisted_session(
        paths: &DaemonPaths,
        path: &Path,
    ) -> Result<Option<(String, Arc<PersistedSession>)>> {
        let mut metadata: DaemonSession = serde_json::from_slice(&fs::read(path)?)?;
        validate_session_id(&metadata.id)?;
        if path.file_stem().and_then(|value| value.to_str()) != Some(metadata.id.as_str()) {
            bail!(
                "metadata filename does not match session id {}",
                metadata.id
            );
        }
        let history_path = paths.history.join(format!("{}.ansi", metadata.id));
        if metadata.temporary {
            let _ = fs::remove_file(&history_path);
            let _ = fs::remove_file(path);
            remove_scratch_dir(paths, &metadata.id);
            eprintln!("muxloomd discarded stale temporary session {}", metadata.id);
            return Ok(None);
        }
        if !history_path.exists() {
            // A delete removes the log before the metadata, so a daemon killed
            // between the two leaves a record with nothing to read. Give it an
            // empty log rather than dropping the record: a history directory
            // that is missing for some other reason must not be read as an
            // instruction to erase every session in it.
            if let Err(error) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&history_path)
            {
                eprintln!(
                    "muxloomd could not restore the history of session {}: {error}",
                    metadata.id
                );
            } else {
                eprintln!(
                    "muxloomd loaded session {} without the history it recorded",
                    metadata.id
                );
            }
        }
        let interrupted = !metadata.dead && !metadata.archived;
        if interrupted {
            recover_interrupted_session(&mut metadata, &history_path);
        }
        metadata.dead = true;
        metadata.pid = None;
        metadata.working = false;
        metadata.needs_attention = false;
        metadata.attention_reason = None;
        if interrupted && let Err(error) = persist_session_metadata(path, &metadata) {
            eprintln!(
                "muxloomd could not record the recovery of session {}: {error:#}",
                metadata.id
            );
        }
        let id = metadata.id.clone();
        Ok(Some((
            id,
            Arc::new(PersistedSession {
                metadata: Mutex::new(metadata),
                history_path,
                metadata_path: path.to_path_buf(),
                discarded: Mutex::new(false),
                line_count: OnceLock::new(),
                columns: 80,
                rows: 24,
            }),
        )))
    }

    /// Retire a session whose daemon was killed before it could record how the
    /// session ended.
    ///
    /// The PTY is gone with the daemon that owned it, so recovery here means
    /// recovering the record: the recap the interrupted daemon never wrote is
    /// rebuilt from the tail of the log, and a note is appended so the archived
    /// transcript says why it stops where it does.
    fn recover_interrupted_session(metadata: &mut DaemonSession, history_path: &Path) {
        let orphan = metadata.pid.filter(|pid| process_alive(*pid));
        // Retired, and recorded as retired: the note below tells whoever
        // reads the transcript that this session was archived, and the flag
        // has to agree with it. When it did not, the record was left claiming
        // to be a session nobody had put down - and with no hour on it, so
        // the folder it sits in ordered it by when it *began*, burying a
        // conversation the daemon ended this morning under everything started
        // since. This is the only account of when it stopped that anyone has:
        // the daemon that would have written one is what went away.
        metadata.archived = true;
        metadata.archived_at.get_or_insert_with(now_secs);
        if metadata.recap.is_none()
            && let Ok(kind) = metadata.kind.parse::<AgentKind>()
            && let Some(tail) = history_tail(history_path, RECENT_OUTPUT_LIMIT as u64)
        {
            metadata.recap = extract_recap(kind, &String::from_utf8_lossy(&tail));
        }
        let note = match orphan {
            Some(pid) => format!(
                "\r\n[muxloom] muxloomd stopped unexpectedly; this session was archived. \
                 Its process {pid} outlived the daemon and can no longer be reached.\r\n"
            ),
            None => "\r\n[muxloom] muxloomd stopped unexpectedly; this session was archived.\r\n"
                .to_string(),
        };
        if let Err(error) = append_history_note(history_path, note.as_bytes()) {
            eprintln!(
                "muxloomd could not note the interruption of session {}: {error:#}",
                metadata.id
            );
        }
        eprintln!(
            "muxloomd recovered interrupted session {} into the archive{}",
            metadata.id,
            match orphan {
                Some(pid) => format!("; its process {pid} is still running unattached"),
                None => String::new(),
            }
        );
    }

    /// Rebuild records for logs whose metadata did not survive.
    ///
    /// The log is the session; the JSON beside it only describes it. Metadata
    /// that a power cut truncated, or that never reached the disk at all, used
    /// to leave a complete transcript unreachable forever. The session id still
    /// carries the kind and the creation time, which is enough to archive the
    /// log so it stays readable and searchable.
    fn adopt_orphaned_histories(
        paths: &DaemonPaths,
        sessions: &mut HashMap<String, Arc<PersistedSession>>,
    ) {
        let Ok(entries) = fs::read_dir(&paths.history) else {
            return;
        };
        for entry in entries.flatten() {
            let history_path = entry.path();
            if history_path.extension().and_then(|value| value.to_str()) != Some("ansi")
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            let Some(id) = history_path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if sessions.contains_key(&id) || validate_session_id(&id).is_err() {
                continue;
            }
            // A log whose keeper socket is still on disk belongs to a session
            // adoption will handle; archiving it here would bury a live one.
            if keeper::socket_path_for(&paths.keepers, &id).exists() {
                continue;
            }
            let metadata_path = paths.sessions.join(format!("{id}.json"));
            if is_temporary_session_id(&id) {
                // A temporary session leaves no transcript behind by design.
                let _ = fs::remove_file(&history_path);
                let _ = fs::remove_file(&metadata_path);
                continue;
            }
            let Some((kind, created_at)) = session_origin_from_id(&id) else {
                eprintln!(
                    "muxloomd left an unrecognized session log in place: {}",
                    history_path.display()
                );
                continue;
            };
            let recap = history_tail(&history_path, RECENT_OUTPUT_LIMIT as u64)
                .and_then(|tail| extract_recap(kind, &String::from_utf8_lossy(&tail)));
            let metadata = DaemonSession {
                id: id.clone(),
                kind: kind.as_str().into(),
                // The working directory lived only in the lost metadata. Home
                // keeps the record listable; a resume asks for a folder anyway.
                path: "~".into(),
                label: "recovered session".into(),
                temporary: false,
                created_at,
                // The record is being written the moment the session is found
                // gone, which is the only account of when it stopped that
                // survived the metadata going missing.
                archived_at: Some(now_secs()),
                pid: None,
                dead: true,
                archived: true,
                recap,
                // Nothing is left to say who the session was talking to, or
                // who started it: the metadata that recorded its folder is
                // what went missing.
                title: None,
                thread: None,
                seed: None,
                // The account of what opened this conversation went missing
                // with the metadata; nothing left can say what it was.
                first_prompt: None,
                working: false,
                needs_attention: false,
                attention_reason: None,
                composer: None,
                parent: None,
                powers: None,
                resumed_from: None,
                resumed_to: None,
            };
            if let Err(error) = persist_session_metadata(&metadata_path, &metadata) {
                eprintln!("muxloomd could not record recovered session {id}: {error:#}");
            }
            eprintln!("muxloomd recovered session log {id} without its metadata");
            sessions.insert(
                id,
                Arc::new(PersistedSession {
                    metadata: Mutex::new(metadata),
                    history_path,
                    metadata_path,
                    discarded: Mutex::new(false),
                    line_count: OnceLock::new(),
                    columns: 80,
                    rows: 24,
                }),
            );
        }
    }

    /// What a session id still says about the session once its metadata is
    /// gone: the agent that ran and when the controller created it.
    fn session_origin_from_id(session_id: &str) -> Option<(AgentKind, u64)> {
        let mut fields = session_id
            .strip_prefix(DAEMON_SESSION_PREFIX)?
            .splitn(3, '-');
        let kind = fields.next()?.parse::<AgentKind>().ok()?;
        let created_at = fields.next().and_then(|value| value.parse().ok())?;
        Some((kind, created_at))
    }

    /// Whether `pid` still names a live process.
    ///
    /// A child normally goes with the daemon that owned it: closing the last
    /// descriptor of its PTY master hangs the terminal up and the foreground
    /// group is sent SIGHUP. One that ignores the hangup outlives it, but
    /// unreachably — nothing can re-open a master another process created. So
    /// this only reports; it never signals. After a reboot the recorded number
    /// is as likely to belong to a stranger as to the agent that ran.
    fn process_alive(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// The last `limit` bytes of a session log, which is where an agent leaves
    /// whatever it has to say for itself.
    fn history_tail(path: &Path, limit: u64) -> Option<Vec<u8>> {
        let mut file = File::open(path).ok()?;
        let end = file.seek(SeekFrom::End(0)).ok()?;
        file.seek(SeekFrom::Start(end.saturating_sub(limit))).ok()?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail).ok()?;
        Some(tail)
    }

    fn append_history_note(path: &Path, note: &[u8]) -> Result<()> {
        let mut history = OpenOptions::new().append(true).open(path)?;
        history.write_all(note)?;
        history.flush()?;
        Ok(())
    }

    pub fn serve(paths: &DaemonPaths) -> Result<()> {
        serve_with_mode(paths, KeeperMode::Process)
    }

    /// Test-only serve whose keepers run on threads: `cargo test` must never
    /// exec its own test binary as a keeper.
    #[cfg(test)]
    pub(crate) fn serve_with_in_process_keepers(paths: &DaemonPaths) -> Result<()> {
        serve_with_mode(paths, KeeperMode::InProcess)
    }

    fn serve_with_mode(paths: &DaemonPaths, keeper_mode: KeeperMode) -> Result<()> {
        paths.prepare()?;
        // Settled before anything is looked at, and held until this process
        // ends: everything below reads and rewrites the state directory on the
        // strength of what it found a moment ago.
        let mut serving = match hold_the_serving_lock(paths)? {
            Some(lock) => lock,
            None => bail!("muxloomd is already running"),
        };
        if paths.socket.exists() {
            if UnixStream::connect(&paths.socket).is_ok() {
                bail!("muxloomd is already running");
            }
            // The lock is this process's, so nothing that knows about one is
            // serving; only a daemon from before there was one can be.
            if the_pid_file_still_names_a_daemon(paths) {
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
        // Only now, with everything the last daemon left behind read and swept
        // up: until this the lock file is still its record, and its record is
        // what says it has gone.
        record_the_serving_daemon(&mut serving);
        fs::write(&paths.generation, format!("{}\n", current_generation()))?;
        // Whatever generation was being asked to make way, it has. The ask is
        // keyed by generation and would be ignored anyway, but leaving it lying
        // around only makes the state directory harder to read.
        forget_handover_ask(paths);
        let _guard = SocketGuard {
            socket: paths.socket.clone(),
            pid: paths.pid.clone(),
        };
        let state = Arc::new(DaemonState::new(paths.clone(), keeper_mode));
        adopt_keeper_sessions(&state);
        // Only now does this generation know which sessions it has, so only now
        // can a leftover scratch folder be told apart from the folder a live
        // temporary session is sitting in.
        sweep_scratch_dirs(&state);
        // Messages the last generation was still holding for a busy session
        // are this one's to deliver now that it owns the sessions.
        spawn_outbox_drainer(&state);
        // Every session's runtime has been writing an account of itself all
        // along; this generation starts reading them again.
        spawn_native_history_reader(&state);
        // Tell this machine's agents where the control surface is and how the
        // fleet works, so one launched here — or on a remote the user never
        // configures by hand — can drive the sessions around it. Only when
        // this is the machine's own daemon: that entry is shared by every
        // agent here, and a daemon serving a scratch directory would point
        // them all at an empty fleet. Nothing here is worth refusing to
        // serve over.
        match crate::mcp_register::register_for_this_daemon(paths.is_the_machines_own()) {
            Ok(written) if !written.is_empty() => {
                for path in written {
                    eprintln!("muxloomd wrote its agent setup into {}", path.display());
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!("muxloomd could not set up its agents: {error:#}"),
        }
        // Every one of these signals terminates the process by default. The
        // sessions themselves live in their keepers, so a caught signal only
        // has to stop serving; catching it keeps the exit deliberate instead
        // of mid-frame.
        let signalled = Arc::new(AtomicBool::new(false));
        for signal in [
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGQUIT,
        ] {
            if let Err(error) = signal_hook::flag::register(signal, Arc::clone(&signalled)) {
                eprintln!("muxloomd could not handle signal {signal}: {error}");
            }
        }
        listener.set_nonblocking(true)?;
        let result = (|| -> Result<()> {
            while still_serving(&state, &signalled) {
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
                        wait_for_connection(&listener, ACCEPT_RECHECK)?;
                    }
                    Err(error) => return Err(error).context("muxloomd accept failed"),
                }
            }
            Ok(())
        })();
        // Sessions are not retired here: their keepers own them, keep writing
        // their histories, and hand them to whichever daemon serves next.
        result
    }

    /// How long the accept loop waits on the socket before looking up to see
    /// whether it is still the generation serving this directory. Nobody waits
    /// on this: a connection arriving inside it is handed over as it lands.
    const ACCEPT_RECHECK: Duration = Duration::from_millis(200);

    /// Wait until a connection is pending, or until the timeout passes.
    ///
    /// The listener is non-blocking so the loop can look at [`still_serving`]
    /// between connections, and it used to do that by sleeping twenty
    /// milliseconds at a time. Nothing was waiting on that sleep except every
    /// client: a connection is queued by the kernel the moment it is made, and
    /// then sat in the backlog until the loop next woke, so an agent's request
    /// paid ten milliseconds of it on average before the daemon had looked at
    /// the socket at all — and the surface every agent uses opens a fresh
    /// connection per request.
    ///
    /// Waiting on the socket itself re-checks just as often and costs the
    /// waiting client nothing.
    fn wait_for_connection(listener: &UnixListener, timeout: Duration) -> io::Result<()> {
        let mut watched = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one initialised pollfd, and a count that says so.
        if unsafe { libc::poll(&mut watched, 1, millis) } < 0 {
            let error = io::Error::last_os_error();
            // A signal is what the loop is looking up to notice, so waking for
            // one is the wait doing its job, not failing at it.
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Whether this generation is still serving, latching a signal into the
    /// shutdown flag on the way past.
    ///
    /// The latch is the point. A signal is this generation being retired, and
    /// the session readers decide what a keeper hanging up meant by reading
    /// that flag: with it set the keeper was taken over, without it the keeper
    /// crashed and the session is dead. Ending the accept loop on a signal
    /// alone left it clear, so the readers of a daemon that had been told to
    /// stand down outlived it just long enough to see the next generation adopt
    /// their keepers, call that a crash, and write `dead` over the sessions
    /// that generation had brought back — every session grey right after an
    /// upgrade, with its keeper and its child running the whole time.
    fn still_serving(state: &DaemonState, signalled: &AtomicBool) -> bool {
        if signalled.load(Ordering::Acquire) {
            state.shutdown.store(true, Ordering::Release);
        }
        !state.shutdown.load(Ordering::Acquire)
    }

    /// Held for as long as this process serves the state directory, and given
    /// up by the kernel however the process ends — including the ends that run
    /// no destructor.
    struct ServeLock {
        /// Held open for the life of the daemon, because that is exactly how
        /// long the lock lasts, and written into once the daemon knows it is
        /// the one serving.
        held: File,
    }

    /// Which daemon serves a state directory, decided once by the kernel
    /// rather than by each arrival looking around.
    ///
    /// Looking was a race, and the handover walks straight into it: a client
    /// stops the daemon it is replacing and starts one, and every other client
    /// on the machine is doing the same thing at the same moment. Each finds
    /// nothing listening, each removes the socket it just found stale, and each
    /// binds. The last to bind owns the path; the others hold a listener on an
    /// unlinked inode and accept nothing for the rest of their lives, having
    /// already written their pid over the winner's — and when one of them goes,
    /// its [`SocketGuard`] takes the winner's socket and pid file with it. A
    /// machine could come out of an upgrade with a daemon nobody can reach and
    /// a pid file naming a daemon that is not serving.
    ///
    /// An exclusive lock answers it before anything is looked at. The file is
    /// never removed, so the lock is on one inode for the life of the directory
    /// and cannot be lost by somebody unlinking the thing it is held on.
    ///
    /// Taking the lock does not record who took it: what the file still says
    /// is how the checks below tell a pid file left behind by a daemon that
    /// died from one that still names a daemon, so it is left alone until
    /// those have been made. See [`record_the_serving_daemon`].
    ///
    /// `Ok(None)` means another daemon is already serving here. A filesystem
    /// that cannot lock at all is not a reason to refuse to serve: say so and
    /// go on, which is no worse than every build before this one.
    fn hold_the_serving_lock(paths: &DaemonPaths) -> Result<Option<ServeLock>> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&paths.lock)
            .with_context(|| format!("failed to open {}", paths.lock.display()))?;
        // Everything that only wants to *know* whether a daemon is serving
        // takes this same lock for a moment to find out, so an occupied answer
        // is worth asking for twice: one held for microseconds is a question
        // being asked, and one held by a daemon is held for its whole life.
        let deadline = Instant::now() + Duration::from_millis(300);
        let taken = loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                break Err(error);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(10));
        };
        if let Err(error) = taken {
            eprintln!(
                "muxloomd cannot lock {}, so it is serving without one: {error}",
                paths.lock.display()
            );
            return Ok(Some(ServeLock { held: file }));
        }
        Ok(Some(ServeLock { held: file }))
    }

    /// Write this daemon into the locked file, beside the pid file and saying
    /// the same thing, so that whoever finds the two of them after this daemon
    /// is gone can tell that it is gone. See
    /// [`the_pid_file_still_names_a_daemon`].
    fn record_the_serving_daemon(lock: &mut ServeLock) {
        let _ = lock.held.set_len(0);
        let _ = lock.held.rewind();
        let _ = lock
            .held
            .write_all(format!("{}\n", std::process::id()).as_bytes());
        let _ = lock.held.flush();
    }

    /// Whether a daemon is serving this directory, asked of the kernel rather
    /// than of a number in a file.
    ///
    /// The lock is released the moment the process ends, however it ends, so
    /// this is true exactly while a daemon is there. `kill(pid, 0)` is not:
    /// it is true of a daemon that has exited and not yet been reaped, and
    /// after a reboot it is true of whichever stranger the kernel handed the
    /// number to next. Both made "muxloomd is running but its socket is not
    /// accessible" a wedge nothing could clear on its own.
    fn a_daemon_is_serving(paths: &DaemonPaths) -> bool {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&paths.lock) else {
            // Nothing that knows about the lock has ever served here, so the
            // number is all there is to go on.
            return daemon_process_alive(paths);
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return true;
        }
        // It came free, so nothing is serving under a lock; a daemon from
        // before there was one still might be.
        the_pid_file_still_names_a_daemon(paths)
    }

    /// Whether the number in the pid file still names the daemon that wrote it.
    ///
    /// Asked only where the lock is known to be free, and it is the record in
    /// the lock file that answers it. A daemon that dies without clearing up —
    /// a `kill -9`, a power cut — leaves its pid file behind, and after a
    /// reboot that number belongs to whoever the kernel handed it to next.
    /// Signalling that stranger was how `stop` reported success and changed
    /// nothing, and treating it as a daemon was how a machine could refuse to
    /// start one forever. The lock file remembers which daemon last held the
    /// lock: a pid file naming that same daemon, with the lock now free, names
    /// somebody who has gone.
    fn the_pid_file_still_names_a_daemon(paths: &DaemonPaths) -> bool {
        if !daemon_process_alive(paths) {
            return false;
        }
        let number = |path: &Path| {
            fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
        };
        match number(&paths.lock) {
            Some(last_to_serve) => number(&paths.pid) != Some(last_to_serve),
            // No record: a daemon from before the lock existed, and the number
            // is all there is to go on.
            None => true,
        }
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

    /// Counts a request for as long as it is being answered, however it ends.
    struct RequestGuard(Arc<DaemonState>);

    impl Drop for RequestGuard {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Take a client onto the daemon's books. `None` is this daemon standing
    /// down: the connection belongs to the next generation, not to this one.
    fn register_client(state: &Arc<DaemonState>) -> Option<ClientGuard> {
        let _registration = state
            .client_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.draining.load(Ordering::Acquire) {
            return None;
        }
        state.clients.fetch_add(1, Ordering::Relaxed);
        Some(ClientGuard(Arc::clone(state)))
    }

    fn serve_client(mut stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
        let Some(_client_guard) = register_client(&state) else {
            return Ok(());
        };
        let outbound = stream.try_clone()?;
        // Nothing the daemon writes to a client may wait on that client for
        // ever - see [`CLIENT_WRITE_TIMEOUT`]. The option belongs to the socket
        // rather than to either handle on it, so reads are unaffected.
        outbound.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
        let writer = Arc::new(Mutex::new(outbound));
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
                        state.in_flight.fetch_add(1, Ordering::AcqRel);
                        thread::spawn(move || {
                            let _in_flight = RequestGuard(Arc::clone(&state));
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
                    FrameKind::OpenStream => {
                        let decoded = frame.decode_json::<OpenStream>()?;
                        // One failing stream must not tear down the whole
                        // multiplexed connection: every stream on it (other
                        // live terminals, keystrokes, file streams) would be
                        // cut and force a seconds-long re-dial. So each arm
                        // returns a Result and the failures are answered where
                        // the stream failed - a per-stream error reply - while
                        // only the pipe staying healthy (being able to write
                        // that reply at all) is allowed to end the connection.
                        let opened = (|| -> anyhow::Result<()> {
                            match decoded {
                                OpenStream::Pty {
                                    session_id,
                                    columns,
                                    rows,
                                    ..
                                } => {
                                    let session = daemon_session(&state, &session_id)?;
                                    // Capture the size before the resize so we can tell
                                    // whether this attach actually changed the viewport.
                                    // A changed size means the daemon's parser just
                                    // reflowed the old-size screen, so a snapshot taken
                                    // now is an intermediate frame, not the live one.
                                    let pre_cols = session.columns.load(Ordering::Relaxed);
                                    let pre_rows = session.rows.load(Ordering::Relaxed);
                                    // Adoption rebuilt this screen from a bounded tail of
                                    // history: a partial frame for an app that
                                    // differential-renders. The first attach must force a
                                    // full repaint instead of shipping the snapshot. The
                                    // flag is only cleared once this attach has actually
                                    // delivered its preamble or snapshot: an attach that
                                    // errors out partway (or is abandoned mid-mash) must
                                    // not burn the one repaint chance the next good
                                    // attach gets.
                                    let rebuilt = session.screen_rebuilt.load(Ordering::Relaxed);
                                    session.resize(columns, rows)?;
                                    let size_changed = pre_cols
                                        != session.columns.load(Ordering::Relaxed)
                                        || pre_rows != session.rows.load(Ordering::Relaxed);
                                    let subscriber_id =
                                        state.next_subscriber.fetch_add(1, Ordering::Relaxed);
                                    // One payload, sent first: the daemon's absolute view of the
                                    // live screen. When the parser mirrors the child's real
                                    // screen, it is the full snapshot (modes + row dump) and
                                    // that is exactly the live screen.
                                    //
                                    // Two cases break that mirror, and both leave a frame the
                                    // app has not painted: an attach that CHANGED the size
                                    // (the parser just reflowed the old-size content into the
                                    // new size — a stale intermediate frame), and a fresh
                                    // adoption (the screen was rebuilt by replaying a bounded
                                    // history tail that starts mid-stream, so most cells were
                                    // never painted at all). In both, a full-screen TUI agent
                                    // repaints itself on SIGWINCH: send only the mode preamble
                                    // (alt buffer, scroll region, clear), nudge the child with
                                    // a two-frame resize, and let the app's own repaint —
                                    // already streaming as live frames — paint the screen. That
                                    // optimization only applies to agent kinds that are
                                    // full-screen TUIs: a plain terminal (or any unknown kind)
                                    // has no such repaint, so it keeps the snapshot.
                                    //
                                    // No scrollback seed goes out here any more: rendering a
                                    // redraw-heavy session's history costs seconds, and while it
                                    // ran the snapshot waited behind it, so the client committed
                                    // a partial live frame and sat on it. Older history is paged
                                    // on demand through read_history when the client scrolls past
                                    // its own emulator buffer.
                                    let on_alt = {
                                        let screen = session
                                            .screen
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                                        screen.screen().alternate_screen()
                                    };
                                    let kind = session
                                        .metadata
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .kind
                                        .parse::<AgentKind>()
                                        .ok();
                                    let repaints_on_resize = matches!(
                                        kind,
                                        Some(AgentKind::Codex)
                                            | Some(AgentKind::Claude)
                                            | Some(AgentKind::OpenCode)
                                            | Some(AgentKind::Pi)
                                    );
                                    // For an adoption, `on_alt` cannot gate the force path: the mirror was
                                    // rebuilt by replaying only the bounded history tail
                                    // (RECENT_OUTPUT_LIMIT), and a real alt-screen agent
                                    // enters the alt buffer once, at startup, which a
                                    // multi-megabyte transcript pushes out of any such
                                    // tail. The replayed parser therefore reads primary
                                    // even though the running child is genuinely on alt,
                                    // so an adopted alt screen must force the repaint on
                                    // the rebuilt flag alone, whatever the mirror reports.
                                    // A size change without a rebuild still needs the live
                                    // mirror's alt flag to choose: only an alt-screen TUI
                                    // repaints on the SIGWINCH the nudge sends, and a
                                    // primary-mode session (a kind running a plain child,
                                    // say) has no such repaint to lean on and keeps the
                                    // snapshot. The preamble echoes the mirror's state,
                                    // but the child's own post-SIGWINCH repaint re-emits
                                    // the correct alt entry, so a mismatched preamble byte
                                    // on the adopted path is self-corrected.
                                    let force_repaint =
                                        repaints_on_resize && (rebuilt || (on_alt && size_changed));
                                    if force_repaint {
                                        // The preamble is an ending (alt buffer, region,
                                        // clear), so it must be the first payload byte a
                                        // client reads on this stream. The broadcast gate
                                        // is the subscriber registration itself: nothing
                                        // is written to an unregistered stream, so the
                                        // preamble goes down first while the gate is
                                        // still shut. A live frame that slipped in
                                        // between an opened gate and the clear would be
                                        // erased by it, and the repaint the nudge
                                        // provokes would land on a screen the clear had
                                        // already wiped behind it - leaving the client
                                        // fed only later diffs, which a cached terminal
                                        // then preserves across every switch back.
                                        write_stream_opened(&writer, &frame, None)?;
                                        for chunk in
                                            session.screen_preamble().chunks(DATA_CHUNK_SIZE)
                                        {
                                            write_frame(
                                                &writer,
                                                &Frame::data(frame.stream_id, 0, chunk, true),
                                            )?;
                                        }
                                    } else {
                                        // The snapshot is an absolute screen state, so
                                        // live frames interleaving ahead of it are
                                        // harmless: it corrects them. Register before it
                                        // is written, so no output is ever lost in a
                                        // registration gap.
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
                                    }
                                    if force_repaint {
                                        // Open the gate only now, behind the clear, and
                                        // then nudge: out and back is two real keeper
                                        // RESIZE frames (old-keeper compatible), the app
                                        // gets SIGWINCH, the final size is exactly the
                                        // attach size, and the app's own repaint - now
                                        // broadcast onto this stream - paints over the
                                        // cleared screen.
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
                                        session.resize(columns.saturating_add(1), rows)?;
                                        session.resize(columns, rows)?;
                                    } else {
                                        for chunk in
                                            session.screen_snapshot().chunks(DATA_CHUNK_SIZE)
                                        {
                                            write_frame(
                                                &writer,
                                                &Frame::data(frame.stream_id, 0, chunk, true),
                                            )?;
                                        }
                                    }
                                    // Delivered: the repaint (or the absolute snapshot)
                                    // has been written to this stream, so the rebuild is
                                    // answered and the next attach runs normally. An
                                    // earlier `?` would have left the flag armed for the
                                    // next attach instead.
                                    session.screen_rebuilt.store(false, Ordering::Relaxed);
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
                                    let nonce =
                                        state.next_subscriber.fetch_add(1, Ordering::Relaxed);
                                    let temporary_path = parent.join(format!(
                                        ".muxloom-upload-{}-{nonce}",
                                        std::process::id()
                                    ));
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
                                    match connect_forward(host.as_str(), port) {
                                        Ok(socket) => {
                                            socket.set_nodelay(true)?;
                                            let reader = socket.try_clone()?;
                                            subscriptions.insert(
                                                frame.stream_id,
                                                ClientStream::Tcp { socket },
                                            );
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
                                        Err(error) => {
                                            anyhow::bail!(
                                                "cannot connect to {host}:{port}: {error}"
                                            );
                                        }
                                    }
                                }
                            }
                            Ok(())
                        })();
                        if let Err(error) = opened {
                            write_stream_error(&writer, &frame, error.to_string())?;
                        }
                    }
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

    fn write_stream_opened<W: Write>(
        writer: &Arc<Mutex<W>>,
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

    fn write_stream_error<W: Write>(
        writer: &Arc<Mutex<W>>,
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

    fn stream_tcp<W: Write>(
        writer: &Arc<Mutex<W>>,
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
                        daemon_generation: current_generation(),
                        capabilities: vec![
                            "multiplex-v1".into(),
                            "compression-lz4-v1".into(),
                            "shell-compat-v1".into(),
                            "pty-v1".into(),
                            "send-input-v1".into(),
                            "attention-patterns-v1".into(),
                            "files-v1".into(),
                            "history-v1".into(),
                            "media-v1".into(),
                            FORWARD_CAPABILITY.into(),
                            LISTENERS_CAPABILITY.into(),
                            "handover-drain-v1".into(),
                            "triggers-v1".into(),
                            TALK_CAPABILITY.into(),
                            DIRECT_CAPABILITY.into(),
                            RELAY_CAPABILITY.into(),
                            CHANNELS_CAPABILITY.into(),
                            PARENT_ALERT_CAPABILITY.into(),
                            LINEAGE_CAPABILITY.into(),
                            HISTORY_SEARCH_CAPABILITY.into(),
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
                let delivered = write_response(
                    writer,
                    request_id,
                    if ready {
                        &DaemonResponse::HandoverReady
                    } else {
                        &DaemonResponse::HandoverDeferred
                    },
                );
                // Draining is the promise this daemon cannot take back: it has
                // already stopped accepting work. Keep it even when the client
                // that asked hung up before the answer reached it, because a
                // daemon left alive and draining answers every later launch
                // with a refusal, and its callers spend the rest of its life
                // on the compatibility fallback.
                if ready {
                    state.shutdown.store(true, Ordering::Release);
                }
                delivered
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
            DaemonRequest::ListSessions { live_only, only } => {
                // Which sessions there are is taken under the lock; what each
                // of them is doing is worked out after letting it go. Reading a
                // session means drawing its screen and running the classifiers
                // over it, and the map this holds is the one every other thing
                // the daemon does has to go through to find a session at all -
                // typing into one, opening its screen, handing it a message. A
                // dashboard asking three times a second is not a reason for
                // those to queue behind a room-full of screens being drawn.
                //
                // Asked about one session, the lookup is the same one and the
                // reading is the part that stops happening: a round about a
                // single id used to draw every screen on the machine to keep
                // one of them.
                let live = {
                    let guard = state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match &only {
                        Some(id) => guard
                            .get(id)
                            .map(Arc::clone)
                            .into_iter()
                            .collect::<Vec<_>>(),
                        None => guard.values().map(Arc::clone).collect::<Vec<_>>(),
                    }
                };
                let mut sessions: Vec<_> = live
                    .iter()
                    .map(|session| {
                        // This pass of classification is also where a waiting
                        // child gets marked for its parent: whoever is asking —
                        // dashboard, MCP client, or controller round — runs the
                        // same edge check, and the edge waits for whichever
                        // controller comes round to collect it.
                        let snapshot = session.snapshot();
                        session.note_parent_alert(&snapshot);
                        snapshot
                    })
                    .collect();
                // The archive this daemon read at startup. A caller watching
                // what is running does not want it: no session in there can
                // change while the daemon runs, and there is one for every
                // conversation the machine has ever held.
                if !live_only {
                    let guard = state
                        .persisted_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match &only {
                        // A session is in one list or the other, so an id
                        // already answered for is not looked for again.
                        Some(id) if sessions.is_empty() => {
                            sessions.extend(guard.get(id).map(|session| session.snapshot()));
                        }
                        Some(_) => {}
                        None => sessions.extend(guard.values().map(|session| session.snapshot())),
                    }
                }
                write_response(writer, request_id, &DaemonResponse::Sessions { sessions })
            }
            DaemonRequest::Lineage => {
                // Read straight off the metadata under each lock: no screen is
                // drawn and no recap is classified, because a parent link
                // cannot be read off a screen. Both maps, because the chain
                // from a live session up to the one that started it can pass
                // through an ancestor that has since been put down — leaving
                // the archive out here would refuse messages that are allowed.
                let mut parents: Vec<(String, Option<String>)> = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .map(|session| session.parentage())
                    .collect();
                parents.extend(
                    state
                        .persisted_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .values()
                        .map(|session| session.parentage()),
                );
                write_response(writer, request_id, &DaemonResponse::Parents { parents })
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
                parent,
                powers,
                initial_prompt,
            } => {
                let _drain_guard = state
                    .client_gate
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let _launch_guard = state
                    .launch_gate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.draining.load(Ordering::Acquire) {
                    bail!("muxloomd is draining for a generation handover");
                }
                let session = launch_session(
                    state,
                    session_id.clone(),
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
                    parent.clone(),
                    powers,
                )?;
                if let Some(prompt) = initial_prompt.as_deref() {
                    queue_seed_prompt(state, &session_id, parent, prompt);
                }
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Launched {
                        session: Box::new(session.snapshot()),
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
            DaemonRequest::SendInput { session_id, bytes } => {
                daemon_session(state, &session_id)?.write_input(&bytes)?;
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::SetTrigger { trigger } => {
                let stored = set_trigger(state, trigger)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Triggers {
                        triggers: vec![stored],
                    },
                )
            }
            DaemonRequest::ListTriggers { session_id } => {
                let triggers = state
                    .triggers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .filter(|armed| {
                        session_id
                            .as_ref()
                            .is_none_or(|wanted| &armed.spec.session_id == wanted)
                    })
                    .map(|armed| armed.spec.clone())
                    .collect();
                write_response(writer, request_id, &DaemonResponse::Triggers { triggers })
            }
            DaemonRequest::DeleteTrigger { id } => {
                {
                    let mut triggers = state
                        .triggers
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let before = triggers.len();
                    triggers.retain(|armed| armed.spec.id != id);
                    if triggers.len() == before {
                        bail!("no trigger {id}");
                    }
                    state.armed.store(triggers.len(), Ordering::Relaxed);
                    state.save_triggers(&triggers);
                }
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::TalkPost { draft } => {
                let message = state.talk()?.post(draft)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Talk {
                        page: TalkPage {
                            messages: vec![message],
                            cursor: String::new(),
                            truncated: false,
                        },
                    },
                )
            }
            DaemonRequest::TalkRead { filter } => {
                let page = state.talk()?.read(&filter);
                write_response(writer, request_id, &DaemonResponse::Talk { page })
            }
            DaemonRequest::TalkStatus { label } => {
                let talk = state.talk()?;
                if let Some(label) = label {
                    talk.set_label(&label)?;
                }
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::TalkBoard {
                        state: talk.state(),
                    },
                )
            }
            DaemonRequest::TalkFetch { from, limit } => write_response(
                writer,
                request_id,
                &DaemonResponse::TalkCarry {
                    messages: state.talk()?.since(&from, limit),
                    added: 0,
                },
            ),
            DaemonRequest::TalkDeliver {
                draft,
                deliver,
                reply_expected,
            } => {
                let (message, delivery, reason) =
                    deliver_direct(state, draft, deliver, reply_expected)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::TalkDelivery {
                        message: Box::new(message),
                        delivery,
                        reason,
                    },
                )
            }
            DaemonRequest::DrainAlerts => {
                // The edges are marked on every classification pass; a
                // controller round takes them once and owns the telling. The
                // sessions map is read and let go before anything is built:
                // `take_parent_alert` snapshots, and a lock held across two
                // maps is how this daemon would stop.
                let sessions: Vec<_> = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .filter(|session| session.alert_pending.load(Ordering::Relaxed))
                    .cloned()
                    .collect();
                let alerts = sessions
                    .iter()
                    .filter_map(|session| session.take_parent_alert())
                    .collect();
                write_response(writer, request_id, &DaemonResponse::Alerts { alerts })
            }
            DaemonRequest::RelaySubmit {
                tool,
                arguments,
                session,
            } => {
                let id = state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .submit(&tool, &arguments, &session, crate::relay::now_ms())?;
                write_response(writer, request_id, &DaemonResponse::RelayTicket { id })
            }
            DaemonRequest::RelayPoll { peers, via, who } => {
                let now = crate::relay::now_ms();
                let mut relay = state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let jobs = relay.poll(now, peers, &via, &who);
                // Told back in the same breath: whatever other controllers
                // have said they can reach from here. This daemon carries
                // nothing itself — it only repeats what it was told, to the
                // one side that can act on it.
                let response = DaemonResponse::RelayWork {
                    jobs,
                    known: relay.peers(now),
                };
                drop(relay);
                write_response(writer, request_id, &response)
            }
            DaemonRequest::RelayPeers => {
                let now = crate::relay::now_ms();
                let relay = state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let response = DaemonResponse::RelayReach {
                    peers: relay.peers(now),
                    attached: relay.attached(now),
                };
                drop(relay);
                write_response(writer, request_id, &response)
            }
            DaemonRequest::ChannelsPut { set } => {
                let mut channels = state
                    .channels
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let changed = channels.adopt(set);
                // Written while the lock is held so the file and the memory
                // never disagree about which revision this machine is at.
                let written = changed
                    .then(|| channels.save(&state.paths.channels))
                    .transpose();
                drop(channels);
                if let Err(error) = written {
                    return write_response(
                        writer,
                        request_id,
                        &DaemonResponse::Error {
                            message: format!("failed to store the channel set: {error:#}"),
                        },
                    );
                }
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::ChannelsGet => {
                let set = state
                    .channels
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .redacted();
                // Handed over and forgotten in the same breath. The dashboard
                // is the only thing that reads a chat, so it is the only place
                // these are of any use, and keeping a second copy here would
                // only be a second thing to keep in step.
                let receipts = std::mem::take(
                    &mut *state
                        .receipts
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                );
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::Channels { set, receipts },
                )
            }
            DaemonRequest::ChannelSent { receipt } => {
                let mut receipts = state
                    .receipts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                receipts.push(receipt);
                let over = receipts.len().saturating_sub(crate::channel::RECEIPT_CAP);
                receipts.drain(..over);
                drop(receipts);
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::RelayComplete { id, ok, output } => {
                state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .complete(&id, ok, output, crate::relay::now_ms());
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::RelayResult { id } => {
                let answer = state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .result(&id, crate::relay::now_ms())?;
                write_response(writer, request_id, &DaemonResponse::Relayed { answer })
            }
            DaemonRequest::TalkAppend { messages } => {
                let added = state.talk()?.merge(messages)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::TalkCarry {
                        messages: Vec::new(),
                        added,
                    },
                )
            }
            DaemonRequest::SetAttentionPatterns { patterns } => {
                *state
                    .attention_patterns
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = patterns;
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::ReadHistory {
                session_id,
                offset_from_bottom,
                lines,
                rendered,
            } => {
                let (history, columns, rows) =
                    if let Ok(session) = daemon_session(state, &session_id) {
                        let history = session.read_history(offset_from_bottom, lines, rendered)?;
                        (
                            history,
                            session.columns.load(Ordering::Relaxed),
                            session.rows.load(Ordering::Relaxed),
                        )
                    } else {
                        let session = persisted_session(state, &session_id)?;
                        let history = session.read_history(offset_from_bottom, lines, rendered)?;
                        (history, session.columns, session.rows)
                    };
                write_chunks(writer, stream::HISTORY, request_id, &history.rows)?;
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::HistoryComplete {
                        total_lines: history.total_lines,
                        columns,
                        rows,
                        offset_from_bottom: history.offset_from_bottom,
                        rendered,
                        reached_start: history.reached_start,
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
            DaemonRequest::SearchHistoryAll {
                query,
                max_matches,
                deep,
            } => {
                let max_matches = max_matches.clamp(1, 50);
                // Handles are taken under each lock and the searching is done
                // after letting go. A capture runs to hundreds of megabytes and
                // there are as many of them as the machine has ever held
                // conversations: holding the map shut for that long would stop
                // every other round on the machine, and this map is the one
                // typing into a session and opening its screen both go through.
                //
                // Live first and then the archive, which is the order
                // `ListSessions` answers in - a session is in one map or the
                // other, so nothing is searched twice.
                let live: Vec<Arc<ManagedSession>> = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .map(Arc::clone)
                    .collect();
                let filed: Vec<Arc<PersistedSession>> = state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .map(Arc::clone)
                    .collect();
                // A live session is in the pool whatever its capture weighs:
                // being open is what makes a session the one being asked about.
                // Filed ones are taken newest first until the budget is spent.
                let running: Vec<SearchableCapture> = live
                    .iter()
                    .filter_map(|session| session.searchable_capture())
                    .collect();
                let mut resting: Vec<SearchableCapture> = filed
                    .iter()
                    .filter_map(|session| session.searchable_capture())
                    .collect();
                drop(live);
                drop(filed);
                let skipped = match deep {
                    true => 0,
                    false => narrow_to_pool(&mut resting, SEARCH_POOL_BYTES),
                };
                let captures: Vec<SearchableCapture> = running.into_iter().chain(resting).collect();
                let searched = captures.len();
                let found = search_captures(&captures, &query, max_matches);
                let hits = captures
                    .into_iter()
                    .zip(found)
                    .filter_map(|((session_id, label, _), matches)| {
                        // A capture that cannot be read is a session the answer
                        // is silent about, the same as one that holds no match.
                        // The round is about the machine, and one unreadable
                        // file is no reason to refuse the rest of it.
                        let matches = matches.ok()?;
                        (!matches.is_empty()).then_some(DaemonHistorySearchHit {
                            session_id,
                            label,
                            matches,
                        })
                    })
                    .collect();
                write_response(
                    writer,
                    request_id,
                    &DaemonResponse::HistorySearch {
                        hits,
                        searched,
                        skipped,
                    },
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
                    // Not a plain remove: a round that took this handle before
                    // the map lost it can still be about to write the record,
                    // and a write that lands after the delete brings the
                    // session back for every daemon that starts afterwards.
                    session.discard();
                } else {
                    let session = state
                        .persisted_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session_id)
                        .with_context(|| format!("unknown daemon session {session_id}"))?;
                    let _ = fs::remove_file(&session.history_path);
                    session.discard();
                }
                remove_scratch_dir(&state.paths, &session_id);
                write_response(writer, request_id, &DaemonResponse::Ack)
            }
            DaemonRequest::SetLabel { session_id, label } => {
                let label = label
                    .trim()
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect::<String>();
                match daemon_session(state, &session_id) {
                    Ok(session) => {
                        session
                            .metadata
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .label = label.clone();
                        session.persist_metadata()?;
                    }
                    Err(_) => {
                        let session = persisted_session(state, &session_id)?;
                        let metadata = {
                            let mut metadata = session
                                .metadata
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            metadata.label = label.clone();
                            metadata.clone()
                        };
                        session.persist(&metadata)?;
                    }
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

    /// How often the retirement watch looks, and how long the daemon has to
    /// have nothing in hand before it stands down.
    const RETIREMENT_POLL: Duration = Duration::from_millis(500);
    const RETIREMENT_QUIET: Duration = Duration::from_secs(3);
    /// How long it holds out for that quiet before standing down regardless. A
    /// dashboard left attached to a pane overnight would otherwise keep a
    /// superseded build serving forever, and what waiting saves is one
    /// reconnect every client here already knows how to make.
    const RETIREMENT_DEADLINE: Duration = Duration::from_secs(300);

    fn prepare_handover(state: &Arc<DaemonState>) -> bool {
        let _registration = state
            .client_gate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.draining.load(Ordering::Acquire) {
            return false;
        }
        if state.clients.load(Ordering::Acquire) == 1 {
            // Live sessions no longer defer a handover: every session this
            // daemon serves is owned by its keeper process, which survives the
            // drain and is adopted by the next generation.
            state.draining.store(true, Ordering::Release);
            return true;
        }
        // Other clients are attached, and stopping now would take their
        // connections with it. That used to be a flat refusal, which meant one
        // client that never lets go kept every later build out for the rest of
        // this daemon's life — and the MCP server holds its bridge open for as
        // long as an agent might use it, so on a machine with agents on it
        // there is always one. It is a postponement now: stand down as soon as
        // nothing would be lost, and after long enough even if something
        // would. A dropped connection is remade; a build that never arrives is
        // not.
        if !state.retiring.swap(true, Ordering::AcqRel) {
            spawn_retirement_watcher(Arc::clone(state));
        }
        false
    }

    /// Wait for a moment between things, then stop serving so the build that
    /// asked can take this daemon's place. Nothing here retires a session: the
    /// keepers own them and the next generation adopts them.
    fn spawn_retirement_watcher(state: Arc<DaemonState>) {
        thread::spawn(move || {
            let armed = Instant::now();
            let mut quiet_since = None;
            while !state.shutdown.load(Ordering::Acquire) {
                thread::sleep(RETIREMENT_POLL);
                if daemon_has_work_in_hand(&state) {
                    quiet_since = None;
                    if armed.elapsed() < RETIREMENT_DEADLINE {
                        continue;
                    }
                } else if quiet_since.get_or_insert_with(Instant::now).elapsed() < RETIREMENT_QUIET
                {
                    continue;
                }
                let _registration = state
                    .client_gate
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.draining.swap(true, Ordering::AcqRel) {
                    return;
                }
                state.shutdown.store(true, Ordering::Release);
                eprintln!(
                    "muxloomd is standing down so a newer build can take over; its sessions keep running"
                );
                return;
            }
        });
    }

    /// Whether stopping right now would cost a client something: an answer it
    /// is waiting for, or a session screen it is watching. Both come back on
    /// their own once the next daemon is up, but only after a visible gap.
    fn daemon_has_work_in_hand(state: &DaemonState) -> bool {
        if state.in_flight.load(Ordering::Acquire) > 0 {
            return true;
        }
        state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|session| {
                !session
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_empty()
            })
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

    /// Remove the scratch folder a temporary session ran in. The name comes
    /// from the session id, which is validated before anything is created
    /// under it, so this can only ever delete a folder muxloom made — never a
    /// working directory a client named.
    fn remove_scratch_dir(paths: &DaemonPaths, session_id: &str) {
        if validate_session_id(session_id).is_err() {
            return;
        }
        let _ = fs::remove_dir_all(paths.scratch.join(session_id));
    }

    /// Drop the scratch folders of temporary sessions that no longer exist.
    /// Called once the generation knows which sessions it has: a daemon killed
    /// outright cannot clean up after itself, and its folders would otherwise
    /// stay until the machine is wiped.
    fn sweep_scratch_dirs(state: &Arc<DaemonState>) {
        let Ok(entries) = fs::read_dir(&state.paths.scratch) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let live = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&id);
            if !live {
                let _ = fs::remove_dir_all(entry.path());
                eprintln!("muxloomd removed the scratch folder of session {id}");
            }
        }
    }

    /// The sessions running in one folder, each with when it was launched -
    /// which is what pairs it off against a transcript. In milliseconds, the
    /// units the transcripts are read in; see [`launched_at_ms`].
    type NativeGroup = Vec<(u64, Arc<ManagedSession>)>;

    /// When a session was launched, in the units a transcript keeps its own
    /// clock in.
    ///
    /// A session records its launch in seconds - it is right there in the
    /// session id - while every time a transcript writes down, and the
    /// modification time of the file itself, is in milliseconds. Compared
    /// directly the launch looks like it happened decades before any
    /// conversation, which turns "the transcript that began nearest to this
    /// launch" into "the oldest transcript in the folder": a fresh agent
    /// listed under a name and a recap belonging to somebody else's work.
    fn launched_at_ms(created_at: u64) -> u64 {
        created_at.saturating_mul(1_000)
    }

    /// Keep reading what each session's runtime writes about itself.
    ///
    /// Nothing announces a transcript: the files belong to the CLI, live
    /// outside anything muxloom owns, and grow whenever a turn ends. So this
    /// looks on a slow round rather than being told — one thread for the whole
    /// daemon, and within it only the sessions and folders that have moved.
    fn spawn_native_history_reader(state: &Arc<DaemonState>) {
        let state = Arc::clone(state);
        thread::spawn(move || {
            while !state.shutdown.load(Ordering::Acquire) && !state.draining.load(Ordering::Acquire)
            {
                thread::sleep(NATIVE_POLL);
                refresh_native_history(&state);
            }
        });
    }

    /// One round: every live session whose runtime keeps a transcript, grouped
    /// by the folder it was started in. The grouping is the point — several
    /// agents can be running in one directory, and which conversation belongs
    /// to which of them is a question only the whole group can answer.
    fn refresh_native_history(state: &Arc<DaemonState>) {
        let sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(Arc::clone)
            .collect::<Vec<_>>();
        let mut folders: HashMap<(AgentKind, String), NativeGroup> = HashMap::new();
        for session in sessions {
            if session.archived.load(Ordering::Relaxed) {
                continue;
            }
            let (kind, path, created_at, dead) = {
                let metadata = session
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                (
                    metadata.kind.parse::<AgentKind>().ok(),
                    metadata.path.clone(),
                    metadata.created_at,
                    metadata.dead,
                )
            };
            let Some(kind) = kind.filter(|kind| kind.has_native_history() && !dead) else {
                continue;
            };
            folders
                .entry((kind, path))
                .or_default()
                .push((launched_at_ms(created_at), session));
        }
        for ((kind, path), mut group) in folders {
            // Oldest first, whatever order the map handed them over in: the
            // matching pairs sessions off against transcripts by when each
            // began, and it should not depend on a hash order.
            group.sort_by_key(|(created_at, _)| *created_at);
            refresh_native_folder(kind, &path, &group);
        }
    }

    /// The sessions running in one folder, and the transcripts in it.
    fn refresh_native_folder(kind: AgentKind, path: &str, group: &[(u64, Arc<ManagedSession>)]) {
        // A session that knows which file is its own only has to read that
        // file, and only when it has grown. Listing the folder is what costs,
        // so it happens only for a session still looking for its conversation.
        let looking = group
            .iter()
            .filter(|(_, session)| refresh_native_claim(kind, session))
            .map(|(created_at, session)| (*created_at, Arc::clone(session)))
            .collect::<Vec<_>>();
        if looking.is_empty() {
            return;
        }
        let since = looking
            .iter()
            .map(|(created_at, _)| *created_at)
            .min()
            .unwrap_or_default()
            .saturating_sub(crate::native_history::START_GRACE_MS);
        let threads = crate::native_history::threads_for(kind, path, since);
        let now = now_ms();
        for (_, session) in &looking {
            session
                .native
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .scanned_at = now;
        }
        if threads.is_empty() {
            return;
        }
        // Every session in the folder, not just the ones looking: a session
        // that already has its conversation is what keeps the others off it.
        let facts = group
            .iter()
            .map(|(created_at, session)| session_facts(*created_at, session))
            .collect::<Vec<_>>();
        let picks = crate::native_history::assign_threads(&facts, &threads);
        for ((_, session), pick) in group.iter().zip(picks) {
            let Some(thread) = pick.and_then(|index| threads.get(index)) else {
                // Nothing here answers to this session. It falls back to what
                // can be read off its screen, and asks again next round.
                continue;
            };
            {
                let mut native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if native
                    .claim
                    .as_ref()
                    .is_some_and(|claim| claim.id == thread.id)
                {
                    continue;
                }
                native.claim = Some(NativeClaim {
                    id: thread.id.clone(),
                    path: thread.path.clone(),
                    read_at: thread.updated_at,
                    title: thread.title.clone(),
                    recap: thread.last_message.clone(),
                });
                // A claim just taken - or traded for a better one - has to
                // earn this round's check like any other; the pass below
                // closes it straight away when the first words already agree.
                native.claim_checked = false;
                native.claim_looks = 0;
            }
            // Outside the lock: persisting takes a snapshot, and a snapshot
            // reads the claim that was just made.
            if let Err(error) = session.persist_metadata() {
                eprintln!("muxloomd could not record what a session is reading: {error:#}");
            }
        }
        // The scan was this round's check. Whatever claim a session holds
        // now, held through the scan or just taken from it, agrees with the
        // transcript's own first words or the session keeps asking. Only a
        // positive agreement closes the guess: silence is the transcript
        // not having said its first thing yet, which is exactly when a
        // crossed claim is still worth catching.
        for (_, session) in group.iter() {
            let prompt = session
                .first_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let mut native = session
                .native
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(claim) = native.claim.as_ref() else {
                continue;
            };
            if threads.iter().any(|thread| {
                thread.id == claim.id
                    && crate::native_history::first_text_agreement(
                        prompt.as_deref(),
                        thread.first_message.as_deref(),
                    ) == crate::native_history::FirstText::Match
            }) {
                native.claim_checked = true;
            }
        }
    }

    /// Bring one session's claimed transcript up to date, and say whether the
    /// folder still has to be looked through on its behalf.
    fn refresh_native_claim(kind: AgentKind, session: &Arc<ManagedSession>) -> bool {
        let claimed = {
            let native = session
                .native
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &native.claim {
                Some(claim) => Ok((
                    claim.id.clone(),
                    claim.path.clone(),
                    claim.read_at,
                    native.claim_checked,
                    native.claim_looks,
                    native.scanned_at,
                )),
                None => Err(native.scanned_at),
            }
        };
        let last_output = session.last_output.load(Ordering::Relaxed);
        let (id, path, read_at, checked, looks, scanned_at) = match claimed {
            Ok(claim) => claim,
            // Never matched. A session that has produced nothing since the
            // last look cannot have started writing a transcript either.
            Err(scanned_at) => return scanned_at == 0 || last_output > scanned_at,
        };
        // A claim taken on timing alone - never weighed against the first
        // words both accounts keep - is still a guess, and a crossed pair of
        // guesses is invisible from in here: each session reads the other's
        // conversation, sees a title and a recap, and has no reason to doubt.
        // Such a claim asks for the folder to be looked through a bounded
        // number of times, on rounds where its own session is talking, until
        // the first words say it is right or say whose it really is.
        if !checked
            && looks < NATIVE_CLAIM_CHECK_LOOKS
            && last_output > scanned_at
            && session
                .first_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
        {
            session
                .native
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .claim_looks += 1;
            return true;
        }
        let written = crate::native_history::last_written(&path).unwrap_or_default();
        if written > read_at {
            let Some(thread) = crate::native_history::reread(kind, &path, &id) else {
                return false;
            };
            {
                let mut native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(claim) = native.claim.as_mut().filter(|claim| claim.id == id) else {
                    return false;
                };
                claim.read_at = thread.updated_at;
                // A read that lands mid-turn finds no answer and no name yet;
                // what the conversation said last is still the truth about it.
                if thread.title.is_some() {
                    claim.title = thread.title;
                }
                if thread.last_message.is_some() {
                    claim.recap = thread.last_message;
                }
            }
            if let Err(error) = session.persist_metadata() {
                eprintln!("muxloomd could not record what a session is reading: {error:#}");
            }
            return false;
        }
        // The transcript has stopped growing. That on its own says nothing:
        // an agent repaints its spinner the whole time it is thinking, so a
        // turn that runs for two minutes produces output continuously and
        // appends nothing until it answers. Reading that as "the words are
        // going somewhere else" is how a working session lost its name.
        //
        // What does mean it is a turn that went by the transcript entirely: a
        // prompt submitted after the transcript last grew, an answer that came
        // back, and quiet since. That is the shape of a conversation cleared
        // and started over, which is the case this is here for.
        let now = now_ms();
        let last_input = session.last_input.load(Ordering::Relaxed);
        let asked_and_answered = last_input > written && last_output > last_input;
        let quiet_since = now.saturating_sub(last_output) >= NATIVE_CLAIM_STALE_MS;
        if !asked_and_answered || !quiet_since {
            return false;
        }
        {
            let mut native = session
                .native
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            native.abandoned.push(id);
            native.claim = None;
            native.scanned_at = 0;
            native.claim_checked = false;
            native.claim_looks = 0;
        }
        {
            // The name and the last answer belonged to that conversation, and
            // it is over. Better nothing than the wrong one until the session
            // is matched to whatever it is writing now.
            let mut metadata = session
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            metadata.thread = None;
            metadata.title = None;
        }
        if let Err(error) = session.persist_metadata() {
            eprintln!("muxloomd could not record what a session is reading: {error:#}");
        }
        true
    }

    /// What the matching is given to go on about one session.
    fn session_facts(created_at: u64, session: &Arc<ManagedSession>) -> NativeFacts {
        let first_prompt = session
            .first_prompt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let persisted = session
            .metadata
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thread
            .clone();
        let native = session
            .native
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        NativeFacts {
            created_at,
            seed: native.seed.clone(),
            // A daemon that restarted holds no claim, but the one before it
            // wrote down which transcript this session was reading.
            claimed: native
                .claim
                .as_ref()
                .map(|claim| claim.id.clone())
                .or(persisted),
            abandoned: native.abandoned.clone(),
            // What the daemon heard the session open with, in this
            // generation's own hearing or the last one's - taken before any
            // other lock here, so no path ever holds these two at once.
            first_prompt,
        }
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
        mut args: Vec<String>,
        environment: Vec<(String, String)>,
        created_at: u64,
        columns: u16,
        rows: u16,
        parent: Option<String>,
        powers: Option<crate::model::Powers>,
    ) -> Result<Arc<ManagedSession>> {
        validate_session_id(&session_id)?;
        // Nobody is in front of a session at the moment it starts, so it starts
        // in the runtime's unattended mode - and that is settled here rather
        // than taken on faith from whoever composed the command line. A muxloom
        // from an older build still launches through this daemon, and a session
        // it starts would otherwise sit on the first approval prompt with no
        // one to answer it.
        //
        // Only ever onto the runtime's own executable, though: these are that
        // CLI's flags and mean nothing to anything else. Whatever a wrapper
        // wants said to the agent it wraps, it is the one that knows how. The
        // flags go in front, because Codex reads a `resume` further along as
        // its subcommand and options come before it.
        if let Ok(kind) = kind.parse::<AgentKind>()
            && Path::new(&executable).file_name() == Some(kind.as_str().as_ref())
            && let Some(unattended) = crate::runtime::missing_unattended_arguments(kind, &args)
        {
            args.splice(
                0..0,
                unattended.iter().map(|argument| (*argument).to_string()),
            );
        }
        // A parent is a session id and is written down as given, even when it
        // names a session on another machine: it says which piece of work this
        // belongs to, and that is true wherever the parent runs. What it must
        // never be is this session, which would make a tree with a loop in it.
        let parent = parent
            .map(|parent| validate_session_id(&parent).map(|()| parent))
            .transpose()?
            .filter(|parent| *parent != session_id);
        let path = if path == "~" {
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        } else {
            path
        };
        // A number already in use is a refusal or a revival, never a second
        // identity for one conversation. A running session holds its id
        // against a launch over it, and that is refused outright - shadowing
        // it would strand the running side's parent links and board threads
        // behind a number that now answers to someone else. An archived
        // record holds its number only for the conversation it recorded; a
        // launch arriving with that number is that conversation coming back,
        // and it revives the record in place - same id, label, parent,
        // creation and history file - instead of minting a fresh id the
        // children's parent links would not follow.
        let seed = kind
            .parse::<AgentKind>()
            .ok()
            .and_then(|kind| crate::native_history::resume_seed(kind, &args));
        let resuming = if temporary {
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
            None
        } else {
            resume_in_place(state, &session_id)?
        };
        // Everything below this line can still fail, and the record is out of
        // the index from here until the live session replaces it.
        let rollback = RevivalRollback::new(state, resuming.as_ref());
        // A temporary session runs in a folder of its own that muxloom makes
        // here and removes with it, whatever directory the client named. A
        // scratch chat that moves into the project you happened to have
        // selected leaves its droppings in a repository that never asked for
        // one, and there is nothing to keep afterwards by definition.
        let path = if temporary {
            let scratch = state.paths.scratch.join(&session_id);
            fs::create_dir_all(&scratch)
                .with_context(|| format!("failed to create {}", scratch.display()))?;
            fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
            scratch.to_string_lossy().into_owned()
        } else {
            path
        };
        if !Path::new(&path).is_dir() {
            bail!("working directory does not exist: {path}");
        }
        // Nobody is in front of a session at the moment it starts, and before
        // a runtime will run at all it wants to know whether this directory
        // may be worked in. Asked on a screen no one is watching, that
        // question does not get answered: the session shows a dialog instead
        // of a prompt box, and a message sent to it waits for an input box
        // that never appears. The directory is the one this launch named -
        // muxloom's own scratch, or a path a person typed into a form - so
        // recording it as trusted says back what starting a session there
        // already said. Best effort: a config that cannot be written leaves
        // the launch exactly where it was, stopped on a dialog.
        if let Ok(kind) = kind.parse::<AgentKind>()
            && let Err(error) =
                crate::mcp_register::trust_directory_for_this_daemon(kind, Path::new(&path))
        {
            eprintln!("muxloomd could not record {path} as trusted for {kind}: {error:#}");
        }
        // A launch under a *new* id can still be somebody's resume: until
        // dashboards ask for the archived id itself, reopening a conversation
        // minted a fresh number, and that split is what orphaned fleets onto
        // dead master ids. Find the archived record this launch reopens -
        // same runtime, same folder, and its command line names the very
        // conversation the record was reading - and repair the split as this
        // launch happens: children repointed, alias written on both records.
        let resumed_from: Option<DaemonSession> = match &resuming {
            Some(_) => None,
            None => archived_resume_match(state, &kind, &path, seed.as_deref()),
        };
        if let Some(previous) = &resumed_from {
            if let Some(successor) = resumed_successor(state, previous) {
                bail!(
                    "session {} was already resumed as {successor}, which is still live; \
                     talk to it instead of resuming twice",
                    previous.id
                );
            }
        }
        let carried = resuming.as_ref().or(resumed_from.as_ref());
        // What a resume carries with it, whatever number it comes back on:
        // a caller who passed no name means the one the conversation had,
        // and a master resumed on its own still hangs off whoever started
        // it. Reviving in place additionally keeps the record's own creation
        // - it is the same session, not a younger one wearing its number.
        let label = if label.trim().is_empty() {
            carried
                .map(|record| record.label.clone())
                .unwrap_or_else(|| label.clone())
        } else {
            label
        };
        let parent = carried.and_then(|record| record.parent.clone()).or(parent);
        let created_at = resuming
            .as_ref()
            .map(|record| record.created_at)
            .unwrap_or(created_at);
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
        // The keeper applies environment pairs in order with later entries
        // winning, so the PATH override and the terminal identity fold in at
        // the end. Everything is resolved here: the keeper's behavior is
        // frozen, so it must never need to learn new launch rules.
        let mut keeper_environment = environment;
        if let Some(prepended_path) = prepended_path {
            keeper_environment.push(("PATH".into(), prepended_path.to_string_lossy().into_owned()));
        }
        keeper_environment.push(("TERM".into(), "xterm-256color".into()));
        keeper_environment.push(("COLORTERM".into(), "truecolor".into()));
        keeper_environment.push(("TERM_PROGRAM".into(), "muxloom".into()));
        // Who and where this session is. An agent running in it inherits these
        // when it starts its own MCP client, so posting to the board or being
        // written to by name needs nothing from the agent itself — and an
        // agent that guessed would guess wrong.
        if let Ok(talk) = state.talk() {
            keeper_environment.push(("MUXLOOM_MACHINE".into(), talk.origin()));
            keeper_environment.push(("MUXLOOM_MACHINE_LABEL".into(), talk.label()));
        }
        keeper_environment.push(("MUXLOOM_SESSION_ID".into(), session_id.clone()));
        keeper_environment.push(("MUXLOOM_SESSION_PATH".into(), path.clone()));
        // Who to answer. Absent on a session a person started, which has
        // nobody to report to and is not held to reporting to anybody.
        if let Some(parent) = parent.as_deref() {
            keeper_environment.push(("MUXLOOM_SESSION_PARENT".into(), parent.into()));
        }
        // Which piece of work this session is part of, for the task scope on
        // the board. Worked out once, here, because parentage is fixed at
        // launch: the chain a session hangs off cannot change under it later,
        // and nothing downstream should have to walk it again.
        keeper_environment.push((
            "MUXLOOM_TASK_ROOT".into(),
            task_root(state, &session_id, parent.as_deref()),
        ));
        keeper_environment.push(("MUXLOOM_SESSION_KIND".into(), kind.clone()));
        if !label.trim().is_empty() {
            keeper_environment.push(("MUXLOOM_SESSION_LABEL".into(), label.clone()));
        }
        // A revival in place that was told nothing takes what the record
        // already held: it is the same session coming back, not a new one
        // being started with nobody watching, and a resume that quietly handed
        // a subagent full powers would be a way to shed its limits by dying.
        let powers = powers.or_else(|| resuming.as_ref().and_then(|record| record.powers.clone()));
        // What the agent that asked for this session handed it. Set here and
        // nowhere else, so an agent cannot read its own limits off a variable
        // it could have written: the muxloom tools inside the session read
        // these, and only a launch puts them there. All three go in together
        // or none do — a variable that is absent means nobody said anything
        // about this session, which is what a person's own agent looks like,
        // and a half-written set would read as full powers on the missing
        // dial.
        if let Some(powers) = &powers {
            keeper_environment.push(("MUXLOOM_MAY_MESSAGE".into(), powers.reach.as_str().into()));
            keeper_environment.push(("MUXLOOM_MAY_LAUNCH".into(), powers.launches_list()));
            keeper_environment.push((
                "MUXLOOM_MAY_REACH_PERSON".into(),
                match powers.may_reach_person {
                    true => "yes".into(),
                    false => "no".into(),
                },
            ));
        }
        let history_path = state.paths.history.join(format!("{session_id}.ansi"));
        let metadata_path = state.paths.sessions.join(format!("{session_id}.json"));
        if !temporary {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&history_path)?;
        }
        // The command line was the one place that said outright which
        // conversation this launch means to reopen; the seed was read off it
        // above, before the id question was answered.
        let spec = keeper::KeeperSpec {
            session_id: session_id.clone(),
            program: program.to_string_lossy().into_owned(),
            args,
            environment: keeper_environment,
            cwd: path.clone(),
            columns: columns.max(20),
            rows: rows.max(5),
            history_path: (!temporary).then(|| history_path.clone()),
            socket_path: keeper::socket_path_for(&state.paths.keepers, &session_id),
        };
        if let Some(previous) = &resumed_from {
            // The split is only survivable if it is recorded while it
            // happens: every child still naming the retired id moves to this
            // one, and both records carry the alias so the move can be
            // followed in either direction afterwards.
            let moved = reparent_children(state, &previous.id, &session_id);
            mark_resumed_to(state, &previous.id, &session_id);
            eprintln!(
                "muxloomd resumed archived session {} as {session_id} with {moved} children repointed",
                previous.id
            );
        }
        let mut metadata = DaemonSession {
            id: session_id.clone(),
            kind,
            path,
            label,
            temporary,
            created_at,
            // Live again: whatever this conversation's last ending was, it is
            // not where it sits in the archive any more.
            archived_at: None,
            pid: None,
            dead: false,
            archived: false,
            // A revival in place keeps what the record knew: its last recap
            // and the thread this very launch reopens. Without a native
            // resume seed the conversation starts fresh, and the old thread
            // belongs to the past.
            recap: resuming.as_ref().and_then(|record| record.recap.clone()),
            title: resuming.as_ref().and_then(|record| record.title.clone()),
            thread: resuming
                .as_ref()
                .filter(|_| seed.is_some())
                .and_then(|record| record.thread.clone()),
            seed: seed.clone(),
            // A launch that reopens a thread reopens the conversation that
            // opened with those words too; one that starts fresh has heard
            // nothing yet, and the recorder will fill this in on the first
            // substantial submission.
            first_prompt: resuming
                .as_ref()
                .filter(|_| seed.is_some())
                .and_then(|record| record.first_prompt.clone()),
            working: false,
            needs_attention: false,
            attention_reason: None,
            composer: None,
            parent,
            // Kept so a resume can hand over the same thing again, and so the
            // next launch of this session stamps the same environment.
            powers,
            resumed_from: resumed_from.as_ref().map(|record| record.id.clone()),
            resumed_to: resuming
                .as_ref()
                .and_then(|record| record.resumed_to.clone()),
        };
        // The record precedes the keeper so a crash between the two leaves a
        // session that can be retired, never a keeper nothing knows about.
        persist_session_metadata(&metadata_path, &metadata)?;
        let (stream, status) = match start_keeper(state, &spec) {
            Ok(connection) => connection,
            Err(error) => {
                if resuming.is_some() {
                    // A revival that never got its keeper must leave the
                    // archive exactly as it found it - the rollback does that
                    // on the way out - and the record and its history file
                    // both predate this launch, so neither is this launch's to
                    // delete.
                    return Err(error);
                }
                let _ = fs::remove_file(&metadata_path);
                if temporary {
                    remove_scratch_dir(&state.paths, &session_id);
                } else {
                    let _ = fs::remove_file(&history_path);
                }
                return Err(error);
            }
        };
        metadata.pid = status.child_pid;
        let first_prompt = metadata.first_prompt.clone();
        let session = Arc::new(ManagedSession {
            metadata: Mutex::new(metadata),
            keeper: Mutex::new(
                stream
                    .try_clone()
                    .context("failed to clone keeper stream")?,
            ),
            // This daemon has owned the PTY since before the child's first
            // byte, so whatever the screen comes to show is this session's
            // own. Nothing has been asked of it yet.
            last_output: AtomicU64::new(now_ms()),
            last_input: AtomicU64::new(0),
            attention_patterns: Arc::clone(&state.attention_patterns),
            subscribers: Mutex::new(HashMap::new()),
            screen: Mutex::new(vt100::Parser::new(rows.max(5), columns.max(20), 0)),
            screen_seq: AtomicU64::new(0),
            screen_text: Mutex::new(None),
            inline: Mutex::new(InlineScrollback::default()),
            codex_activity: Mutex::new(CodexActivity::default()),
            draft_watch: Mutex::new(None),
            // A revival that reopens the thread carries the opening words too;
            // a fresh session starts with nothing heard, and the recorder
            // holds the first substantial submission. A launch told to reopen
            // a thread is a third case: this daemon spawned the child, but
            // the conversation opened before it heard anything - and the CLI
            // replays that opening into whatever file it writes now - so what
            // it hears first is not the first thing the person said.
            first_prompt: Mutex::new(first_prompt),
            first_prompt_armed: AtomicBool::new(seed.is_none()),
            screen_rebuilt: AtomicBool::new(false),
            screen_recap: Mutex::new(None),
            screen_settled: AtomicBool::new(false),
            notice: Mutex::new(None),
            alert_pending: AtomicBool::new(false),
            alert_edge: Mutex::new(None),
            native: Mutex::new(NativeLink {
                seed,
                ..NativeLink::default()
            }),
            history_path,
            metadata_path,
            discarded: Mutex::new(false),
            archived: AtomicBool::new(false),
            line_count: AtomicUsize::new(0),
            columns: AtomicU16::new(columns.max(20)),
            rows: AtomicU16::new(rows.max(5)),
        });
        // Output the child produced before the keeper connection existed —
        // the first prompt of a fast-starting agent — is only in the history
        // file. The greeting's byte count splits the transcript exactly, so
        // replaying that prefix leaves the screen gapless and duplicate-free.
        if !temporary
            && let Some(head) = history_prefix_tail(
                &session.history_path,
                status.history_bytes,
                RECENT_OUTPUT_LIMIT as u64,
            )
        {
            session.record_output(&head);
        }
        session.persist_metadata()?;
        state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, Arc::clone(&session));
        rollback.replaced();
        spawn_session_reader(state, Arc::clone(&session), stream);
        Ok(session)
    }

    /// The archived record a launch has taken out of the index, put back if
    /// the launch does not go on to replace it.
    ///
    /// Taking the record is how a revival comes back as itself, but a launch
    /// can still fail afterwards for reasons that have nothing to do with the
    /// archive - the folder it ran in is gone, its CLI is no longer installed,
    /// the keeper will not start - and the record is then the only copy of who
    /// that session was. Left out of the index it is not merely invisible: the
    /// next attempt on the same number finds nothing to revive, so it mints a
    /// fresh conversation wearing the number and writes over the record's
    /// metadata, and the label, the parent, the creation time and the children
    /// hanging off it are gone for good. So the take is undone on every way
    /// out but the one that succeeds.
    struct RevivalRollback<'a> {
        state: &'a DaemonState,
        record: Option<&'a DaemonSession>,
    }

    impl<'a> RevivalRollback<'a> {
        fn new(state: &'a DaemonState, record: Option<&'a DaemonSession>) -> Self {
            Self { state, record }
        }

        /// The launch reached the point where the live session holds the
        /// number, so there is nothing left to put back.
        fn replaced(mut self) {
            self.record = None;
        }
    }

    impl Drop for RevivalRollback<'_> {
        fn drop(&mut self) {
            let Some(record) = self.record.take() else {
                return;
            };
            // Back into the index it came out of, and no other. A session
            // this daemon ran itself and has not reloaded since sits among
            // the live records however it ended, and the take leaves it
            // there; filing it under the archive instead would be worse than
            // losing it, because a record in the archive that never reached
            // the archive is one `resume_in_place` refuses to hand back at
            // all - the number would answer nothing and revive as nothing
            // until the daemon restarted.
            if self
                .state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&record.id)
            {
                return;
            }
            // The file first, because a launch that got as far as writing its
            // own metadata has already written over the record's - and then
            // read back rather than trusted, so what returns to the index is
            // what a restart would find there.
            let path = self
                .state
                .paths
                .sessions
                .join(format!("{}.json", record.id));
            let _ = persist_session_metadata(&path, record);
            if let Ok(Some((id, entry))) = load_persisted_session(&self.state.paths, &path) {
                self.state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, entry);
            }
        }
    }

    /// Reclaim a session id for the record that already holds it, or say the
    /// id is taken. `Ok(Some(record))` hands the launch the archived record
    /// this very id names - out of the daemon's index, its metadata left on
    /// disk only until the revived record replaces it - because a launch
    /// arriving with an archived session's number *is* that conversation
    /// coming back, and it must come back as itself: the same id children and
    /// board threads still name, the same label, the same history file. A
    /// running session holding the id is a refusal, never a shadow: one
    /// conversation never gets two ids, and one id never gets two.
    fn resume_in_place(state: &DaemonState, session_id: &str) -> Result<Option<DaemonSession>> {
        // Launches are serialized by the caller's client gate, so the two
        // indexes cannot shift under this between look and take.
        let ended = {
            let live = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match live.get(session_id) {
                Some(session) => {
                    let snapshot = session.snapshot();
                    if snapshot.temporary {
                        bail!("daemon session already exists: {session_id}");
                    }
                    if !snapshot.archived && !snapshot.dead {
                        bail!(
                            "session {session_id} is still live; refusing to resume over a \
                             running session - archive it first"
                        );
                    }
                    // Archived or ended with the keeper already gone: the map
                    // entry only stood where an archive retirement would have
                    // retired it, and the launch reclaims the slot by writing
                    // the live session over it at the end. Left in place until
                    // then, because a launch that fails on the way there is a
                    // record that has to still be here afterwards, and this is
                    // the only copy of it there is.
                    Some(snapshot)
                }
                None => None,
            }
        };
        let (record, from_archive) = match ended {
            Some(record) => (record, false),
            None => {
                let filed = state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(session_id)
                    .map(|entry| entry.snapshot());
                let Some(snapshot) = filed else {
                    return Ok(None);
                };
                // The rule the running index is judged by, asked here too:
                // retired, or ended and never retired, is a number its own
                // conversation can come back on. It used to take the archive
                // flag alone, which quietly meant every session that simply
                // ended stopped being revivable the moment a daemon restarted
                // and read it back off the disk - the same number, refused
                // for the same conversation, on the strength of nothing but
                // which index it was sitting in. Nothing in this one has a
                // keeper: a record with a socket still beside it is left to
                // adoption rather than read in here.
                if !snapshot.archived && !snapshot.dead {
                    bail!("daemon session already exists: {session_id}");
                }
                (snapshot, true)
            }
        };
        // Asked of the record wherever it was found. A restart is where this
        // is easiest to lose: the record a conversation moved away from is
        // read back off the disk with the move still written on it, and
        // reviving it there would put one conversation in two places.
        if let Some(successor) = resumed_successor(state, &record) {
            bail!(
                "session {session_id} was already resumed as {successor}, which is still \
                 live; talk to it instead of resuming twice"
            );
        }
        if from_archive {
            // Taken only now the record is the launch's to have: a refusal
            // above has to leave the index as it found it, and nothing is
            // watching to put a record back until the launch arms its
            // rollback.
            state
                .persisted_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(session_id);
        }
        Ok(Some(record))
    }

    /// The stopped record a new-id launch reopens: same runtime, same
    /// folder, and the command line names the very conversation the record
    /// was matched to - or, never matched, the one its own launch was told to
    /// reopen. Deliberately narrow: without that seed to agree on, a kind and
    /// a folder are not evidence that two sessions are one conversation.
    fn archived_resume_match(
        state: &DaemonState,
        kind: &str,
        path: &str,
        seed: Option<&str>,
    ) -> Option<DaemonSession> {
        let wanted = seed?;
        // Which sessions there are is taken under the locks; what each of them
        // is is read after letting them go. A live session's snapshot draws its
        // screen, and the map it is held in is the one every other request goes
        // through to find a session at all.
        let live = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(Arc::clone)
            .collect::<Vec<_>>();
        let persisted = state
            .persisted_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(Arc::clone)
            .collect::<Vec<_>>();
        live.iter()
            .map(|session| session.snapshot())
            .chain(persisted.iter().map(|entry| entry.snapshot()))
            // Stopped, however it stopped. What must never be matched is a
            // session still running - reopening one would repoint its fleet
            // onto a second number while the first still answers - and being
            // retired is not the only way to have stopped. An agent whose
            // turn ended, or a child the daemon found gone when it came back,
            // is a record nobody archived and a conversation just as over,
            // and skipping those is what left a reopen minting a fresh number
            // with the old one's fleet still hanging off it.
            .filter(|record| record.archived || record.dead)
            .filter(|record| record.kind == kind && record.path == path)
            .filter(|record| {
                record.thread.as_deref() == Some(wanted) || record.seed.as_deref() == Some(wanted)
            })
            .max_by(|left, right| {
                (left.created_at, left.id.as_str()).cmp(&(right.created_at, right.id.as_str()))
            })
    }

    /// The live session an archived record's conversation already moved to,
    /// if any: resuming a record whose successor still runs would put one
    /// conversation in two places at once.
    fn resumed_successor(state: &DaemonState, record: &DaemonSession) -> Option<String> {
        let successor = record.resumed_to.as_deref()?;
        let live = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live.get(successor)
            .map(|session| session.snapshot())
            .filter(|snapshot| !snapshot.dead && !snapshot.archived)
            .map(|_| successor.to_string())
    }

    /// Record on the retired side where its conversation moved to, in both
    /// places it can rest: the archived index and an ended entry still held
    /// live until the next daemon retires it.
    fn mark_resumed_to(state: &DaemonState, previous_id: &str, successor: &str) {
        let entry = state
            .persisted_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(previous_id)
            .map(Arc::clone);
        if let Some(entry) = entry {
            let record = {
                let mut metadata = entry
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                metadata.resumed_to = Some(successor.to_string());
                metadata.clone()
            };
            if let Err(error) = entry.persist(&record) {
                eprintln!("muxloomd could not record a resume alias on {previous_id}: {error:#}");
            }
        }
        // Found under the map lock, written to with it let go: persisting a
        // session draws its screen and syncs a file to disk, and the map is
        // what every other request has to go through to reach any session.
        let session = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(previous_id)
            .map(Arc::clone);
        if let Some(session) = session {
            session
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .resumed_to = Some(successor.to_string());
            let _ = session.persist_metadata();
        }
    }

    /// Point every child still naming a retired master at its successor,
    /// live and archived alike, and persist each rewrite. The parent link is
    /// the fleet's only spine - fleet listings, alerts, and the next resume's
    /// subtree all read it - so leaving it on a dead id is how a resumed
    /// master came back to an empty fleet.
    fn reparent_children(state: &DaemonState, previous_id: &str, successor: &str) -> usize {
        let mut moved = 0;
        // Every rewrite below draws a screen and syncs a file to disk, and a
        // master coming back can have a whole subtree under it. Doing that with
        // the session map in hand shuts the daemon for as long as it takes:
        // nothing else can so much as find a session to type into. So the map
        // is only held long enough to say who is in it.
        let live = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(Arc::clone)
            .collect::<Vec<_>>();
        for session in &live {
            if session.session_id() == successor {
                continue;
            }
            let reparented = {
                let mut metadata = session
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if metadata.parent.as_deref() == Some(previous_id) {
                    metadata.parent = Some(successor.to_string());
                    true
                } else {
                    false
                }
            };
            if reparented {
                let _ = session.persist_metadata();
                moved += 1;
            }
        }
        let persisted = state
            .persisted_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(Arc::clone)
            .collect::<Vec<_>>();
        for entry in &persisted {
            if entry.snapshot().id == successor {
                continue;
            }
            let record = {
                let mut metadata = entry
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if metadata.parent.as_deref() == Some(previous_id) {
                    metadata.parent = Some(successor.to_string());
                    Some(metadata.clone())
                } else {
                    None
                }
            };
            if let Some(record) = record {
                if let Err(error) = entry.persist(&record) {
                    eprintln!("muxloomd could not repoint a child of {previous_id}: {error:#}");
                }
                moved += 1;
            }
        }
        moved
    }

    /// The tail of the first `prefix` bytes of a history file, bounded by
    /// `limit`. The file only ever grows, so the prefix is stable even while
    /// the keeper keeps appending behind it.
    fn history_prefix_tail(path: &Path, prefix: u64, limit: u64) -> Option<Vec<u8>> {
        if prefix == 0 {
            return None;
        }
        let mut file = File::open(path).ok()?;
        let start = prefix.saturating_sub(limit);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = vec![0u8; (prefix - start) as usize];
        file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    /// Relay keeper frames into the session until the keeper goes away. The
    /// keeper appends history itself, so output here only feeds the screen and
    /// the attached subscribers.
    fn spawn_session_reader(
        state: &Arc<DaemonState>,
        session: Arc<ManagedSession>,
        mut stream: UnixStream,
    ) {
        let state = Arc::clone(state);
        thread::spawn(move || {
            let watched = session.session_id();
            let exited = loop {
                match keeper::read_frame(&mut stream) {
                    Ok(Some((keeper::frame::DATA, payload))) => {
                        session.last_output.store(now_ms(), Ordering::Relaxed);
                        session.record_output(&payload);
                        session.broadcast(&payload);
                        // Every byte this session produces passes here, so a
                        // daemon nobody armed pays one relaxed load for it.
                        if state.armed.load(Ordering::Relaxed) > 0 {
                            fire_triggers(&state, &session, &watched);
                        }
                    }
                    Ok(Some((keeper::frame::EXITED, _))) => break true,
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break false,
                }
            };
            // A deleted session was already removed from the map with its
            // files; recording its death would recreate the metadata.
            let still_tracked = {
                let sessions = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sessions
                    .get(&session.session_id())
                    .is_some_and(|tracked| Arc::ptr_eq(tracked, &session))
            };
            if session.temporary() {
                // The files go before the map entry. Leaving the session in the
                // map a moment longer costs nothing, while dropping it first
                // publishes a gone session whose record is still on disk — and
                // whatever looks next, a scan or a restarting daemon, would
                // find a temporary session that was supposed to leave nothing.
                let _ = fs::remove_file(&session.history_path);
                session.discard();
                remove_scratch_dir(&state.paths, &session.session_id());
                if still_tracked {
                    state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&session.session_id());
                }
                state.drop_triggers_for(&watched);
                session.send_quit();
                return;
            }
            if exited {
                if still_tracked {
                    session.mark_dead();
                }
                state.drop_triggers_for(&watched);
                session.send_quit();
                return;
            }
            // The stream ended without an exit: the keeper crashed, or a newer
            // daemon adopted the session out from under a draining one. Only
            // the crash is a death — a drain leaves the record alone for the
            // generation that took over, and so do the triggers armed on it.
            if still_tracked
                && !state.draining.load(Ordering::Acquire)
                && !state.shutdown.load(Ordering::Acquire)
            {
                session.mark_dead();
                state.drop_triggers_for(&watched);
            }
        });
    }

    /// Launch the keeper for a session and greet it. Tests run the identical
    /// keeper loop on a thread instead of spawning the binary.
    fn start_keeper(
        state: &DaemonState,
        spec: &keeper::KeeperSpec,
    ) -> Result<(UnixStream, keeper::KeeperStatus)> {
        match state.keeper_mode {
            KeeperMode::InProcess => {
                let listener = keeper::bind_socket(&spec.socket_path)?;
                let spec = spec.clone();
                thread::spawn(move || {
                    if let Err(error) = keeper::run(spec, listener) {
                        eprintln!("muxloomd in-process keeper failed: {error:#}");
                    }
                });
            }
            KeeperMode::Process => spawn_keeper_process(state, spec)?,
        }
        connect_keeper(&spec.socket_path, Duration::from_secs(5)).map_err(|error| {
            // A keeper that dies on its way up says why on its own stderr and
            // nowhere else: the socket it would have explained itself over is
            // exactly what it never reached. Left out, the launch failure
            // surfaces as "muxloomd was unavailable" and blames the daemon for
            // whatever the keeper actually hit.
            match keeper_log_tail(&state.paths, &spec.session_id) {
                Some(tail) => anyhow!("{error:#}; the keeper's own log says: {tail}"),
                None => error,
            }
        })
    }

    /// The last few lines a keeper wrote before it gave up, bounded so a log
    /// that grew is still cheap to look at and a notice stays one line.
    fn keeper_log_tail(paths: &DaemonPaths, session_id: &str) -> Option<String> {
        const WINDOW: u64 = 8 * 1024;
        const LIMIT: usize = 300;
        let path = paths.keepers.join(format!("{session_id}.log"));
        let text = String::from_utf8_lossy(&history_tail(&path, WINDOW)?).into_owned();
        let tail: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .rev()
            .take(3)
            .collect();
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.into_iter().rev().collect::<Vec<_>>().join("; ");
        if joined.chars().count() > LIMIT {
            joined = joined.chars().take(LIMIT).collect::<String>() + "…";
        }
        Some(joined)
    }

    /// The binary this daemon starts its keepers from.
    ///
    /// Not, in the end, the path it was executed from. A keeper may have to be
    /// started long after a package manager has moved, replaced or deleted
    /// that file — an upgrade in the middle of a working day is exactly when
    /// that happens — and what a process reports as its own path outlives the
    /// file itself. The spawn then fails with "no such file", every launch
    /// from then on quietly falls back to legacy tmux, and the daemon looks
    /// healthy the whole time. So a serving daemon takes a copy of itself
    /// while it still can, and starts its keepers from that.
    ///
    /// It has to be a copy of *this* build. A keeper speaks its daemon's
    /// protocol, so reaching for whichever muxloomd happens to be installed
    /// now would trade a launch that fails loudly for one that misbehaves.
    fn keeper_executable_for(paths: &DaemonPaths, keeper_mode: KeeperMode) -> PathBuf {
        let running = std::env::current_exe().unwrap_or_default();
        if keeper_mode != KeeperMode::Process {
            return running;
        }
        match stash_executable(paths, &running) {
            Ok(stashed) => stashed,
            Err(error) => {
                eprintln!(
                    "muxloomd could not keep a copy of itself, so its sessions depend on {} staying where it is: {error:#}",
                    running.display()
                );
                running
            }
        }
    }

    /// Put a copy of `running` in the state directory and forget every copy
    /// that is not it.
    ///
    /// The name is the generation, which ends in the running file's own size
    /// and write time, so a rebuilt working tree is never mistaken for a copy
    /// already taken. Discarding the others is safe while their keepers run: a
    /// running process holds its file open, and on unix that is all it needs.
    fn stash_executable(paths: &DaemonPaths, running: &Path) -> Result<PathBuf> {
        // Read before copying: an unreadable executable is worth saying so
        // about here, rather than failing halfway through the copy below.
        fs::metadata(running).with_context(|| format!("cannot read {}", running.display()))?;
        let name = format!("muxloomd-{}", slugged(&current_generation()));
        let stashed = paths.bin.join(&name);
        if !stashed.exists() {
            // Whole or not at all: a copy interrupted halfway is a file that
            // exists, has the right name, and cannot be executed.
            let pending = paths.bin.join(format!("{name}.{}", std::process::id()));
            let copied = fs::copy(running, &pending)
                .with_context(|| format!("failed to copy {}", running.display()))
                .and_then(|_| {
                    fs::set_permissions(&pending, fs::Permissions::from_mode(0o700))
                        .context("failed to make the copy executable")
                })
                .and_then(|()| fs::rename(&pending, &stashed).context("failed to put the copy in"));
            if let Err(error) = copied {
                let _ = fs::remove_file(&pending);
                return Err(error);
            }
        }
        if let Ok(entries) = fs::read_dir(&paths.bin) {
            for entry in entries.flatten() {
                if entry.file_name() != name.as_str() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(stashed)
    }

    /// A string turned into one that can be a file name.
    fn slugged(text: &str) -> String {
        text.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect()
    }

    /// Spawn the detached `muxloomd keeper` that owns one session. The spec
    /// travels over stdin so environment values never appear in `ps`.
    fn spawn_keeper_process(state: &DaemonState, spec: &keeper::KeeperSpec) -> Result<()> {
        let executable = state.keeper_executable.clone();
        let log = open_log(&state.paths.keepers.join(format!("{}.log", spec.session_id)))?;
        let error_log = log.try_clone()?;
        let mut command = Command::new(executable);
        command
            .arg("keeper")
            .current_dir("/")
            .stdin(Stdio::piped())
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
        let mut child = command
            .spawn()
            .context("failed to start the session keeper")?;
        let mut stdin = child.stdin.take().context("keeper has no stdin")?;
        let spec_line = serde_json::to_string(spec).context("failed to encode the keeper spec")?;
        stdin
            .write_all(spec_line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .context("failed to hand the keeper its spec")?;
        drop(stdin);
        // Reap the keeper whenever it exits so it never lingers as a zombie of
        // a long-lived daemon.
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// Connect to a keeper socket and read its greeting.
    fn connect_keeper(
        socket_path: &Path,
        patience: Duration,
    ) -> Result<(UnixStream, keeper::KeeperStatus)> {
        let deadline = Instant::now() + patience;
        let mut stream = loop {
            match UnixStream::connect(socket_path) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                    let _ = error;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("keeper never answered at {}", socket_path.display())
                    });
                }
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let status = keeper::read_greeting(&mut stream)?;
        stream.set_read_timeout(None)?;
        Ok((stream, status))
    }

    /// Adopt every session whose keeper outlived the previous daemon, and
    /// retire the ones whose keeper is gone. Runs before the socket serves
    /// clients so the first ListSessions already sees the adopted sessions.
    /// Socket filenames are digests, so the walk goes record → socket; a
    /// socket no record accounts for is dismissed rather than left running.
    fn adopt_keeper_sessions(state: &Arc<DaemonState>) {
        let mut accounted: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = fs::read_dir(&state.paths.sessions) {
            for entry in entries.flatten() {
                let metadata_path = entry.path();
                if metadata_path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Some(id) = metadata_path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                if validate_session_id(&id).is_err()
                    || state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .contains_key(&id)
                {
                    continue;
                }
                let socket_path = keeper::socket_path_for(&state.paths.keepers, &id);
                if !socket_path.exists() {
                    continue;
                }
                accounted.push(socket_path.clone());
                match adopt_keeper_session(state, &id, &socket_path) {
                    Ok(true) => eprintln!("muxloomd adopted running session {id}"),
                    Ok(false) => {
                        eprintln!("muxloomd retired session {id}; its keeper had finished")
                    }
                    Err(error) => eprintln!("muxloomd could not adopt session {id}: {error:#}"),
                }
            }
        }
        if let Ok(entries) = fs::read_dir(&state.paths.keepers) {
            for entry in entries.flatten() {
                let socket_path = entry.path();
                if socket_path.extension().and_then(|value| value.to_str()) != Some("sock")
                    || accounted.contains(&socket_path)
                {
                    continue;
                }
                dismiss_orphan_keeper(&socket_path);
            }
        }
    }

    /// End a keeper no session record accounts for. Without a record the
    /// session cannot be served or archived, and a keeper left running would
    /// hold its child forever.
    fn dismiss_orphan_keeper(socket_path: &Path) {
        let Ok((mut stream, status)) = connect_keeper(socket_path, Duration::from_millis(300))
        else {
            let _ = fs::remove_file(socket_path);
            return;
        };
        eprintln!(
            "muxloomd dismissing the keeper of unrecorded session {}",
            status.session_id
        );
        if status.alive {
            let _ = keeper::write_frame(&mut stream, keeper::frame::KILL, &[]);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            while let Ok(Some((kind, _))) = keeper::read_frame(&mut stream) {
                if kind == keeper::frame::EXITED {
                    break;
                }
            }
        }
        let _ = keeper::write_frame(&mut stream, keeper::frame::QUIT, &[]);
    }

    /// Adopt one keeper-owned session; `Ok(true)` means it is live again under
    /// this daemon, `Ok(false)` that it was retired into the archive.
    fn adopt_keeper_session(
        state: &Arc<DaemonState>,
        id: &str,
        socket_path: &Path,
    ) -> Result<bool> {
        let metadata_path = state.paths.sessions.join(format!("{id}.json"));
        let history_path = state.paths.history.join(format!("{id}.ansi"));
        let Ok((stream, status)) = connect_keeper(socket_path, Duration::from_millis(500)) else {
            // The keeper is gone and left its socket behind. Retire the record
            // the way an interrupted daemon's sessions are retired.
            let _ = fs::remove_file(socket_path);
            if metadata_path.is_file()
                && let Ok(Some((id, session))) =
                    load_persisted_session(&state.paths, &metadata_path)
            {
                state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, session);
            }
            return Ok(false);
        };
        let mut metadata: DaemonSession =
            serde_json::from_slice(&fs::read(&metadata_path).with_context(|| {
                format!("adopted keeper has no metadata {}", metadata_path.display())
            })?)?;
        if metadata.id != id {
            bail!("metadata id {} does not match {id}", metadata.id);
        }
        if !status.alive {
            // The child died while no daemon was listening. The keeper kept
            // the transcript; record the death and dismiss it.
            metadata.dead = true;
            metadata.pid = None;
            metadata.working = false;
            metadata.needs_attention = false;
            metadata.attention_reason = None;
            persist_session_metadata(&metadata_path, &metadata)?;
            let mut dismiss = stream;
            let _ = keeper::write_frame(&mut dismiss, keeper::frame::QUIT, &[]);
            if let Ok(Some((id, session))) = load_persisted_session(&state.paths, &metadata_path) {
                state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, session);
            }
            return Ok(false);
        }
        metadata.dead = false;
        metadata.pid = status.child_pid;
        let archived = metadata.archived;
        let temporary = metadata.temporary;
        let seed = metadata.seed.clone();
        // What the previous generation heard the session open with belongs
        // to this one now: without it a first submission the new daemon never
        // saw would be invented from whatever it types next, and the matcher
        // would check a claim against the wrong words.
        let first_prompt = metadata.first_prompt.clone();
        let columns = status.columns.max(20);
        let rows = status.rows.max(5);
        let session = Arc::new(ManagedSession {
            metadata: Mutex::new(metadata),
            keeper: Mutex::new(
                stream
                    .try_clone()
                    .context("failed to clone keeper stream")?,
            ),
            // Nothing heard from it yet, and nothing asked of it yet. A screen
            // is evidence of what a session is doing only while something
            // vouches that it is the screen the session has *now*, and that
            // something is having heard it. Which matters most on adoption:
            // there the screen is replayed out of the capture and may be
            // drawing a turn that ended an hour ago, so counting the takeover
            // as the session having just spoken is how every untouched agent
            // on a machine lit up as working at once whenever a new build took
            // over.
            last_output: AtomicU64::new(0),
            last_input: AtomicU64::new(0),
            attention_patterns: Arc::clone(&state.attention_patterns),
            subscribers: Mutex::new(HashMap::new()),
            screen: Mutex::new(vt100::Parser::new(rows, columns, 0)),
            screen_seq: AtomicU64::new(0),
            screen_text: Mutex::new(None),
            inline: Mutex::new(InlineScrollback::default()),
            codex_activity: Mutex::new(CodexActivity::default()),
            draft_watch: Mutex::new(None),
            // Read back out of the metadata, the same as the seed and the
            // claim below: this daemon did not hear the conversation open,
            // and nothing it hears now is the opening, so the recorder is
            // disarmed whatever the metadata held.
            first_prompt: Mutex::new(first_prompt),
            first_prompt_armed: AtomicBool::new(false),
            // The screen below is replayed from a bounded history tail (or is
            // empty for a temporary session): a partial frame for an app that
            // differential-renders, so its first attach forces a repaint.
            screen_rebuilt: AtomicBool::new(true),
            screen_recap: Mutex::new(None),
            screen_settled: AtomicBool::new(false),
            notice: Mutex::new(None),
            // A child that fell onto its question while no daemon was watching
            // gets its edge marked on the first look: no edge is recorded yet,
            // so adoption does not mute the parent's alert.
            alert_pending: AtomicBool::new(false),
            alert_edge: Mutex::new(None),
            // The command line that started this one belongs to a keeper this
            // daemon did not spawn, so both of the things it said - what the
            // launch meant to reopen, and what the last generation matched it
            // to - are read back out of the metadata instead.
            native: Mutex::new(NativeLink {
                seed,
                ..NativeLink::default()
            }),
            history_path,
            metadata_path,
            discarded: Mutex::new(false),
            archived: AtomicBool::new(archived),
            line_count: AtomicUsize::new(0),
            columns: AtomicU16::new(columns),
            rows: AtomicU16::new(rows),
        });
        // Rebuild the screen, and with it the working/attention
        // classification, from the transcript the keeper kept appending while
        // no daemon was watching. The greeting's byte count bounds the read so
        // nothing streaming in now is doubled.
        if !temporary
            && let Some(tail) = history_prefix_tail(
                &session.history_path,
                status.history_bytes,
                RECENT_OUTPUT_LIMIT as u64,
            )
        {
            session.record_output(&tail);
        }
        session.persist_metadata()?;
        state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string(), Arc::clone(&session));
        spawn_session_reader(state, session, stream);
        Ok(true)
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }

    /// Seconds since the epoch, the unit a session's own timestamps are kept
    /// in: `created_at` is seconds, so what it is compared against must be.
    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
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

    /// The session at the top of the chain of subagents `session_id` hangs
    /// off: the task it belongs to. A session nobody started is its own task.
    ///
    /// A parent is a bare session id with no machine on it, so a chain that
    /// leaves this machine stops at the last id this daemon can resolve. That
    /// is the right place to stop rather than a shortcoming: both machines end
    /// up naming the same id, which is what makes a task spanning two of them
    /// one task.
    fn task_root(state: &DaemonState, session_id: &str, parent: Option<&str>) -> String {
        let mut root = session_id.to_string();
        let mut seen = BTreeSet::from([root.clone()]);
        let mut next = parent.map(str::to_string);
        while let Some(id) = next {
            if !seen.insert(id.clone()) {
                // Somebody's records have a loop in them. Whatever the chain
                // means, it has no top, and the walk has to end somewhere.
                break;
            }
            next = session_parent(state, &id);
            root = id;
        }
        root
    }

    /// Who started a session, as this daemon has it recorded. A session it has
    /// never heard of has no parent it can name, which ends a walk.
    fn session_parent(state: &DaemonState, session_id: &str) -> Option<String> {
        if let Ok(session) = daemon_session(state, session_id) {
            return session.snapshot().parent;
        }
        persisted_session(state, session_id)
            .ok()
            .and_then(|session| session.snapshot().parent)
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
            let symlink = file_type.is_symlink();
            // Classify a link by what it points at: a link to a directory has to
            // be enterable, and a link to a file has to be previewable. A broken
            // link resolves to nothing and stays Other.
            let resolved = if symlink {
                fs::metadata(entry.path()).ok().map(|metadata| {
                    if metadata.is_dir() {
                        FileEntryKind::Directory
                    } else if metadata.is_file() {
                        FileEntryKind::File
                    } else {
                        FileEntryKind::Other
                    }
                })
            } else if file_type.is_dir() {
                Some(FileEntryKind::Directory)
            } else if file_type.is_file() {
                Some(FileEntryKind::File)
            } else {
                Some(FileEntryKind::Other)
            };
            let kind = resolved.unwrap_or(FileEntryKind::Other);
            let metadata = if symlink {
                fs::metadata(entry.path()).ok()
            } else {
                entry.metadata().ok()
            };
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path().to_string_lossy().into_owned(),
                kind,
                symlink,
                size: if kind == FileEntryKind::File {
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
            truncated: false,
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
        /// This session's id and the one that started it. A chain from a live
        /// session up to the one that started it can pass through an ancestor
        /// that has since been put down, so the archive answers this too.
        fn parentage(&self) -> (String, Option<String>) {
            let metadata = self
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (metadata.id.clone(), metadata.parent.clone())
        }

        fn snapshot(&self) -> DaemonSession {
            self.metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// The capture to search and the name to answer with, or nothing at all
        /// when this session keeps no capture.
        fn searchable_capture(&self) -> Option<SearchableCapture> {
            let metadata = self
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (!metadata.temporary).then(|| {
                (
                    metadata.id.clone(),
                    metadata.label.clone(),
                    self.history_path.clone(),
                )
            })
        }

        /// Write this record to its file, unless the file is gone on purpose.
        ///
        /// A handle to a record outlives its removal from the archive index:
        /// every round that walks the sessions takes the handles under the lock
        /// and does its work after letting go, so a rewrite can still be in
        /// flight when a delete lands. Writing it afterwards puts the file back,
        /// the next daemon reads it in at startup, and the session the person
        /// deleted is in their archive again - permanently, because nothing
        /// looks at that file again to notice it should not exist.
        ///
        /// The gate is held across the write, and [`Self::discard`] takes it to
        /// remove the file, so the two cannot cross: either the write finishes
        /// and the delete takes it away, or the delete goes first and the write
        /// is dropped.
        fn persist(&self, metadata: &DaemonSession) -> Result<()> {
            let discarded = self
                .discarded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *discarded {
                return Ok(());
            }
            persist_session_metadata(&self.metadata_path, metadata)
        }

        /// Take this record off disk for good. Anything still holding a handle
        /// and about to write finds the gate shut and writes nothing.
        fn discard(&self) {
            let mut discarded = self
                .discarded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *discarded = true;
            let _ = fs::remove_file(&self.metadata_path);
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
                metadata.archived_at.get_or_insert_with(now_secs);
                metadata.clone()
            };
            self.persist(&metadata)
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
        ) -> Result<HistoryRead> {
            if self.snapshot().temporary {
                return Ok(HistoryRead::empty());
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

        /// The capture to search and the name to answer with, or nothing at all
        /// when this session keeps no capture.
        ///
        /// Read off the metadata alone: naming a session is not a question the
        /// screen answers, and drawing one per session is what a search of the
        /// whole machine exists to avoid. Handing back the path rather than
        /// doing the search is what lets the caller let go of the session
        /// before reading hundreds of megabytes off the disk.
        fn searchable_capture(&self) -> Option<SearchableCapture> {
            let metadata = self
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (!metadata.temporary).then(|| {
                (
                    metadata.id.clone(),
                    metadata.label.clone(),
                    self.history_path.clone(),
                )
            })
        }

        /// The last answer visible on the session's screen, or the last one
        /// that was.
        ///
        /// `visible_screen` is the screen as the terminal draws it rather than
        /// the bytes that drew it: an agent paints its window with cursor
        /// moves, so the raw stream has no lines in it - a status bar arrives
        /// as `manualmodeon?forshortcuts` and the composer arrives one
        /// keystroke at a time, each repaint appended to the last. Rendering
        /// first is what makes the difference between reading a sentence and
        /// reading the pixels it was written with.
        /// `visible_screen` is `None` for a screen nothing will paint again:
        /// the final reading has already been taken and kept, and drawing the
        /// grid a second time could only reach the same answer.
        fn recap_on_screen(&self, kind: AgentKind, visible_screen: Option<&str>) -> Option<String> {
            let drawn = visible_screen.and_then(|screen| extract_recap(kind, screen));
            let mut kept = self
                .screen_recap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if drawn.is_some() {
                kept.clone_from(&drawn);
                return drawn;
            }
            kept.clone()
        }

        /// This session's id and the one that started it, read off the
        /// metadata alone.
        ///
        /// The cheap half of [`Self::snapshot`], for the callers that only ever
        /// wanted the parent link: nothing here touches the screen lock, so
        /// asking the whole machine costs a map walk rather than a room full of
        /// grids laid out as text.
        fn parentage(&self) -> (String, Option<String>) {
            let metadata = self
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (metadata.id.clone(), metadata.parent.clone())
        }

        fn snapshot(&self) -> DaemonSession {
            let mut snapshot = self
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            snapshot.archived = self.archived.load(Ordering::Relaxed);
            // The opening words this daemon heard outrank the ones it loaded;
            // an adopted session holds only what the last generation wrote,
            // and that must survive the re-persist adoption does.
            if let Some(prompt) = self
                .first_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                snapshot.first_prompt = Some(prompt);
            }
            if let Ok(kind) = snapshot.kind.parse::<AgentKind>() {
                // Drawn once and read five ways. Laying the grid out as text is
                // most of what classifying a session costs, and every reading
                // below wants the same picture of the same moment - so taking
                // it twice would not only cost twice, it would let the screen
                // move in between and answer two questions about two screens.
                //
                // A stopped session is where that stops being true. Nothing
                // paints one, so its picture is finished: the recap is the only
                // reading still wanted from it, and once that has been taken it
                // has been taken for good. The record stays in the map for as
                // long as the daemon runs, so without this the share of every
                // listing spent redrawing screens that cannot move grows with
                // uptime -- a third of them here, after a night.
                let stopped = snapshot.dead || snapshot.archived;
                let visible_screen =
                    match stopped && self.screen_settled.swap(true, Ordering::Relaxed) {
                        true => None,
                        false => Some(self.visible_screen()),
                    };
                // What the runtime wrote down about itself beats anything read
                // off its screen: it is the turn as the agent meant it, not
                // the frame the terminal happened to be painting.
                let native = {
                    let native = self
                        .native
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    native
                        .claim
                        .as_ref()
                        .map(|claim| (claim.id.clone(), claim.title.clone(), claim.recap.clone()))
                };
                let mut native_recap = None;
                if let Some((id, title, recap)) = native {
                    snapshot.thread = Some(id);
                    if title.is_some() {
                        snapshot.title = title;
                    }
                    native_recap = recap;
                }
                // Only when the runtime's own account of itself is not to be
                // had. What it wrote down is the turn as the agent meant it;
                // the screen is a guess at the same thing.
                snapshot.recap =
                    native_recap.or_else(|| self.recap_on_screen(kind, visible_screen.as_deref()));
                if stopped {
                    snapshot.pid = None;
                    snapshot.working = false;
                    snapshot.needs_attention = false;
                    snapshot.attention_reason = None;
                    snapshot.composer = None;
                } else {
                    // A running session is never the settled one, so the screen
                    // was drawn above and every reading below has it.
                    let visible_screen = visible_screen.unwrap_or_else(|| Arc::from(""));
                    snapshot.composer = composer(kind, &visible_screen);
                    let patterns = self
                        .attention_patterns
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    snapshot.attention_reason = attention_reason(kind, &visible_screen, &patterns);
                    snapshot.needs_attention = snapshot.attention_reason.is_some();
                    // A screen claiming a turn is running is believed for as
                    // long as the marker it is claiming it with can be
                    // believed while nothing comes off the PTY: seconds for a
                    // spinner, which stops turning the moment the CLI stops
                    // painting, but minutes for an interrupt hint held on a
                    // status bar, which is exactly what a turn that shells out
                    // to a build leaves behind while it says nothing at all.
                    // Either way it takes having heard the session at least
                    // once: a screen replayed into an adopted session is a
                    // record of what it was doing, not of what it is doing.
                    let heard = self.last_output.load(Ordering::Relaxed);
                    let patience = match working_marker_is_held(&visible_screen) {
                        true => WORKING_HELD_QUIET_MS,
                        false => WORKING_TICKING_QUIET_MS,
                    };
                    let fresh = heard != 0 && now_ms().saturating_sub(heard) < patience;
                    let working_hint = self
                        .codex_activity
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .working();
                    snapshot.working = !snapshot.needs_attention
                        && fresh
                        && if kind == AgentKind::Codex {
                            working_hint.unwrap_or_else(|| agent_is_working(kind, &visible_screen))
                        } else {
                            agent_is_working(kind, &visible_screen)
                        };
                    // A trigger that fired asked for someone: it outranks the
                    // classification, which only knows what is on screen.
                    if let Some(notice) = self
                        .notice
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                    {
                        snapshot.attention_reason = Some(notice);
                        snapshot.needs_attention = true;
                        snapshot.working = false;
                    }
                }
            }
            snapshot
        }

        /// Mark a tell owed to this session's parent - but only for what
        /// counts as an edge: a child with a parent, alive, and now asking
        /// for attention. The first tell fires as the classification turns;
        /// while the *same* question sits there the tells run out along
        /// `PARENT_ALERT_REMINDERS_MS` and then stop, because a stuck screen,
        /// most of all a wrongly-classified one, must not become a
        /// minute-ticker at its parent.
        ///
        /// What makes the next question a *different* one is the child having
        /// stopped waiting for `PARENT_ALERT_SETTLE_MS` first. The keys still
        /// say which question it is, and a new one after a settled stretch is
        /// told about at once, schedule and all - but a key that changes while
        /// the child has been waiting the whole time is a reading changing, not
        /// the child moving, and it buys nothing.
        ///
        /// This is the difference between a leash and a leash with a catch on
        /// it. Both keys are read off a live session: the reason off the
        /// screen, the last words out of the runtime's transcript when it has
        /// one and off the screen when it does not. A claim dropped and
        /// retaken swaps the second reading for the other source's answer -
        /// a *different* string, not a missing one - while the child sits on
        /// the very same dialog it has sat on all afternoon. Every such swap
        /// used to start the count at zero with the first tell due at once,
        /// which is how one parked child spent twelve hours telling its parent
        /// about itself fifty times in the words of the schedule that exists
        /// to stop exactly that.
        ///
        /// An attention pass that reads *not* attention is therefore the only
        /// thing that refills the tank, and only by lasting: a classifier
        /// blinking off for a pass and back is nowhere near the settle time,
        /// and a child that answers one dialog and lands on the next inside a
        /// minute rides the schedule it was already on - which owes it a
        /// reminder at the minute mark anyway.
        ///
        /// Marking is cheap and unconditional because a daemon does not know
        /// whether any controller will ever ask for these; the gate that turns
        /// the feature off is the controller's own config, on the way out.
        fn note_parent_alert(&self, snapshot: &DaemonSession) {
            if snapshot.parent.is_none() || snapshot.dead || snapshot.archived {
                return;
            }
            let now = now_ms();
            if !snapshot.needs_attention {
                // Not waiting is not nothing to record: it is the only reading
                // that can make the next question a new one. Timed from the
                // first pass that saw it, so that how long the child has been
                // free is a fact about the child rather than about how often
                // anybody happened to look.
                if let Some(edge) = self
                    .alert_edge
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
                    && edge.free_since == 0
                {
                    edge.free_since = now;
                }
                return;
            }
            let reason_key = snapshot.attention_reason.clone().unwrap_or_default();
            let recap_key = snapshot
                .recap
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            // The child is waiting, so whatever free stretch it was in has
            // ended - closed here, before anything below can return early on
            // this pass. A stretch left standing while a tell is pending would
            // be spent minutes later by a child that has been waiting all
            // along, which is the catch this is here to take off the leash.
            let settled = {
                let mut guard = self
                    .alert_edge
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match guard.as_mut() {
                    Some(edge) => {
                        let settled = edge.free_since != 0
                            && now.saturating_sub(edge.free_since) >= PARENT_ALERT_SETTLE_MS;
                        edge.free_since = 0;
                        settled
                    }
                    None => false,
                }
            };
            if self.alert_pending.load(Ordering::Relaxed) {
                return;
            }
            let mut guard = self
                .alert_edge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let replace = match guard.as_ref() {
                // Both keys are readings, and a reading can be missing or come
                // from the other source while the question behind it does not
                // move: the runtime drops and retakes its transcript claim and
                // the last words fall back to the screen's answer, a screen
                // behind a modal has no last word to scrape at all. An edge
                // replaced on any of that is the ceiling coming off - the count
                // starts at zero, the first tell is due at once, and the parent
                // gets the minute-ticker this schedule exists to prevent. So a
                // key change is only news about a child that has been seen free
                // long enough for there to be a new question to be news about.
                Some(edge) => {
                    settled
                        && (edge.reason_key != reason_key
                            || (!recap_key.is_empty()
                                && !edge.recap_key.is_empty()
                                && edge.recap_key != recap_key))
                }
                None => true,
            };
            if replace {
                // A new question: told about like it is new, schedule and all.
                *guard = Some(AlertEdge {
                    reason_key,
                    recap_key,
                    told: 0,
                    last_claimed_at: 0,
                    free_since: 0,
                });
            } else if let Some(edge) = guard.as_mut()
                && edge.recap_key.is_empty()
            {
                // The reading that was missing when this edge was installed,
                // arriving late. Learning it costs nothing, changes nothing
                // owed, and is what lets the next *real* change be seen as one
                // rather than measured against a blank.
                edge.recap_key = recap_key;
            }
            let edge = guard.as_mut().expect("an edge was just installed");
            // `told` counts what has been claimed for this pair, first tell
            // included; reminder n (1-based) waits PARENT_ALERT_REMINDERS[n-1]
            // since the last claim, and nothing is ever due past the schedule.
            let due = match edge.told {
                0 => true,
                n if usize::try_from(n).is_ok_and(|n| n <= PARENT_ALERT_REMINDERS_MS.len()) => {
                    now.saturating_sub(edge.last_claimed_at)
                        >= PARENT_ALERT_REMINDERS_MS[usize::try_from(n).unwrap() - 1]
                }
                _ => false,
            };
            if !due {
                return;
            }
            edge.told += 1;
            edge.last_claimed_at = now;
            drop(guard);
            self.alert_pending.store(true, Ordering::Relaxed);
        }

        /// Hand the marked tell over, once. The controller that takes it owns
        /// the delivery; if the delivery fails, the schedule above covers it:
        /// the still-unanswered question is a reminder away, not an hour.
        /// A child that died or was archived while waiting says nothing: its
        /// prompt is not asking anybody any more.
        fn take_parent_alert(&self) -> Option<ParentAlert> {
            if !self.alert_pending.swap(false, Ordering::AcqRel) {
                return None;
            }
            let snapshot = self.snapshot();
            if snapshot.dead || snapshot.archived {
                return None;
            }
            let parent_session_id = snapshot.parent.clone()?;
            let at = self
                .alert_edge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|edge| edge.last_claimed_at)
                .unwrap_or(0);
            Some(ParentAlert {
                session_id: snapshot.id.clone(),
                parent_session_id,
                kind: snapshot.kind.clone(),
                label: snapshot.label.clone(),
                attention_reason: snapshot.attention_reason.clone(),
                recap: snapshot.recap.clone(),
                at,
            })
        }

        /// Write what this session is to its file, unless the file is gone on
        /// purpose.
        ///
        /// A handle to a session outlives its removal from the map: the rounds
        /// that walk the sessions take the handles under the lock and do the
        /// work - drawing a screen, syncing a file - after letting go, so a
        /// write can still be in flight when a delete lands. Writing it
        /// afterwards puts the record back, the next daemon reads it in at
        /// startup, and the session the person deleted is in their archive
        /// again for good. The pump loop has always checked the map before
        /// recording a death for exactly this reason; this is that check, made
        /// the session's own and unmissable.
        ///
        /// The gate is held across the write and [`Self::discard`] takes it to
        /// remove the file, so the two cannot cross.
        fn persist_metadata(&self) -> Result<()> {
            // Outside the gate: a snapshot draws this session's screen and
            // takes its own locks, and the gate is only ever the innermost one.
            let record = self.snapshot();
            let discarded = self
                .discarded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *discarded {
                return Ok(());
            }
            persist_session_metadata(&self.metadata_path, &record)
        }

        /// Take this session's record off disk for good. Anything still holding
        /// a handle and about to write finds the gate shut and writes nothing.
        fn discard(&self) {
            let mut discarded = self
                .discarded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *discarded = true;
            let _ = fs::remove_file(&self.metadata_path);
        }

        fn keeper_frame(&self, kind: u8, payload: &[u8]) -> Result<()> {
            let mut stream = self
                .keeper
                .lock()
                .map_err(|_| anyhow!("session keeper stream is poisoned"))?;
            keeper::write_frame(&mut *stream, kind, payload)
                .context("failed to reach the session keeper")
        }

        fn resize(&self, columns: u16, rows: u16) -> Result<()> {
            let columns = columns.max(20);
            let rows = rows.max(5);
            if self.columns.load(Ordering::Relaxed) == columns
                && self.rows.load(Ordering::Relaxed) == rows
            {
                // Re-attaching at an unchanged viewport must not re-SIGWINCH the
                // child: a full-screen TUI (opencode) reflows its whole screen on
                // a resize, so a redundant frame costs a redraw for no change.
                return Ok(());
            }
            self.columns.store(columns, Ordering::Relaxed);
            self.rows.store(rows, Ordering::Relaxed);
            {
                let mut screen = self
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // A reflow rewrites every row, so it counts as changing the
                // grid every bit as much as output does.
                self.screen_seq.fetch_add(1, Ordering::AcqRel);
                resize_parser(&mut screen, rows, columns);
            }
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&columns.to_be_bytes());
            payload[2..].copy_from_slice(&rows.to_be_bytes());
            self.keeper_frame(keeper::frame::RESIZE, &payload)
        }

        fn write_input(&self, bytes: &[u8]) -> Result<()> {
            // Someone is typing here, so whatever a trigger wanted looked at
            // has been looked at.
            self.notice
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            // Take the clock by swapping, not storing: whether anything was
            // ever asked of this session before this payload is what tells
            // the opening words from the sentence that followed them. Only
            // the very first input can be the opening of the conversation,
            // and a prompt the daemon missed - typed at a terminal in pieces
            // too small to record - is not to be invented from a later one.
            let ever_asked = self.last_input.swap(now_ms(), Ordering::Relaxed) != 0;
            if !ever_asked && self.first_prompt_armed.load(Ordering::Relaxed) {
                self.record_first_prompt(bytes);
            }
            self.keeper_frame(keeper::frame::DATA, bytes)
        }

        /// Keep the first words this session was opened with, in the daemon's
        /// own hearing.
        ///
        /// A submission arrives whole - delivered messages as one bracketed
        /// paste, controller input as one payload - while keystrokes typed at
        /// an attached terminal arrive a few bytes at a time, each too short
        /// to pass the length floor and the escape-sequence guard. So the
        /// first payload that reads as a sentence is the opening line, and
        /// nothing later overwrites it.
        fn record_first_prompt(&self, bytes: &[u8]) {
            if self
                .first_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
            {
                return;
            }
            let Ok(submitted) = std::str::from_utf8(bytes) else {
                return;
            };
            // The paste brackets and the Enter that submits are how the text
            // travelled, not what was said; an escape anywhere else is raw
            // key noise, not words.
            let submitted = submitted.replace("\x1b[200~", "").replace("\x1b[201~", "");
            let submitted = submitted.trim();
            if submitted.contains('\x1b')
                || submitted.chars().count() < crate::native_history::MIN_FIRST_TEXT_CHARS
            {
                return;
            }
            {
                let mut first = self
                    .first_prompt
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if first.is_some() {
                    return;
                }
                *first = Some(submitted.to_string());
            }
            // The one write that makes the opening outlive this daemon: a
            // restarted generation that never heard what the session was
            // asked cannot check its claim against content.
            let _ = self.persist_metadata();
        }

        fn set_notice(&self, text: String) {
            *self
                .notice
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
            let _ = self.persist_metadata();
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

        /// Ask the keeper to kill the child. The death lands asynchronously on
        /// the session's reader thread; an unreachable keeper falls back to
        /// signalling the child directly so a stop always means stop.
        fn stop(&self) -> Result<()> {
            if self.keeper_frame(keeper::frame::KILL, &[]).is_err()
                && let Some(pid) = self
                    .metadata
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pid
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            Ok(())
        }

        /// Dismiss a keeper whose child is gone; it removes its socket and
        /// exits. Harmless if the keeper already left.
        fn send_quit(&self) {
            let _ = self.keeper_frame(keeper::frame::QUIT, &[]);
        }

        /// The mode preamble every attaching client needs before row content:
        /// the screen buffer the daemon is on, the app's scroll region, and a
        /// full clear. Sent on its own when an attach just resized an
        /// alt-screen TUI, whose row dump at that moment is a reflowed copy of
        /// the old-size screen — the app's post-SIGWINCH repaint (streamed as
        /// live frames) is what paints the real one.
        fn screen_preamble(&self) -> Vec<u8> {
            let screen = self
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let inline = self
                .inline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut out = Vec::new();

            // Match the client's active screen buffer.  A fresh client parser
            // starts in the primary buffer, so explicitly enter/leave alt to
            // match the daemon's state.
            if screen.screen().alternate_screen() {
                out.extend_from_slice(b"\x1b[?1049h");
            } else {
                out.extend_from_slice(b"\x1b[?1049l");
            }

            // Reinstall the scroll region the app has set, so the client's
            // InlineScrollback tracks the same region and subsequent scrolls
            // behave identically.
            if let Some(region) = inline.region_sequence() {
                out.extend_from_slice(region.as_bytes());
            }

            out.extend_from_slice(b"\x1b[2J");
            out
        }

        /// Serialises the current screen state as escape-sequence bytes that,
        /// fed to a fresh vt100 parser of the same dimensions, reproduce the
        /// exact visible state (contents, cursor, input modes, scroll region,
        /// alt-screen flag).  Replaces the old "replay last 2 MiB of raw bytes"
        /// approach which was lossy for alt-screen TUIs.
        fn screen_snapshot(&self) -> Vec<u8> {
            let mut out = self.screen_preamble();
            let screen = self
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Full absolute redraw: all rows (with SGR) + cursor position +
            // input modes + title. The clear already went out in the preamble
            // — `state_formatted` only emits the rows that carry content, so
            // without it a live frame interleaved before the rows (the writer
            // is locked per frame) could leave bytes on a row the daemon's
            // screen has blank.
            out.extend_from_slice(&screen.screen().state_formatted());
            out
        }

        /// Take in everything the session has just written: the screen the
        /// daemon keeps of it, what that says about whether it is working, and
        /// how far its transcript has got.
        ///
        /// Every byte every session produces arrives here, on the thread
        /// draining that session's PTY, in whatever sized pieces the PTY hands
        /// over - an agent repainting its window does so a few hundred bytes at
        /// a time, tens of times a second. So this runs on the critical path of
        /// the child's own output, and work proportional to anything but
        /// The screen laid out as text. Walking the grid is most of what
        /// classifying a session costs, and only output and a resize can change
        /// what the walk would find -- so a reading taken since the last of
        /// those is that reading again. Without this a machine full of sessions
        /// sitting still laid the same unchanged pictures out over and over:
        /// once per listing round, once per trigger pass, once per draft check,
        /// each of them taking the screen lock away from the reader thread that
        /// wants it to paint.
        fn visible_screen(&self) -> Arc<str> {
            let wanted = self.screen_seq.load(Ordering::Acquire);
            if let Some((taken_at, text)) = self
                .screen_text
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                && *taken_at == wanted
            {
                return Arc::clone(text);
            }
            let (taken_at, drawn) = {
                let screen = self
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Read while holding the lock the mutators bump it under, so
                // the count and the picture are the same moment.
                (
                    self.screen_seq.load(Ordering::Acquire),
                    Arc::<str>::from(screen.screen().contents()),
                )
            };
            let mut kept = self
                .screen_text
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Two readings can race past each other on the way here; the older
            // one must not be the one that stays.
            if kept.as_ref().is_none_or(|(seen, _)| *seen < taken_at) {
                *kept = Some((taken_at, Arc::clone(&drawn)));
            }
            drawn
        }

        /// `bytes` does not belong in it.
        fn record_output(&self, bytes: &[u8]) {
            self.line_count.fetch_add(
                bytes.iter().filter(|&&byte| byte == b'\n').count(),
                Ordering::Relaxed,
            );
            {
                let mut screen = self
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut inline = self
                    .inline
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Before the grid moves, so a reading that has not yet taken
                // this lock cannot mistake the picture it is about to get for
                // the one it already has.
                self.screen_seq.fetch_add(1, Ordering::AcqRel);
                // Route through the same InlineScrollback the client uses so the
                // daemon's screen state stays consistent with what a client
                // would render (scroll-region rewriting included).
                inline.process(&mut screen, bytes);
            }
            self.codex_activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .process(bytes);
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
                    // A frame that went out in part leaves everything else on
                    // that connection reading the rest of this payload as its
                    // next header: the client's other streams and its request
                    // responses all sit on this one socket. What failed is the
                    // connection, not the subscription, so close it and let the
                    // client come back rather than hand it garbage.
                    let _ = subscriber
                        .writer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .shutdown(Shutdown::Both);
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
                // The first death is the one that counts: a record read again
                // by a later generation must not be restamped with the moment
                // that generation noticed it was gone.
                metadata.archived_at.get_or_insert_with(now_secs);
            }
            let _ = self.persist_metadata();
        }

        fn read_history(
            &self,
            offset_from_bottom: usize,
            lines: usize,
            rendered: bool,
        ) -> Result<HistoryRead> {
            if self.temporary() {
                return Ok(HistoryRead::empty());
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
            let mut file = File::create(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(metadata)?)?;
            // Reach the disk before the rename publishes the name. A crash in
            // between is exactly the case this metadata exists to survive, and
            // an unsynced rename can leave the record empty rather than stale.
            file.sync_all()?;
            drop(file);
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

    /// A page of a session's history as one read returns it.
    struct HistoryRead {
        rows: Vec<u8>,
        /// How many rows the read found in all, its screen included.
        total_lines: usize,
        /// The offset the read could honour, which falls short of the one asked
        /// for when the history does not reach that far.
        offset_from_bottom: usize,
        /// Whether the read started at the beginning of the log.
        ///
        /// This is what separates a page that has found the oldest row there is
        /// from one that only reached as far back as it was asked to. A render
        /// replays a window of the log, so on its own `total_lines` measures
        /// that window; only a read that began at byte zero measures the
        /// session, and a client needs the difference to know where to stop
        /// scrolling.
        reached_start: bool,
    }

    impl HistoryRead {
        /// The answer for a session that keeps no transcript.
        fn empty() -> Self {
            Self {
                rows: Vec::new(),
                total_lines: 0,
                offset_from_bottom: 0,
                reached_start: true,
            }
        }
    }

    /// Read a session's history back as rendered rows rather than raw log
    /// lines, by replaying the tail of the log through an emulator.
    ///
    /// How far back a window of bytes reaches in rows depends entirely on what
    /// the agent writes — a full-screen redraw costs a screenful of bytes and
    /// moves the history along by nothing — so the render starts at a window of
    /// `least` bytes and widens until it holds the rows that were asked for, it
    /// has read the log from the start, or it has read `most`.
    /// How much of a log to replay for a page that deep, before any widening.
    ///
    /// The window used to be one size for every caller: whatever the deepest
    /// page anyone asks for needs, which is an attach seeding five thousand
    /// rows of scrollback. But a wait polling a session reads eighty rows once
    /// a round, and a screen read two hundred, and those paid the attach's
    /// window every time -- sixteen megabytes seeked to and fed through a fresh
    /// emulator, once a second, per waiting agent, to answer with a screen.
    ///
    /// So the guess is made from the page instead. It is only a guess, because
    /// what a row costs in the log is what the agent painted it with; the
    /// widening in [`render_history_file`] is what corrects it, and a page that
    /// comes back short is read again from four times as far back.
    fn seed_window(lines: usize, columns: u16, least: u64, most: u64) -> u64 {
        let most = most.max(1);
        u64::try_from(lines)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(columns.max(1)))
            .saturating_mul(SCROLLBACK_SEED_BYTES_PER_CELL)
            .clamp(least.max(1).min(most), most)
    }

    fn render_history_file(
        path: &Path,
        columns: u16,
        rows: u16,
        offset_from_bottom: usize,
        lines: usize,
        least: u64,
        most: u64,
    ) -> Result<HistoryRead> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open history {}", path.display()))?;
        let end = file.metadata()?.len();
        let wanted = lines.max(1);
        let mut window = seed_window(wanted, columns, least, most);
        let mut reached: Option<usize> = None;
        loop {
            let start = end.saturating_sub(window);
            file.seek(SeekFrom::Start(start))?;
            let (page, total_lines, actual_offset) = render_history_rows(
                BufReader::new(&mut file).take(end - start),
                columns,
                rows,
                offset_from_bottom,
                wanted,
            )?;
            // Reaching the row that was asked for is not enough: the rows above
            // it have to be there too. A page that arrives short cannot be
            // scrolled through, and the request for the next one rounds back to
            // this same offset and asks for it again.
            let filled = total_lines.saturating_sub(actual_offset) >= wanted;
            // Widening is how the read reaches rows it has not seen yet, so a
            // window that quadrupled and came back holding no more of them has
            // reached every row the log has to give.
            //
            // Without this the only other way out is the ceiling, and an agent
            // that paints its screen in place always takes it: on the alternate
            // screen nothing ever scrolls off, so the render finds a screenful
            // however far back it reads and `filled` can never come true. That
            // is a hundred and twenty-eight megabytes seeked to and fed through
            // a fresh emulator to answer with that same screenful -- per screen
            // read, and once a second for every agent sitting in a wait.
            let widened_in_vain = reached.is_some_and(|before| total_lines <= before);
            if (actual_offset >= offset_from_bottom && filled)
                || widened_in_vain
                || start == 0
                || window >= most
            {
                return Ok(HistoryRead {
                    rows: page,
                    total_lines,
                    offset_from_bottom: actual_offset,
                    reached_start: start == 0,
                });
            }
            reached = Some(total_lines);
            window = window.saturating_mul(4).min(most);
        }
    }

    fn read_history_file(
        path: &Path,
        total_lines: usize,
        rows: u16,
        offset_from_bottom: usize,
        lines: usize,
    ) -> Result<HistoryRead> {
        let scrollback = total_lines.saturating_sub(usize::from(rows));
        let actual_offset = offset_from_bottom.min(scrollback);
        let end = total_lines.saturating_sub(actual_offset);
        // A raw read walks the log from the top counting newlines, because the
        // line it is asked for is only findable that way. Asking for none of
        // them is asking how long the log is, and the answer is already in
        // hand: the count is an atomic on a live session and read once per
        // process on a stopped one. Reading the file to say nothing back is how
        // the backup came to lift every byte of every session off the disk,
        // every five minutes, to learn a number per session.
        if lines == 0 {
            return Ok(HistoryRead {
                rows: Vec::new(),
                total_lines,
                offset_from_bottom: actual_offset,
                reached_start: true,
            });
        }
        let start = end.saturating_sub(lines);
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
        Ok(HistoryRead {
            rows: output,
            total_lines,
            offset_from_bottom: actual_offset,
            // Raw reads count the whole log, so `total_lines` already measures
            // the session rather than the reach of one page.
            reached_start: true,
        })
    }

    /// A capture a search can be put to: the session's id, the label to answer
    /// with, and the file holding what it said.
    type SearchableCapture = (String, String, PathBuf);

    /// How much filed capture a near search will read before it stops.
    ///
    /// A machine that has been running for months keeps several gigabytes of
    /// capture, and a search that reads all of it takes seconds - for a word
    /// that, nearly always, was said in a session still open or lately closed.
    /// So the near search is bounded by what it reads rather than by how many
    /// files it opens: one enormous capture is the same cost as a hundred small
    /// ones, and it is the bytes that make a caller wait.
    const SEARCH_POOL_BYTES: u64 = 512 * 1024 * 1024;

    /// Cut a set of filed captures down to the ones a near search reads, newest
    /// written first, and say how many were left behind.
    ///
    /// Recency is taken from the capture's own modification time rather than
    /// from the session record: it is one `stat` per file against reading the
    /// file itself, and it answers the question actually being asked - when
    /// something last happened in this session - for a session filed after a
    /// resume as truthfully as for one filed once.
    ///
    /// A capture that cannot be stat'd sorts oldest rather than being dropped,
    /// so a machine whose clock or filesystem is strange still searches, just
    /// in an order it did not choose.
    fn narrow_to_pool(captures: &mut Vec<SearchableCapture>, budget: u64) -> usize {
        let mut by_recency: Vec<(Option<SystemTime>, u64, SearchableCapture)> = captures
            .drain(..)
            .map(|capture| {
                let stat = fs::metadata(&capture.2).ok();
                let written = stat.as_ref().and_then(|stat| stat.modified().ok());
                let bytes = stat.as_ref().map(|stat| stat.len()).unwrap_or(0);
                (written, bytes, capture)
            })
            .collect();
        // Newest first, which puts the ones with no time at the back: nothing
        // sorts below every time there is, and reversing that leaves it last.
        by_recency.sort_by_key(|(written, ..)| Reverse(*written));
        let mut budget = budget;
        let mut skipped = 0usize;
        for (_, bytes, capture) in by_recency {
            // Charged after the fact, so the newest capture is always read even
            // if it alone is bigger than the whole budget. A pool that can come
            // back empty because one session wrote a lot is worse than one that
            // occasionally reads more than it meant to.
            match budget > 0 {
                true => {
                    budget = budget.saturating_sub(bytes);
                    captures.push(capture);
                }
                false => skipped += 1,
            }
        }
        skipped
    }

    /// How many captures are read at once when the whole machine is searched.
    ///
    /// The work is a byte scan over a file, so it wants more than one thread
    /// and nothing like one per session: the captures on a machine that has
    /// been running a while come to several gigabytes between them, and reading
    /// a hundred and seventy files at once only makes the disk seek.
    const SEARCH_READERS: usize = 8;

    /// Every named capture searched for the same word, answered in the order it
    /// was asked in.
    ///
    /// One thread per capture would be as many threads as the machine has ever
    /// held conversations, and one thread for all of them leaves a search of a
    /// multi-gigabyte machine reading files end to end while the caller waits.
    /// So a fixed few readers take the captures in turn.
    ///
    /// The answers are put back where they were asked from rather than in the
    /// order they finish, because the order captures are searched in is the
    /// order the caller listed them - live sessions first, then filed ones -
    /// and that is the order the answer reads best in.
    fn search_captures(
        captures: &[SearchableCapture],
        query: &str,
        max_matches: usize,
    ) -> Vec<Result<Vec<DaemonHistoryMatch>>> {
        let readers = SEARCH_READERS.min(captures.len());
        if readers <= 1 {
            return captures
                .iter()
                .map(|(_, _, path)| search_history_file(path, query, max_matches))
                .collect();
        }
        let mut found: Vec<Option<Result<Vec<DaemonHistoryMatch>>>> =
            (0..captures.len()).map(|_| None).collect();
        let next = AtomicUsize::new(0);
        let done = Mutex::new(Vec::new());
        thread::scope(|scope| {
            for _ in 0..readers {
                scope.spawn(|| {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some((_, _, path)) = captures.get(at) else {
                            return;
                        };
                        let matches = search_history_file(path, query, max_matches);
                        // The lock is taken to hand back an answer already
                        // arrived at, never to read a file under it.
                        done.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((at, matches));
                    }
                });
            }
        });
        for (at, matches) in done
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            found[at] = Some(matches);
        }
        found
            .into_iter()
            .map(|matches| matches.unwrap_or_else(|| Ok(Vec::new())))
            .collect()
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
        // Whether every line has to be case folded properly to answer this
        // query, or whether the bytes can be read as they lie. Folding is a
        // fresh allocation per line, and a capture runs to hundreds of
        // megabytes: it costs about ten times what the byte scan does.
        let folding = query_needs_unicode_folding(&query);
        // A word can fall across the seam between one stretch of a line and the
        // next, so this much of the stretch before is searched again with the
        // one after. Folding can lengthen what it folds, so the seam is
        // measured generously rather than exactly.
        let seam_bytes = query.len().saturating_mul(4).min(SEARCH_LINE_CHUNK);
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut buffer = Vec::new();
        let mut seam: Vec<u8> = Vec::new();
        let mut line_number = 0usize;
        let mut matches = Vec::new();
        // Whether this stretch carries on the line the last one held, and
        // whether that line has already been answered with. A line is reported
        // once however many stretches it takes to read.
        let mut continuing = false;
        let mut reported = false;
        while let Some(ended) = read_line_stretch(&mut reader, &mut buffer, SEARCH_LINE_CHUNK)? {
            if !continuing {
                line_number += 1;
                reported = false;
            }
            continuing = !ended;
            if !reported {
                let hit = search_stretch(&buffer, &query, folding) || {
                    !seam.is_empty() && {
                        seam.extend_from_slice(&buffer[..seam_bytes.min(buffer.len())]);
                        search_stretch(&seam, &query, folding)
                    }
                };
                // The text answered with is the stretch the word was found in,
                // which for a line that fits in one is the line itself.
                if hit && let Some(found) = history_match(&buffer, line_number) {
                    matches.push(found);
                    reported = true;
                    if matches.len() > max_matches {
                        matches.remove(0);
                    }
                }
            }
            seam.clear();
            if !ended {
                seam.extend_from_slice(&buffer[buffer.len().saturating_sub(seam_bytes)..]);
            }
        }
        Ok(matches)
    }

    /// Read the next stretch of a log line into `buffer`: at most `cap` bytes,
    /// or through the newline that ends the line, whichever comes first.
    ///
    /// `None` is the end of the file. `Some(true)` means the line ended inside
    /// what was read, `Some(false)` that it runs on into the next stretch.
    fn read_line_stretch(
        reader: &mut impl BufRead,
        buffer: &mut Vec<u8>,
        cap: usize,
    ) -> io::Result<Option<bool>> {
        buffer.clear();
        while buffer.len() < cap {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                // The end of the file ends the line it was in the middle of.
                return Ok(match buffer.is_empty() {
                    true => None,
                    false => Some(true),
                });
            }
            let room = (cap - buffer.len()).min(available.len());
            match available[..room].iter().position(|&byte| byte == b'\n') {
                Some(at) => {
                    buffer.extend_from_slice(&available[..=at]);
                    reader.consume(at + 1);
                    return Ok(Some(true));
                }
                None => {
                    buffer.extend_from_slice(&available[..room]);
                    reader.consume(room);
                }
            }
        }
        Ok(Some(false))
    }

    fn search_stretch(bytes: &[u8], query: &str, folding: bool) -> bool {
        match folding {
            true => String::from_utf8_lossy(bytes)
                .to_lowercase()
                .contains(query),
            false => line_contains_folded(bytes, query.as_bytes()),
        }
    }

    fn history_match(bytes: &[u8], line_number: usize) -> Option<DaemonHistoryMatch> {
        let text = String::from_utf8_lossy(bytes);
        let text = text
            .trim()
            .chars()
            .filter(|character| !character.is_control())
            .take(500)
            .collect::<String>();
        if text.is_empty() {
            return None;
        }
        let lower = text.to_lowercase();
        Some(DaemonHistoryMatch {
            recap: lower.contains("※ recap:")
                || lower.contains("※ recap：")
                || lower.starts_with("recap:"),
            line_number,
            text,
        })
    }

    /// Whether a query can only be answered by case folding the text it is
    /// matched against: it holds a letter outside ASCII whose shape changes
    /// with its case. A query written in ASCII folds byte for byte, and one
    /// written in a script that has no case - CJK, digits, punctuation, which
    /// lowercase to themselves - needs no folding at all. Those are the queries
    /// people type; the accented remainder keeps the slow, exact path.
    ///
    /// `query` is already lowercased.
    fn query_needs_unicode_folding(query: &str) -> bool {
        query.chars().any(|character| {
            !character.is_ascii() && character.to_uppercase().next() != Some(character)
        })
    }

    /// Whether `line` holds `needle`, matched byte for byte with ASCII letters
    /// folded. `needle` is already lowercased; see
    /// [`query_needs_unicode_folding`] for when this is the whole answer.
    fn line_contains_folded(line: &[u8], needle: &[u8]) -> bool {
        let Some(first) = needle.first().copied() else {
            return true;
        };
        let mut rest = line;
        while rest.len() >= needle.len() {
            let Some(offset) = rest
                .iter()
                .position(|byte| byte.eq_ignore_ascii_case(&first))
            else {
                return false;
            };
            rest = &rest[offset..];
            if rest.len() < needle.len() {
                return false;
            }
            if rest[..needle.len()]
                .iter()
                .zip(needle)
                .all(|(byte, wanted)| byte.eq_ignore_ascii_case(wanted))
            {
                return true;
            }
            rest = &rest[1..];
        }
        false
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

    fn write_response<W: Write>(
        writer: &Arc<Mutex<W>>,
        request_id: u64,
        response: &DaemonResponse,
    ) -> Result<()> {
        write_frame(
            writer,
            &Frame::json(FrameKind::Response, 0, request_id, response)?,
        )
    }

    fn write_frame<W: Write>(writer: &Arc<Mutex<W>>, frame: &Frame) -> Result<()> {
        frame.write_to(
            &mut *writer
                .lock()
                .map_err(|_| anyhow!("daemon connection writer is poisoned"))?,
        )
    }

    pub fn bridge(paths: &DaemonPaths) -> Result<()> {
        let mut daemon = connect_or_start(paths)?;
        let mut outbound = daemon.try_clone()?;
        let forwarding = Arc::new(BridgeForwarding::default());
        // Both the daemon pump and every forwarded socket write here, so the
        // handle is shared and only ever written a whole frame at a time.
        let client = Arc::new(Mutex::new(io::stdout()));
        let input = {
            let forwarding = Arc::clone(&forwarding);
            let client = Arc::clone(&client);
            thread::spawn(move || -> Result<()> {
                let mut inbound = io::stdin().lock();
                let result = pump_client_frames(&mut inbound, &mut outbound, &forwarding, &client);
                // Release any forwarded socket still waiting on stream credit
                // the client will now never send.
                forwarding.flow.disconnect();
                let _ = outbound.shutdown(Shutdown::Write);
                result
            })
        };
        let mut buffer = vec![0; DATA_CHUNK_SIZE];
        let result = (|| -> Result<()> {
            loop {
                // A daemon that serves forwarding itself needs nothing from
                // this process but bytes, and staying out of the frames keeps
                // the bridge forward compatible with frames it predates.
                if forwarding.mode() == BridgeMode::Passthrough {
                    let read = daemon.read(&mut buffer)?;
                    if read == 0 {
                        return Ok(());
                    }
                    let mut client = client
                        .lock()
                        .map_err(|_| anyhow!("muxloomd bridge output is poisoned"))?;
                    client.write_all(&buffer[..read])?;
                    client.flush()?;
                    continue;
                }
                let Some(frame) = Frame::read_from(&mut daemon)? else {
                    return Ok(());
                };
                let frame = if frame.kind == FrameKind::Response
                    && forwarding.mode() == BridgeMode::Negotiating
                {
                    negotiate_bridge_capabilities(frame, &forwarding)
                } else {
                    frame
                };
                write_frame(&client, &frame)?;
            }
        })();
        forwarding.flow.disconnect();
        if result.is_ok() {
            input
                .join()
                .map_err(|_| anyhow!("muxloomd bridge input thread panicked"))??;
        }
        result
    }

    /// Which of the frames crossing this bridge it has to understand.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum BridgeMode {
        /// The daemon has not answered the client's `Hello` yet, so whether it
        /// serves forwarding is still unknown.
        Negotiating,
        /// The daemon serves forwarding, so frames pass through as raw bytes.
        Passthrough,
        /// The daemon predates forwarding and this bridge serves it instead.
        Forwarding,
    }

    /// TCP forwarding served by the bridge process rather than by the daemon.
    ///
    /// Forwarding wants a socket and a byte pump, never any session state, so
    /// the bridge can serve it alone. Sometimes it must: a daemon that live
    /// sessions pin to an older generation never gains the capability, because
    /// the handover that would replace it stays deferred for exactly as long
    /// as those sessions run. Answering here keeps forwarding available
    /// against a daemon the client can neither use for it nor replace, and
    /// costs the running agents nothing.
    #[derive(Default)]
    struct BridgeForwarding {
        mode: AtomicU8,
        flow: StreamFlow,
        sockets: Mutex<HashMap<u32, TcpStream>>,
    }

    impl BridgeForwarding {
        fn mode(&self) -> BridgeMode {
            match self.mode.load(Ordering::Acquire) {
                1 => BridgeMode::Passthrough,
                2 => BridgeMode::Forwarding,
                _ => BridgeMode::Negotiating,
            }
        }

        /// Settle the mode before the `Hello` that decides it reaches the
        /// client, so the frames the client sends in reply are read in the
        /// mode its capabilities promised.
        fn settle(&self, mode: BridgeMode) {
            self.mode.store(
                match mode {
                    BridgeMode::Negotiating => 0,
                    BridgeMode::Passthrough => 1,
                    BridgeMode::Forwarding => 2,
                },
                Ordering::Release,
            );
        }

        fn owns(&self, stream_id: u32) -> bool {
            self.sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&stream_id)
        }

        fn close(&self, stream_id: u32) {
            let socket = self
                .sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&stream_id);
            if let Some(socket) = socket {
                let _ = socket.shutdown(Shutdown::Both);
            }
            self.flow.close(stream_id);
        }
    }

    /// Pass the daemon's `Hello` on to the client, claiming the forwarding
    /// capabilities this bridge serves when the daemon does not serve them.
    fn negotiate_bridge_capabilities(frame: Frame, forwarding: &BridgeForwarding) -> Frame {
        let Ok(DaemonResponse::Hello {
            daemon_version,
            protocol_version,
            pid,
            mut capabilities,
            daemon_generation,
        }) = frame.decode_json::<DaemonResponse>()
        else {
            return frame;
        };
        if capabilities.iter().any(|it| it == FORWARD_CAPABILITY) {
            forwarding.settle(BridgeMode::Passthrough);
            return frame;
        }
        capabilities.push(FORWARD_CAPABILITY.into());
        if !capabilities.iter().any(|it| it == LISTENERS_CAPABILITY) {
            capabilities.push(LISTENERS_CAPABILITY.into());
        }
        let supplemented = Frame::json(
            FrameKind::Response,
            frame.stream_id,
            frame.request_id,
            &DaemonResponse::Hello {
                daemon_version: daemon_version.clone(),
                protocol_version,
                pid,
                capabilities,
                daemon_generation,
            },
        );
        match supplemented {
            Ok(supplemented) => {
                forwarding.settle(BridgeMode::Forwarding);
                eprintln!(
                    "muxloomd bridge serves TCP forwarding for daemon {daemon_version}, which predates it"
                );
                supplemented
            }
            // Nothing here is worth failing the whole connection over: without
            // the capability the client simply reports forwarding unavailable,
            // exactly as it did before.
            Err(error) => {
                eprintln!("muxloomd bridge could not offer TCP forwarding: {error:#}");
                frame
            }
        }
    }

    fn pump_client_frames<R: Read, D: Write, W: Write + Send + 'static>(
        inbound: &mut R,
        daemon: &mut D,
        forwarding: &Arc<BridgeForwarding>,
        client: &Arc<Mutex<W>>,
    ) -> Result<()> {
        loop {
            if forwarding.mode() == BridgeMode::Passthrough {
                io::copy(inbound, daemon)?;
                return Ok(());
            }
            let Some(frame) = Frame::read_from(inbound)? else {
                return Ok(());
            };
            if !serve_bridge_frame(&frame, forwarding, client)? {
                frame.write_to(daemon)?;
            }
        }
    }

    /// Serve a client frame that belongs to forwarding this bridge took on,
    /// reporting whether it was answered here instead of at the daemon.
    fn serve_bridge_frame<W: Write + Send + 'static>(
        frame: &Frame,
        forwarding: &Arc<BridgeForwarding>,
        client: &Arc<Mutex<W>>,
    ) -> Result<bool> {
        if forwarding.mode() != BridgeMode::Forwarding {
            return Ok(false);
        }
        match frame.kind {
            FrameKind::OpenStream => {
                let Ok(OpenStream::Tcp { host, port }) = frame.decode_json::<OpenStream>() else {
                    return Ok(false);
                };
                open_bridge_tcp(frame, forwarding, client, &host, port)?;
                Ok(true)
            }
            FrameKind::Request => {
                if !matches!(
                    frame.decode_json::<DaemonRequest>(),
                    Ok(DaemonRequest::ListTcpListeners)
                ) {
                    return Ok(false);
                }
                let response = match tcp_listener_ports() {
                    Ok(ports) => DaemonResponse::TcpListeners { ports },
                    Err(error) => DaemonResponse::Error {
                        message: error.to_string(),
                    },
                };
                write_response(client, frame.request_id, &response)?;
                Ok(true)
            }
            FrameKind::Data => {
                if !forwarding.owns(frame.stream_id) {
                    return Ok(false);
                }
                let payload = frame.decoded_payload()?;
                let written = {
                    let mut sockets = forwarding
                        .sockets
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match sockets.get_mut(&frame.stream_id) {
                        Some(socket) => socket.write_all(&payload).is_ok(),
                        None => return Ok(false),
                    }
                };
                // One forwarded connection going away must not take the bridge
                // — and with it every session on this machine — down with it.
                if !written {
                    forwarding.close(frame.stream_id);
                    write_frame(
                        client,
                        &Frame::new(FrameKind::CloseStream, frame.stream_id, 0, vec![]),
                    )?;
                }
                Ok(true)
            }
            FrameKind::WindowUpdate => {
                if !forwarding.owns(frame.stream_id) {
                    return Ok(false);
                }
                forwarding.flow.add(frame.stream_id, frame.window_credit()?);
                Ok(true)
            }
            FrameKind::CloseStream => {
                if !forwarding.owns(frame.stream_id) {
                    return Ok(false);
                }
                forwarding.close(frame.stream_id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// How long a forwarded connection may spend reaching its far end before
    /// the client that asked for it is told no. The connect is made on the
    /// frame loop that reads that client, so waiting out the operating
    /// system's own patience -- over a minute for an address that answers
    /// nothing at all -- holds up that client's heartbeats, its window updates
    /// and every request queued behind it for the whole of it.
    const FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Reach `host:port` inside `budget`, trying each address the name resolves
    /// to and spending the one budget across all of them rather than granting
    /// each its own.
    fn connect_forward_within(host: &str, port: u16, budget: Duration) -> io::Result<TcpStream> {
        let deadline = Instant::now() + budget;
        let mut refusal = None;
        for address in (host, port).to_socket_addrs()? {
            // Never zero: a zero timeout is rejected outright, which would
            // turn a spent budget into an error about the argument rather
            // than about the address.
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            match TcpStream::connect_timeout(&address, left) {
                Ok(socket) => return Ok(socket),
                Err(error) => refusal = Some(error),
            }
        }
        Err(refusal.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no address for {host}:{port}"),
            )
        }))
    }

    fn connect_forward(host: &str, port: u16) -> io::Result<TcpStream> {
        connect_forward_within(host, port, FORWARD_CONNECT_TIMEOUT)
    }

    fn open_bridge_tcp<W: Write + Send + 'static>(
        frame: &Frame,
        forwarding: &Arc<BridgeForwarding>,
        client: &Arc<Mutex<W>>,
        host: &str,
        port: u16,
    ) -> Result<()> {
        let socket = match connect_forward(host, port) {
            Ok(socket) => socket,
            Err(error) => {
                return write_stream_error(
                    client,
                    frame,
                    format!("cannot connect to {host}:{port}: {error}"),
                );
            }
        };
        socket.set_nodelay(true)?;
        let reader = socket.try_clone()?;
        forwarding
            .sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(frame.stream_id, socket);
        write_stream_opened(client, frame, None)?;
        forwarding.flow.open(frame.stream_id);
        let client = Arc::clone(client);
        let forwarding = Arc::clone(forwarding);
        let stream_id = frame.stream_id;
        thread::spawn(move || {
            if let Err(error) = stream_tcp(&client, &forwarding.flow, stream_id, reader) {
                eprintln!("muxloomd bridge TCP stream {stream_id} failed: {error:#}");
            }
            forwarding.close(stream_id);
        });
        Ok(())
    }

    /// How long a cold start may take before the caller gives up on it. A
    /// machine under load — a CI runner compiling and testing at once — can
    /// spend seconds only on exec'ing a large unoptimized binary, and giving
    /// up early sends the launch down the legacy fallback for no reason.
    const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(20);

    /// How long a superseded daemon may go on deferring before the build that
    /// asked stops waiting and takes its place.
    ///
    /// A handover is a request, and everything that decides how it is answered
    /// runs inside the daemon being replaced — which is, by definition, the
    /// older build. So every improvement to the answer arrives too late for
    /// the daemons that need it: one from before [`spawn_retirement_watcher`]
    /// refuses outright while more than one client is attached, and on a
    /// machine with agents on it there always is one, because every SSH bridge
    /// holds a connection open. Left to cooperation alone such a daemon serves
    /// forever, the machine reads as outdated for the rest of its life, and
    /// the only way out is the user pressing the forced-update key.
    ///
    /// So the patience lives on this side too, where the *new* build runs: ask,
    /// wait longer than a daemon that can retire itself would ever need, then
    /// stop it. Nothing is lost by that — the keepers own the sessions and the
    /// next generation adopts them — beyond the connections of clients that
    /// already know how to make new ones.
    const HANDOVER_PATIENCE: Duration = Duration::from_secs(10 * 60);

    pub fn connect_or_start(paths: &DaemonPaths) -> Result<UnixStream> {
        if let Ok(mut stream) = UnixStream::connect(&paths.socket) {
            if generation_is_current_after_settling(paths) {
                forget_handover_ask(paths);
                return Ok(stream);
            }
            match prepare_atomic_handover(&mut stream) {
                // The ask itself broke, rather than being answered. The daemon
                // hung up mid-request because it is already draining, or it
                // could not read the frame at all — which is what a protocol
                // version this build does not share looks like from the far
                // side. Both were fatal here, so the error went back to the
                // caller and the lines below never ran: the machine kept a
                // daemon nobody could finish a sentence with, and the build
                // that would have replaced it never started. Neither case is
                // a reason to fail. Ask for a fresh connection — one that
                // comes back is a daemon still serving, and one that does not
                // is a daemon nobody can use — and where nothing comes back,
                // clear what is left of it and start.
                Err(error) => {
                    crate::debug::log(
                        "daemon",
                        format!("the running daemon could not be asked to hand over: {error:#}"),
                    );
                    drop(stream);
                    if let Ok(stream) = UnixStream::connect(&paths.socket) {
                        return Ok(stream);
                    }
                    let _ = stop_running_daemon(paths);
                    forget_handover_ask(paths);
                }
                // Deferred: a daemon that knows the request means to stand
                // down as soon as nothing would be lost. Give it that time,
                // and take its place if it never does.
                Ok(Some(false)) => {
                    if !handover_is_overdue(paths) {
                        return Ok(stream);
                    }
                    drop(stream);
                    if !take_the_place_of_the_running_daemon(paths)
                        && let Ok(stream) = UnixStream::connect(&paths.socket)
                    {
                        return Ok(stream);
                    }
                }
                Ok(Some(true)) => {
                    drop(stream);
                    wait_for_daemon_stop(paths)?;
                    forget_handover_ask(paths);
                }
                // Too old to know the request at all. Handing over used to
                // want an idle daemon, which a machine with agents on it never
                // is, so the same patience decides the rest.
                Ok(None) => {
                    if daemon_is_idle_for_handover(&mut stream).unwrap_or(false) {
                        drop(stream);
                        stop_running_daemon(paths)?;
                        forget_handover_ask(paths);
                    } else {
                        if !handover_is_overdue(paths) {
                            return Ok(stream);
                        }
                        drop(stream);
                        if !take_the_place_of_the_running_daemon(paths)
                            && let Ok(stream) = UnixStream::connect(&paths.socket)
                        {
                            return Ok(stream);
                        }
                    }
                }
            }
        }
        if a_daemon_is_serving(paths) {
            bail!("muxloomd is running but its socket is not accessible");
        }
        paths.prepare()?;
        spawn_background(paths)?;
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            match UnixStream::connect(&paths.socket) {
                Ok(stream) => return Ok(stream),
                Err(error) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                    let _ = error;
                }
                Err(error) => {
                    // The daemon writes why it could not serve into its own
                    // log and then exits. Carry the end of it up with the
                    // connection error, which on its own says only that
                    // nothing was listening.
                    let tail = log_tail(&paths.log, 20);
                    return Err(error).with_context(|| {
                        format!(
                            "muxloomd did not start at {} within {}s{tail}",
                            paths.socket.display(),
                            DAEMON_START_TIMEOUT.as_secs()
                        )
                    });
                }
            }
        }
    }

    /// A daemon binds its socket a moment before it stamps its generation, so
    /// a client that connects in between reads a missing or superseded stamp
    /// and puts a daemon that just started through a handover it does not
    /// need. Look once more before believing the stamp.
    /// How long a stamp that reads as superseded is given to turn out to be a
    /// daemon still writing it.
    const GENERATION_SETTLE: Duration = Duration::from_millis(100);

    /// The running stamp this process has already waited out. Settling asks
    /// whether a stamp that reads as old is a daemon still starting, and that
    /// is a question about one particular stamp: asking it again on every
    /// request cost a tenth of a second a time for the whole of a deferred
    /// handover, which on a machine with agents attached is minutes.
    static SETTLED_GENERATION: OnceLock<Mutex<Option<String>>> = OnceLock::new();

    fn generation_is_current_after_settling(paths: &DaemonPaths) -> bool {
        if running_generation_is_current(paths) {
            return true;
        }
        let settled = SETTLED_GENERATION.get_or_init(|| Mutex::new(None));
        // No stamp at all is a daemon that has not written one yet, which is
        // the very case the wait is for and never an answer to remember.
        let running = fs::read_to_string(&paths.generation).unwrap_or_default();
        if !running.trim().is_empty()
            && settled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_deref()
                == Some(running.as_str())
        {
            return false;
        }
        thread::sleep(GENERATION_SETTLE);
        if running_generation_is_current(paths) {
            return true;
        }
        if let Ok(stamp) = fs::read_to_string(&paths.generation)
            && !stamp.trim().is_empty()
        {
            *settled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stamp);
        }
        false
    }

    /// The end of the daemon's log, phrased to sit inside an error message.
    /// Empty when there is nothing to read, so the error reads normally.
    fn log_tail(path: &Path, lines: usize) -> String {
        let Ok(text) = fs::read_to_string(path) else {
            return String::new();
        };
        let tail = text
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        if tail.trim().is_empty() {
            return String::new();
        }
        format!("; the daemon log ends with: {tail}")
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

    /// Whether to ask the daemon already running to make way for this build.
    ///
    /// Never for one that outranks it. Two builds that each believe they are
    /// the current one would otherwise take turns retiring each other for as
    /// long as both are in use, and every turn costs every attached client its
    /// connection.
    fn should_replace_generation(running: &str) -> bool {
        generation_makes_way(running, &current_generation())
    }

    /// The comparison itself, with both stamps spelled out.
    ///
    /// Rank decides it, and equal rank does not hand over. That last part is
    /// the whole of a bug worth spelling out: a machine runs the controller and
    /// the companion beside it, cut from one commit and ranking equal, and they
    /// are two different files. While any difference in the stamp was enough,
    /// every dashboard round retired the daemon the MCP servers had started and
    /// every MCP call retired the one the dashboard started back — hundreds of
    /// handovers in a day on one ordinary machine, each of them dropping every
    /// client's connection and throwing away everything the daemon had worked
    /// out about the sessions it was holding.
    ///
    /// The exception is the build nobody numbered. `cargo build` stamps
    /// `local:local` whatever it compiled, so between two hand-made builds the
    /// rank says nothing at all and the copy on disk is the only thing that
    /// tells yesterday's source from today's — which is exactly what a
    /// developer rebuilding means to happen. Two copies are only comparable
    /// when they are copies of the same file, so that case asks for the name
    /// as well: a `muxloom` and a `muxloomd` compiled from one tree are no more
    /// each other's rebuild than the installed pair are.
    fn generation_makes_way(running: &str, current: &str) -> bool {
        if running.trim() == current.trim() {
            return false;
        }
        match (generation_rank(running), generation_rank(current)) {
            (Some(running_rank), Some(current_rank)) => {
                let rebuilt_in_place = running_rank.height == u64::MAX
                    && current_rank.height == u64::MAX
                    && stamped_executable_name(running).is_some()
                    && stamped_executable_name(running) == stamped_executable_name(current);
                match rebuilt_in_place {
                    true => running_rank <= current_rank,
                    false => running_rank < current_rank,
                }
            }
            // Nothing legible to order them by, so fall back to what this did
            // before there was an order: any difference is a handover.
            _ => true,
        }
    }

    fn running_generation_is_current(paths: &DaemonPaths) -> bool {
        match fs::read_to_string(&paths.generation) {
            Ok(running) => !should_replace_generation(&running),
            // No stamp at all: either a daemon from before they existed, or one
            // that has not finished starting. Settling tells them apart.
            Err(_) => false,
        }
    }

    /// Whether this build outranks the one running, which is the only case
    /// that justifies stopping a daemon that will not step aside on its own.
    ///
    /// The whole rank, not the version alone. Comparing versions meant a fleet
    /// on nightlies could never escalate: every build between two releases
    /// carries one version number, so a daemon six weeks behind read as an
    /// equal, the patience clock never started, and the only way past a daemon
    /// that kept deferring was somebody pressing the forced-update key on each
    /// machine in turn.
    ///
    /// Strictly outranks, and that is what keeps this from turning into a
    /// fight. Equal rank never forces, so two builds of one commit leave each
    /// other alone; and where one does outrank the other the ordering is the
    /// same on both sides, so the lower one reads the higher as current and
    /// does not ask at all. A build made by hand sits above every numbered
    /// build of its version and can retire an installed release, which is what
    /// somebody who just compiled it meant to happen; the release cannot do it
    /// back.
    fn outranks_running_generation(running: &str) -> bool {
        match (
            generation_rank(running),
            generation_rank(&current_generation()),
        ) {
            (Some(running), Some(current)) => running < current,
            // Nothing legible to order it by: a stamp from before there were
            // any, which every build since outranks.
            (None, _) => true,
            _ => false,
        }
    }

    /// How long this build has been asking the daemon now running to make way,
    /// recording the ask when it is the first one. `None` when the running
    /// generation is not one this build may force out at all.
    ///
    /// The ask outlives the process that makes it: a bridge is remade whenever
    /// the controller reconnects, and an MCP call opens and closes a connection
    /// of its own, so no one client is around long enough to keep count. The
    /// state directory is, and the daemon it describes is the one serving it.
    fn handover_ask_age(paths: &DaemonPaths) -> Option<Duration> {
        let running = fs::read_to_string(&paths.generation).unwrap_or_default();
        let running = running.trim();
        if !outranks_running_generation(running) {
            return None;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        let asked_at = fs::read_to_string(&paths.handover)
            .ok()
            .and_then(|noted| {
                let (generation, since) = noted.trim().rsplit_once('\t')?;
                (generation == running).then(|| since.trim().parse::<u64>().ok())?
            })
            // A clock that moved backwards would otherwise hold the ask open
            // forever; treat it as a fresh one.
            .filter(|asked_at| *asked_at <= now);
        match asked_at {
            Some(asked_at) => Some(Duration::from_millis(now - asked_at)),
            None => {
                let _ = fs::write(&paths.handover, format!("{running}\t{now}"));
                Some(Duration::ZERO)
            }
        }
    }

    fn handover_is_overdue(paths: &DaemonPaths) -> bool {
        handover_ask_age(paths).is_some_and(|age| age >= HANDOVER_PATIENCE)
    }

    fn forget_handover_ask(paths: &DaemonPaths) {
        let _ = fs::remove_file(&paths.handover);
    }

    /// Stop a superseded daemon that has had long enough to stand down by
    /// itself and has not, so this build can serve in its place.
    ///
    /// Best effort, and deliberately so: if it cannot be stopped the caller
    /// goes on using it, which is exactly what it would have done anyway.
    fn take_the_place_of_the_running_daemon(paths: &DaemonPaths) -> bool {
        eprintln!(
            "muxloomd {} is taking over from a daemon that has deferred the handover for {} minutes; its sessions keep running",
            env!("CARGO_PKG_VERSION"),
            HANDOVER_PATIENCE.as_secs() / 60
        );
        match stop_running_daemon(paths) {
            Ok(()) => {
                forget_handover_ask(paths);
                true
            }
            Err(error) => {
                eprintln!("muxloomd could not stop the daemon it is replacing: {error:#}");
                false
            }
        }
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
            // Only what is running: the archive is all dead and archived
            // already, so it can only add weight to the answer.
            &DaemonRequest::ListSessions {
                live_only: true,
                only: None,
            },
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

    /// Stop the daemon serving these paths, and say what happened.
    ///
    /// Sessions are not part of this: their keepers own them, keep writing
    /// their histories, and are adopted by whichever daemon serves next — the
    /// one the first client to ask for something starts. This is the way out
    /// of the one case generations will not decide by themselves. A build only
    /// asks for the place of one it outranks, so a deliberate downgrade leaves
    /// the newer daemon serving until something stops it, and this is that
    /// something.
    pub fn stop(paths: &DaemonPaths) -> Result<()> {
        if !a_daemon_is_serving(paths) {
            println!("muxloomd is not running");
            return Ok(());
        }
        stop_running_daemon(paths)?;
        println!("muxloomd stopped; its sessions keep running and the next one adopts them");
        Ok(())
    }

    fn stop_running_daemon(paths: &DaemonPaths) -> Result<()> {
        // Nothing is serving, so the pid file is what a daemon that died
        // without clearing up left behind and the number in it is nobody's
        // business of muxloom's to signal. Sweep up instead: this used to
        // report a daemon stopped after sending a stranger a SIGTERM.
        if !a_daemon_is_serving(paths) {
            let _ = fs::remove_file(&paths.socket);
            let _ = fs::remove_file(&paths.pid);
            return Ok(());
        }
        let pid = fs::read_to_string(&paths.pid)
            .context("muxloomd has no pid file")?
            .trim()
            .parse::<i32>()
            .context("muxloomd pid is invalid")?;
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result != 0 {
            return Err(io::Error::last_os_error()).context("failed to stop muxloomd");
        }
        wait_for_daemon_stop_saying(paths, "muxloomd did not stop")
    }

    fn wait_for_daemon_stop(paths: &DaemonPaths) -> Result<()> {
        wait_for_daemon_stop_saying(
            paths,
            "muxloomd did not stop after accepting generation handover",
        )
    }

    /// Wait for the daemon to be gone, then clear away what it left.
    ///
    /// Gone by the lock, not by the pid: a daemon that has exited and not yet
    /// been reaped by the client that started it still answers `kill(pid, 0)`,
    /// so waiting on the number alone timed out on a daemon that had already
    /// stopped and turned an ordinary handover into a failed call.
    fn wait_for_daemon_stop_saying(paths: &DaemonPaths, complaint: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while a_daemon_is_serving(paths) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if a_daemon_is_serving(paths) {
            bail!("{complaint}");
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

            let read =
                render_history_file(&path, 20, 5, 300, 40, window, log.len() as u64).unwrap();
            let page = String::from_utf8_lossy(&read.rows).into_owned();

            assert_eq!(read.offset_from_bottom, 300, "the row that was asked for");
            assert!(read.total_lines > 300, "rows in all: {}", read.total_lines);
            assert!(page.contains("line9"), "rows from back then: {page:?}");
            assert!(
                !page.contains("line400"),
                "and not the newest ones: {page:?}"
            );

            // A window that may not widen answers with what it could reach.
            let shallow = render_history_file(&path, 20, 5, 300, 40, window, window).unwrap();
            assert!(
                shallow.offset_from_bottom < 300,
                "as far back as one window reaches: {}",
                shallow.offset_from_bottom
            );
            assert!(
                !shallow.reached_start,
                "and says it never got to the top of the log"
            );
        }

        #[test]
        fn a_history_page_stops_widening_once_widening_stops_finding_rows() {
            // The paired case to the test above, where every frame ends in a
            // newline so widening keeps turning up rows. An agent on the
            // alternate screen paints without one: the cursor is put back and
            // the row overwritten, so nothing ever scrolls off and no window,
            // however far back it reaches, holds more than a screenful. The
            // read used to widen through that all the way to its ceiling before
            // answering with the screenful it already had on its first look.
            let path = test_state("render-history-still")
                .paths
                .history
                .join("log.ansi");
            let mut log: String = (1..=200)
                .map(|line| format!("scrolled{line}\r\n"))
                .collect();
            for frame in 1..=8_000 {
                log.push_str(&format!("\x1b[1;1H\x1b[Kpaint{frame}"));
            }
            fs::write(&path, &log).unwrap();

            // The seed reaches 12,800 bytes and quadruples from there, so the
            // first two looks both land inside the repaint; the third would
            // span the whole log and reach the rows at the top of it.
            let read = render_history_file(&path, 20, 5, 0, 40, 4 * 1024, 1024 * 1024).unwrap();

            assert!(
                !read.reached_start,
                "the read ran to the top of the log to find rows it had already found"
            );
            assert!(
                read.total_lines < 40,
                "and a screenful is all there was: {}",
                read.total_lines
            );
            let page = String::from_utf8_lossy(&read.rows).into_owned();
            assert!(page.contains("paint8000"), "the newest paint: {page:?}");
        }

        #[test]
        fn a_history_page_that_read_the_whole_log_says_so() {
            // Without this a client has no boundary to stop a scroll at: the
            // size a rendered page reports measures the window it replayed, so
            // one that reached the row asked for always reads as "there may be
            // more above" -- and the view scrolls off the top of the session.
            let path = test_state("render-history-top")
                .paths
                .history
                .join("log.ansi");
            let log: String = (1..=30).map(|line| format!("line{line}\r\n")).collect();
            fs::write(&path, &log).unwrap();

            let read = render_history_file(&path, 20, 5, 0, 500, 16 * 1024, 64 * 1024).unwrap();
            assert!(read.reached_start, "a 30-row log is read whole");
            assert_eq!(read.offset_from_bottom, 0);
            // The rows above the screen are the ones a client may scroll to.
            assert!(read.total_lines >= 30, "rows in all: {}", read.total_lines);

            // Asking past the oldest row answers with the oldest row there is.
            let read = render_history_file(&path, 20, 5, 5_000, 500, 16 * 1024, 64 * 1024).unwrap();
            assert!(read.reached_start);
            assert_eq!(read.offset_from_bottom, read.total_lines - 5);
        }

        #[test]
        fn a_history_read_that_asks_for_no_lines_does_not_touch_the_log() {
            // A raw read finds the line it was asked for by counting newlines
            // from the top of the log, so asking for one line of a session that
            // paints its screen -- whose few newlines are all at the end -- reads
            // the whole session. The backup asks every session on the machine how
            // long its log is once every five minutes, and asked for one line to
            // find out: every byte of every log lifted off the disk, per pass,
            // for a number the daemon already holds.
            let state = test_state("history-count-only");
            let path = state.paths.history.join("log.ansi");
            let log: String = (1..=5).map(|line| format!("line{line}\r\n")).collect();
            fs::write(&path, &log).unwrap();

            let counted = read_history_file(&path, 5, 5, 0, 0).unwrap();
            assert!(counted.rows.is_empty(), "no lines were asked for");
            assert_eq!(counted.total_lines, 5, "and the count came back anyway");

            // Nothing opens the log, so it does not have to be there: this stops
            // holding the moment the read walks a file again.
            let absent = state.paths.history.join("never-written.ansi");
            let counted = read_history_file(&absent, 5_000_000, 24, 0, 0).unwrap();
            assert_eq!(counted.total_lines, 5_000_000, "the count it was handed");

            // A read that does ask for a line still gets the newest one.
            let page = read_history_file(&path, 5, 5, 0, 1).unwrap();
            assert_eq!(String::from_utf8_lossy(&page.rows), "line5\r\n");
        }

        #[test]
        fn a_search_reads_a_line_longer_than_a_screen_in_stretches() {
            // A capture is not made of lines. An agent that paints its screen
            // puts the cursor back where it wants it rather than ending a line,
            // so a whole session arrives as one line -- four hundred megabytes
            // of it on this machine, holding two newlines. The search read a
            // line whole before looking at it: one allocation the size of the
            // session, and two more that size again to case fold it.
            let state = test_state("search-long-line");
            let path = state.paths.history.join("paint.ansi");
            let mut log = String::from("first line of the log\r\n");
            while log.len() < SEARCH_LINE_CHUNK * 3 {
                log.push_str("\x1b[1;1H\x1b[Kpaint");
            }
            log.push_str("the needle we are after");
            fs::write(&path, &log).unwrap();

            // Finding it at all means the scan carried on past the first
            // stretch, and found it once rather than once per stretch.
            let found = search_history_file(&path, "needle we are after", 12).unwrap();
            assert_eq!(found.len(), 1, "the line that held it: {found:?}");
            assert_eq!(found[0].line_number, 2, "the line after the first");

            // And no stretch of that line was ever held whole.
            let mut reader = BufReader::new(File::open(&path).unwrap());
            let mut buffer = Vec::new();
            let ended = read_line_stretch(&mut reader, &mut buffer, SEARCH_LINE_CHUNK).unwrap();
            assert_eq!(ended, Some(true), "the first line ends where it ends");
            assert_eq!(buffer, b"first line of the log\r\n");
            let ended = read_line_stretch(&mut reader, &mut buffer, SEARCH_LINE_CHUNK).unwrap();
            assert_eq!(ended, Some(false), "and the one after it runs on");
            assert_eq!(
                buffer.len(),
                SEARCH_LINE_CHUNK,
                "a stretch of the line, not the line"
            );

            // A word lying across the seam between two stretches is in neither
            // of them, so the tail of one is searched again with the next.
            let seam = state.paths.history.join("seam.ansi");
            let needle = "straddling-the-seam";
            let mut log = String::new();
            while log.len() < SEARCH_LINE_CHUNK - 5 {
                log.push('x');
            }
            log.push_str(needle);
            fs::write(&seam, &log).unwrap();
            let found = search_history_file(&seam, needle, 12).unwrap();
            assert_eq!(found.len(), 1, "a word split across the seam: {found:?}");
        }

        #[test]
        fn a_waiting_accept_loop_takes_a_connection_as_it_lands() {
            let state = test_state("accept-wait");
            let listener = UnixListener::bind(&state.paths.socket).unwrap();
            listener.set_nonblocking(true).unwrap();

            // With nothing connecting, the wait is its own timeout: this is the
            // loop looking up to see whether it still serves the directory.
            let started = Instant::now();
            wait_for_connection(&listener, Duration::from_millis(150)).unwrap();
            assert!(
                started.elapsed() >= Duration::from_millis(100),
                "waited {:?}",
                started.elapsed()
            );
            assert!(listener.accept().is_err(), "and nothing had connected");

            // A client already in the backlog is handed over at once. Sleeping
            // the interval instead -- which is what the loop used to do, and
            // what any wait that does not watch the socket does -- would sit on
            // this connection for the whole five seconds.
            let client = UnixStream::connect(&state.paths.socket).unwrap();
            let started = Instant::now();
            wait_for_connection(&listener, Duration::from_secs(5)).unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "a queued connection waited {:?}",
                started.elapsed()
            );
            assert!(
                listener.accept().is_ok(),
                "and it is the one that connected"
            );
            drop(client);
        }

        #[test]
        fn a_page_reads_back_as_far_as_the_page_is_deep() {
            // A wait polls eighty rows once a round, a screen read asks for two
            // hundred, and an attach seeds five thousand. One window served all
            // three, so the two shallow ones seeked back and replayed the
            // attach's sixteen megabytes through a fresh emulator -- every
            // round, for every waiting agent -- to answer with a screen the
            // daemon was already holding.
            const COLUMNS: u16 = 200;
            let window = |rows| {
                seed_window(
                    rows,
                    COLUMNS,
                    SCROLLBACK_SEED_BYTES_MIN,
                    SCROLLBACK_SEED_BYTES_MAX,
                )
            };

            assert!(
                window(80) * 32 < window(5_000),
                "a poll reads a fraction of what a seed does: {} against {}",
                window(80),
                window(5_000)
            );
            assert!(
                window(5_000) > 8 * 1024 * 1024,
                "and the deepest page still reads back about what it always \
                 did: {}",
                window(5_000)
            );
            // Deeper pages reach further back, between the two bounds.
            assert!(window(200) > window(80) && window(2_000) > window(200));
            assert_eq!(
                window(1),
                SCROLLBACK_SEED_BYTES_MIN,
                "one row still reads a window worth opening the file for"
            );
            assert_eq!(
                seed_window(
                    usize::MAX,
                    u16::MAX,
                    SCROLLBACK_SEED_BYTES_MIN,
                    SCROLLBACK_SEED_BYTES_MAX
                ),
                SCROLLBACK_SEED_BYTES_MAX,
                "and no page reads past the ceiling"
            );
            // A caller whose ceiling is under the floor is held to the ceiling,
            // which is what the widening test asks for when it forbids widening.
            assert_eq!(seed_window(1, 1, 4_096, 512), 512);
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

        #[test]
        fn a_listing_classifies_a_symlink_by_what_it_points_at() {
            let root = std::env::temp_dir().join(format!(
                "muxloomd-symlinks-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("releases")).unwrap();
            fs::write(root.join("notes.txt"), b"hello").unwrap();
            std::os::unix::fs::symlink(root.join("releases"), root.join("current")).unwrap();
            std::os::unix::fs::symlink(root.join("notes.txt"), root.join("latest.txt")).unwrap();
            std::os::unix::fs::symlink(root.join("gone"), root.join("broken")).unwrap();

            let listing = native_list_files(root.to_str().unwrap()).unwrap();
            let entry = |name: &str| {
                listing
                    .entries
                    .iter()
                    .find(|entry| entry.name == name)
                    .unwrap_or_else(|| panic!("{name} missing from listing"))
                    .clone()
            };

            // A link to a directory has to open like a directory, or a deploy
            // tree laid out as current -> releases/vN is a dead end.
            let current = entry("current");
            assert_eq!(current.kind, FileEntryKind::Directory);
            assert!(current.symlink);
            // A link to a file previews, and reports the target's size.
            let latest = entry("latest.txt");
            assert_eq!(latest.kind, FileEntryKind::File);
            assert!(latest.symlink);
            assert_eq!(latest.size, 5);
            // A link that resolves to nothing stays unopenable.
            let broken = entry("broken");
            assert_eq!(broken.kind, FileEntryKind::Other);
            assert!(broken.symlink);
            // Plain entries are unchanged and never flagged as links.
            assert_eq!(entry("releases").kind, FileEntryKind::Directory);
            assert!(!entry("releases").symlink);
            assert!(!entry("notes.txt").symlink);

            fs::remove_dir_all(&root).ok();
        }

        #[test]
        fn a_daemon_keeps_a_copy_of_itself_that_outlives_its_install() {
            let root = std::env::temp_dir().join(format!(
                "muxloomd-stash-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            let paths = DaemonPaths::under(root.clone());
            paths.prepare().unwrap();
            let install = root.join("installed-muxloomd");
            fs::write(&install, b"#!/bin/sh\nexit 7\n").unwrap();
            fs::set_permissions(&install, fs::Permissions::from_mode(0o755)).unwrap();
            // Something an older generation left behind.
            let stale = paths.bin.join("muxloomd-older-1-1");
            fs::write(&stale, b"old").unwrap();

            let stashed = stash_executable(&paths, &install).unwrap();
            assert!(stashed.starts_with(&paths.bin));
            assert_eq!(fs::read(&stashed).unwrap(), fs::read(&install).unwrap());
            assert_ne!(
                fs::metadata(&stashed).unwrap().permissions().mode() & 0o100,
                0
            );
            // Only ever one copy: the previous generation's is not this
            // daemon's build, and nothing is going to start a keeper from it.
            assert!(!stale.exists());

            // Asking again for the same binary settles on the same copy
            // rather than writing it out a second time.
            assert_eq!(stash_executable(&paths, &install).unwrap(), stashed);

            // The whole point: the install goes away — an upgrade, an
            // uninstall — and a keeper can still be started.
            fs::remove_file(&install).unwrap();
            assert!(stashed.exists());
            assert_eq!(
                Command::new(&stashed).status().unwrap().code(),
                Some(7),
                "the copy has to still run"
            );

            fs::remove_dir_all(&root).ok();
        }

        fn test_state(name: &str) -> Arc<DaemonState> {
            // A short, fixed prefix: the state dir carries keeper sockets, and
            // a socket path must stay under the ~104-byte sockaddr_un limit —
            // macOS's per-user temp dir alone nearly exhausts it.
            let root = PathBuf::from("/tmp").join(format!(
                "mxl-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            ));
            let paths = DaemonPaths::under(root);
            paths.prepare().unwrap();
            Arc::new(DaemonState::new(paths, KeeperMode::InProcess))
        }

        /// Take the state directory away once the daemon has finished writing
        /// to it.
        ///
        /// Serving the last request is not the daemon going quiet. A stop is
        /// asynchronous on purpose — the kill goes to the keeper and the death
        /// lands later, on the session's own reader thread, which marks the
        /// record dead and writes it out one more time — and the transcript
        /// scan persists on its own schedule besides. So joining the thread
        /// that served the client says nothing about the two threads still
        /// holding a pen.
        ///
        /// What that costs a removal is not a permissions error or a missing
        /// file, either of which would name itself. `persist_session_metadata`
        /// publishes by writing a uniquely named temporary beside the record
        /// and renaming it, so a write landing mid-walk puts a *new* entry in a
        /// directory the removal has already emptied, and the failure surfaces
        /// three levels up as `ENOTEMPTY` on the root — the one error message
        /// that says nothing about which file or why. Retrying is the whole
        /// answer: those writers are finishing, not starting, and a loaded CI
        /// runner is only the machine slow enough to let one of them be caught
        /// at it.
        fn discard_root(root: PathBuf) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match fs::remove_dir_all(&root) {
                    Ok(()) => return,
                    Err(error) if Instant::now() >= deadline => {
                        panic!("could not remove {}: {error}", root.display())
                    }
                    Err(_) => thread::sleep(Duration::from_millis(25)),
                }
            }
        }

        fn armed_trigger(
            matched: bool,
            cooldown_ms: u64,
            last_fired_at: Option<u64>,
        ) -> ArmedTrigger {
            ArmedTrigger {
                spec: Trigger {
                    id: "trg-1".into(),
                    session_id: "session-1".into(),
                    pattern: "Ready".into(),
                    action: TriggerAction::Notify {
                        text: "it said ready".into(),
                    },
                    once: false,
                    cooldown_ms,
                    created_at: 100,
                    last_fired_at,
                    fires: 0,
                },
                matched,
            }
        }

        #[test]
        fn a_trigger_fires_on_arrival_and_not_while_its_pattern_sits_there() {
            // The pattern arrives.
            assert!(trigger_fires(&armed_trigger(false, 0, None), true, 1_000));
            // It is still there on the next frame, and on every frame after.
            assert!(!trigger_fires(&armed_trigger(true, 0, None), true, 1_000));
            // It goes away without firing anything.
            assert!(!trigger_fires(&armed_trigger(true, 0, None), false, 1_000));
            // A pattern that flickers back inside the cooldown is one event.
            assert!(!trigger_fires(
                &armed_trigger(false, 5_000, Some(1_000)),
                true,
                3_000
            ));
            // Past the cooldown it is a new one.
            assert!(trigger_fires(
                &armed_trigger(false, 5_000, Some(1_000)),
                true,
                6_000
            ));
        }

        #[test]
        fn an_archived_session_revives_on_its_own_id_and_keeps_its_record() {
            let state = test_state("revive-id");
            let id = "muxloomd-terminal-revive-id";
            let first = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                111,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            first.write_input(b"marker line\r").unwrap();
            let history = state.paths.history.join(format!("{id}.ansi"));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fs::read(&history)
                .unwrap_or_default()
                .windows(11)
                .any(|w| w == b"marker line")
            {
                assert!(Instant::now() < deadline, "the history never saw the input");
                thread::sleep(Duration::from_millis(20));
            }
            first.archive().unwrap();
            drop(first);

            // The same number arriving again is that conversation coming back,
            // not a second identity for it: the record revives in place.
            let revived = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                999,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let snapshot = revived.snapshot();
            assert_eq!(snapshot.id, id);
            assert_eq!(snapshot.label, "coordinator");
            assert_eq!(snapshot.created_at, 111);
            assert!(!snapshot.archived);
            // The archived record is gone from the archive and there is
            // exactly one holder of the number again.
            assert!(
                !state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains_key(id)
            );
            assert_eq!(
                state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .filter(|session| session.snapshot().id == id)
                    .count(),
                1
            );
            // And its history was appended to, not replaced.
            assert!(
                fs::read(&history)
                    .unwrap()
                    .windows(11)
                    .any(|w| w == b"marker line")
            );
        }

        #[test]
        fn a_revival_that_cannot_start_leaves_the_archive_where_it_found_it() {
            let state = test_state("revive-refused");
            let id = "muxloomd-terminal-revive-refused";
            let first = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                111,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            first.archive().unwrap();
            drop(first);
            // Whether the conversation is still there to come back as itself,
            // asked of both indexes because an archive that has not been
            // reloaded since still sits among the live records - and asked
            // about who it was, not just that something answers to the
            // number.
            let still_archived = || {
                let live = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(id)
                    .map(|session| session.snapshot());
                let record = match live {
                    Some(record) => Some(record),
                    None => state
                        .persisted_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(id)
                        .map(|entry| entry.snapshot()),
                };
                record.is_some_and(|record| {
                    record.archived && record.label == "coordinator" && record.created_at == 111
                })
            };
            assert!(still_archived(), "the fixture never reached the archive");

            // The two ways a revival of a real record is refused by something
            // outside the record: the folder that conversation ran in is gone,
            // and the CLI it ran is no longer installed. Both are ordinary on
            // a machine a few months on from the conversation.
            let missing = state.paths.scratch.join("a-folder-that-was-removed");
            let refused = |path: String, executable: String| {
                let outcome = launch_session(
                    &state,
                    id.into(),
                    "terminal".into(),
                    path,
                    String::new(),
                    false,
                    executable,
                    vec![],
                    vec![],
                    999,
                    80,
                    24,
                    None,
                    None,
                );
                assert!(
                    outcome.is_err(),
                    "a revival that cannot start must not report success"
                );
            };
            refused(missing.to_string_lossy().into_owned(), "/bin/cat".into());
            assert!(
                still_archived(),
                "a revival refused for its folder took the record with it"
            );
            refused("/tmp".into(), "muxloom-no-such-binary-anywhere".into());
            assert!(
                still_archived(),
                "a revival refused for its executable took the record with it"
            );

            // Which is the whole point: the conversation is still there to
            // come back as itself once the reason it could not start is dealt
            // with. A record that had been dropped would come back here as a
            // nameless session created just now.
            let revived = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                999,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let snapshot = revived.snapshot();
            assert_eq!(snapshot.label, "coordinator");
            assert_eq!(snapshot.created_at, 111);
            assert!(!still_archived());
        }

        #[test]
        fn a_refused_revival_puts_the_record_back_in_the_index_it_came_out_of() {
            let state = test_state("revive-indexes");
            let refuse = |state: &Arc<DaemonState>, id: &str| {
                let missing = state.paths.scratch.join("a-folder-that-was-removed");
                let outcome = launch_session(
                    state,
                    id.into(),
                    "terminal".into(),
                    missing.to_string_lossy().into_owned(),
                    String::new(),
                    false,
                    "/bin/cat".into(),
                    vec![],
                    vec![],
                    999,
                    80,
                    24,
                    None,
                    None,
                );
                assert!(outcome.is_err(), "the folder is gone; the launch cannot be");
            };

            // The archive index, which is where every record sits once a
            // daemon has restarted and read it back off disk.
            let filed = "muxloomd-terminal-revive-filed";
            let first = launch_session(
                &state,
                filed.into(),
                "terminal".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                111,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            first.archive().unwrap();
            let record = first.snapshot();
            drop(first);
            // A daemon that has gone leaves records behind and not keepers,
            // so the restart reads this one out of a directory with nothing
            // listening in it - which is the only way a record reaches the
            // archive index rather than being left to adoption.
            let next = test_state("revive-filed");
            persist_session_metadata(&next.paths.sessions.join(format!("{filed}.json")), &record)
                .unwrap();
            fs::write(next.paths.history.join(format!("{filed}.ansi")), b"").unwrap();
            let restarted = Arc::new(DaemonState::new(next.paths.clone(), KeeperMode::InProcess));
            let archived = |state: &DaemonState| {
                state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(filed)
                    .map(|entry| entry.snapshot())
            };
            assert!(
                archived(&restarted).is_some(),
                "the restart never read the record back"
            );
            refuse(&restarted, filed);
            let back = archived(&restarted)
                .expect("the archive lost the record it handed to a launch that failed");
            assert_eq!(back.label, "coordinator");
            // Still *archived*, which is the part that matters: the archive
            // hands a record back only if it reached the archive, so one
            // filed there in any other state is one the number can never
            // revive as again.
            assert!(back.archived);

            // And the running index, where a session this daemon ran itself
            // stays however it ended - a record there has not been filed and
            // must not be.
            let ended = "muxloomd-terminal-revive-ended";
            let worker = launch_session(
                &state,
                ended.into(),
                "terminal".into(),
                "/tmp".into(),
                "worker".into(),
                false,
                "/bin/echo".into(),
                vec![],
                vec![],
                222,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker.snapshot().dead {
                assert!(Instant::now() < deadline, "the session never ended");
                thread::sleep(Duration::from_millis(20));
            }
            drop(worker);
            refuse(&state, ended);
            let live = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(ended)
                .map(|session| session.snapshot());
            let live = live.expect("a session that only ended was taken out of the running index");
            assert_eq!(live.label, "worker");
            assert!(
                state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(ended)
                    .is_none(),
                "an ended session was filed under the archive it never reached"
            );

            // Which is the whole point of putting each back where it was:
            // both numbers still revive as the conversations they were.
            for (state, id, label, created) in [
                (&restarted, filed, "coordinator", 111),
                (&state, ended, "worker", 222),
            ] {
                let revived = launch_session(
                    state,
                    id.into(),
                    "terminal".into(),
                    "/tmp".into(),
                    String::new(),
                    false,
                    "/bin/cat".into(),
                    vec![],
                    vec![],
                    999,
                    80,
                    24,
                    None,
                    None,
                )
                .unwrap();
                let snapshot = revived.snapshot();
                assert_eq!(snapshot.label, label);
                assert_eq!(snapshot.created_at, created);
            }
        }

        /// A record on disk, put where a restart will read it into the
        /// archive index: a folder with no keeper socket in it, which is the
        /// only way a record gets there rather than being left to adoption.
        fn restarted_around(name: &str, record: &DaemonSession) -> Arc<DaemonState> {
            let next = test_state(name);
            persist_session_metadata(
                &next.paths.sessions.join(format!("{}.json", record.id)),
                record,
            )
            .unwrap();
            fs::write(next.paths.history.join(format!("{}.ansi", record.id)), b"").unwrap();
            Arc::new(DaemonState::new(next.paths.clone(), KeeperMode::InProcess))
        }

        #[test]
        fn a_session_that_only_ended_comes_back_on_its_own_number_after_a_restart() {
            let state = test_state("ended-revives-after-restart");
            let id = "muxloomd-terminal-ended-restart";
            let worker = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "worker".into(),
                false,
                "/bin/echo".into(),
                vec![],
                vec![],
                333,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker.snapshot().dead {
                assert!(Instant::now() < deadline, "the session never ended");
                thread::sleep(Duration::from_millis(20));
            }
            let record = worker.snapshot();
            drop(worker);
            assert!(
                !record.archived,
                "a session that ended on its own is not a session anyone retired"
            );

            let restarted = restarted_around("ended-restart-read-back", &record);
            let filed = restarted
                .persisted_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id)
                .map(|entry| entry.snapshot())
                .expect("the restart never read the record back");
            assert!(!filed.archived && filed.dead);

            // The number is the conversation's, and a daemon restarting in
            // between is not an event the conversation took part in.
            let revived = launch_session(
                &restarted,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                999,
                80,
                24,
                None,
                None,
            )
            .expect("a session that only ended could not come back on its own number");
            let snapshot = revived.snapshot();
            assert_eq!(snapshot.label, "worker");
            assert_eq!(snapshot.created_at, 333);
        }

        #[test]
        fn a_session_the_daemon_never_put_down_is_filed_as_one_it_did() {
            let state = test_state("interrupted-filed");
            let id = "muxloomd-terminal-interrupted";
            let session = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "worker".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                111,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            // A record caught mid-flight: how this session ended went unwritten
            // because the daemon that would have written it is what stopped.
            let mut record = session.snapshot();
            assert!(!record.dead && !record.archived);
            record.pid = None;

            let restarted = restarted_around("interrupted-read-back", &record);
            let filed = restarted
                .persisted_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id)
                .map(|entry| entry.snapshot())
                .expect("the restart never read the record back");
            assert!(filed.dead);
            assert!(
                filed.archived,
                "the transcript is told this session was archived; the record has to say the same"
            );
            assert!(
                filed.archived_at.is_some(),
                "nothing says when it stopped, so its folder will order it by when it began"
            );
            drop(session);
        }

        #[test]
        fn a_record_that_already_moved_is_refused_out_of_the_archive_too() {
            let state = test_state("moved-record-refused");
            let previous = "muxloomd-terminal-moved-from";
            let successor = "muxloomd-terminal-moved-to";
            let first = launch_session(
                &state,
                previous.into(),
                "terminal".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                444,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            first.archive().unwrap();
            let mut record = first.snapshot();
            drop(first);
            record.dead = true;
            record.pid = None;
            record.resumed_to = Some(successor.into());

            let restarted = restarted_around("moved-record-read-back", &record);
            let running = launch_session(
                &restarted,
                successor.into(),
                "terminal".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                555,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            // The move is written on the record, and the record outlives the
            // daemon that wrote it: reading it back off the disk must not
            // read the move off it too.
            let error = match launch_session(
                &restarted,
                previous.into(),
                "terminal".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                999,
                80,
                24,
                None,
                None,
            ) {
                Ok(_) => panic!("one conversation was started in two places at once"),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("already resumed as") && error.contains(successor),
                "the refusal never names where the conversation went: {error}"
            );
            // And the record is still there to be told that again.
            assert!(
                restarted
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains_key(previous),
                "a refused revival took the record with it"
            );
            drop(running);
        }

        #[test]
        fn a_live_session_refuses_a_launch_over_its_number() {
            let state = test_state("live-holder-refuse");
            let id = "muxloomd-terminal-live-holder";
            let first = launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "first".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                111,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let error = match launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "second".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                999,
                80,
                24,
                None,
                None,
            ) {
                Ok(_) => panic!("a live session's number must never be launched over"),
                Err(error) => error,
            };
            assert!(format!("{error:#}").contains("still live"), "{error:#}");
            assert_eq!(first.snapshot().label, "first");
        }

        #[test]
        fn a_conversation_that_ended_on_its_own_is_still_the_one_a_reopen_matches() {
            let state = test_state("reopen-ended");
            let master = "muxloomd-claude-reopen-master";
            let successor = "muxloomd-claude-reopen-successor";
            let old = launch_session(
                &state,
                master.into(),
                "claude".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/echo".into(),
                vec!["--resume".into(), "ses-ended".into()],
                vec![],
                100,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let child = launch_session(
                &state,
                "muxloomd-claude-reopen-child".into(),
                "claude".into(),
                "/tmp".into(),
                "worker".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                101,
                80,
                24,
                Some(master.into()),
                None,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !old.snapshot().dead {
                assert!(Instant::now() < deadline, "the master never ended");
                thread::sleep(Duration::from_millis(20));
            }
            // Nobody retired it. It stopped by itself, which is the ordinary
            // way an agent's turn runs out.
            assert!(!old.snapshot().archived);
            drop(old);

            let revived = launch_session(
                &state,
                successor.into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec!["--resume".into(), "ses-ended".into()],
                vec![],
                200,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let snapshot = revived.snapshot();
            assert_eq!(
                snapshot.resumed_from.as_deref(),
                Some(master),
                "the reopen never found the conversation its own command line names"
            );
            assert_eq!(snapshot.label, "coordinator");
            assert_eq!(
                child.snapshot().parent.as_deref(),
                Some(successor),
                "the fleet was left hanging off a number that has stopped answering"
            );
        }

        #[test]
        fn a_new_launch_for_an_archived_conversation_takes_its_fleet_and_leaves_an_alias() {
            let state = test_state("compat-alias");
            let master = "muxloomd-claude-compat-master";
            let successor = "muxloomd-claude-compat-successor";
            let old = launch_session(
                &state,
                master.into(),
                "claude".into(),
                "/tmp".into(),
                "coordinator".into(),
                false,
                "/bin/cat".into(),
                vec!["--resume".into(), "ses-old".into()],
                vec![],
                100,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let live_child = launch_session(
                &state,
                "muxloomd-claude-compat-live".into(),
                "claude".into(),
                "/tmp".into(),
                "worker-one".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                101,
                80,
                24,
                Some(master.into()),
                None,
            )
            .unwrap();
            let archived_child = launch_session(
                &state,
                "muxloomd-claude-compat-arch".into(),
                "claude".into(),
                "/tmp".into(),
                "worker-two".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                102,
                80,
                24,
                Some(master.into()),
                None,
            )
            .unwrap();
            archived_child.archive().unwrap();
            drop(archived_child);
            old.archive().unwrap();
            drop(old);

            // The old conversation coming back under a fresh number is the
            // split the alias fields exist to record: the fleet follows the
            // number that answers, and the old record points at it. In this
            // process the archived records still sit in the live map under
            // their archived flag (the persisted map is a restarted daemon's
            // recollection of the same files); both views are what
            // archived_resume_match and reparent_children read.
            let revived = launch_session(
                &state,
                successor.into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec!["--resume".into(), "ses-old".into()],
                vec![],
                200,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let snapshot = revived.snapshot();
            assert_eq!(snapshot.label, "coordinator");
            assert_eq!(snapshot.resumed_from.as_deref(), Some(master));
            // The live child was repointed at the successor.
            assert_eq!(live_child.snapshot().parent.as_deref(), Some(successor));

            // The archived child and the retired master, still in the live
            // map as archived records, took the rewrite too.
            let sessions = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let arch_child = sessions
                .get("muxloomd-claude-compat-arch")
                .expect("the archived child is still recorded here")
                .snapshot();
            assert!(arch_child.archived);
            assert_eq!(arch_child.parent.as_deref(), Some(successor));
            let retired = sessions
                .get(master)
                .expect("the retired master is still recorded here")
                .snapshot();
            assert!(retired.archived);
            assert_eq!(retired.resumed_to.as_deref(), Some(successor));
            drop(sessions);

            // The resumed-successor guard that stops a second fork of one
            // conversation needs the successor to still be live, which this
            // unit harness's /bin/cat stand-in cannot guarantee past its own
            // exit; the guard itself is exercised end-to-end by the control
            // surface test `a_resume_by_muxloom_id_refuses_the_session_that_is_still_live`.
        }

        #[test]
        fn a_trigger_outlives_the_daemon_that_took_it() {
            let state = test_state("triggers-reload");
            state.save_triggers(&[armed_trigger(false, 5_000, Some(400))]);

            // What a handover leaves behind is a file, so the next generation
            // is the one that has to make sense of it.
            let restarted = DaemonState::new(state.paths.clone(), KeeperMode::InProcess);
            let triggers = restarted
                .triggers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(triggers.len(), 1);
            assert_eq!(restarted.armed.load(Ordering::Relaxed), 1);
            assert_eq!(triggers[0].spec.pattern, "Ready");
            assert_eq!(triggers[0].spec.last_fired_at, Some(400));
            // A restored screen is not a new arrival: whatever is on it when
            // the daemon comes back counts as already seen.
            assert!(triggers[0].matched);
            drop(triggers);

            fs::remove_dir_all(&state.paths.root).ok();
        }

        #[test]
        fn a_trigger_armed_after_a_frame_was_drawn_waits_for_the_next_one() {
            // A session with the pattern already on its screen. Arming against
            // that is the ordinary case - `set_trigger` reads the screen and
            // starts the trigger matched, so the text sitting there is text
            // that was already seen.
            let state = Arc::new(test_state("trigger-freshness"));
            let session = launch_session(
                &state,
                "muxloomd-terminal-trigger-freshness".into(),
                "terminal".into(),
                "/tmp".into(),
                "watched".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                333,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let watched = session.session_id();
            session.record_output(b"Ready\r\n");

            // The state a race would leave behind: the arriving output was
            // drawn into a frame, then the trigger was armed off a newer
            // picture of the screen than that frame holds. Read against the
            // older one, text that was there when it was armed reads as
            // having just turned up.
            let arm = |created_at: u64, matched: bool| {
                let mut spec = armed_trigger(matched, 0, None).spec;
                spec.session_id = watched.clone();
                spec.created_at = created_at;
                *state
                    .triggers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    vec![ArmedTrigger { spec, matched }];
            };
            let notice = || {
                session
                    .notice
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            };

            arm(now_ms() + 60_000, false);
            fire_triggers(&state, &session, &watched);
            assert_eq!(
                notice(),
                None,
                "a trigger younger than the frame must not be judged by it"
            );
            let held = state
                .triggers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(held.len(), 1, "and it must not be spent either");
            assert_eq!(held[0].spec.fires, 0);
            assert!(!held[0].matched, "its own priming is what stands");
            drop(held);

            // Older than the frame, everything else equal, it fires: the guard
            // above is holding back exactly this.
            arm(now_ms() - 60_000, false);
            fire_triggers(&state, &session, &watched);
            assert_eq!(notice().as_deref(), Some("it said ready"));

            discard_root(state.paths.root.clone());
        }

        #[test]
        fn a_launch_in_flight_lets_a_client_in_but_not_a_handover() {
            let state = test_state("gate-split");
            // What a launch holds while it forks a keeper, seeds the screen and
            // syncs the record to disk.
            let drain_guard = state
                .client_gate
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let launch_guard = state
                .launch_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            // A connection arriving in the middle of that is taken at once.
            let arriving = Arc::clone(&state);
            let taken = thread::spawn(move || register_client(&arriving));
            thread::sleep(Duration::from_millis(200));
            assert!(
                taken.is_finished(),
                "a client arriving during a launch waited for the launch"
            );
            let admitted = taken.join().unwrap();
            assert!(admitted.is_some(), "the client was turned away");

            // The drain still waits for it. Standing down halfway through a
            // launch is the whole reason the gate is there.
            let draining = Arc::clone(&state);
            let handover = thread::spawn(move || prepare_handover(&draining));
            thread::sleep(Duration::from_millis(200));
            assert!(!handover.is_finished(), "a handover cut into a launch");
            drop(launch_guard);
            drop(drain_guard);
            assert!(handover.join().unwrap(), "the handover never went through");
            drop(admitted);
        }

        #[test]
        fn a_forward_to_an_address_that_answers_nothing_gives_up_on_its_own_budget() {
            use std::net::TcpListener;

            // The working path first: a forward that can be made is still made.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let bound = listener.local_addr().unwrap();
            let reached = connect_forward_within(
                &bound.ip().to_string(),
                bound.port(),
                FORWARD_CONNECT_TIMEOUT,
            );
            assert!(
                reached.is_ok(),
                "a reachable forward was refused: {reached:?}"
            );

            // And one that cannot be made comes back on the budget it was
            // given. 192.0.2.1 is TEST-NET-1, which is documentation address
            // space and is routed nowhere: the connect either hangs until the
            // budget runs out, or -- where the host has no route for it at all
            // -- is refused sooner. Either is an answer; what this rules out is
            // the third case, waiting out the operating system's own patience,
            // which is over a minute here and blocks the frame loop for all of
            // it.
            let started = Instant::now();
            let answer = connect_forward_within("192.0.2.1", 9, Duration::from_millis(150));
            let waited = started.elapsed();
            assert!(answer.is_err(), "an address routed nowhere was reached");
            assert!(
                waited < Duration::from_secs(5),
                "a forward nobody answers held the frame loop for {waited:?}"
            );
        }

        #[test]
        fn a_stamp_already_waited_out_is_not_waited_out_again() {
            let root = PathBuf::from("/tmp").join(format!(
                "mxl-settle-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            ));
            let paths = DaemonPaths::under(root);
            paths.prepare().unwrap();
            // A stamp this build outranks, so the answer is always "not
            // current" and what is being measured is only the waiting.
            let old = format!("0.0.1:protocol-1:local:local:settle-{}", std::process::id());
            fs::write(&paths.generation, &old).unwrap();

            let asked = |paths: &DaemonPaths| {
                let started = Instant::now();
                let current = generation_is_current_after_settling(paths);
                assert!(!current, "a stamp this build outranks read as current");
                started.elapsed()
            };

            let first = asked(&paths);
            assert!(
                first >= GENERATION_SETTLE,
                "the first ask did not wait for the stamp to settle: {first:?}"
            );
            let second = asked(&paths);
            assert!(
                second < GENERATION_SETTLE,
                "a stamp already waited out was waited out again: {second:?}"
            );

            // A different stamp is a different question, and gets the wait.
            fs::write(&paths.generation, format!("{old}-again")).unwrap();
            let third = asked(&paths);
            assert!(
                third >= GENERATION_SETTLE,
                "a stamp nobody has waited out was answered from memory: {third:?}"
            );
        }

        #[test]
        fn a_screen_that_has_not_moved_is_laid_out_as_text_once() {
            let state = test_state("screen-cache");
            let id = "muxloomd-terminal-cache-1";
            launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "still".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                224,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let session = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id)
                .map(Arc::clone)
                .unwrap();

            // Holding the screen is how the question gets asked: a reading that
            // walks the grid has to wait here, and one served from the last
            // walk does not.
            let read_while = |session: &Arc<ManagedSession>| {
                let held = session
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let probe = Arc::clone(session);
                let reading = thread::spawn(move || probe.visible_screen());
                thread::sleep(Duration::from_millis(200));
                let walked = !reading.is_finished();
                drop(held);
                (walked, reading.join().unwrap())
            };

            let first = session.visible_screen();
            let (walked, again) = read_while(&session);
            assert!(!walked, "a screen that had not moved was walked again");
            assert_eq!(&*first, &*again, "and the second reading differed");

            // Output moves the grid, and the next reading has to go and look.
            session.record_output(b"cached-screen-probe\r\n");
            let (walked, after) = read_while(&session);
            assert!(walked, "a screen that moved was served from the last walk");
            assert!(
                after.contains("cached-screen-probe"),
                "the fresh reading missed what arrived: {after:?}"
            );

            // And a reflow rewrites every row, so it counts as moving the grid
            // every bit as much as output does.
            session.resize(100, 30).ok();
            let (walked, _) = read_while(&session);
            assert!(walked, "a reflowed screen was served from the last walk");
        }

        #[test]
        fn a_stopped_session_is_read_off_its_screen_once_and_never_again() {
            let state = test_state("settled-screen");
            let id = "muxloomd-terminal-settled-1";
            launch_session(
                &state,
                id.into(),
                "terminal".into(),
                "/tmp".into(),
                "over".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                222,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let session = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id)
                .map(Arc::clone)
                .unwrap();

            // Move the grid, then hold the screen: a reading that has to go and
            // look waits here, and one that will not look does not. Moving it
            // first is what rules out the other reason not to look, which is
            // that the last walk is still good -- see
            // [`a_screen_that_has_not_moved_is_laid_out_as_text_once`].
            let drawn_after_moving = |session: &Arc<ManagedSession>, mark: &[u8]| {
                session.record_output(mark);
                let held = session
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let probe = Arc::clone(session);
                let reading = thread::spawn(move || probe.snapshot());
                thread::sleep(Duration::from_millis(200));
                let waited = !reading.is_finished();
                drop(held);
                reading.join().unwrap();
                waited
            };

            // A running session is read off its screen every time it moves,
            // because its screen is what it is doing now.
            assert!(
                drawn_after_moving(&session, b"one\r\n"),
                "a running session is drawn"
            );

            // Stopped, nothing will paint it again. The next reading still
            // takes the picture -- and it is the last one that ever will.
            session.archived.store(true, Ordering::Relaxed);
            assert!(
                drawn_after_moving(&session, b"two\r\n"),
                "the final reading is still taken off the screen"
            );
            assert!(
                !drawn_after_moving(&session, b"three\r\n"),
                "a session nothing can paint was laid out as text again"
            );

            // Death is the same door, and marking it walks through the final
            // reading itself: persisting the record is what takes it.
            let other = "muxloomd-terminal-settled-2";
            launch_session(
                &state,
                other.into(),
                "terminal".into(),
                "/tmp".into(),
                "over".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                223,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let other = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(other)
                .map(Arc::clone)
                .unwrap();
            other.mark_dead();
            assert!(
                !drawn_after_moving(&other, b"four\r\n"),
                "a dead session was laid out as text again"
            );
            assert!(other.snapshot().dead, "and it is still the same record");
        }

        #[test]
        fn a_repeating_round_is_answered_with_what_is_running_and_not_the_archive() {
            // A machine with a conversation behind it: one record put down, and
            // a daemon starting on top of it.
            let initial = test_state("live-only");
            let paths = initial.paths.clone();
            drop(initial);
            let put_down = "muxloomd-terminal-1700000111-9-1";
            let mut retired = live_metadata(put_down, "terminal", None);
            retired.dead = true;
            retired.archived = true;
            retired.archived_at = Some(1);
            retired.working = false;
            persist_session_metadata(&paths.sessions.join(format!("{put_down}.json")), &retired)
                .unwrap();
            fs::write(
                paths.history.join(format!("{put_down}.ansi")),
                b"yesterday\n",
            )
            .unwrap();

            // Nothing running can change that record, which is why a round
            // watching what is running has no use for it.
            let state = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            assert!(
                state
                    .persisted_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains_key(put_down),
                "the archived record has to be in the archive for this to say anything"
            );
            let running = "muxloomd-terminal-live-only-now";
            launch_session(
                &state,
                running.into(),
                "terminal".into(),
                "/tmp".into(),
                "today".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                222,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            let (mut client, server) = UnixStream::pair().unwrap();
            let serve = Arc::clone(&state);
            let handle = thread::spawn(move || serve_client(server, serve));
            let mut ask = |request_id: u64, live_only: bool, only: Option<&str>| {
                Frame::json(
                    FrameKind::Request,
                    0,
                    request_id,
                    &DaemonRequest::ListSessions {
                        live_only,
                        only: only.map(str::to_string),
                    },
                )
                .unwrap()
                .write_to(&mut client)
                .unwrap();
                loop {
                    let frame = Frame::read_from(&mut client).unwrap().unwrap();
                    if frame.kind != FrameKind::Response || frame.request_id != request_id {
                        continue;
                    }
                    match frame.decode_json::<DaemonResponse>().unwrap() {
                        DaemonResponse::Sessions { sessions } => {
                            return sessions
                                .into_iter()
                                .map(|session| session.id)
                                .collect::<Vec<_>>();
                        }
                        response => panic!("unexpected response {response:?}"),
                    }
                }
            };

            // What a dashboard asks several times a second, and what it costs:
            // one record, not one per conversation the machine has ever held.
            assert_eq!(ask(20, true, None), vec![running.to_string()]);
            // Asked for the whole list, the answer still holds the archive - an
            // older client sends no flag at all and must keep seeing it.
            let mut everything = ask(21, false, None);
            everything.sort();
            assert_eq!(everything, vec![put_down.to_string(), running.to_string()]);

            // What a wait asks once a second for a minute: the one session it
            // is watching, and not the rest of the machine drawn and
            // classified to be thrown away.
            assert_eq!(ask(22, true, Some(running)), vec![running.to_string()]);
            // An id that is running is answered without the archive being
            // opened, and one that is only in the archive is still found by a
            // round that asked for it - that is the round a wait ends on.
            assert_eq!(ask(23, true, Some(put_down)), Vec::<String>::new());
            assert_eq!(ask(24, false, Some(put_down)), vec![put_down.to_string()]);
            assert_eq!(ask(25, false, Some(running)), vec![running.to_string()]);
            // An id nobody holds is nothing, not everybody.
            assert_eq!(
                ask(26, false, Some("no-such-session")),
                Vec::<String>::new()
            );

            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
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
        fn a_failing_stream_reply_keeps_the_multiplexed_connection_alive() {
            // One bad open must not tear the whole client connection down: an
            // unknown session id answers with a per-stream error, and the next
            // request still gets its reply on the same socket. Before the
            // isolation this returned an error straight up the frame loop and
            // closed the connection, killing every other live stream and
            // forcing a seconds-long re-dial on the next operation.
            let (mut client, server) = UnixStream::pair().unwrap();
            let state = test_state("stream-isolation");
            let handle = thread::spawn(move || serve_client(server, state));

            Frame::json(
                FrameKind::OpenStream,
                99,
                0,
                &OpenStream::Pty {
                    session_id: "no-such-session".into(),
                    columns: 80,
                    rows: 24,
                    scrollback_rows: 0,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();

            let reply = Frame::read_from(&mut client).unwrap().unwrap();
            assert_eq!(
                reply.kind,
                FrameKind::Error,
                "the bad open is an error reply"
            );
            assert_eq!(
                reply.stream_id, 99,
                "the error names the stream that failed"
            );
            match reply.decode_json::<DaemonResponse>().unwrap() {
                DaemonResponse::Error { message } => {
                    assert!(
                        message.contains("no-such-session") || message.contains("session"),
                        "the message names the failed stream: {message}"
                    );
                }
                response => panic!("expected an error, got {response:?}"),
            }

            // The connection is still alive: the same client is served again.
            Frame::json(FrameKind::Request, 0, 8, &DaemonRequest::Ping)
                .unwrap()
                .write_to(&mut client)
                .unwrap();
            let mut pong = false;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !pong && std::time::Instant::now() < deadline {
                if let Some(frame) = Frame::read_from(&mut client).unwrap() {
                    if frame.kind == FrameKind::Response && frame.request_id == 8 {
                        assert!(
                            matches!(
                                frame.decode_json::<DaemonResponse>().unwrap(),
                                DaemonResponse::Pong { .. }
                            ),
                            "the connection must survive the failed stream"
                        );
                        pong = true;
                    }
                }
            }
            assert!(pong, "the ping after the failed stream must be answered");
            drop(client);
            handle.join().unwrap().unwrap();
        }

        #[test]
        fn a_session_started_by_an_agent_is_recorded_under_it() {
            let state = test_state("subagent");
            let root = state.paths.root.clone();
            let start = |id: &str, parent: Option<&str>| {
                launch_session(
                    &state,
                    id.into(),
                    "terminal".into(),
                    "/tmp".into(),
                    id.into(),
                    false,
                    "/bin/cat".into(),
                    vec![],
                    vec![],
                    1,
                    80,
                    24,
                    parent.map(Into::into),
                    None,
                )
                .unwrap()
            };

            let lead = start("muxloomd-terminal-lead", None);
            // A person's session belongs to nobody.
            assert_eq!(lead.snapshot().parent, None);

            let child = start("muxloomd-terminal-child", Some("muxloomd-terminal-lead"));
            assert_eq!(
                child.snapshot().parent.as_deref(),
                Some("muxloomd-terminal-lead")
            );
            // And it survives the daemon that recorded it: a tree that only
            // existed in memory would come back flat after every restart.
            let reloaded: DaemonSession = serde_json::from_str(
                &fs::read_to_string(state.paths.sessions.join("muxloomd-terminal-child.json"))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(reloaded.parent.as_deref(), Some("muxloomd-terminal-lead"));

            // A parent naming itself is not a tree, and is dropped rather than
            // refused: the launch is fine, only the claim about it is not.
            let loop_back = start("muxloomd-terminal-loop", Some("muxloomd-terminal-loop"));
            assert_eq!(loop_back.snapshot().parent, None);

            // The parent it names does not have to be a session this daemon
            // holds — an agent on another machine is still the agent whose
            // work this is.
            let far = start("muxloomd-terminal-far", Some("muxloomd-claude-elsewhere"));
            assert_eq!(
                far.snapshot().parent.as_deref(),
                Some("muxloomd-claude-elsewhere")
            );

            // Which piece of work each of them is part of, which is what the
            // task scope on the board is keyed by.
            let grandchild = start(
                "muxloomd-terminal-grandchild",
                Some("muxloomd-terminal-child"),
            );
            assert_eq!(
                task_root(&state, "muxloomd-terminal-lead", None),
                "muxloomd-terminal-lead"
            );
            for under in ["muxloomd-terminal-child", "muxloomd-terminal-grandchild"] {
                assert_eq!(
                    task_root(&state, under, session_parent(&state, under).as_deref()),
                    "muxloomd-terminal-lead",
                    "{under} should belong to the task its chain hangs off"
                );
            }
            // A chain that leaves this machine stops at the last id there is
            // anything to resolve — which is the id the other machine names
            // too, so both halves of the task agree on it.
            assert_eq!(
                task_root(
                    &state,
                    "muxloomd-terminal-far",
                    Some("muxloomd-claude-elsewhere")
                ),
                "muxloomd-claude-elsewhere"
            );

            for session in [lead, child, loop_back, far, grandchild] {
                session.stop().ok();
            }
            fs::remove_dir_all(&root).ok();
        }

        #[test]
        fn send_input_types_into_the_pty_without_attaching_a_stream() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let state = test_state("send-input");
            let root = state.paths.root.clone();
            let server_state = Arc::clone(&state);
            let handle = thread::spawn(move || serve_client(server, server_state));

            let session = launch_session(
                &state,
                "muxloomd-terminal-send-input".into(),
                "terminal".into(),
                "/tmp".into(),
                "send input".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let before = session.snapshot();

            Frame::json(
                FrameKind::Request,
                0,
                20,
                &DaemonRequest::Hello {
                    client_version: env!("CARGO_PKG_VERSION").into(),
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 20 {
                    match frame.decode_json::<DaemonResponse>().unwrap() {
                        DaemonResponse::Hello { capabilities, .. } => {
                            assert!(capabilities.iter().any(|it| it == "send-input-v1"));
                        }
                        response => panic!("unexpected hello response {response:?}"),
                    }
                    break;
                }
            }

            Frame::json(
                FrameKind::Request,
                0,
                21,
                &DaemonRequest::SendInput {
                    session_id: "muxloomd-terminal-send-input".into(),
                    bytes: b"send-input-probe\r".to_vec(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 21 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }

            let probe = b"send-input-probe";
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut output = Vec::new();
            while Instant::now() < deadline {
                output = history_tail(&session.history_path, RECENT_OUTPUT_LIMIT as u64)
                    .unwrap_or_default();
                if output.windows(probe.len()).any(|window| window == probe) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                output.windows(probe.len()).any(|window| window == probe),
                "typed bytes must reach the PTY output"
            );
            // Typing must not resize the session the way an attach would.
            let after = session.snapshot();
            assert_eq!(
                (before.pid, session.columns.load(Ordering::Relaxed)),
                (after.pid, 80)
            );

            Frame::json(
                FrameKind::Request,
                0,
                22,
                &DaemonRequest::SendInput {
                    session_id: "muxloomd-terminal-send-input-missing".into(),
                    bytes: b"x".to_vec(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 22 {
                    match frame.decode_json::<DaemonResponse>().unwrap() {
                        DaemonResponse::Error { message } => {
                            assert!(message.contains("unknown daemon session"));
                        }
                        response => panic!("unexpected response {response:?}"),
                    }
                    break;
                }
            }

            session.archive().unwrap();
            drop(client);
            handle.join().unwrap().unwrap();
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn a_handover_a_second_client_defers_still_happens_once_nothing_is_in_hand() {
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
                    parent: None,
                    powers: None,
                    initial_prompt: None,
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
            // The client-side probe used against legacy daemons still reports
            // live sessions: an old daemon really cannot hand them over.
            assert!(!daemon_is_idle_for_handover(&mut client).unwrap());

            // A second client defers the handover; a live keeper-owned session
            // does not — it transfers to the next generation.
            let (second, server) = UnixStream::pair().unwrap();
            let second_state = Arc::clone(&state);
            let second_handle = thread::spawn(move || serve_client(server, second_state));
            let deadline = Instant::now() + Duration::from_secs(3);
            while state.clients.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(!prepare_handover(&state));

            // Deferred, not refused. Neither client is waiting on an answer or
            // watching a screen, so the daemon stands down of its own accord
            // and leaves the next generation to adopt the session — even
            // though the second client is still sitting there, which is what
            // an agent's MCP bridge does for as long as the agent runs.
            let deadline = Instant::now() + RETIREMENT_DEADLINE;
            while !state.shutdown.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(25));
            }
            assert!(
                state.draining.load(Ordering::Acquire) && state.shutdown.load(Ordering::Acquire),
                "a second client must postpone the handover, not cancel it"
            );
            drop(second);
            second_handle.join().unwrap().unwrap();

            let (mut rejected, server) = UnixStream::pair().unwrap();
            let draining_state = Arc::clone(&state);
            let rejected_handle = thread::spawn(move || serve_client(server, draining_state));
            let mut byte = [0_u8; 1];
            assert_eq!(rejected.read(&mut byte).unwrap(), 0);
            rejected_handle.join().unwrap().unwrap();
            drop(client);
            handle.join().unwrap().unwrap();
            for session in state
                .sessions
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect::<Vec<_>>()
            {
                session.stop().unwrap();
            }
        }

        /// Two builds that each believe they are the current one would take
        /// turns retiring each other for as long as both are in use, and every
        /// turn costs every attached client its connection. So they are
        /// ordered: the package version first, then how many commits are
        /// behind the build, with a build made by hand in front of every
        /// numbered one of its version and a build too old to say behind them
        /// all.
        #[test]
        fn generations_of_one_version_are_ordered_by_where_they_came_from() {
            let rank = |stamp: &str| generation_rank(stamp).unwrap();
            let legacy = rank("0.5.4:protocol-3:abc123");
            let release = rank("0.5.4:protocol-3:abc123:1200");
            let nightly = rank("0.5.4:protocol-3:def456:1300");
            let handmade = rank("0.5.4:protocol-3:local:local");
            let next = rank("0.5.5:protocol-3:ghi789:1400");

            assert!(legacy < release, "a build from before the order yields");
            assert!(release < nightly, "a later commit count is a later build");
            assert!(nightly < handmade, "a build made by hand is meant to win");
            assert!(handmade < next, "but never against a later version");
        }

        #[test]
        fn a_handover_is_asked_for_only_by_a_build_that_outranks_the_running_one() {
            let current = current_generation();
            assert!(!should_replace_generation(&current), "itself");
            assert!(
                !should_replace_generation(&format!("{current}\n")),
                "the stamp is read back off disk, newline and all"
            );
            assert!(
                !should_replace_generation("999.0.0:protocol-3:abc123:99999"),
                "a build this one cannot be newer than keeps its place"
            );
            assert!(should_replace_generation("0.0.1:protocol-1:abc123:1"));
            assert!(
                should_replace_generation("0.0.1:protocol-1:abc123"),
                "a daemon from before generations were ordered still yields"
            );

            // Same rank, different copy of the same hand-made file: two
            // compiles of one tree, and handing over is the whole point of
            // rebuilding.
            let mine = "0.5.5:protocol-1:local:local:muxloomd@200-2";
            assert!(generation_makes_way(
                "0.5.5:protocol-1:local:local:muxloomd@100-1",
                mine
            ));
        }

        /// Nothing CI does not fill in tells two hand-made builds apart, and
        /// nobody builds by hand except to run what they just changed. Without
        /// the file's own identity in the stamp both compiles say `local:local`
        /// and the daemon from before the change serves on.
        #[test]
        fn two_builds_of_one_tree_are_told_apart_by_the_file_each_one_is() {
            let current = current_generation();
            let fields: Vec<&str> = current.split(':').collect();
            assert_eq!(fields.len(), 5, "{current}");
            let running = std::env::current_exe().expect("this test is a file somewhere");
            assert_eq!(
                Some(format!(
                    "{}@{}",
                    running.file_name().unwrap().to_string_lossy(),
                    crate::daemon::file_identity(&running).unwrap()
                )),
                Some(fields[4].to_string()),
                "the last field names the running executable and says which copy"
            );
            assert_eq!(
                crate::daemon::stamped_executable_name(&current),
                running.file_name().unwrap().to_str(),
                "and the name can be read back out of it"
            );

            // What a rebuild of the same tree looks like: everything CI would
            // have stamped is identical, the file is called the same thing, and
            // only the copy has moved on.
            let rebuilt = "0.5.5:protocol-1:local:local:muxloomd@1-1";
            let mine = "0.5.5:protocol-1:local:local:muxloomd@2-2";
            assert!(
                generation_makes_way(rebuilt, mine),
                "a daemon running the file this build replaced makes way"
            );
            assert_eq!(
                generation_rank(rebuilt),
                generation_rank(mine),
                "and it is still the same rank, so it asks rather than insists"
            );
            assert!(!outranks_running_generation(rebuilt));
        }

        /// The pair every machine runs: a controller and the companion beside
        /// it, cut from one commit, ranking equal, and two different files. A
        /// stamp that said only which copy was running made each of them read
        /// the other's daemon as a rebuild of its own — so the dashboard retired
        /// what the MCP servers had started and the MCP servers retired what the
        /// dashboard started back, hundreds of times over one day, every one of
        /// them dropping every attached client and losing everything the daemon
        /// had worked out about the sessions it held.
        #[test]
        fn the_controller_and_the_companion_beside_it_leave_each_other_alone() {
            for (id, height) in [("aaac2e0", "265"), ("local", "local")] {
                let stamp = |file: &str| format!("0.5.5:protocol-1:{id}:{height}:{file}");
                let controller = stamp("muxloom@14772976-1788000000000");
                let companion = stamp("muxloomd@7825712-1788000000000");
                assert!(
                    !generation_makes_way(&controller, &companion),
                    "{height}: the companion must not retire the controller's daemon"
                );
                assert!(
                    !generation_makes_way(&companion, &controller),
                    "{height}: nor the controller the companion's"
                );
            }

            // Nor may a file that cannot name itself claim to be anyone's
            // rebuild: a package manager that replaced both of them mid-life
            // leaves two stamps saying `unknown`, and reading those as copies
            // of one file is the same fight by another name.
            let replaced =
                |id: &str, height: &str| format!("0.5.5:protocol-1:{id}:{height}:unknown");
            assert!(!generation_makes_way(
                &replaced("aaac2e0", "local"),
                &replaced("4013819", "local")
            ));
            assert!(
                generation_makes_way(&replaced("aaac2e0", "265"), &replaced("4013819", "local")),
                "a build that genuinely outranks it still says so"
            );
        }

        /// Asking is not the same as being allowed to insist. Equal rank asks
        /// and waits; only a build that actually outranks the one running may
        /// end the argument, because only there is somebody being upgraded.
        #[test]
        fn only_a_build_that_outranks_the_running_one_may_stop_it() {
            let current = current_generation();
            assert!(!outranks_running_generation(&current), "itself");
            assert!(outranks_running_generation("0.0.1:protocol-1:abc123:1"));
            assert!(
                outranks_running_generation("0.5.4:protocol-1:96012c2"),
                "the stamp a daemon from before heights were recorded leaves"
            );
            assert!(!outranks_running_generation(
                "999.0.0:protocol-3:abc123:99999"
            ));
            assert!(
                outranks_running_generation(""),
                "no stamp at all predates generations, and every build outranks that"
            );

            // Two compiles of one tree are the same rank, and the same rank
            // never insists: they replace each other all day and neither gets
            // to shoot the other.
            let mut fields: Vec<&str> = current.split(':').collect();
            fields[2] = "a-different-commit";
            assert!(
                !outranks_running_generation(&fields.join(":")),
                "same rank, different build: ask, do not insist"
            );
        }

        /// The escalation a fleet on nightlies needs. Every build between two
        /// releases carries one version number, so while only the version was
        /// compared a daemon six weeks behind read as an equal: the patience
        /// clock never started, and the only way past a daemon that kept
        /// deferring was somebody pressing the forced-update key on each
        /// machine in turn.
        #[test]
        fn a_later_nightly_of_one_version_outranks_an_earlier_one() {
            let earlier = generation_rank("0.5.5:protocol-1:aaac2e0:265:1-1");
            let later = generation_rank("0.5.5:protocol-1:3af6b11:287:1-1");
            assert!(earlier < later);

            // And a build somebody compiled by hand sits above every numbered
            // build of its version, so a working tree may retire the release
            // installed beside it and not the other way about.
            let by_hand = generation_rank("0.5.5:protocol-1:local:local:1-1");
            assert!(later < by_hand);
        }

        /// The ask has to survive the client that made it: every bridge and
        /// every MCP call is a new process, so patience kept in memory would
        /// restart from zero each time and never run out.
        #[test]
        fn the_ask_to_make_way_is_remembered_across_the_clients_that_make_it() {
            let root = std::env::temp_dir().join(format!(
                "muxloomd-ask-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            let paths = DaemonPaths::under(root.clone());
            paths.prepare().unwrap();

            // Nothing running yet: no stamp is a daemon from before there were
            // any, and it gets asked like every other.
            assert_eq!(handover_ask_age(&paths), Some(Duration::ZERO));
            assert!(!handover_is_overdue(&paths));

            fs::write(&paths.generation, "0.5.4:protocol-1:96012c2\n").unwrap();
            assert_eq!(handover_ask_age(&paths), Some(Duration::ZERO));
            let noted = fs::read_to_string(&paths.handover).unwrap();
            assert!(
                noted.starts_with("0.5.4:protocol-1:96012c2\t"),
                "the ask names the generation it is about: {noted}"
            );

            // A second client finds the first client's ask rather than making
            // its own, which is what lets the wait actually accumulate.
            let long_ago = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                - (HANDOVER_PATIENCE.as_millis() as u64 + 1_000);
            fs::write(
                &paths.handover,
                format!("0.5.4:protocol-1:96012c2\t{long_ago}"),
            )
            .unwrap();
            assert!(handover_is_overdue(&paths));

            // Another daemon took over in the meantime: the wait is about the
            // one that was refusing, so it starts again for the new one.
            fs::write(&paths.generation, "0.5.4:protocol-1:something-else\n").unwrap();
            assert_eq!(handover_ask_age(&paths), Some(Duration::ZERO));
            assert!(!handover_is_overdue(&paths));

            // A build this one cannot be newer than is never asked at all, so
            // there is nothing to grow overdue.
            fs::write(&paths.generation, "999.0.0:protocol-3:abc123:99999\n").unwrap();
            assert_eq!(handover_ask_age(&paths), None);
            assert!(!handover_is_overdue(&paths));

            forget_handover_ask(&paths);
            assert!(!paths.handover.exists());

            fs::remove_dir_all(&root).ok();
        }

        /// A handover the asking client never hears about still has to happen.
        /// By the time the daemon answers it has already stopped taking work,
        /// so if the answer cannot be delivered it must go anyway: a daemon
        /// left alive and draining refuses every later launch, and its callers
        /// spend the rest of its life on the compatibility fallback.
        #[test]
        fn an_accepted_handover_stops_the_daemon_even_when_the_answer_is_lost() {
            let (client, server) = UnixStream::pair().unwrap();
            let state = test_state("handover-hangup");
            // The sole client the handover requires, counted exactly as
            // serve_client counts it on the way in.
            state.clients.store(1, Ordering::Relaxed);
            // The request has already been read; only the answer has nowhere
            // left to go.
            drop(client);
            let writer = Arc::new(Mutex::new(server));
            let delivered = handle_request(&writer, &state, 71, DaemonRequest::PrepareHandover);
            assert!(
                delivered.is_err(),
                "this test is about a lost answer, so writing one must fail"
            );
            assert!(
                state.draining.load(Ordering::Acquire),
                "a sole client's handover request must be accepted"
            );
            assert!(
                state.shutdown.load(Ordering::Acquire),
                "a daemon that accepted a handover must stop even if the answer never arrived"
            );
        }

        /// A machine is told the channels once and remembers them. The file is
        /// where an agent here looks when it has something to tell a human, so
        /// it has to outlive the daemon that was handed it — and it has to be
        /// unreadable to everyone else, because it holds an app secret.
        #[test]
        fn a_pushed_channel_set_is_kept_privately_and_answered_without_its_secret() {
            use crate::channel::{ChannelBinding, ChannelKind};

            let (mut client, server) = UnixStream::pair().unwrap();
            let state = test_state("channels");
            let writer = Arc::new(Mutex::new(server));
            let answer = |client: &mut UnixStream, id: u64| loop {
                let frame = Frame::read_from(client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == id {
                    return frame.decode_json::<DaemonResponse>().unwrap();
                }
            };
            let set = ChannelSet {
                revision: 4,
                bindings: vec![ChannelBinding {
                    id: "lark-1".into(),
                    kind: ChannelKind::Lark,
                    label: "Team".into(),
                    app_id: "cli_1".into(),
                    secret: "shhh".into(),
                    route: "oc_1".into(),
                    route_label: "Team".into(),
                    preferred: true,
                    ..Default::default()
                }],
            };

            handle_request(
                &writer,
                &state,
                1,
                DaemonRequest::ChannelsPut { set: set.clone() },
            )
            .unwrap();
            assert!(matches!(answer(&mut client, 1), DaemonResponse::Ack));
            assert_eq!(ChannelSet::read(&state.paths.channels), set);
            let mode = fs::metadata(&state.paths.channels)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "an app secret must not be readable by others");

            handle_request(&writer, &state, 2, DaemonRequest::ChannelsGet).unwrap();
            match answer(&mut client, 2) {
                DaemonResponse::Channels { set, .. } => {
                    assert_eq!(set.revision, 4);
                    assert_eq!(set.bindings[0].route, "oc_1");
                    assert!(
                        set.bindings[0].secret.is_empty(),
                        "the answer says what is bound, never what it is bound with"
                    );
                }
                other => panic!("unexpected answer: {other:?}"),
            }

            // A controller that has fallen behind must not be able to take a
            // machine's channels away from it.
            handle_request(
                &writer,
                &state,
                3,
                DaemonRequest::ChannelsPut {
                    set: ChannelSet::default(),
                },
            )
            .unwrap();
            assert!(matches!(answer(&mut client, 3), DaemonResponse::Ack));
            assert_eq!(ChannelSet::read(&state.paths.channels), set);

            fs::remove_dir_all(&state.paths.root).ok();
        }

        /// An agent's MCP surface is its own process, so a receipt has nowhere
        /// to rest but here, and nothing to do with it but hand it to the one
        /// dashboard that reads the chat.
        #[test]
        fn a_receipt_waits_here_for_a_dashboard_and_is_handed_over_once() {
            use crate::channel::ChannelReceipt;

            let (mut client, server) = UnixStream::pair().unwrap();
            let state = test_state("receipts");
            let writer = Arc::new(Mutex::new(server));
            let answer = |client: &mut UnixStream, id: u64| loop {
                let frame = Frame::read_from(client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == id {
                    return frame.decode_json::<DaemonResponse>().unwrap();
                }
            };
            let receipt = ChannelReceipt {
                channel: "lark-1".into(),
                message_id: "om_1".into(),
                machine: String::new(),
                session_id: "a7f3c1".into(),
                label: "lexer".into(),
            };

            handle_request(
                &writer,
                &state,
                1,
                DaemonRequest::ChannelSent {
                    receipt: receipt.clone(),
                },
            )
            .unwrap();
            assert!(matches!(answer(&mut client, 1), DaemonResponse::Ack));

            handle_request(&writer, &state, 2, DaemonRequest::ChannelsGet).unwrap();
            match answer(&mut client, 2) {
                DaemonResponse::Channels { receipts, .. } => assert_eq!(receipts, vec![receipt]),
                other => panic!("unexpected answer: {other:?}"),
            }
            // Taken, not copied: a second dashboard asking must not be told
            // about a reply the first one is already holding.
            handle_request(&writer, &state, 3, DaemonRequest::ChannelsGet).unwrap();
            match answer(&mut client, 3) {
                DaemonResponse::Channels { receipts, .. } => assert!(receipts.is_empty()),
                other => panic!("unexpected answer: {other:?}"),
            }

            fs::remove_dir_all(&state.paths.root).ok();
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
                None,
                None,
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
            // When it was put down, which is what orders an archive. Recorded
            // once: a later look at the same record must not restamp it.
            let put_down = archived.archived_at.expect("archived_at is recorded");
            assert!(put_down >= archived.created_at);
            session.mark_dead();
            assert_eq!(session.snapshot().archived_at, Some(put_down));
        }

        /// A question as the daemon sees it: the configured words, and the
        /// yes/no pair that makes them a question rather than prose.
        fn approval_screen(reason_word: &str) -> Vec<u8> {
            format!("\x1b[2J\x1b[H{reason_word} needed\n> 1. Yes\n  2. No").into_bytes()
        }

        #[test]
        fn a_quiet_pty_stops_counting_as_working_and_sunk_patterns_classify_waiting() {
            let state = test_state("freshness");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-codex-freshness".into(),
                "codex".into(),
                "/tmp".into(),
                "freshness".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            session.record_output("\x1b[2J\x1b[H• Working (2s • esc to interrupt)".as_bytes());
            assert!(session.snapshot().working);

            // An interrupt hint is painted once and held for the whole turn,
            // so a quiet PTY under one says nothing: a turn that shells out to
            // a build sounds exactly like this, and calling it stopped is how
            // a working agent came to report itself finished.
            session.last_output.store(
                now_ms().saturating_sub(WORKING_TICKING_QUIET_MS + 1),
                Ordering::Relaxed,
            );
            assert!(session.snapshot().working);

            // Quiet for long enough, though, and what is on screen is a frame
            // frozen by a turn that ended or wedged.
            session.last_output.store(
                now_ms().saturating_sub(WORKING_HELD_QUIET_MS + 1),
                Ordering::Relaxed,
            );
            assert!(!session.snapshot().working);
            session.last_output.store(now_ms(), Ordering::Relaxed);
            assert!(session.snapshot().working);

            // A spinner and its counter are the other kind of marker: drawn
            // afresh about once a second, so one that has not moved in a
            // quarter of a minute is one nobody is drawing.
            session.record_output("\x1b[2J\x1b[H✻ Cogitating… (12s · ↓ 1.2k tokens)".as_bytes());
            assert!(session.snapshot().working);
            session.last_output.store(
                now_ms().saturating_sub(WORKING_TICKING_QUIET_MS + 1),
                Ordering::Relaxed,
            );
            assert!(!session.snapshot().working);
            session.last_output.store(now_ms(), Ordering::Relaxed);
            assert!(session.snapshot().working);

            // Patterns a controller sank down classify waiting on the
            // daemon's own snapshots, custom wording included. The reason
            // reads back as the question the screen is asking, which is what
            // the pattern matched inside.
            session.record_output(&approval_screen("gpu quota approval"));
            *state
                .attention_patterns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec!["gpu quota approval".into()];
            let snapshot = session.snapshot();
            assert!(snapshot.needs_attention);
            assert_eq!(
                snapshot.attention_reason.as_deref(),
                Some("gpu quota approval needed")
            );

            session.archive().unwrap();
            fs::remove_dir_all(root).unwrap();
        }

        /// Launch a quiet `/bin/cat` child under a parent, for tests about
        /// what the child's screen means to the agent that started it.
        fn launch_child_with_parent(
            state: &Arc<DaemonState>,
            name: &str,
            parent: Option<&str>,
        ) -> Arc<ManagedSession> {
            launch_session(
                state,
                format!("muxloomd-codex-{name}"),
                "codex".into(),
                "/tmp".into(),
                name.into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                parent.map(str::to_string),
                None,
            )
            .unwrap()
        }

        /// A client that stopped reading is let go of, rather than taking the
        /// session down with it. The thread writing these frames is the thread
        /// draining the session's keeper, and behind the keeper is the PTY: a
        /// dashboard suspended mid-attach fills its socket, and with nothing to
        /// time the write out the agent ends up blocked on its own output.
        #[test]
        fn a_client_that_stopped_reading_is_let_go_of_rather_than_wedging_the_session() {
            let state = test_state("broadcast-wedge");
            let session = launch_child_with_parent(&state, "wedged", None);
            let (mut client, server) = UnixStream::pair().unwrap();
            // serve_client sets this on every connection; a short one here so
            // the test does not sit out the real window.
            server
                .set_write_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            // Held here as serve_client holds it: the connection carries this
            // client's other streams and its request answers too, so letting
            // the subscription go does not on its own close anything.
            let connection = Arc::new(Mutex::new(server));
            session
                .subscribers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    1,
                    Subscriber {
                        stream_id: 7,
                        writer: Arc::clone(&connection),
                    },
                );

            // Nobody ever reads `client`. Write until the socket will take no
            // more, which on a real link is a dashboard that stopped keeping up.
            let payload = vec![b'x'; DATA_CHUNK_SIZE];
            let started = Instant::now();
            let attached = || {
                !session
                    .subscribers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_empty()
            };
            for _ in 0..256 {
                if !attached() {
                    break;
                }
                session.broadcast(&payload);
            }
            assert!(
                !attached(),
                "a client taking nothing at all has to be let go of"
            );
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "broadcasting must not wait on a client indefinitely"
            );

            // And the connection went with it: everything else sharing that
            // socket would read the tail of the half-written frame as a header.
            client
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut sink = [0_u8; 64 * 1024];
            let closed = loop {
                match client.read(&mut sink) {
                    Ok(0) => break true,
                    Ok(_) => continue,
                    Err(_) => break false,
                }
            };
            assert!(closed, "a connection written half a frame has to be closed");
            drop(connection);
        }

        /// Rewind the current edge's last tell so the next reminder comes due
        /// without the test waiting real minutes for it.
        fn age_parent_alert(child: &ManagedSession, ms: u64) {
            if let Some(edge) = child
                .alert_edge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_mut()
            {
                edge.last_claimed_at = edge.last_claimed_at.saturating_sub(ms);
            }
        }

        /// Read the child as free for long enough that what it is waiting on
        /// afterwards counts as a different question, without the test standing
        /// there for a minute. One pass says it stopped waiting; the rewind is
        /// the minute it then spent not waiting.
        fn settle_parent_alert(child: &ManagedSession) {
            let mut free = child.snapshot();
            free.needs_attention = false;
            free.attention_reason = None;
            child.note_parent_alert(&free);
            if let Some(edge) = child
                .alert_edge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_mut()
            {
                edge.free_since = edge.free_since.saturating_sub(PARENT_ALERT_SETTLE_MS + 1);
            }
        }

        /// What the parent depends on: one tell per question, widening
        /// reminders (60s, 5min, 15min) while it sits unanswered, then silence
        /// until the question actually changes - so one stuck screen, least
        /// of all a wrongly-classified one, cannot become a minute-ticker.
        #[test]
        fn a_waiting_child_is_told_about_once_and_the_reminders_widen_to_silence() {
            let state = test_state("parent-edge");
            let root = state.paths.root.clone();
            *state
                .attention_patterns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec!["gpu quota approval".into(), "second opinion".into()];
            let child = launch_child_with_parent(&state, "parent-edge", Some("the-parent"));
            let note = |child: &ManagedSession| {
                let snapshot = child.snapshot();
                child.note_parent_alert(&snapshot);
            };

            // An idle child with an idle screen says nothing to its parent.
            child.record_output(b"\x1b[2J\x1b[Hready when you are");
            note(&child);
            assert!(child.take_parent_alert().is_none());

            // A working one says less: the edge is the fall onto a question,
            // not the child being busy. The bullet and middle dot are spelled
            // as their UTF-8 bytes because byte strings take no \u escapes.
            child
                .record_output(b"\x1b[2J\x1b[H\xe2\x80\xa2 Working (2s \xc2\xb7 esc to interrupt)");
            assert!(child.snapshot().working);
            note(&child);
            assert!(child.take_parent_alert().is_none());

            // The moment it stops working and asks for approval, the first
            // tell is immediate, and it says who is waiting and why.
            child.record_output(&approval_screen("gpu quota approval"));
            note(&child);
            let alert = child.take_parent_alert().expect("the edge is handed over");
            assert_eq!(alert.session_id, "muxloomd-codex-parent-edge");
            assert_eq!(alert.parent_session_id, "the-parent");
            assert_eq!(alert.kind, "codex");
            assert_eq!(
                alert.attention_reason.as_deref(),
                Some("gpu quota approval needed")
            );

            // The same question afterwards: nothing in the same breath, the
            // first reminder at a minute, the second not before five.
            note(&child);
            assert!(child.take_parent_alert().is_none());
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[0] + 1);
            note(&child);
            assert!(child.take_parent_alert().is_some(), "first reminder at 60s");
            // The margins are minutes wide, not a millisecond: the test's own
            // clock has to fit inside the "not yet" one.
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[1] - 60_000);
            note(&child);
            assert!(
                child.take_parent_alert().is_none(),
                "the second reminder waits the full five minutes"
            );
            age_parent_alert(&child, 60_001);
            note(&child);
            assert!(child.take_parent_alert().is_some(), "second at 5min");
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[2] + 1);
            note(&child);
            assert!(child.take_parent_alert().is_some(), "third at 15min");

            // Ceiling: the first tell and three reminders is all the same
            // question ever gets. An hour later, still silence - and a
            // classifier blinking off and back onto the same question does
            // not hand the ticker back.
            age_parent_alert(&child, 3_600_000);
            note(&child);
            assert!(
                child.take_parent_alert().is_none(),
                "four tells is the leash"
            );
            let mut blink = child.snapshot();
            blink.needs_attention = false;
            child.note_parent_alert(&blink);
            age_parent_alert(&child, 3_600_000);
            note(&child);
            assert!(
                child.take_parent_alert().is_none(),
                "an attention flicker resets nothing"
            );

            // Silence means *until the child gets to a different question*,
            // and getting to one means having stopped waiting on the way. A key
            // changing under a child that has been waiting all along is the
            // reading changing: last words the child never had, and last words
            // that read differently while the same dialog sits there.
            let mut moved = child.snapshot();
            moved.recap = Some("waiting on the gpu quota".into());
            child.note_parent_alert(&moved);
            assert!(
                child.take_parent_alert().is_none(),
                "last words merely turning up are not a new question"
            );
            moved.recap = Some("but this line is new".into());
            child.note_parent_alert(&moved);
            assert!(
                child.take_parent_alert().is_none(),
                "nor are they, read differently, under a child that never moved"
            );

            // Free for a minute, and then the same words are a new question:
            // this is the door the schedule is meant to reopen, and the only
            // one.
            settle_parent_alert(&child);
            moved.recap = Some("and now it is asking something else".into());
            child.note_parent_alert(&moved);
            assert!(
                child.take_parent_alert().is_some(),
                "a new question after a settled stretch is told about at once"
            );
            settle_parent_alert(&child);
            child.record_output(&approval_screen("second opinion"));
            note(&child);
            let alert = child.take_parent_alert().expect("new reason is a new edge");
            assert_eq!(
                alert.attention_reason.as_deref(),
                Some("second opinion needed")
            );

            // A child nobody started is no one's errand: the same fall marks
            // nothing for no parent to hear about.
            let orphan = launch_child_with_parent(&state, "parent-edge-orphan", None);
            orphan.record_output(&approval_screen("gpu quota approval"));
            let snapshot = orphan.snapshot();
            assert!(snapshot.needs_attention);
            orphan.note_parent_alert(&snapshot);
            assert!(orphan.take_parent_alert().is_none());

            child.archive().unwrap();
            orphan.archive().unwrap();
            discard_root(root);
        }

        /// The leash holds through a *missing* reading, which is the way it
        /// actually gets tested. Last words are scraped, and scraping fails
        /// where the question does not: the runtime drops and retakes its
        /// transcript claim, a modal covers the last thing said. Counting
        /// either blink as a new question restarts the schedule at zero with
        /// the first tell due at once, and a child parked on one dialog for an
        /// afternoon becomes a nag a minute at its parent - the exact failure
        /// the widening reminders exist to prevent, arriving through the door
        /// marked "told about at once".
        #[test]
        fn last_words_going_missing_and_coming_back_is_not_a_new_question() {
            let state = test_state("parent-edge-blink");
            let root = state.paths.root.clone();
            *state
                .attention_patterns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec!["gpu quota approval".into()];
            let told = |child: &ManagedSession, recap: Option<&str>| {
                let mut snapshot = child.snapshot();
                snapshot.recap = recap.map(str::to_string);
                child.note_parent_alert(&snapshot);
                child.take_parent_alert().is_some()
            };

            // A question that arrives with its last words attached, and a
            // reading that then blinks out and back while nothing moves.
            let child = launch_child_with_parent(&state, "parent-edge-blink", Some("the-parent"));
            child.record_output(&approval_screen("gpu quota approval"));
            assert!(told(&child, Some("waiting on the gpu quota")), "first tell");
            assert!(!told(&child, None), "last words going missing is not news");
            assert!(
                !told(&child, Some("waiting on the gpu quota")),
                "nor is them coming back"
            );

            // And the blinking has not refilled the tank behind itself: the
            // reminders come due where they always would have, and the fourth
            // tell is still the last one.
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[0] + 1);
            assert!(told(&child, None), "first reminder, on the same schedule");
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[1] + 1);
            assert!(told(&child, Some("waiting on the gpu quota")), "second");
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[2] + 1);
            assert!(told(&child, None), "third");
            age_parent_alert(&child, 3_600_000);
            assert!(
                !told(&child, Some("waiting on the gpu quota")),
                "four tells is still the leash"
            );

            // The other direction: an edge installed while the reading was
            // missing learns it when it turns up - quietly, owing nothing for
            // the arrival - and that is what lets a real change afterwards be
            // measured against real last words instead of against a blank.
            let late = launch_child_with_parent(&state, "parent-edge-late", Some("the-parent"));
            late.record_output(&approval_screen("gpu quota approval"));
            assert!(told(&late, None), "the first tell, last words or not");
            assert!(
                !told(&late, Some("waiting on the gpu quota")),
                "the reading turning up is not a new question"
            );
            age_parent_alert(&late, PARENT_ALERT_REMINDERS_MS[0] + 1);
            assert!(told(&late, Some("waiting on the gpu quota")), "reminder");
            settle_parent_alert(&late);
            assert!(
                told(&late, Some("now waiting on a second opinion")),
                "a real change over a settled child is still told about at once"
            );

            child.archive().unwrap();
            late.archive().unwrap();
            discard_root(root);
        }

        /// The way this actually failed in the fleet, which the missing-reading
        /// test above does not cover: the last words are read out of the
        /// runtime's transcript when it has a claim on one and off the screen
        /// when it does not, and those two do not agree. A claim dropped and
        /// retaken swaps one answer for the other and back - both of them
        /// there, neither of them empty, the child never having moved off the
        /// permission dialog it has been parked on all afternoon.
        ///
        /// Counting each swap as a new question is a fresh schedule every time,
        /// each with its first tell due at once. One OpenCode session sitting
        /// on one dialog told its parent about itself fifty times in twelve
        /// hours that way, every message word for word the same as the last.
        #[test]
        fn last_words_changing_hands_between_their_two_sources_is_not_a_new_question() {
            let state = test_state("parent-edge-sources");
            let root = state.paths.root.clone();
            *state
                .attention_patterns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec!["gpu quota approval".into()];
            let told = |child: &ManagedSession, recap: &str| {
                let mut snapshot = child.snapshot();
                snapshot.recap = Some(recap.to_string());
                child.note_parent_alert(&snapshot);
                child.take_parent_alert().is_some()
            };
            // What the transcript says the session last said, and what can be
            // scraped off the same session's screen. Both are true; they are
            // answers to slightly different questions.
            let claimed = "let me inspect the environment first";
            let scraped = "$ venv/bin/pip list | grep -iE mlx";

            let child = launch_child_with_parent(&state, "parent-edge-sources", Some("the-parent"));
            child.record_output(&approval_screen("gpu quota approval"));
            assert!(told(&child, claimed), "first tell");
            assert!(
                !told(&child, scraped),
                "the claim dropping is not the child moving"
            );
            assert!(!told(&child, claimed), "nor is the claim coming back again");

            // Twelve hours of that, at the rate a dashboard and a room full of
            // MCP clients ask: the swap is the only thing happening, and the
            // schedule is the only thing that may speak.
            for _ in 0..64 {
                assert!(!told(&child, scraped));
                assert!(!told(&child, claimed));
            }
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[0] + 1);
            assert!(told(&child, scraped), "first reminder, at a minute");
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[1] + 1);
            assert!(told(&child, claimed), "second, at five");
            age_parent_alert(&child, PARENT_ALERT_REMINDERS_MS[2] + 1);
            assert!(told(&child, scraped), "third, at fifteen");
            age_parent_alert(&child, 86_400_000);
            for _ in 0..64 {
                assert!(!told(&child, claimed), "four tells is the whole leash");
                assert!(!told(&child, scraped), "four tells is the whole leash");
            }

            child.archive().unwrap();
            discard_root(root);
        }

        /// Weighing whether one agent may write into another is a question
        /// about parent links, and it is asked before every message and every
        /// keystroke one sends the other. It used to be asked with
        /// `ListSessions`, which draws every screen on the machine and carries
        /// back every conversation it has ever held, to read two fields off
        /// each record and throw the rest away.
        ///
        /// The archive is in the answer on purpose: the chain from a live
        /// session up to the one that started it can pass through an ancestor
        /// that has since been put down, and a lineage missing that link reads
        /// as "somebody else's session" and refuses a message that is allowed.
        #[test]
        fn a_lineage_round_carries_the_parent_links_and_not_the_sessions() {
            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(server));
            let answer = |client: &mut UnixStream, id: u64| loop {
                let frame = Frame::read_from(client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == id {
                    return frame.decode_json::<DaemonResponse>().unwrap();
                }
            };

            // An ancestor that was put down before this daemon started, which
            // is the only way a record reaches the archive index rather than
            // being adopted.
            let filed = "muxloomd-terminal-lineage-filed";
            let staging = test_state("lineage-stage");
            let elder = launch_session(
                &staging,
                filed.into(),
                "terminal".into(),
                "/tmp".into(),
                "the elder".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                111,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            elder.archive().unwrap();
            let record = elder.snapshot();
            drop(elder);

            let state = test_state("lineage");
            persist_session_metadata(&state.paths.sessions.join(format!("{filed}.json")), &record)
                .unwrap();
            fs::write(state.paths.history.join(format!("{filed}.ansi")), b"").unwrap();
            let root = state.paths.root.clone();
            let state = Arc::new(DaemonState::new(state.paths.clone(), KeeperMode::InProcess));

            let child = launch_child_with_parent(&state, "lineage-child", Some(filed));
            let orphan = launch_child_with_parent(&state, "lineage-orphan", None);

            // A client too old to know the question falls back to the whole
            // list, and it decides which by this string: a daemon that answers
            // `Lineage` without saying so leaves every client paying the old
            // price forever.
            handle_request(
                &writer,
                &state,
                0,
                DaemonRequest::Hello {
                    client_version: env!("CARGO_PKG_VERSION").into(),
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .unwrap();
            let DaemonResponse::Hello { capabilities, .. } = answer(&mut client, 0) else {
                panic!("a hello must answer with a hello");
            };
            assert!(
                capabilities.iter().any(|it| it == LINEAGE_CAPABILITY),
                "the daemon must say it can answer a lineage round: {capabilities:?}"
            );

            handle_request(&writer, &state, 1, DaemonRequest::Lineage).unwrap();
            let DaemonResponse::Parents { parents } = answer(&mut client, 1) else {
                panic!("a lineage round must answer with parents");
            };
            let parent_of = |id: &str| {
                parents
                    .iter()
                    .find(|(session, _)| session == id)
                    .unwrap_or_else(|| panic!("{id} is missing from the lineage: {parents:?}"))
                    .1
                    .clone()
            };
            assert_eq!(
                parent_of(&child.snapshot().id),
                Some(filed.to_string()),
                "a live session's link to the one that started it"
            );
            assert_eq!(
                parent_of(filed),
                None,
                "the archived ancestor is in the answer, or the chain through it breaks"
            );
            assert_eq!(parent_of(&orphan.snapshot().id), None);

            child.archive().unwrap();
            orphan.archive().unwrap();
            discard_root(staging.paths.root.clone());
            discard_root(root);
        }

        /// Searching the machine is one round, and it names the sessions it
        /// found for itself.
        ///
        /// Asking per session meant fetching the list first - which draws every
        /// live screen to answer - and then one round trip per capture, for
        /// hundreds of sessions that mostly do not hold the word. The names
        /// have to come back with the matches or the caller needs that list
        /// anyway and nothing is saved.
        #[test]
        fn a_history_search_round_asks_every_capture_at_once_and_names_what_it_found() {
            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(server));
            let answer = |client: &mut UnixStream, id: u64| loop {
                let frame = Frame::read_from(client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == id {
                    return frame.decode_json::<DaemonResponse>().unwrap();
                }
            };
            let append = |path: PathBuf, line: &str| {
                // Appended rather than written: a live session's keeper holds
                // the same file open, and truncating it under the keeper is not
                // what a capture growing looks like.
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(line.as_bytes())
                    .unwrap();
            };

            // A session put down before this daemon started, which is the only
            // way a record reaches the archive index. The archive is most of
            // what a machine has ever held, so a search that skipped it would
            // answer about the last few hours only.
            let filed = "muxloomd-terminal-search-filed";
            let staging = test_state("search-all-stage");
            let elder = launch_session(
                &staging,
                filed.into(),
                "terminal".into(),
                "/tmp".into(),
                "the filed one".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                222,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            elder.archive().unwrap();
            let record = elder.snapshot();
            drop(elder);

            let state = test_state("search-all");
            persist_session_metadata(&state.paths.sessions.join(format!("{filed}.json")), &record)
                .unwrap();
            fs::write(
                state.paths.history.join(format!("{filed}.ansi")),
                b"an anchovy was filed here\n",
            )
            .unwrap();
            let root = state.paths.root.clone();
            let state = Arc::new(DaemonState::new(state.paths.clone(), KeeperMode::InProcess));

            let holder = launch_child_with_parent(&state, "search-holder", None);
            let quiet = launch_child_with_parent(&state, "search-quiet", None);
            append(
                state
                    .paths
                    .history
                    .join(format!("{}.ansi", holder.snapshot().id)),
                "an anchovy swam past\n",
            );
            append(
                state
                    .paths
                    .history
                    .join(format!("{}.ansi", quiet.snapshot().id)),
                "nothing of the sort\n",
            );

            // A temporary session leaves no transcript by design, so a capture
            // sitting at its name is not the machine's history and is not
            // searched. Written anyway, because the skip has to be the reason
            // it stays out of the answer.
            let fleeting = launch_session(
                &state,
                "muxloomd-codex-search-fleeting".into(),
                "codex".into(),
                "/tmp".into(),
                "the fleeting one".into(),
                true,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            append(
                state
                    .paths
                    .history
                    .join(format!("{}.ansi", fleeting.snapshot().id)),
                "an anchovy in passing\n",
            );

            // A client too old to know the question walks the sessions itself,
            // and it decides which by this string: a daemon that answers
            // without saying so leaves every client paying the old price.
            handle_request(
                &writer,
                &state,
                0,
                DaemonRequest::Hello {
                    client_version: env!("CARGO_PKG_VERSION").into(),
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .unwrap();
            let DaemonResponse::Hello { capabilities, .. } = answer(&mut client, 0) else {
                panic!("a hello must answer with a hello");
            };
            assert!(
                capabilities
                    .iter()
                    .any(|it| it == HISTORY_SEARCH_CAPABILITY),
                "the daemon must say it can search every capture at once: {capabilities:?}"
            );

            handle_request(
                &writer,
                &state,
                1,
                DaemonRequest::SearchHistoryAll {
                    query: "anchovy".into(),
                    max_matches: 10,
                    deep: false,
                },
            )
            .unwrap();
            let DaemonResponse::HistorySearch {
                hits,
                searched,
                skipped,
            } = answer(&mut client, 1)
            else {
                panic!("a history search must answer with hits");
            };
            assert_eq!(
                (searched, skipped),
                (3, 0),
                "three captures kept between them, and a near search reaches all of \
                 them when they are this small"
            );
            let found: Vec<(String, String)> = hits
                .iter()
                .map(|hit| (hit.session_id.clone(), hit.label.clone()))
                .collect();
            assert_eq!(
                found,
                vec![
                    (holder.snapshot().id, "search-holder".into()),
                    (filed.into(), "the filed one".into()),
                ],
                "the live session then the archived one, each carrying its label"
            );
            assert_eq!(
                hits[1].matches.first().map(|item| item.text.as_str()),
                Some("an anchovy was filed here"),
                "the matching line comes back with the hit"
            );

            holder.archive().unwrap();
            quiet.archive().unwrap();
            // A temporary session is never filed, so it is stopped instead.
            fleeting.stop().unwrap();
            discard_root(staging.paths.root.clone());
            discard_root(root);
        }

        #[test]
        fn a_near_search_keeps_the_captures_written_last_and_counts_what_it_dropped() {
            let state = test_state("search-pool");
            let root = state.paths.root.clone();
            let capture = |name: &str, bytes: usize| {
                let path = state.paths.history.join(format!("{name}.ansi"));
                fs::write(&path, "x".repeat(bytes)).unwrap();
                // Far enough apart that no filesystem's timestamp resolution
                // can call two of these the same moment.
                thread::sleep(Duration::from_millis(10));
                (name.to_string(), name.to_string(), path)
            };
            // Written oldest first, and handed over in that order too, so that
            // keeping the right two proves the sort and not the input order.
            let mut captures = vec![
                capture("stale", 100),
                capture("older", 100),
                capture("newest", 100),
            ];

            let skipped = narrow_to_pool(&mut captures, 150);

            assert_eq!(
                captures
                    .iter()
                    .map(|(id, ..)| id.as_str())
                    .collect::<Vec<_>>(),
                vec!["newest", "older"],
                "newest written first, and only as far as the budget reaches"
            );
            assert_eq!(skipped, 1, "the capture left unread is counted, not hidden");

            // The newest is read even when it alone is over budget: a pool that
            // can come back empty is worse than one that reads too much.
            let mut lone = vec![capture("enormous", 100)];
            assert_eq!(narrow_to_pool(&mut lone, 1), 0);
            assert_eq!(lone.len(), 1, "the first capture is never the one skipped");

            discard_root(root);
        }

        /// The other half of one round: whoever lists the sessions runs the
        /// edge check, and the one `DrainAlerts` hands the marked edges over -
        /// then forgets them, the way receipts are handed over.
        #[test]
        fn a_controller_round_collects_the_waiting_children_it_asked_for_once() {
            let (mut client, server) = UnixStream::pair().unwrap();
            let state = test_state("drain-alerts");
            let writer = Arc::new(Mutex::new(server));
            let answer = |client: &mut UnixStream, id: u64| loop {
                let frame = Frame::read_from(client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == id {
                    return frame.decode_json::<DaemonResponse>().unwrap();
                }
            };
            *state
                .attention_patterns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec!["gpu quota approval".into()];
            let child = launch_child_with_parent(&state, "drain-alerts", Some("the-parent"));
            child.record_output(&approval_screen("gpu quota approval"));
            assert!(child.snapshot().needs_attention);

            // A ListSessions is what marks it - the dashboard's own poll does
            // not take it away from the controller that will deliver it.
            handle_request(
                &writer,
                &state,
                1,
                DaemonRequest::ListSessions {
                    live_only: true,
                    only: None,
                },
            )
            .unwrap();
            assert!(matches!(
                answer(&mut client, 1),
                DaemonResponse::Sessions { .. }
            ));
            assert!(child.alert_pending.load(Ordering::Relaxed));
            handle_request(
                &writer,
                &state,
                2,
                DaemonRequest::ListSessions {
                    live_only: true,
                    only: None,
                },
            )
            .unwrap();
            assert!(matches!(
                answer(&mut client, 2),
                DaemonResponse::Sessions { .. }
            ));

            handle_request(&writer, &state, 3, DaemonRequest::DrainAlerts).unwrap();
            match answer(&mut client, 3) {
                DaemonResponse::Alerts { alerts } => {
                    assert_eq!(alerts.len(), 1);
                    assert_eq!(alerts[0].session_id, "muxloomd-codex-drain-alerts");
                    assert_eq!(alerts[0].parent_session_id, "the-parent");
                }
                other => panic!("unexpected answer: {other:?}"),
            }
            // Handed over, so gone: the next ask hears nothing until the
            // reminder schedule marks the still-unanswered question again.
            handle_request(&writer, &state, 4, DaemonRequest::DrainAlerts).unwrap();
            match answer(&mut client, 4) {
                DaemonResponse::Alerts { alerts } => assert!(alerts.is_empty()),
                other => panic!("unexpected answer: {other:?}"),
            }

            child.archive().unwrap();
            fs::remove_dir_all(&state.paths.root).ok();
        }

        /// A session records when it was launched in seconds; a transcript
        /// stamps itself in milliseconds. Handed to the matching as they are,
        /// every launch looks like it happened decades before every
        /// conversation in the folder, and "the transcript that began nearest
        /// this launch" quietly becomes "the oldest transcript here" - a fresh
        /// agent listed under a name and a recap belonging to work somebody
        /// did last month.
        #[test]
        fn a_launch_is_matched_against_transcripts_on_a_transcripts_own_clock() {
            fn thread(id: &str, started_at: u64) -> crate::native_history::NativeThread {
                crate::native_history::NativeThread {
                    id: id.into(),
                    path: PathBuf::from(format!("/tmp/{id}.jsonl")),
                    cwd: "/work".into(),
                    started_at,
                    updated_at: started_at,
                    forked_from: None,
                    title: None,
                    last_message: None,
                    first_message: None,
                }
            }

            // The launch as a session records it, and the same instant as a
            // transcript would stamp it.
            let launched_seconds = 1_787_649_863;
            let launched_ms = 1_787_649_863_000;
            let facts = [NativeFacts {
                created_at: launched_at_ms(launched_seconds),
                ..NativeFacts::default()
            }];
            let threads = [
                // Started a moment after the launch: this session's own.
                thread("its-own", launched_ms + 1_200),
                thread("last-month", launched_ms - 30 * 86_400_000),
            ];
            assert_eq!(
                crate::native_history::assign_threads(&facts, &threads),
                [Some(0)]
            );
        }

        /// The runtime's own account of the turn is what the session is
        /// listed by, and it is read again only when it has been added to.
        #[test]
        fn a_session_takes_its_name_and_its_recap_from_the_transcript_it_writes() {
            let state = test_state("native-read");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-claude-native-read".into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            let transcript = root.join("thread-1.jsonl");
            fs::write(
                &transcript,
                concat!(
                    r#"{"type":"ai-title","aiTitle":"an older name"}"#,
                    "\n",
                    r#"{"type":"ai-title","aiTitle":"the pty reader"}"#,
                    "\n",
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done, and the tests pass."}]}}"#,
                    "\n",
                ),
            )
            .unwrap();
            let written = crate::native_history::last_written(&transcript).unwrap();
            {
                let mut native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                native.claim = Some(NativeClaim {
                    id: "thread-1".into(),
                    path: transcript.clone(),
                    // Older than the file, so this round has something to read.
                    read_at: written.saturating_sub(1),
                    title: None,
                    recap: None,
                });
                native.scanned_at = written;
            }
            session.last_output.store(written, Ordering::Relaxed);

            assert!(!refresh_native_claim(AgentKind::Claude, &session));
            let snapshot = session.snapshot();
            assert_eq!(snapshot.title.as_deref(), Some("the pty reader"));
            assert_eq!(snapshot.recap.as_deref(), Some("Done, and the tests pass."));
            assert_eq!(snapshot.thread.as_deref(), Some("thread-1"));
            // Persisted with it, so a daemon that restarts goes on reading the
            // same conversation instead of matching it again from scratch.
            let stored: DaemonSession =
                serde_json::from_slice(&fs::read(&session.metadata_path).unwrap()).unwrap();
            assert_eq!(stored.thread.as_deref(), Some("thread-1"));
            assert_eq!(stored.title.as_deref(), Some("the pty reader"));

            // Nothing has been added to it, so nothing is read.
            fs::write(&transcript, "").unwrap();
            assert!(!refresh_native_claim(AgentKind::Claude, &session));
            assert_eq!(
                session.snapshot().title.as_deref(),
                Some("the pty reader"),
                "a transcript that has not grown is not read again"
            );

            session.archive().unwrap();
            fs::remove_dir_all(root).unwrap();
        }

        /// The matching can only tell a claim from a guess if the facts it is
        /// handed carry what the session was asked to open with - the live
        /// recording of this generation, the persisted one after a restart.
        #[test]
        fn the_matching_is_told_what_the_session_was_asked() {
            let state = test_state("native-facts");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-claude-native-facts".into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            assert_eq!(
                session_facts(1, &session).first_prompt,
                None,
                "nothing has been heard yet"
            );
            *session
                .first_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some("fix the render glitch in the pty reader".into());
            assert_eq!(
                session_facts(1, &session).first_prompt.as_deref(),
                Some("fix the render glitch in the pty reader"),
                "the live recording reaches the matching"
            );

            session.archive().unwrap();
            fs::remove_dir_all(root).unwrap();
        }

        /// A second launch into the same folder is a second conversation, not
        /// an echo of the first: the opening line is recorded once, from the
        /// first payload the session is ever given, and what a restart brings
        /// back is the recording - never a later payload dressed as it.
        #[test]
        fn the_first_thing_a_session_is_asked_is_recorded_once_and_kept() {
            let state = test_state("native-record");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-claude-native-record".into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            session
                .write_input(b"fix the render glitch in the pty reader\r")
                .unwrap();
            assert_eq!(
                session
                    .first_prompt
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_deref(),
                Some("fix the render glitch in the pty reader"),
                "the opening is heard and kept"
            );
            // Persisted, so the next daemon generation matches against the
            // same opening the first one heard.
            let stored: DaemonSession =
                serde_json::from_slice(&fs::read(&session.metadata_path).unwrap()).unwrap();
            assert_eq!(
                stored.first_prompt.as_deref(),
                Some("fix the render glitch in the pty reader")
            );

            session
                .write_input(b"and now a second, quite different sentence\r")
                .unwrap();
            assert_eq!(
                session
                    .first_prompt
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_deref(),
                Some("fix the render glitch in the pty reader"),
                "the second payload is somebody else's sentence"
            );

            // A session whose real opening was a keystroke burst too small to
            // be a sentence keeps its opening unknown: the next substantial
            // payload came after the conversation started and must not be
            // recorded in its place.
            let burst = launch_session(
                &state,
                "muxloomd-claude-native-burst".into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            burst.write_input(b"hi\r").unwrap();
            burst
                .write_input(b"a longer payload, arriving only second\r")
                .unwrap();
            assert_eq!(
                burst
                    .first_prompt
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_deref(),
                None,
                "a missed opening is not invented from a later input"
            );

            session.archive().unwrap();
            burst.archive().unwrap();
            fs::remove_dir_all(root).ok();
        }

        /// A claim taken on timing - which is how a crossed pair is made -
        /// asks for the folder to be looked through until the first words
        /// settle it, and stops asking once settled or out of budget.
        #[test]
        fn an_unweighed_claim_asks_for_the_folder_a_bounded_number_of_times() {
            let state = test_state("native-check");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-claude-native-check".into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            {
                let mut native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                native.claim = Some(NativeClaim {
                    id: "maybe-mine".into(),
                    path: root.join("maybe-mine.jsonl"),
                    // Nothing new to read; only the check can ask for a look.
                    read_at: u64::MAX,
                    title: None,
                    recap: None,
                });
            }
            *session
                .first_prompt
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some("fix the render glitch in the pty reader".into());
            session.last_output.store(now_ms(), Ordering::Relaxed);

            for _ in 0..NATIVE_CLAIM_CHECK_LOOKS {
                assert!(
                    refresh_native_claim(AgentKind::Claude, &session),
                    "an unchecked claim asks the folder once more"
                );
            }
            assert!(
                !refresh_native_claim(AgentKind::Claude, &session),
                "and stops asking once the budget is spent"
            );

            // A claim the first words have agreed with stops asking at once.
            {
                let mut native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                native.claim_checked = true;
            }
            assert!(!refresh_native_claim(AgentKind::Claude, &session));

            session.archive().unwrap();
            // The keeper may still be closing its history file; removing the
            // scratch root is best-effort, as elsewhere in these tests.
            fs::remove_dir_all(root).ok();
        }

        /// Put a file's modification time where a test needs it, so a
        /// transcript can be one that stopped growing a while ago.
        fn set_modified(path: &std::path::Path, epoch_ms: u64) {
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(epoch_ms);
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(when))
                .unwrap();
        }

        /// A conversation cleared with `/clear` is closed where it stands and
        /// a new file begins. Nothing says so; the session simply goes on
        /// talking into a transcript that is not the one it was matched to.
        ///
        /// The telling part is what it takes to be sure of that, because an
        /// agent that is only thinking looks the same from the outside: it
        /// prints continuously and appends nothing.
        #[test]
        fn a_session_lets_go_of_a_transcript_it_has_talked_past() {
            let state = test_state("native-stale");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-claude-native-stale".into(),
                "claude".into(),
                "/tmp".into(),
                String::new(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            let transcript = root.join("thread-1.jsonl");
            fs::write(&transcript, "{}\n").unwrap();
            // The conversation was closed five minutes ago and the file has
            // not been touched since.
            let written = now_ms().saturating_sub(5 * 60_000);
            set_modified(&transcript, written);
            assert_eq!(
                crate::native_history::last_written(&transcript),
                Some(written)
            );
            {
                let mut native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                native.claim = Some(NativeClaim {
                    id: "thread-1".into(),
                    path: transcript.clone(),
                    read_at: written,
                    title: Some("the conversation before".into()),
                    recap: Some("what it said last".into()),
                });
                native.scanned_at = written;
            }

            // Thinking: nothing has been asked since the file last grew, and
            // the spinner has been printing the whole time.
            session.last_output.store(now_ms(), Ordering::Relaxed);
            assert!(
                !refresh_native_claim(AgentKind::Claude, &session),
                "a session that has only just spoken is still on its thread"
            );
            assert_eq!(
                session.snapshot().title.as_deref(),
                Some("the conversation before")
            );

            // Still thinking, and long enough that the answer is well past the
            // minute the transcript has been quiet for. This is the reading
            // that used to take a working session's name away from it.
            session
                .last_input
                .store(written.saturating_sub(60_000), Ordering::Relaxed);
            session.last_output.store(now_ms(), Ordering::Relaxed);
            assert!(
                !refresh_native_claim(AgentKind::Claude, &session),
                "an agent that is thinking out loud has not gone anywhere"
            );
            assert_eq!(
                session.snapshot().title.as_deref(),
                Some("the conversation before")
            );

            // Asked something after the file stopped growing, answered, and
            // quiet since: the words went into a transcript that is not this
            // one.
            session
                .last_input
                .store(written.saturating_add(1_000), Ordering::Relaxed);
            session
                .last_output
                .store(now_ms().saturating_sub(2 * 60_000), Ordering::Relaxed);
            assert!(
                refresh_native_claim(AgentKind::Claude, &session),
                "the folder has to be looked through for wherever it went"
            );
            {
                let native = session
                    .native
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                assert!(native.claim.is_none());
                assert_eq!(native.abandoned, vec!["thread-1".to_string()]);
            }
            let snapshot = session.snapshot();
            assert_eq!(
                snapshot.title, None,
                "that name belonged to a conversation that is over"
            );
            assert_eq!(snapshot.thread, None);

            session.archive().unwrap();
            fs::remove_dir_all(root).unwrap();
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
                None,
                None,
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

        /// A recap has to come off the screen the agent drew, not the bytes it
        /// sent to draw it. A full-screen agent positions the cursor for every
        /// line it paints, so the raw stream is one unbroken run with the
        /// status bar and every keystroke of the composer folded into it, and
        /// nothing read out of that is anything anybody said.
        #[test]
        fn a_recap_is_read_off_the_drawn_screen_and_stands_until_there_is_another() {
            let state = test_state("recap-screen");
            let root = state.paths.root.clone();
            let session = launch_session(
                &state,
                "muxloomd-claude-recap-screen".into(),
                "claude".into(),
                "/tmp".into(),
                "recap off the screen".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();

            session.record_output(
                concat!(
                    "\x1b[2J",
                    "\x1b[3;1H⏺ The renderer keeps its width across restarts.",
                    "\x1b[20;1H⏸ manual mode on · ? for shortcuts ◉ xhigh · /effort",
                    "\x1b[22;1H❯ ",
                )
                .as_bytes(),
            );
            assert_eq!(
                session.snapshot().recap.as_deref(),
                Some("The renderer keeps its width across restarts.")
            );

            // The answer scrolls away behind a long tool call. What is left on
            // screen is the frame around the conversation, and the frame is
            // not a new answer.
            session.record_output(
                concat!(
                    "\x1b[2J",
                    "\x1b[19;1H✻ Whirlpooling… (21s · ↓ 25 tokens · thought for 17s)",
                    "\x1b[20;1H⏸ manual mode on · ? for shortcuts ◉ xhigh · /effort",
                )
                .as_bytes(),
            );
            assert_eq!(
                session.snapshot().recap.as_deref(),
                Some("The renderer keeps its width across restarts."),
                "the last answer stands until the agent gives another"
            );

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
                None,
                None,
            )
            .unwrap();

            assert!(session.snapshot().temporary);
            assert!(!paths.history.join(format!("{session_id}.ansi")).exists());
            let history = session.read_history(0, 100, false).unwrap();
            assert!(history.rows.is_empty() && history.total_lines == 0);
            assert!(session.search_history("anything", 10).unwrap().is_empty());
            assert!(session.archive().is_err());

            session.stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            // Spun rather than slept: the point of the assertions below is that
            // the record is already gone the instant the session leaves the
            // map, and a poll that naps for 20ms hands a daemon that cleans up
            // in the other order 20ms of cover.
            while state.sessions.lock().unwrap().contains_key(session_id)
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
            assert!(!state.sessions.lock().unwrap().contains_key(session_id));
            assert!(!paths.sessions.join(format!("{session_id}.json")).exists());
            assert!(!paths.history.join(format!("{session_id}.ansi")).exists());
            discard_root(paths.root);
        }

        /// The muxloom that composes a launch can be older than the daemon that
        /// runs it - an upgrade replaces the daemon while every client already
        /// talking to it keeps the build it started with. A session started by
        /// one of those would sit on its runtime's first approval prompt with
        /// nobody in front of it, so the daemon settles the mode itself, and
        /// leaves alone both a mode already chosen and an executable that is
        /// not the runtime whose flags these are.
        #[test]
        fn a_daemon_starts_an_agent_unattended_even_when_its_client_did_not() {
            let state = test_state("unattended");
            let paths = state.paths.clone();
            let dir = paths.root.join("fake-runtime");
            fs::create_dir_all(&dir).unwrap();

            // Stands in for the agent CLI: writes down how it was called, then
            // sits quietly on the pty like any other session.
            let record_argv = |name: &str| {
                let recorded = dir.join(format!("{name}.argv"));
                let executable = dir.join(name);
                fs::write(
                    &executable,
                    format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexec cat\n",
                        recorded.display()
                    ),
                )
                .unwrap();
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
                (executable, recorded)
            };
            let start = |session_id: &str, executable: &Path, args: Vec<String>| {
                launch_session(
                    &state,
                    session_id.into(),
                    "claude".into(),
                    "/tmp".into(),
                    session_id.into(),
                    false,
                    executable.to_string_lossy().into_owned(),
                    args,
                    Vec::new(),
                    1,
                    80,
                    24,
                    None,
                    None,
                )
                .unwrap()
            };
            let argv = |recorded: &Path| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !recorded.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(20));
                }
                fs::read_to_string(recorded)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            };

            // An older client's command line: it chose a model, not a mode.
            let (executable, recorded) = record_argv("claude");
            let old = start(
                "muxloomd-claude-unattended-old",
                &executable,
                vec!["--model".into(), "opus".into()],
            );
            assert_eq!(
                argv(&recorded),
                ["--permission-mode", "auto", "--model", "opus"],
                "a client that named no mode must not leave the session waiting on a prompt"
            );
            old.stop().unwrap();

            // A mode the person chose is a decision, whoever passed it along.
            let (executable, recorded) = record_argv("claude");
            let _ = fs::remove_file(&recorded);
            let chosen = start(
                "muxloomd-claude-unattended-chosen",
                &executable,
                vec!["--permission-mode".into(), "plan".into()],
            );
            assert_eq!(argv(&recorded), ["--permission-mode", "plan"]);
            chosen.stop().unwrap();

            // These are Claude's own flags. Something else standing in its
            // place is not owed them and would not know what to do with them.
            let (executable, recorded) = record_argv("claude-wrapper");
            let wrapped = start(
                "muxloomd-claude-unattended-wrapped",
                &executable,
                vec!["--model".into(), "opus".into()],
            );
            assert_eq!(
                argv(&recorded),
                ["--model", "opus"],
                "a wrapper must be called the way it was configured"
            );
            wrapped.stop().unwrap();

            discard_root(paths.root);
        }

        #[test]
        fn a_temporary_session_runs_in_a_scratch_folder_that_dies_with_it() {
            let state = test_state("scratch");
            let paths = state.paths.clone();
            let session_id = "muxloomd-temporal-codex-scratch";
            let scratch = paths.scratch.join(session_id);
            // A folder the client named, and the folder the session actually
            // gets: a scratch chat never moves into the project it was started
            // from.
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
                None,
                None,
            )
            .unwrap();
            assert_eq!(session.snapshot().path, scratch.to_string_lossy());
            assert!(scratch.is_dir());

            // A leftover from a daemon that was killed outright is swept once
            // this generation knows which sessions it has -- and the folder of
            // a session it does have is not.
            let stale = paths.scratch.join("muxloomd-temporal-codex-gone");
            fs::create_dir_all(&stale).unwrap();
            sweep_scratch_dirs(&state);
            assert!(!stale.exists());
            assert!(scratch.is_dir(), "a live session keeps its folder");

            session.stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while state.sessions.lock().unwrap().contains_key(session_id)
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
            assert!(!scratch.exists(), "the folder ends with the session");
            discard_root(paths.root);
        }

        /// A delete has to outlast the writes that were already on their way.
        ///
        /// Nothing here walks the session map with the map lock in hand: the
        /// transcript scan, the session listing, a resume repointing a whole
        /// subtree - each takes the handles under the lock and does its work
        /// after letting go, because that work draws screens and syncs files.
        /// So a handle to a session outlives its removal from the map by
        /// however long the round it is part of takes, and a write landing
        /// after the delete writes the record back. Nothing looks at that file
        /// again to notice it should not be there; the next daemon reads it in
        /// at startup, and the conversation the person deleted is in their
        /// archive from then on.
        #[test]
        fn a_deleted_session_is_not_written_back_by_a_round_that_was_already_holding_it() {
            let state = test_state("delete-outlives");
            let paths = state.paths.clone();
            let session_id = "muxloomd-claude-deleted";
            let session = launch_session(
                &state,
                session_id.into(),
                "claude".into(),
                "/tmp".into(),
                "on its way out".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let record = paths.sessions.join(format!("{session_id}.json"));
            assert!(record.exists(), "a launch records what it started");

            // What the delete request does, in the order it does it: out of the
            // index the daemon finds sessions by, then off the disk.
            state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(session_id);
            session.discard();
            assert!(!record.exists());

            // And the round that took the handle a moment before finishes.
            session.persist_metadata().unwrap();
            assert!(
                !record.exists(),
                "the deleted session came back, and every daemon after this one reads it in"
            );
            session.stop().unwrap();

            // The archive's own records take the same care. A resume rewrites
            // the parent of every child the old master had, archived ones
            // included, and a delete can land in the middle of that walk.
            let archived_id = "muxloomd-claude-1700000222-9-1";
            let archived_path = paths.sessions.join(format!("{archived_id}.json"));
            let mut retired = live_metadata(archived_id, "claude", None);
            retired.dead = true;
            retired.archived = true;
            retired.archived_at = Some(1);
            retired.working = false;
            persist_session_metadata(&archived_path, &retired).unwrap();
            fs::write(paths.history.join(format!("{archived_id}.ansi")), b"done\n").unwrap();
            let restarted = DaemonState::new(paths.clone(), KeeperMode::InProcess);
            let entry = restarted
                .persisted_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(archived_id)
                .map(Arc::clone)
                .expect("the archived record has to be in the archive for this to say anything");
            entry.discard();
            assert!(!archived_path.exists());
            entry.persist(&retired).unwrap();
            assert!(
                !archived_path.exists(),
                "the deleted record came back into the archive"
            );

            discard_root(paths.root);
        }

        #[test]
        fn a_keeper_that_dies_on_its_way_up_is_reported_in_its_own_words() {
            let state = test_state("keeper-log");
            let paths = state.paths.clone();
            let session_id = "muxloomd-codex-stillborn";

            // Nothing to add is the normal case: a keeper that greeted wrote
            // nothing here, and a launch that failed elsewhere must not have a
            // stale line pinned to it.
            assert_eq!(keeper_log_tail(&paths, session_id), None);

            // The keeper's stderr is the only place its dying words land, so
            // the last of them is what a failed launch has to carry.
            let log = paths.keepers.join(format!("{session_id}.log"));
            fs::write(
                &log,
                "starting\n\nfailed to spawn '/usr/bin/codex': Resource temporarily unavailable\n",
            )
            .unwrap();
            let tail = keeper_log_tail(&paths, session_id).unwrap();
            assert!(tail.contains("Resource temporarily unavailable"), "{tail}");
            assert!(tail.contains("starting"), "{tail}");

            // A log that ran long stays one readable line either way.
            fs::write(&log, format!("{}\n", "chatter ".repeat(4096))).unwrap();
            let tail = keeper_log_tail(&paths, session_id).unwrap();
            assert!(tail.chars().count() <= 301, "{}", tail.chars().count());
            assert!(tail.ends_with('…'), "{tail}");

            discard_root(paths.root);
        }

        /// Searching a fleet means reading every capture it has ever written,
        /// and those run to gigabytes, so the line matcher is where the time
        /// goes. It reads the bytes as they lie for the queries people type and
        /// still answers them the way case folding would.
        #[test]
        fn history_is_searched_case_insensitively_whatever_the_query_is_written_in() {
            let paths = DaemonPaths::under(PathBuf::from("/tmp").join(format!(
                "mxl-search-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            )));
            paths.prepare().unwrap();
            let history = paths.history.join("search.ansi");
            fs::write(
                &history,
                "Cargo Test all green\n\
                 ※ recap: 改好了分页器\n\
                 rien à signaler\n\
                 RIEN À SIGNALER\n",
            )
            .unwrap();

            let matched = |query: &str| {
                search_history_file(&history, query, 10)
                    .unwrap()
                    .into_iter()
                    .map(|found| (found.line_number, found.recap))
                    .collect::<Vec<_>>()
            };
            // ASCII folds byte for byte, in either direction.
            assert_eq!(matched("cargo test"), [(1, false)]);
            assert_eq!(matched("CARGO TEST"), [(1, false)]);
            // A script with no case is matched exactly, and a recap still reads
            // as one.
            assert_eq!(matched("分页器"), [(2, true)]);
            // An accented query is the one case that still needs folding, and
            // it must not have been lost with the fast path.
            assert_eq!(matched("rien à"), [(3, false), (4, false)]);
            assert_eq!(matched("RIEN À"), [(3, false), (4, false)]);
            assert!(matched("nothing of the sort").is_empty());
            // The query is what decides which path is taken.
            assert!(!query_needs_unicode_folding("cargo test"));
            assert!(!query_needs_unicode_folding("改好了分页器"));
            assert!(query_needs_unicode_folding("rien à"));

            discard_root(paths.root);
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
                    archived_at: None,
                    pid: None,
                    dead: true,
                    archived: true,
                    recap: Some("completed the persistent work".into()),
                    title: None,
                    thread: None,
                    seed: None,
                    first_prompt: None,
                    working: false,
                    needs_attention: false,
                    attention_reason: None,
                    composer: None,
                    parent: None,
                    powers: None,
                    resumed_from: None,
                    resumed_to: None,
                },
            )
            .unwrap();
            fs::write(
                paths.history.join(format!("{session_id}.ansi")),
                b"first line\npersistent result\nlast line\n",
            )
            .unwrap();

            for _ in 0..2 {
                let restarted = DaemonState::new(paths.clone(), KeeperMode::InProcess);
                assert!(restarted.sessions.lock().unwrap().is_empty());
                let persisted = persisted_session(&restarted, session_id).unwrap();
                let snapshot = persisted.snapshot();
                assert!(snapshot.archived && snapshot.dead);
                assert_eq!(
                    snapshot.recap.as_deref(),
                    Some("completed the persistent work")
                );
                let history = persisted.read_history(0, 10, false).unwrap();
                assert_eq!(history.total_lines, 3);
                assert_eq!(history.offset_from_bottom, 0);
                assert_eq!(
                    String::from_utf8_lossy(&history.rows),
                    "first line\npersistent result\nlast line\n"
                );
                let matches = persisted.search_history("PERSISTENT", 10).unwrap();
                assert_eq!(matches.len(), 1);
                assert_eq!(matches[0].line_number, 2);
            }

            discard_root(paths.root);
        }

        fn live_metadata(session_id: &str, kind: &str, pid: Option<u32>) -> DaemonSession {
            DaemonSession {
                id: session_id.into(),
                kind: kind.into(),
                path: "/tmp/project".into(),
                label: "interrupted work".into(),
                temporary: false,
                created_at: 7,
                archived_at: None,
                pid,
                dead: false,
                archived: false,
                recap: None,
                title: None,
                thread: None,
                seed: None,
                first_prompt: None,
                resumed_from: None,
                resumed_to: None,
                working: true,
                needs_attention: false,
                attention_reason: None,
                composer: None,
                parent: None,
                powers: None,
            }
        }

        #[test]
        fn a_killed_daemon_recovers_its_sessions_into_the_archive() {
            let initial = test_state("interrupted");
            let paths = initial.paths.clone();
            drop(initial);
            let session_id = "muxloomd-claude-1700000000-9-0";
            let metadata_path = paths.sessions.join(format!("{session_id}.json"));
            persist_session_metadata(
                &metadata_path,
                &live_metadata(session_id, "claude", Some(u32::MAX)),
            )
            .unwrap();
            fs::write(
                paths.history.join(format!("{session_id}.ansi")),
                b"first line\n\xe2\x8f\xba refactored the parser\nlast line\n",
            )
            .unwrap();

            for restart in 0..2 {
                let restarted = DaemonState::new(paths.clone(), KeeperMode::InProcess);
                let persisted = persisted_session(&restarted, session_id)
                    .expect("an interrupted session must survive its daemon");
                let snapshot = persisted.snapshot();
                assert!(snapshot.dead, "restart {restart} left the session live");
                assert!(snapshot.pid.is_none() && !snapshot.working);
                assert_eq!(snapshot.label, "interrupted work");
                assert_eq!(snapshot.path, "/tmp/project");
                assert_eq!(snapshot.recap.as_deref(), Some("refactored the parser"));
                let matches = persisted
                    .search_history("stopped unexpectedly", 10)
                    .unwrap();
                assert_eq!(
                    matches.len(),
                    1,
                    "restart {restart} must note the interruption exactly once"
                );
                assert_eq!(persisted.search_history("first line", 10).unwrap().len(), 1);
            }

            let recorded: DaemonSession =
                serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
            assert!(recorded.dead && recorded.pid.is_none());
            discard_root(paths.root);
        }

        #[test]
        fn an_interrupted_temporary_session_is_still_discarded() {
            let initial = test_state("interrupted-temporary");
            let paths = initial.paths.clone();
            drop(initial);
            let session_id = "muxloomd-temporal-codex-1700000000-9-0";
            let mut metadata = live_metadata(session_id, "codex", Some(u32::MAX));
            metadata.temporary = true;
            persist_session_metadata(
                &paths.sessions.join(format!("{session_id}.json")),
                &metadata,
            )
            .unwrap();
            fs::write(
                paths.history.join(format!("{session_id}.ansi")),
                b"scratch\n",
            )
            .unwrap();

            let restarted = DaemonState::new(paths.clone(), KeeperMode::InProcess);
            assert!(restarted.persisted_sessions.lock().unwrap().is_empty());
            assert!(!paths.sessions.join(format!("{session_id}.json")).exists());
            assert!(!paths.history.join(format!("{session_id}.ansi")).exists());
            discard_root(paths.root);
        }

        #[test]
        fn a_session_log_outliving_its_metadata_is_archived_from_the_log_alone() {
            let initial = test_state("orphan-history");
            let paths = initial.paths.clone();
            drop(initial);
            let session_id = "muxloomd-codex-1700000042-9-3";
            // A metadata write the crash truncated reads back as nothing.
            fs::write(paths.sessions.join(format!("{session_id}.json")), b"").unwrap();
            fs::write(
                paths.history.join(format!("{session_id}.ansi")),
                b"early output\n\xe2\x80\xa2 shipped the release\n",
            )
            .unwrap();
            fs::write(paths.history.join("not-a-muxloom-log.ansi"), b"unrelated\n").unwrap();

            let restarted = DaemonState::new(paths.clone(), KeeperMode::InProcess);
            let persisted = persisted_session(&restarted, session_id)
                .expect("a log without metadata must still be reachable");
            let snapshot = persisted.snapshot();
            assert!(snapshot.dead && snapshot.archived);
            assert_eq!(snapshot.kind, "codex");
            assert_eq!(snapshot.created_at, 1_700_000_042);
            assert_eq!(snapshot.recap.as_deref(), Some("shipped the release"));
            assert_eq!(
                persisted.search_history("early output", 10).unwrap().len(),
                1
            );
            assert!(
                paths.history.join("not-a-muxloom-log.ansi").exists(),
                "an unrecognized log must be left alone, not deleted"
            );
            // The rebuilt record is written back, so the next start is ordinary.
            assert!(
                DaemonState::new(paths.clone(), KeeperMode::InProcess)
                    .persisted_sessions
                    .lock()
                    .unwrap()
                    .contains_key(session_id)
            );
            discard_root(paths.root);
        }

        #[test]
        fn a_session_whose_history_is_gone_still_loads_and_reads() {
            let initial = test_state("history-gone");
            let paths = initial.paths.clone();
            drop(initial);
            let session_id = "muxloomd-claude-1700000000-9-1";
            let metadata_path = paths.sessions.join(format!("{session_id}.json"));
            let mut metadata = live_metadata(session_id, "claude", None);
            metadata.dead = true;
            metadata.archived = true;
            persist_session_metadata(&metadata_path, &metadata).unwrap();

            let restarted = DaemonState::new(paths.clone(), KeeperMode::InProcess);
            let persisted = persisted_session(&restarted, session_id)
                .expect("a missing log must not erase the session that recorded it");
            let history = persisted.read_history(0, 10, false).unwrap();
            assert!(history.rows.is_empty() && history.total_lines == 0);
            assert!(persisted.search_history("anything", 10).unwrap().is_empty());
            discard_root(paths.root);
        }

        /// A resumed conversation began long before the session that reopened
        /// it, so nothing about when each started can pair the two. The
        /// command line said so outright, but it belongs to a keeper the next
        /// daemon did not spawn - and a daemon that restarts before the match
        /// has been made would otherwise have nothing left to go on.
        #[test]
        fn a_daemon_that_restarts_still_knows_which_conversation_was_reopened() {
            let state = test_state("handover-seed");
            let paths = state.paths.clone();
            let session_id = "muxloomd-claude-1700000000-9-3";
            // Stands in for `claude --resume <id>`: a shell that sits on the
            // PTY reading, with the flag among its arguments where the seed is
            // read from.
            let launched = launch_session(
                &state,
                session_id.into(),
                "claude".into(),
                "/tmp".into(),
                "resumed work".into(),
                false,
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "cat".into(),
                    "muxloom".into(),
                    "--resume".into(),
                    "the-conversation".into(),
                ],
                vec![],
                5,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            assert_eq!(
                launched.snapshot().seed.as_deref(),
                Some("the-conversation"),
                "the launch writes down what it was told to reopen"
            );
            state.draining.store(true, Ordering::Release);
            drop(launched);
            drop(state);

            let restarted = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            adopt_keeper_sessions(&restarted);
            let adopted = daemon_session(&restarted, session_id)
                .expect("a live keeper session must be adopted, not archived");
            assert_eq!(
                session_facts(0, &adopted).seed.as_deref(),
                Some("the-conversation"),
                "and the next generation matches on it as the first one would have"
            );

            adopted.stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !adopted.snapshot().dead && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            discard_root(paths.root);
        }

        #[test]
        fn a_stopped_daemon_hands_its_live_sessions_to_the_next_generation() {
            let state = test_state("handover-adopt");
            let paths = state.paths.clone();
            let session_id = "muxloomd-terminal-1700000000-9-2";
            let launched = launch_session(
                &state,
                session_id.into(),
                "terminal".into(),
                "/tmp".into(),
                "still running".into(),
                false,
                "/bin/cat".into(),
                vec![],
                vec![],
                5,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let child_pid = launched.snapshot().pid.expect("launched session has a pid");
            // A handover drains the old daemon before the next one starts, so
            // its reader must read the keeper hanging up as the transfer it is
            // rather than a death.
            state.draining.store(true, Ordering::Release);
            drop(launched);
            drop(state);

            let restarted = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            adopt_keeper_sessions(&restarted);
            let adopted = daemon_session(&restarted, session_id)
                .expect("a live keeper session must be adopted, not archived");
            let snapshot = adopted.snapshot();
            assert!(!snapshot.dead, "adoption must keep the session live");
            assert_eq!(
                snapshot.pid,
                Some(child_pid),
                "the adopted session is the same process, not a relaunch"
            );
            assert_eq!(snapshot.label, "still running");

            // The adopted session is fully served: input reaches the PTY and
            // the transcript keeps growing across the generation change.
            adopted.write_input(b"adopted-generation-probe\r").unwrap();
            let probe = b"adopted-generation-probe";
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut output = Vec::new();
            while Instant::now() < deadline {
                output = history_tail(&adopted.history_path, RECENT_OUTPUT_LIMIT as u64)
                    .unwrap_or_default();
                if output.windows(probe.len()).any(|window| window == probe) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                output.windows(probe.len()).any(|window| window == probe),
                "typed bytes must reach the adopted PTY"
            );

            adopted.stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !adopted.snapshot().dead && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(adopted.snapshot().dead, "a stopped adopted session dies");
            discard_root(paths.root);
        }

        /// The test above sets the flag by hand because that is what the loop
        /// is meant to have done. A signal has to be latched, not merely
        /// noticed: the readers decide what a keeper hanging up meant by
        /// reading it, and they run on past the round that saw the signal.
        #[test]
        fn a_signalled_daemon_says_so_where_its_readers_look() {
            let state = test_state("signal-latch");
            let paths = state.paths.clone();
            let signalled = AtomicBool::new(false);

            assert!(
                still_serving(&state, &signalled),
                "nothing has happened yet"
            );
            assert!(!state.shutdown.load(Ordering::Acquire));

            signalled.store(true, Ordering::Release);
            assert!(
                !still_serving(&state, &signalled),
                "a signal ends the round"
            );
            assert!(
                state.shutdown.load(Ordering::Acquire),
                "and every reader is told, which is the whole of it"
            );
            assert!(
                !still_serving(&state, &AtomicBool::new(false)),
                "the flag outlives the signal that set it"
            );

            discard_root(paths.root);
        }

        /// The screen an adopted session is handed is a replay of what its
        /// keeper captured while no daemon was watching, so the turn it is
        /// drawing may have ended hours before the handover. Taking the
        /// takeover for a session that has just spoken is how a machine's
        /// untouched agents all lit up as working together, every time a new
        /// build took over.
        #[test]
        fn an_adopted_session_only_counts_as_working_once_this_daemon_has_heard_it() {
            let state = test_state("handover-working");
            let paths = state.paths.clone();
            let session_id = "muxloomd-codex-1700000000-9-4";
            let launched = launch_session(
                &state,
                session_id.into(),
                "codex".into(),
                "/tmp".into(),
                "mid-turn at the handover".into(),
                false,
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '\\033[2J\\033[H• Working (2s • esc to interrupt)'; cat".into(),
                ],
                vec![],
                5,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !launched.snapshot().working && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                launched.snapshot().working,
                "the child painted a turn that is running"
            );
            state.draining.store(true, Ordering::Release);
            drop(launched);
            drop(state);

            let restarted = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            adopt_keeper_sessions(&restarted);
            let adopted = daemon_session(&restarted, session_id)
                .expect("a live keeper session must be adopted, not archived");
            let replayed = adopted
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .screen()
                .contents();
            assert!(
                agent_is_working(AgentKind::Codex, &replayed),
                "the replayed screen still carries the marker"
            );
            assert!(
                !adopted.snapshot().working,
                "but nothing has been heard from the session since the handover"
            );

            // Once it speaks for itself the same screen means what it says.
            adopted.write_input(b"\r").unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !adopted.snapshot().working && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                adopted.snapshot().working,
                "hearing the session makes its screen current again"
            );

            adopted.stop().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !adopted.snapshot().dead && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            discard_root(paths.root);
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
                    parent: None,
                    powers: None,
                    initial_prompt: None,
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
        fn opencode_and_pi_messages_end_in_plain_enter_not_bracketed_paste() {
            // The delivered bytes must end with the Enter keystroke the TUI
            // binds its submit to, not a newline or an escape sequence that
            // leaves the draft sitting in the box.
            let body = "line one\n\nline two";
            let oc = message_bytes(AgentKind::OpenCode, body);
            assert_eq!(oc, "line one \u{23ce} line two\r".as_bytes().to_vec());
            assert!(
                !oc.starts_with(b"\x1b[200~"),
                "opencode must not use bracketed paste"
            );
            let pi = message_bytes(AgentKind::Pi, body);
            assert_eq!(pi, oc);
            let codex = message_bytes(AgentKind::Codex, body);
            assert!(
                codex.starts_with(b"\x1b[200~"),
                "codex uses bracketed paste"
            );
            assert_eq!(*codex.last().unwrap(), b'\r');
            let claude = message_bytes(AgentKind::Claude, body);
            assert!(
                claude.starts_with(b"\x1b[200~"),
                "claude uses bracketed paste"
            );
            assert_eq!(*claude.last().unwrap(), b'\r');
        }

        /// Launch an alt-screen child that draws a marker row, then idles.
        /// `kind` is what the session is recorded as (the attach path gates
        /// its payload choice on it), independent of the actual command.
        fn launch_alt_screen_marker_session(
            client: &mut UnixStream,
            label: &str,
            session_id: &str,
            kind: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Frame::json(
                FrameKind::Request,
                0,
                1,
                &DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: kind.into(),
                    path: "/tmp".into(),
                    label: label.into(),
                    temporary: false,
                    executable: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "printf '\\033[?1049h'; printf 'TASKCMARKER\\n'; \
                         while :; do sleep 0.1; done"
                            .into(),
                    ],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                    parent: None,
                    powers: None,
                    initial_prompt: None,
                },
            )?
            .write_to(client)?;
            loop {
                let frame = Frame::read_from(client)?.unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 1 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    return Ok(());
                }
            }
        }

        /// The first data frame an attach sends on the pty stream.
        fn first_attach_payload(
            client: &mut UnixStream,
            session_id: &str,
            columns: u16,
            rows: u16,
        ) -> Result<String, Box<dyn std::error::Error>> {
            Frame::json(
                FrameKind::OpenStream,
                stream::PTY_BASE,
                2,
                &OpenStream::Pty {
                    session_id: session_id.into(),
                    columns,
                    rows,
                    scrollback_rows: 0,
                },
            )?
            .write_to(client)?;
            loop {
                let frame =
                    Frame::read_from(client)?.ok_or("stream closed before the attach payload")?;
                if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE {
                    return Ok(
                        String::from_utf8_lossy(&frame.decoded_payload().unwrap()).into_owned()
                    );
                }
            }
        }

        #[test]
        fn an_alt_screen_attach_at_a_new_size_sends_only_the_preamble() {
            // The pane changed size and the child sits in the alt screen: the
            // parser just reflowed the old-size screen, so attach must not
            // commit that intermediate frame. It sends the mode preamble
            // (alt enter + clear) and leaves the repaint to the app.
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let state = test_state("altsize");
            let paths = state.paths.clone();
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-terminal-altsize";
            launch_alt_screen_marker_session(&mut client, "altsize", session_id, "opencode")
                .unwrap();
            // Let the child enter the alt screen and draw its marker row.
            thread::sleep(Duration::from_millis(750));
            let payload = first_attach_payload(&mut client, session_id, 100, 40).unwrap();
            assert!(
                payload.starts_with("\x1b[?1049h"),
                "preamble must enter the alt screen the child is on: {payload:?}"
            );
            assert!(
                payload.contains("\x1b[2J"),
                "preamble must clear the screen: {payload:?}"
            );
            assert!(
                !payload.contains("TASKCMARKER"),
                "a size-changed alt-screen attach must not dump the reflowed rows: {payload:?}"
            );
            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 3 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        #[test]
        fn an_alt_screen_attach_at_the_same_size_still_sends_the_full_snapshot() {
            // No size change, no reflow: the snapshot is the live screen, so
            // the full row dump still goes out (and carries the marker row).
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let state = test_state("altresame");
            let paths = state.paths.clone();
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-terminal-altresame";
            launch_alt_screen_marker_session(&mut client, "altresame", session_id, "opencode")
                .unwrap();
            thread::sleep(Duration::from_millis(750));
            let payload = first_attach_payload(&mut client, session_id, 80, 24).unwrap();
            assert!(
                payload.starts_with("\x1b[?1049h"),
                "snapshot must enter the alt screen the child is on: {payload:?}"
            );
            assert!(
                payload.contains("TASKCMARKER"),
                "a same-size attach must send the full snapshot: {payload:?}"
            );
            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 3 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        #[test]
        fn a_terminal_kind_attach_at_a_new_size_keeps_the_full_snapshot() {
            // A plain terminal has no post-SIGWINCH repaint to lean on, so even
            // when the pane changed size its attach must still deliver the full
            // snapshot (reflowed or not) — never the bare preamble, which would
            // leave its screen blank (the embedded-pty smoke scenario).
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let state = test_state("altterm");
            let paths = state.paths.clone();
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-terminal-altterm";
            launch_alt_screen_marker_session(&mut client, "altterm", session_id, "terminal")
                .unwrap();
            thread::sleep(Duration::from_millis(750));
            let payload = first_attach_payload(&mut client, session_id, 100, 40).unwrap();
            assert!(
                payload.starts_with("\x1b[?1049h"),
                "snapshot must enter the alt screen the child is on: {payload:?}"
            );
            assert!(
                payload.contains("TASKCMARKER"),
                "a terminal-kind size-changed attach must keep the full snapshot: {payload:?}"
            );
            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 3 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        fn queued_message(deliver: TalkDeliver) -> TalkQueued {
            TalkQueued {
                message_id: "msg-taskd".into(),
                session_id: "muxloomd-terminal-taskd".into(),
                body: "an envelope".into(),
                queued_at: 1_000,
                deliver,
                from: None,
                text: "hello".into(),
                channel: None,
                session_label: None,
            }
        }

        #[test]
        fn a_freshly_typed_draft_holds_the_message_back() {
            // A box that changed moments ago is being typed into: appending our
            // message would fold it into somebody's live sentence.
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                true,
                false,
                Some(DELIVER_STALE_DRAFT_MS - 1)
            ));
            // The very first sighting of a draft is never stale.
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                true,
                false,
                Some(0)
            ));
        }

        #[test]
        fn a_stale_draft_releases_the_message() {
            // A draft that has not changed for the grace period is one nobody
            // is coming back to: deliver over it rather than holding to the
            // five-minute backstop.
            assert!(stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                true,
                false,
                Some(DELIVER_STALE_DRAFT_MS)
            ));
            assert!(stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                true,
                false,
                Some(DELIVER_STALE_DRAFT_MS + 5_000)
            ));
            // No box text could be read: treat it as not-yet-stale and let the
            // old patience rules keep owning it rather than guess.
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                true,
                false,
                None
            ));
        }

        #[test]
        fn an_idle_session_releases_the_message_over_a_fresh_draft() {
            // The turn is over, so a waiting message no longer risks being read
            // mid-sentence: the draft's freshness stops mattering.
            assert!(stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                false,
                false,
                Some(0)
            ));
        }

        #[test]
        fn a_forced_message_skips_the_draft_wait_entirely() {
            // A `now` sender asked for it in regardless of state.
            assert!(stale_draft_due(
                &queued_message(TalkDeliver::Now),
                Composer::Occupied,
                true,
                false,
                Some(0)
            ));
            assert!(stale_draft_due(
                &queued_message(TalkDeliver::Now),
                Composer::Ready,
                true,
                false,
                Some(0)
            ));
        }

        #[test]
        fn the_when_idle_and_attention_gates_still_hold_over_a_stale_draft() {
            // A `when_idle` sender asked to wait out the turn, and a question
            // on screen is answered by whoever is there, not by a queued
            // message: neither is rescued by a stale draft.
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::WhenIdle),
                Composer::Occupied,
                true,
                false,
                Some(DELIVER_STALE_DRAFT_MS)
            ));
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Occupied,
                true,
                true,
                Some(DELIVER_STALE_DRAFT_MS)
            ));
            // A ready or absent box has no draft to age: the old patience rules
            // own it.
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Ready,
                true,
                false,
                Some(DELIVER_STALE_DRAFT_MS)
            ));
            assert!(!stale_draft_due(
                &queued_message(TalkDeliver::Auto),
                Composer::Absent,
                true,
                false,
                Some(DELIVER_STALE_DRAFT_MS)
            ));
        }

        #[test]
        fn a_session_held_on_a_question_is_told_about_rather_than_promised_delivery() {
            // The case that sent somebody looking: a session freshly launched
            // from a chat app came up on its own onboarding question, so it
            // shows no prompt box. `due` holds every message for as long as
            // that is true, which means the patience never runs out and the
            // wait ends thirty minutes later in a bounce. The answer has to
            // say that the sender is waiting on a person, and name what for.
            let held = queued_reason(Composer::Absent, true, Some("Security guide"));
            assert!(
                held.contains("waiting on a question (Security guide)"),
                "{held}"
            );
            assert!(held.contains("Answer the question"), "{held}");
            assert!(!held.contains("as soon as one appears"), "{held}");
            // Same answer whichever way the box is unavailable.
            assert_eq!(
                held,
                queued_reason(Composer::Occupied, true, Some("Security guide"))
            );
            // A question nobody could put a name to still gets the point across.
            let unnamed = queued_reason(Composer::Absent, true, None);
            assert!(unnamed.contains("waiting on a question, and"), "{unnamed}");

            // With no question up, the old answers stand: these really do
            // clear on their own, and saying so is not a false promise.
            let starting = queued_reason(Composer::Absent, false, None);
            assert!(starting.contains("as soon as one appears"), "{starting}");
            let drafting = queued_reason(Composer::Occupied, false, None);
            assert!(
                drafting.contains("as soon as the prompt clears"),
                "{drafting}"
            );
            // A ready box is typed into whether or not a question is up, so
            // reaching the reason with one means a `when_idle` sender mid-turn.
            let mid_turn = queued_reason(Composer::Ready, true, Some("Security guide"));
            assert!(mid_turn.contains("goes in when it finishes"), "{mid_turn}");
            assert_eq!(mid_turn, queued_reason(Composer::Ready, false, None));
        }

        #[test]
        fn composer_text_reads_only_the_opencode_draft() {
            // The draft rows only: the model status line against the box bottom
            // belongs to the app, not to the sender, and must not be read as
            // draft text. The shapes below are the real 1.18.23 box: an input
            // row, an empty row, then the wrapping status line under the border.
            let screen = "┃ a stale draft\n┃\n┃ Build · m\n╹▀▀▀▀▀▀▀\n";
            assert_eq!(
                composer_text(AgentKind::OpenCode, screen).as_deref(),
                Some("a stale draft")
            );
            let idle = "┃ Ask anything...\n┃\n┃ Build · m\n╹▀▀▀▀▀▀▀\n";
            assert_eq!(
                composer_text(AgentKind::OpenCode, idle).as_deref(),
                Some("")
            );
            assert_eq!(
                composer_text(AgentKind::OpenCode, "just some transcript\n"),
                None
            );
        }

        /// Launch a child that draws an opencode-shaped composer box holding a
        /// draft, then idles — a user who typed and walked away.
        fn launch_stale_draft_session(
            client: &mut UnixStream,
            label: &str,
            session_id: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Frame::json(
                FrameKind::Request,
                0,
                1,
                &DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: "opencode".into(),
                    path: "/tmp".into(),
                    label: label.into(),
                    temporary: false,
                    executable: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        // The real 1.18.23 box: an input row, an empty row,
                        // then the wrapping status line under the border.
                        "printf '\\033[2J\\033[H┃ a stale draft\\n┃\\n┃ Build · m\\n╹▀▀▀▀▀▀▀\\n'; \
                         while :; do sleep 0.1; done"
                            .into(),
                    ],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                    parent: None,
                    powers: None,
                    initial_prompt: None,
                },
            )?
            .write_to(client)?;
            loop {
                let frame = Frame::read_from(client)?.unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 1 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    return Ok(());
                }
            }
        }

        #[test]
        fn the_draft_age_clock_ages_an_unchanged_draft_from_its_first_sighting() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let state = test_state("draftage");
            let paths = state.paths.clone();
            let client_state = Arc::clone(&state);
            let handle = thread::spawn(move || serve_client(server, client_state));
            let session_id = "muxloomd-terminal-draftage";
            launch_stale_draft_session(&mut client, "draftage", session_id).unwrap();
            // Let the child draw the box.
            thread::sleep(Duration::from_millis(750));
            let session = state
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(session_id)
                .cloned()
                .expect("the launched session");
            let t0 = 1_000_000u64;
            assert_eq!(
                draft_age_ms(&session, &AgentKind::OpenCode, t0),
                Some(0),
                "the first sighting of the draft is age zero"
            );
            assert_eq!(
                draft_age_ms(&session, &AgentKind::OpenCode, t0 + 16_000),
                Some(16_000),
                "an unchanged draft ages up without resetting"
            );
            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 3 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        #[test]
        fn an_attach_delivers_the_snapshot_without_replaying_history() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let state = test_state("noseed");
            let paths = state.paths.clone();
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-terminal-noseed";
            Frame::json(
                FrameKind::Request,
                0,
                1,
                &DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: "terminal".into(),
                    path: "/tmp".into(),
                    label: "noseed".into(),
                    temporary: false,
                    executable: "/bin/cat".into(),
                    args: vec![],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                    parent: None,
                    powers: None,
                    initial_prompt: None,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 1 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    break;
                }
            }

            // Give the session a log an old attach would have spent a long time
            // rendering, with a marker in every line so a leak is unmistakable.
            let history = paths.history.join(format!("{session_id}.ansi"));
            let mut log = String::new();
            for index in 0..4000 {
                log.push_str(&format!("history line {index} SEEDMARKER\n"));
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&history)
                .unwrap();
            file.write_all(log.as_bytes()).unwrap();

            Frame::json(
                FrameKind::OpenStream,
                stream::PTY_BASE,
                2,
                &OpenStream::Pty {
                    session_id: session_id.into(),
                    columns: 80,
                    rows: 24,
                    scrollback_rows: 2000,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();

            // An idle session emits no live output, so the first data frame on
            // the stream is exactly what attach chose to send: the live
            // snapshot, with the seeded history nowhere in it.
            let mut first: Option<Vec<u8>> = None;
            for _ in 0..64 {
                let Some(frame) = Frame::read_from(&mut client).unwrap_or(None) else {
                    break;
                };
                if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE {
                    first = Some(frame.decoded_payload().unwrap().to_vec());
                    break;
                }
            }
            let first = first.expect("attach sends no data at all");
            let text = String::from_utf8_lossy(&first);
            assert!(
                text.starts_with("\x1b[?1049l") || text.starts_with("\x1b[?1049h"),
                "attach must send the screen snapshot first, got: {text:?}"
            );
            assert!(
                !text.contains("SEEDMARKER"),
                "attach must not replay the session's history: {text:?}"
            );

            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 3 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        #[test]
        fn a_same_size_attach_does_not_resignal_the_child() {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let state = test_state("noresize");
            let paths = state.paths.clone();
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-terminal-noresize";
            Frame::json(
                FrameKind::Request,
                0,
                1,
                &DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: "terminal".into(),
                    path: "/tmp".into(),
                    label: "noresize".into(),
                    temporary: false,
                    executable: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "trap 'printf WINCHMARKER' WINCH; while :; do sleep 0.1; done".into(),
                    ],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                    parent: None,
                    powers: None,
                    initial_prompt: None,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 1 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    break;
                }
            }

            // Attach at the size the session already has: the daemon must not
            // resend a RESIZE, which a full-screen TUI would read as a reason
            // to reflow its whole screen.
            Frame::json(
                FrameKind::OpenStream,
                stream::PTY_BASE,
                2,
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

            // An idle child is quiet: whatever the attach just sent, the child
            // must not answer it. Read until the stream goes quiet (a timed-out
            // read) and check the trap's marker never made it out.
            let mut after_attach = Vec::new();
            let deadline = Instant::now() + Duration::from_millis(1200);
            while Instant::now() < deadline {
                match Frame::read_from(&mut client) {
                    Ok(Some(frame))
                        if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE =>
                    {
                        after_attach.extend_from_slice(&frame.decoded_payload().unwrap());
                    }
                    _ => break,
                }
            }
            let after_attach = String::from_utf8_lossy(&after_attach);
            assert!(
                !after_attach.contains("WINCHMARKER"),
                "a same-size attach re-signalled the child: {after_attach:?}"
            );

            // A real size change must still reach the child.
            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Resize {
                    session_id: session_id.into(),
                    columns: 100,
                    rows: 24,
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            let mut after_resize = String::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !after_resize.contains("WINCHMARKER") && Instant::now() < deadline {
                if let Ok(Some(frame)) = Frame::read_from(&mut client) {
                    if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE {
                        after_resize
                            .push_str(&String::from_utf8_lossy(&frame.decoded_payload().unwrap()));
                    }
                }
            }
            assert!(
                after_resize.contains("WINCHMARKER"),
                "a genuine resize must still reach the child: {after_resize:?}"
            );

            Frame::json(
                FrameKind::Request,
                0,
                4,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 4 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        /// Launch an alt-screen child that draws a marker row, prints
        /// WINCHMARKER on every SIGWINCH, then idles.
        fn launch_alt_screen_winch_session(
            client: &mut UnixStream,
            label: &str,
            session_id: &str,
            kind: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Frame::json(
                FrameKind::Request,
                0,
                1,
                &DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: kind.into(),
                    path: "/tmp".into(),
                    label: label.into(),
                    temporary: false,
                    executable: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "printf '\\033[?1049h'; printf 'TASKCMARKER\\n'; \
                         trap 'printf WINCHMARKER' WINCH; while :; do sleep 0.1; done"
                            .into(),
                    ],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                    parent: None,
                    powers: None,
                    initial_prompt: None,
                },
            )?
            .write_to(client)?;
            loop {
                let frame = Frame::read_from(client)?.unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 1 {
                    assert!(matches!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Launched { .. }
                    ));
                    return Ok(());
                }
            }
        }

        #[test]
        fn an_adopted_alt_screen_session_forces_a_repaint_on_a_same_size_attach() {
            // Adoption rebuilds the screen from a bounded tail of history: for
            // a differential-rendering TUI that is a partial frame. The first
            // attach must not ship it — it sends the preamble and forces the
            // child to repaint — even though the size did not change.
            let state = test_state("adoptwinch");
            let paths = state.paths.clone();
            let session_id = "muxloomd-opencode-adoptwinch";
            let launched = launch_session(
                &state,
                session_id.into(),
                "opencode".into(),
                "/tmp".into(),
                "adoptwinch".into(),
                false,
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '\\033[?1049h'; printf 'WINCHSCREEN\\n'; \
                     trap 'printf WINCHMARKER' WINCH; while :; do sleep 0.1; done"
                        .into(),
                ],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            // Let the child paint, so the tail the next generation replays
            // holds its screen.
            thread::sleep(Duration::from_millis(750));
            // A session this daemon spawned itself is never marked rebuilt.
            assert!(!launched.screen_rebuilt.load(Ordering::Relaxed));
            // The old generation drains and dies; the child's keeper survives.
            state.draining.store(true, Ordering::Release);
            drop(launched);
            drop(state);

            let restarted = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            adopt_keeper_sessions(&restarted);
            let adopted = daemon_session(&restarted, session_id)
                .expect("a live keeper session must be adopted, not archived");
            // Adoption rebuilt its screen from the history tail, so its first
            // attach is told to force a repaint.
            assert!(adopted.screen_rebuilt.load(Ordering::Relaxed));

            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let serve = Arc::clone(&restarted);
            let handle = thread::spawn(move || serve_client(server, serve));
            // Attach at the size the session already has: no reflow happened,
            // so only the rebuilt flag can justify the repaint path.
            Frame::json(
                FrameKind::OpenStream,
                stream::PTY_BASE,
                1,
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
            // Judge a window, not the first frame: the child's repaint arrives
            // as live broadcast, so read until the whole exchange is in. The
            // preamble must be in it, the partial snapshot must not be, and the
            // child must have felt the nudge.
            let mut window = String::new();
            let mut first_data: Option<String> = None;
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline
                && !(window.contains("\x1b[?1049h") && window.contains("WINCHMARKER"))
            {
                match Frame::read_from(&mut client) {
                    Ok(Some(frame))
                        if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE =>
                    {
                        let payload =
                            String::from_utf8_lossy(&frame.decoded_payload().unwrap()).into_owned();
                        first_data.get_or_insert_with(|| payload.clone());
                        window.push_str(&payload);
                    }
                    // The stream-open ack (and other housekeeping) comes first.
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            assert!(
                window.contains("\x1b[?1049h"),
                "a rebuilt same-size attach must send the preamble: {window:?}"
            );
            assert!(
                window.contains("\x1b[2J"),
                "the preamble must clear the screen: {window:?}"
            );
            assert!(
                !window.contains("WINCHSCREEN"),
                "the partial rebuilt frame must not be committed: {window:?}"
            );
            assert!(
                window.contains("WINCHMARKER"),
                "the repaint nudge must SIGWINCH the child: {window:?}"
            );
            // And the order is the contract, not just the presence: the
            // preamble is the first payload out - the broadcast gate opens
            // only behind the clear - and the child's repaint lands after it.
            // A live frame erased by a clear that follows it is the blank-pane
            // bug this ordering exists to prevent.
            let first_data = first_data.expect("the attach sends a payload");
            assert!(
                first_data.starts_with("\x1b[?1049h") && first_data.contains("\x1b[2J"),
                "the preamble alone opens the stream: {first_data:?}"
            );
            assert!(
                window.find("\x1b[2J") < window.find("WINCHMARKER"),
                "every repaint must follow the clear: {window:?}"
            );
            // ...and the PTY ends at exactly the attach size.
            assert_eq!(adopted.columns.load(Ordering::Relaxed), 80);
            assert_eq!(adopted.rows.load(Ordering::Relaxed), 24);
            // The flag is consumed: the next attach is an ordinary one.
            assert!(!adopted.screen_rebuilt.load(Ordering::Relaxed));

            Frame::json(
                FrameKind::Request,
                0,
                2,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 2 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        #[test]
        fn an_adopted_alt_screen_with_a_deep_tail_still_forces_a_repaint_on_a_same_size_attach() {
            // The real-world adoption: by the time a daemon hands over, a
            // redraw-heavy alt-screen agent has produced far more history than
            // the RECENT_OUTPUT_LIMIT adoption tail replays, and it entered the
            // alt buffer once at the very start - long before that tail. So the
            // rebuilt mirror ends on the *primary* screen even though the running
            // child is genuinely on alt. The old gate keyed the force-repaint on
            // the mirror's alt flag, so this attach shipped the stale primary
            // snapshot, cleared screen_rebuilt, and left the alt UI blank; seen
            // by the user as "opencode 终端切回后 UI 未完整渲染". It must instead
            // force a repaint: preamble first, then a resize nudge the child
            // actually feels, so its own SIGWINCH repaint restores the alt screen.
            //
            // To reproduce, the child enters alt then writes >RECENT_OUTPUT_LIMIT
            // of filler, so the adopted (bounded) tail contains no alt entry and
            // the parser rebuilds primary.
            let state = test_state("adoptdeepalt");
            let paths = state.paths.clone();
            let session_id = "muxloomd-opencode-adoptdeepalt";
            let launched = launch_session(
                &state,
                session_id.into(),
                "opencode".into(),
                "/tmp".into(),
                "adoptdeepalt".into(),
                false,
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '\\033[?1049h'; printf 'ALTSCREENMARKER\\n'; \
                     head -c 3145728 /dev/zero | tr '\\0' x; \
                     trap 'printf WINCHMARKER' WINCH; while :; do sleep 0.1; done"
                        .into(),
                ],
                vec![],
                1,
                80,
                24,
                None,
                None,
            )
            .unwrap();
            // Give the child time to enter alt and flush >2 MiB to history.
            thread::sleep(Duration::from_secs(3));
            assert!(
                !launched.screen_rebuilt.load(Ordering::Relaxed),
                "a self-launched session is never rebuilt"
            );
            // The old generation drains and dies; the child's keeper survives.
            state.draining.store(true, Ordering::Release);
            drop(launched);
            drop(state);

            let restarted = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            adopt_keeper_sessions(&restarted);
            let adopted = daemon_session(&restarted, session_id)
                .expect("a live keeper session must be adopted, not archived");
            assert!(
                adopted.screen_rebuilt.load(Ordering::Relaxed),
                "adoption rebuilds from a bounded tail and must mark the screen rebuilt"
            );
            // The adopted mirror reads primary: the deep tail never saw the alt entry.
            {
                let screen = adopted
                    .screen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                assert!(
                    !screen.screen().alternate_screen(),
                    "the replayed 2 MiB tail must start after the alt entry, leaving the \
                     adopted mirror on the primary screen - that is the inadequate mirror \
                     the old gate trusted"
                );
            }

            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let serve = Arc::clone(&restarted);
            let handle = thread::spawn(move || serve_client(server, serve));
            // Same-size attach: no reflow, so only the rebuilt flag can justify
            // the repaint path - and it must, because the mirror is inadequate.
            Frame::json(
                FrameKind::OpenStream,
                stream::PTY_BASE,
                1,
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
            let mut window = String::new();
            let mut first_data: Option<String> = None;
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && !window.contains("WINCHMARKER") {
                match Frame::read_from(&mut client) {
                    Ok(Some(frame))
                        if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE =>
                    {
                        let payload =
                            String::from_utf8_lossy(&frame.decoded_payload().unwrap()).into_owned();
                        first_data.get_or_insert_with(|| payload.clone());
                        window.push_str(&payload);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            // The force path — not the stale snapshot — ran: the child was nudged,
            // so it physically received SIGWINCH. The old gate (keyed on the
            // primary-reading mirror) never ran this, which is the regression.
            assert!(
                window.contains("WINCHMARKER"),
                "an adopted alt-screen with a deep tail must be nudged into repainting \
                 even though its rebuilt mirror reads primary: {window:?}"
            );
            let first_data = first_data.expect("the attach sends a payload");
            assert!(
                (first_data.starts_with("\x1b[?1049h") || first_data.starts_with("\x1b[?1049l"))
                    && first_data.contains("\x1b[2J"),
                "the force path opens the stream with a mode preamble (mirror state + clear), \
                 never the raw snapshot: {first_data:?}"
            );
            // The flag is consumed only because a real repaint was demanded.
            assert!(!adopted.screen_rebuilt.load(Ordering::Relaxed));

            Frame::json(
                FrameKind::Request,
                0,
                2,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 2 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
        }

        #[test]
        fn a_same_size_attach_to_a_fresh_alt_screen_tui_keeps_the_snapshot_and_no_winch() {
            // A session this daemon spawned itself has a live, complete screen:
            // a same-size attach ships the full snapshot (the marker row is in
            // it) and must not re-signal the child — no SIGWINCH.
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let state = test_state("freshwinch");
            let paths = state.paths.clone();
            let handle = thread::spawn(move || serve_client(server, state));
            let session_id = "muxloomd-opencode-freshwinch";
            launch_alt_screen_winch_session(&mut client, "freshwinch", session_id, "opencode")
                .unwrap();
            thread::sleep(Duration::from_millis(750));
            let payload = first_attach_payload(&mut client, session_id, 80, 24).unwrap();
            assert!(
                payload.starts_with("\x1b[?1049h"),
                "snapshot must enter the alt screen the child is on: {payload:?}"
            );
            assert!(
                payload.contains("TASKCMARKER"),
                "a fresh same-size attach must keep the full snapshot: {payload:?}"
            );
            // No repaint nudge went out: the child never saw a resize.
            let mut window = String::new();
            let deadline = Instant::now() + Duration::from_millis(1200);
            while Instant::now() < deadline {
                match Frame::read_from(&mut client) {
                    Ok(Some(frame))
                        if frame.kind == FrameKind::Data && frame.stream_id == stream::PTY_BASE =>
                    {
                        window
                            .push_str(&String::from_utf8_lossy(&frame.decoded_payload().unwrap()));
                    }
                    // The stream-open ack (and other housekeeping) comes first.
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            assert!(
                !window.contains("WINCHMARKER"),
                "a fresh same-size attach must not re-SIGWINCH the child: {window:?}"
            );
            Frame::json(
                FrameKind::Request,
                0,
                3,
                &DaemonRequest::Archive {
                    session_id: session_id.into(),
                },
            )
            .unwrap()
            .write_to(&mut client)
            .unwrap();
            loop {
                let frame = Frame::read_from(&mut client).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 3 {
                    assert_eq!(
                        frame.decode_json::<DaemonResponse>().unwrap(),
                        DaemonResponse::Ack
                    );
                    break;
                }
            }
            drop(client);
            handle.join().unwrap().unwrap();
            discard_root(paths.root);
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

        fn hello_frame(capabilities: &[&str]) -> Frame {
            Frame::json(
                FrameKind::Response,
                0,
                1,
                &DaemonResponse::Hello {
                    daemon_version: "0.3.0".into(),
                    protocol_version: PROTOCOL_VERSION,
                    pid: 4321,
                    capabilities: capabilities.iter().map(|it| (*it).to_string()).collect(),
                    daemon_generation: "0.3.0:protocol-1:abc:7:1-1".into(),
                },
            )
            .unwrap()
        }

        fn hello_capabilities(frame: &Frame) -> Vec<String> {
            match frame.decode_json::<DaemonResponse>().unwrap() {
                DaemonResponse::Hello { capabilities, .. } => capabilities,
                other => panic!("expected a hello, got {other:?}"),
            }
        }

        #[test]
        fn a_bridge_claims_forwarding_only_for_a_daemon_that_lacks_it() {
            // A daemon old enough to predate forwarding: the bridge adds the
            // capabilities it will serve itself, so the client stops reporting
            // forwarding unavailable against a daemon it cannot replace.
            let forwarding = BridgeForwarding::default();
            assert_eq!(forwarding.mode(), BridgeMode::Negotiating);
            let supplemented =
                negotiate_bridge_capabilities(hello_frame(&["files-v1"]), &forwarding);
            assert_eq!(forwarding.mode(), BridgeMode::Forwarding);
            assert_eq!(
                hello_capabilities(&supplemented),
                ["files-v1", FORWARD_CAPABILITY, LISTENERS_CAPABILITY]
            );

            // A daemon that serves forwarding itself is left entirely alone,
            // and the bridge goes back to pumping bytes it never inspects.
            let forwarding = BridgeForwarding::default();
            let original = hello_frame(&["files-v1", FORWARD_CAPABILITY, LISTENERS_CAPABILITY]);
            let passed = negotiate_bridge_capabilities(original.clone(), &forwarding);
            assert_eq!(forwarding.mode(), BridgeMode::Passthrough);
            assert_eq!(passed.payload, original.payload);
        }

        #[test]
        fn a_bridge_serves_the_tcp_forwarding_its_daemon_predates() {
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

            let forwarding = Arc::new(BridgeForwarding::default());
            negotiate_bridge_capabilities(hello_frame(&["files-v1"]), &forwarding);
            let client = Arc::new(Mutex::new(Vec::<u8>::new()));
            let stream_id = stream::MEDIA_BASE + 11;
            let mut inbound = Vec::new();
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
            .write_to(&mut inbound)
            .unwrap();
            Frame::data(stream_id, 0, b"ping", false)
                .write_to(&mut inbound)
                .unwrap();
            Frame::json(FrameKind::Request, 0, 41, &DaemonRequest::ListTcpListeners)
                .unwrap()
                .write_to(&mut inbound)
                .unwrap();
            // Everything forwarding does not own still belongs to the daemon.
            Frame::json(FrameKind::Request, 0, 42, &DaemonRequest::Ping)
                .unwrap()
                .write_to(&mut inbound)
                .unwrap();

            let mut daemon = Vec::new();
            pump_client_frames(&mut inbound.as_slice(), &mut daemon, &forwarding, &client).unwrap();

            let mut daemon = daemon.as_slice();
            let passed = Frame::read_from(&mut daemon).unwrap().unwrap();
            assert_eq!(passed.request_id, 42);
            assert!(Frame::read_from(&mut daemon).unwrap().is_none());

            // The upstream reply arrives on the socket thread, so the frames
            // the bridge wrote are read back until the echo shows up.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut opened = false;
            let mut listeners = false;
            let mut echoed = Vec::new();
            while echoed != b"pong" {
                assert!(Instant::now() < deadline, "the bridge never echoed");
                let written = client
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let mut written = written.as_slice();
                opened = false;
                listeners = false;
                echoed.clear();
                while let Some(frame) = Frame::read_from(&mut written).unwrap() {
                    match frame.kind {
                        FrameKind::OpenStream if frame.stream_id == stream_id => opened = true,
                        FrameKind::Response if frame.request_id == 41 => listeners = true,
                        FrameKind::Data if frame.stream_id == stream_id => {
                            echoed.extend_from_slice(&frame.decoded_payload().unwrap());
                        }
                        _ => {}
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(opened, "the forwarded stream was never acknowledged");
            assert!(listeners, "the listener request was left for the daemon");
            upstream_handle.join().unwrap();
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

        #[test]
        fn set_label_renames_a_live_session_in_memory_and_on_disk() {
            let (client, server) = UnixStream::pair().unwrap();
            let state = test_state("setlabel-live");
            let paths = state.paths.clone();
            let writer = Arc::new(Mutex::new(server));
            let session_id = "muxloomd-terminal-setlabel";

            handle_request(
                &writer,
                &state,
                1,
                DaemonRequest::Launch {
                    session_id: session_id.into(),
                    kind: "terminal".into(),
                    path: "/tmp".into(),
                    label: "before rename".into(),
                    temporary: false,
                    executable: "/bin/cat".into(),
                    args: vec![],
                    environment: vec![],
                    created_at: 1,
                    columns: 80,
                    rows: 24,
                    parent: None,
                    powers: None,
                    initial_prompt: None,
                },
            )
            .unwrap();

            // Padding and a control character ride in and are stripped out.
            handle_request(
                &writer,
                &state,
                2,
                DaemonRequest::SetLabel {
                    session_id: session_id.into(),
                    label: "  now the head name\u{7}  ".into(),
                },
            )
            .unwrap();

            // The snapshot the dashboard reads carries the new name.
            assert_eq!(
                daemon_session(&state, session_id).unwrap().snapshot().label,
                "now the head name"
            );
            // And it reached the disk, so a restarted daemon keeps it.
            let on_disk: DaemonSession = serde_json::from_slice(
                &fs::read(paths.sessions.join(format!("{session_id}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(on_disk.label, "now the head name");

            // An id nobody knows is an error, not a silent ack.
            assert!(
                handle_request(
                    &writer,
                    &state,
                    3,
                    DaemonRequest::SetLabel {
                        session_id: "muxloomd-terminal-nobody".into(),
                        label: "x".into(),
                    },
                )
                .is_err()
            );

            daemon_session(&state, session_id).unwrap().stop().unwrap();
            drop(client);
            discard_root(paths.root);
        }

        #[test]
        fn set_label_renames_a_persisted_session_on_disk() {
            let initial = test_state("setlabel-persisted");
            let paths = initial.paths.clone();
            drop(initial);
            let session_id = "muxloomd-claude-setlabel";
            let metadata_path = paths.sessions.join(format!("{session_id}.json"));
            persist_session_metadata(
                &metadata_path,
                &DaemonSession {
                    id: session_id.into(),
                    kind: "claude".into(),
                    path: "/tmp/project".into(),
                    label: "original".into(),
                    temporary: false,
                    created_at: 42,
                    archived_at: None,
                    pid: None,
                    dead: true,
                    archived: true,
                    recap: None,
                    title: None,
                    thread: None,
                    seed: None,
                    first_prompt: None,
                    resumed_from: None,
                    resumed_to: None,
                    working: false,
                    needs_attention: false,
                    attention_reason: None,
                    composer: None,
                    parent: None,
                    powers: None,
                },
            )
            .unwrap();

            // A fresh daemon that reloaded the persisted session can rename it.
            let state = Arc::new(DaemonState::new(paths.clone(), KeeperMode::InProcess));
            let (client, server) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(server));
            handle_request(
                &writer,
                &state,
                1,
                DaemonRequest::SetLabel {
                    session_id: session_id.into(),
                    label: "  renamed afterwards  ".into(),
                },
            )
            .unwrap();

            let reloaded: DaemonSession =
                serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
            assert_eq!(reloaded.label, "renamed afterwards");

            drop(client);
            discard_root(paths.root);
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

    pub fn stop(_: &DaemonPaths) -> Result<()> {
        bail!("muxloomd is currently supported on Unix targets")
    }

    pub fn request_status(_: &DaemonPaths) -> Result<crate::daemon_protocol::DaemonResponse> {
        bail!("muxloomd is currently supported on Unix targets")
    }
}

#[cfg(not(unix))]
pub use unsupported::*;
