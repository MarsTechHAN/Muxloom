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
            atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result, anyhow, bail};

    use crate::{
        daemon_protocol::{
            DATA_CHUNK_SIZE, DaemonHistoryMatch, DaemonRequest, DaemonResponse, DaemonSession,
            Frame, FrameKind, INITIAL_STREAM_WINDOW, OpenStream, PROTOCOL_VERSION, StreamOpened,
            Trigger, TriggerAction, stream,
        },
        keeper,
        model::{
            AgentKind, DirectoryListing, FileEntry, FileEntryKind, FileListing, FilePreview,
            FilePreviewKind,
        },
        native_history::SessionFacts as NativeFacts,
        recap::extract_recap,
        relay::{RELAY_CAPABILITY, RelayQueue},
        runtime::{
            DAEMON_SESSION_PREFIX, agent_is_working, attention_reason, is_temporary_session_id,
        },
        talk::{
            DIRECT_CAPABILITY, TALK_CAPABILITY, TalkDeliver, TalkDraft, TalkKind, TalkMessage,
            TalkPage, TalkQueued, TalkStore, folded, paste_bytes, render_delivery,
        },
        terminal_session::{
            CodexActivity, render_history_rows, render_scrollback_seed, resize_parser,
        },
    };

    const RECENT_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
    /// How recently the PTY must have produced output for a session to count
    /// as working. Working CLIs repaint their spinners far more often.
    const WORKING_OUTPUT_FRESHNESS_MS: u64 = 15_000;
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
    /// The most messages that may be waiting for sessions to be free. Past
    /// this something is queueing faster than the machine can read, and the
    /// oldest of them are already stale.
    const OUTBOX_LIMIT: usize = 128;
    /// How often a queued message looks at whether its session has finished.
    const OUTBOX_POLL: Duration = Duration::from_secs(1);
    /// How often a session's runtime is asked what it has been writing about
    /// itself. Soon enough that a finished turn shows up under the session
    /// while whoever asked for it is still looking, rarely enough that the
    /// reading costs nothing.
    const NATIVE_POLL: Duration = Duration::from_secs(5);
    /// How long a session may go on producing output over a transcript that
    /// has stopped growing before muxloom takes it to be writing somewhere
    /// else. Clearing a conversation does exactly that: the old transcript is
    /// closed where it stands and a new one begins.
    const NATIVE_CLAIM_STALE_MS: u64 = 60_000;

    #[derive(Debug, Clone)]
    pub struct DaemonPaths {
        pub root: PathBuf,
        pub socket: PathBuf,
        pub pid: PathBuf,
        pub log: PathBuf,
        pub generation: PathBuf,
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
                log: root.join("muxloomd.log"),
                generation: root.join("muxloomd.generation"),
                history: root.join("history"),
                sessions: root.join("sessions"),
                keepers: root.join("keepers"),
                scratch: root.join("scratch"),
                triggers: root.join("triggers.json"),
                outbox: root.join("talk/outbox.json"),
                talk: root.join("talk"),
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
        keeper_mode: KeeperMode,
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
        /// When the PTY last produced output (epoch ms). Both CLIs repaint
        /// their spinner continuously while working, so a screen that still
        /// says "working" over a quiet PTY is a leftover, not a state.
        last_output: AtomicU64,
        /// Shared with [`DaemonState::attention_patterns`].
        attention_patterns: Arc<Mutex<Vec<String>>>,
        subscribers: Mutex<HashMap<u64, Subscriber>>,
        screen: Mutex<vt100::Parser>,
        codex_activity: Mutex<CodexActivity>,
        recent_output: Mutex<Vec<u8>>,
        /// What a `notify` trigger left for whoever looks next. It reads as an
        /// attention reason until someone types into the session, which is the
        /// one signal that the message was seen.
        notice: Mutex<Option<String>>,
        /// The transcript the runtime in this session is writing about itself,
        /// once the folder has been looked at.
        native: Mutex<NativeLink>,
        history_path: PathBuf,
        metadata_path: PathBuf,
        archived: AtomicBool,
        line_count: AtomicUsize,
        columns: AtomicU16,
        rows: AtomicU16,
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
                keeper_mode,
                attention_patterns: Arc::new(Mutex::new(Vec::new())),
                triggers: Mutex::new(triggers),
                armed,
                talk: OnceLock::new(),
                outbox: Mutex::new(outbox),
                pending,
                draining_outbox: AtomicBool::new(false),
                directs: Mutex::new(HashMap::new()),
                relay: Mutex::new(RelayQueue::default()),
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
            .screen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .screen()
            .contents()
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
        let mut fired = Vec::new();
        {
            let mut triggers = state
                .triggers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !triggers
                .iter()
                .any(|armed| armed.spec.session_id == session_id)
            {
                return;
            }
            // Only now, with a trigger known to be watching, is rendering the
            // screen worth it.
            let screen = session
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .screen()
                .contents()
                .to_lowercase();
            let now = now_ms();
            let mut changed = false;
            triggers.retain_mut(|armed| {
                if armed.spec.session_id != session_id {
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
        let message = state.talk()?.post(draft)?;
        let body = render_delivery(&message, reply_expected);
        // A session waiting on a prompt is not free: the paste would be typed
        // into whatever question is on the screen and answer it with this.
        let free = !snapshot.working && !snapshot.needs_attention;
        if deliver == TalkDeliver::Now || free {
            return Ok(match type_message(&session, kind, &body) {
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
            outbox.push(TalkQueued {
                message_id: message.id.clone(),
                session_id: to.session_id,
                body,
                queued_at: now_ms(),
                deliver,
            });
            state.save_outbox(&outbox);
        }
        spawn_outbox_drainer(state);
        let reason = match deliver {
            TalkDeliver::WhenIdle => "the session is busy; the message goes in when it is free",
            _ => "the session is busy; the message goes in when it is free, or in a minute anyway",
        };
        Ok((message, "queued".into(), Some(reason.into())))
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

    /// Type a rendered message into a session as one submission.
    ///
    /// Codex and Claude Code both understand bracketed paste, which is what
    /// gets a multi-line envelope into the prompt whole; typed as bytes, every
    /// newline in it would submit what came before. The other runtimes are not
    /// known to, so they are told the same thing folded onto one line: uglier,
    /// and still one message rather than eight.
    fn type_message(session: &ManagedSession, kind: AgentKind, body: &str) -> Result<()> {
        let bytes = if matches!(kind, AgentKind::Codex | AgentKind::Claude) {
            paste_bytes(body, true)
        } else {
            let mut folded = folded(body).into_bytes();
            folded.push(b'\r');
            folded
        };
        session.write_input(&bytes)
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
        {
            let mut outbox = state
                .outbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = outbox.len();
            outbox.retain(|queued| {
                let Some(session) = sessions.get(&queued.session_id) else {
                    eprintln!(
                        "muxloomd dropped message {}: session {} is gone",
                        queued.message_id, queued.session_id
                    );
                    return false;
                };
                let snapshot = session.snapshot();
                let kind = snapshot.kind.parse::<AgentKind>().ok();
                let Some(kind) = kind.filter(|_| !snapshot.dead && !snapshot.archived) else {
                    eprintln!(
                        "muxloomd dropped message {}: session {} has ended",
                        queued.message_id, queued.session_id
                    );
                    return false;
                };
                if queued.due(!snapshot.working && !snapshot.needs_attention, now) {
                    due.push((Arc::clone(session), kind, queued.clone()));
                    return false;
                }
                if queued.expired(now) {
                    eprintln!(
                        "muxloomd gave up on message {}: session {} stayed busy",
                        queued.message_id, queued.session_id
                    );
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
                eprintln!(
                    "muxloomd could not deliver message {}: {error:#}",
                    queued.message_id
                );
            }
        }
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
                pid: None,
                dead: true,
                archived: true,
                recap,
                // Nothing is left to say who the session was talking to: the
                // metadata that recorded its folder is what went missing.
                title: None,
                thread: None,
                working: false,
                needs_attention: false,
                attention_reason: None,
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
            while !state.shutdown.load(Ordering::Acquire) && !signalled.load(Ordering::Acquire) {
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
        })();
        // Sessions are not retired here: their keepers own them, keep writing
        // their histories, and hand them to whichever daemon serves next.
        result
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
            DaemonRequest::RelaySubmit { tool, arguments } => {
                let id = state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .submit(&tool, &arguments, crate::relay::now_ms())?;
                write_response(writer, request_id, &DaemonResponse::RelayTicket { id })
            }
            DaemonRequest::RelayPoll => {
                let jobs = state
                    .relay
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .poll(crate::relay::now_ms());
                write_response(writer, request_id, &DaemonResponse::RelayWork { jobs })
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
                remove_scratch_dir(&state.paths, &session_id);
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
        // Live sessions no longer defer a handover: every session this daemon
        // serves is owned by its keeper process, which survives the drain and
        // is adopted by the next generation.
        state.draining.store(true, Ordering::Release);
        true
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
    /// which is what pairs it off against a transcript.
    type NativeGroup = Vec<(u64, Arc<ManagedSession>)>;

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
                .push((created_at, session));
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
            }
            // Outside the lock: persisting takes a snapshot, and a snapshot
            // reads the claim that was just made.
            if let Err(error) = session.persist_metadata() {
                eprintln!("muxloomd could not record what a session is reading: {error:#}");
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
                Some(claim) => Ok((claim.id.clone(), claim.path.clone(), claim.read_at)),
                None => Err(native.scanned_at),
            }
        };
        let last_output = session.last_output.load(Ordering::Relaxed);
        let (id, path, read_at) = match claimed {
            Ok(claim) => claim,
            // Never matched. A session that has produced nothing since the
            // last look cannot have started writing a transcript either.
            Err(scanned_at) => return scanned_at == 0 || last_output > scanned_at,
        };
        let written = crate::native_history::last_written(&path).unwrap_or_default();
        if written > read_at {
            let Some(thread) = crate::native_history::reread(kind, &path) else {
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
        // The transcript has stopped growing while the session goes on
        // talking: whatever it is saying is being written somewhere else.
        // Clearing a conversation does precisely this.
        if last_output <= written.saturating_add(NATIVE_CLAIM_STALE_MS) {
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
        keeper_environment.push(("MUXLOOM_SESSION_KIND".into(), kind.clone()));
        if !label.trim().is_empty() {
            keeper_environment.push(("MUXLOOM_SESSION_LABEL".into(), label.clone()));
        }
        let history_path = state.paths.history.join(format!("{session_id}.ansi"));
        let metadata_path = state.paths.sessions.join(format!("{session_id}.json"));
        if !temporary {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&history_path)?;
        }
        // The command line is the one place that says outright which
        // conversation this launch means to reopen, and it is about to be
        // handed to the keeper.
        let seed = kind
            .parse::<AgentKind>()
            .ok()
            .and_then(|kind| crate::native_history::resume_seed(kind, &args));
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
        let mut metadata = DaemonSession {
            id: session_id.clone(),
            kind,
            path,
            label,
            temporary,
            created_at,
            pid: None,
            dead: false,
            archived: false,
            recap: None,
            title: None,
            thread: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
        };
        // The record precedes the keeper so a crash between the two leaves a
        // session that can be retired, never a keeper nothing knows about.
        persist_session_metadata(&metadata_path, &metadata)?;
        let (stream, status) = match start_keeper(state, &spec) {
            Ok(connection) => connection,
            Err(error) => {
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
        let session = Arc::new(ManagedSession {
            metadata: Mutex::new(metadata),
            keeper: Mutex::new(
                stream
                    .try_clone()
                    .context("failed to clone keeper stream")?,
            ),
            last_output: AtomicU64::new(now_ms()),
            attention_patterns: Arc::clone(&state.attention_patterns),
            subscribers: Mutex::new(HashMap::new()),
            screen: Mutex::new(vt100::Parser::new(rows.max(5), columns.max(20), 0)),
            codex_activity: Mutex::new(CodexActivity::default()),
            recent_output: Mutex::new(Vec::new()),
            notice: Mutex::new(None),
            native: Mutex::new(NativeLink {
                seed,
                ..NativeLink::default()
            }),
            history_path,
            metadata_path,
            archived: AtomicBool::new(false),
            line_count: AtomicUsize::new(0),
            columns: AtomicU16::new(columns.max(20)),
            rows: AtomicU16::new(rows.max(5)),
        });
        // Output the child produced before the keeper connection existed —
        // the first prompt of a fast-starting agent — is only in the history
        // file. The greeting's byte count splits the transcript exactly, so
        // replaying that prefix leaves the ring gapless and duplicate-free.
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
        spawn_session_reader(state, Arc::clone(&session), stream);
        Ok(session)
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
    /// keeper appends history itself, so output here only feeds the screen,
    /// the retained ring, and the attached subscribers.
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
                let _ = fs::remove_file(&session.metadata_path);
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

    /// Spawn the detached `muxloomd keeper` that owns one session. The spec
    /// travels over stdin so environment values never appear in `ps`.
    fn spawn_keeper_process(state: &DaemonState, spec: &keeper::KeeperSpec) -> Result<()> {
        let executable = std::env::current_exe().context("failed to find muxloomd executable")?;
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
        let columns = status.columns.max(20);
        let rows = status.rows.max(5);
        let session = Arc::new(ManagedSession {
            metadata: Mutex::new(metadata),
            keeper: Mutex::new(
                stream
                    .try_clone()
                    .context("failed to clone keeper stream")?,
            ),
            last_output: AtomicU64::new(now_ms()),
            attention_patterns: Arc::clone(&state.attention_patterns),
            subscribers: Mutex::new(HashMap::new()),
            screen: Mutex::new(vt100::Parser::new(rows, columns, 0)),
            codex_activity: Mutex::new(CodexActivity::default()),
            recent_output: Mutex::new(Vec::new()),
            notice: Mutex::new(None),
            // The command line that started this one belongs to a keeper this
            // daemon did not spawn. What the last generation had matched it to
            // is in the metadata, and the first refresh picks it up from
            // there.
            native: Mutex::new(NativeLink::default()),
            history_path,
            metadata_path,
            archived: AtomicBool::new(archived),
            line_count: AtomicUsize::new(0),
            columns: AtomicU16::new(columns),
            rows: AtomicU16::new(rows),
        });
        // Rebuild the screen, the retained ring, and with them the
        // working/attention classification from the transcript the keeper
        // kept appending while no daemon was watching. The greeting's byte
        // count bounds the read so nothing streaming in now is doubled.
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
            if let Ok(kind) = snapshot.kind.parse::<AgentKind>() {
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
                // A shell gets no recap, and its scrollback is megabytes: copy
                // it out only for a runtime that has something to say - and
                // only when its own account of itself is not to be had.
                snapshot.recap = native_recap.or_else(|| {
                    (kind != AgentKind::Terminal)
                        .then(|| {
                            let recent = self
                                .recent_output
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            extract_recap(kind, &String::from_utf8_lossy(&recent))
                        })
                        .flatten()
                });
                if snapshot.dead || snapshot.archived {
                    snapshot.pid = None;
                    snapshot.working = false;
                    snapshot.needs_attention = false;
                    snapshot.attention_reason = None;
                } else {
                    let patterns = self
                        .attention_patterns
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    snapshot.attention_reason = attention_reason(kind, &visible_screen, &patterns);
                    snapshot.needs_attention = snapshot.attention_reason.is_some();
                    // Both CLIs repaint their spinner continuously while
                    // working; a screen still claiming so over a PTY that has
                    // gone quiet is a leftover of an ended or wedged turn.
                    let fresh = now_ms().saturating_sub(self.last_output.load(Ordering::Relaxed))
                        < WORKING_OUTPUT_FRESHNESS_MS;
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

        fn persist_metadata(&self) -> Result<()> {
            persist_session_metadata(&self.metadata_path, &self.snapshot())
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
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&columns.max(20).to_be_bytes());
            payload[2..].copy_from_slice(&rows.max(5).to_be_bytes());
            self.keeper_frame(keeper::frame::RESIZE, &payload)
        }

        fn write_input(&self, bytes: &[u8]) -> Result<()> {
            // Someone is typing here, so whatever a trigger wanted looked at
            // has been looked at.
            self.notice
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            self.keeper_frame(keeper::frame::DATA, bytes)
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
        let mut window = least.max(1);
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
            if (actual_offset >= offset_from_bottom && filled) || start == 0 || window >= most {
                return Ok(HistoryRead {
                    rows: page,
                    total_lines,
                    offset_from_bottom: actual_offset,
                    reached_start: start == 0,
                });
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
    ) -> Result<HistoryRead> {
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
        Ok(HistoryRead {
            rows: output,
            total_lines,
            offset_from_bottom: actual_offset,
            // Raw reads count the whole log, so `total_lines` already measures
            // the session rather than the reach of one page.
            reached_start: true,
        })
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

    fn open_bridge_tcp<W: Write + Send + 'static>(
        frame: &Frame,
        forwarding: &Arc<BridgeForwarding>,
        client: &Arc<Mutex<W>>,
        host: &str,
        port: u16,
    ) -> Result<()> {
        let socket = match TcpStream::connect((host, port)) {
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

    pub fn connect_or_start(paths: &DaemonPaths) -> Result<UnixStream> {
        if let Ok(mut stream) = UnixStream::connect(&paths.socket) {
            if generation_is_current_after_settling(paths) {
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
    fn generation_is_current_after_settling(paths: &DaemonPaths) -> bool {
        if running_generation_is_current(paths) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
        running_generation_is_current(paths)
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
                output = session
                    .recent_output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
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
        fn generation_handover_requires_a_sole_client_but_not_idle_sessions() {
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
            drop(second);
            second_handle.join().unwrap().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while state.clients.load(Ordering::Relaxed) > 1 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                prepare_handover(&state),
                "a live keeper session must not defer the handover"
            );

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
            )
            .unwrap();
            session.record_output("\x1b[2J\x1b[H• Working (2s • esc to interrupt)".as_bytes());
            assert!(session.snapshot().working);

            // A screen still claiming work over a PTY that has gone quiet is a
            // leftover of an ended turn, not a state.
            session.last_output.store(
                now_ms().saturating_sub(WORKING_OUTPUT_FRESHNESS_MS + 1),
                Ordering::Relaxed,
            );
            assert!(!session.snapshot().working);
            session.last_output.store(now_ms(), Ordering::Relaxed);
            assert!(session.snapshot().working);

            // Patterns a controller sank down classify waiting on the
            // daemon's own snapshots, custom wording included.
            session.record_output(b"\x1b[2J\x1b[Hgpu quota approval needed");
            *state
                .attention_patterns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec!["gpu quota approval".into()];
            let snapshot = session.snapshot();
            assert!(snapshot.needs_attention);
            assert_eq!(
                snapshot.attention_reason.as_deref(),
                Some("gpu quota approval")
            );

            session.archive().unwrap();
            fs::remove_dir_all(root).unwrap();
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

        /// A conversation cleared with `/clear` is closed where it stands and
        /// a new file begins. Nothing says so; the session simply goes on
        /// talking over a transcript that has stopped growing.
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
            )
            .unwrap();

            let transcript = root.join("thread-1.jsonl");
            fs::write(&transcript, "{}\n").unwrap();
            let written = crate::native_history::last_written(&transcript).unwrap();
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

            session.last_output.store(written, Ordering::Relaxed);
            assert!(
                !refresh_native_claim(AgentKind::Claude, &session),
                "a session that has only just spoken is still on its thread"
            );
            assert_eq!(
                session.snapshot().title.as_deref(),
                Some("the conversation before")
            );

            session.last_output.store(
                written
                    .saturating_add(NATIVE_CLAIM_STALE_MS)
                    .saturating_add(1),
                Ordering::Relaxed,
            );
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
            fs::remove_dir_all(paths.root).unwrap();
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
            fs::remove_dir_all(paths.root).unwrap();
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
                    title: None,
                    thread: None,
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

            fs::remove_dir_all(paths.root).unwrap();
        }

        fn live_metadata(session_id: &str, kind: &str, pid: Option<u32>) -> DaemonSession {
            DaemonSession {
                id: session_id.into(),
                kind: kind.into(),
                path: "/tmp/project".into(),
                label: "interrupted work".into(),
                temporary: false,
                created_at: 7,
                pid,
                dead: false,
                archived: false,
                recap: None,
                title: None,
                thread: None,
                working: true,
                needs_attention: false,
                attention_reason: None,
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
            fs::remove_dir_all(paths.root).unwrap();
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
            fs::remove_dir_all(paths.root).unwrap();
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
            fs::remove_dir_all(paths.root).unwrap();
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
            fs::remove_dir_all(paths.root).unwrap();
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
                output = adopted
                    .recent_output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
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
