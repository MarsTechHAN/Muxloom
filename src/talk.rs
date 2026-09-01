//! The talk board: what agents and humans say to one another, kept where
//! everyone can read it.
//!
//! Every machine muxloom reaches holds the same log, and the log is
//! append-only: a message is minted once, by the machine it was written on,
//! and every other machine only ever adds it. Nothing is a queue and nothing
//! is a master copy, which is what makes replication safe in a star topology
//! where two machines never speak to each other directly — the controller can
//! carry entries in any order, twice, or late, and every machine still ends up
//! with the same board.
//!
//! A machine tracks what it has with a version vector: the highest run of
//! sequence numbers it holds unbroken for each origin. Two machines diff their
//! vectors to work out what to send. Clocks are only ever used for display and
//! ordering, never for deciding what is missing.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap},
    fmt,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    bridge::BridgePool,
    debug,
    model::{Composer, Target, Transport},
    runtime::Runtime,
};

/// Capability that says a daemon carries a talk board.
pub const TALK_CAPABILITY: &str = "talk-v1";
/// Capability that says a daemon will put a message in front of one of its
/// agent sessions.
pub const DIRECT_CAPABILITY: &str = "direct-v1";
/// The longest a single message may be. A board is for saying things to each
/// other, not for shipping files around.
pub const MAX_TEXT: usize = 8 * 1024;
/// How many messages one origin keeps before the oldest are compacted away.
const RETAIN_PER_ORIGIN: usize = 20_000;
/// How long a message is kept regardless of how few there are.
const RETAIN_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// How far past the retention limit a log may run before it is rewritten.
/// Compaction rewrites a whole file, so it must not happen on every post.
const COMPACT_SLACK: usize = 512;
/// The most messages one read or one replication fetch carries.
pub const MAX_PAGE: usize = 500;

/// Where a message belongs. `machine` is an origin key, not a display name, so
/// a machine that gets renamed keeps its board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum TalkScope {
    /// Everyone, everywhere.
    Global,
    /// Everyone working on one machine.
    Machine { machine: String },
    /// Everyone working in one directory on one machine — the default, and the
    /// one that behaves like a project channel.
    Path { machine: String, path: String },
    /// Everyone working on one task: an agent, every subagent it started, and
    /// everything those started in turn. `root_session` is the session at the
    /// top of that chain, and it is the same id on every machine the task
    /// reaches — a subagent started across a machine boundary is still part of
    /// the same piece of work.
    Task {
        machine: String,
        root_session: String,
    },
}

impl TalkScope {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Machine { .. } => "machine",
            Self::Path { .. } => "path",
            Self::Task { .. } => "task",
        }
    }

    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Machine { machine } | Self::Path { machine, .. } | Self::Task { machine, .. } => {
                Some(machine)
            }
        }
    }

    /// The task a message belongs to, when it was said to one.
    pub fn task(&self) -> Option<&str> {
        match self {
            Self::Task { root_session, .. } => Some(root_session),
            _ => None,
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Path { path, .. } => Some(path),
            _ => None,
        }
    }

    /// Fill in the machine a scope was posted from. A poster names the scope it
    /// wants and leaves the machine empty; the daemon that mints the message is
    /// the one that knows its own origin key.
    fn anchor(&mut self, origin: &str) {
        match self {
            Self::Global => {}
            Self::Machine { machine } | Self::Path { machine, .. } | Self::Task { machine, .. } => {
                if machine.trim().is_empty() {
                    *machine = origin.to_string();
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TalkKind {
    /// Something said to whoever is reading the scope.
    Message,
    /// Something written down to be found later: the board doubles as the
    /// memory a session cannot keep.
    Note,
    /// A message delivered straight into one session, recorded here so the
    /// board shows what agents said to each other and delivery can be audited.
    Direct,
}

impl TalkKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Note => "note",
            Self::Direct => "direct",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "message" => Self::Message,
            "note" => Self::Note,
            "direct" => Self::Direct,
            other => bail!("unknown talk kind {other}: use message, note, or direct"),
        })
    }
}

/// Who is speaking, as far as the board can tell. Everything but the machine
/// comes from the session the poster runs in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkVoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `codex`, `claude`, `terminal`, or nothing when the poster is not a
    /// muxloom session at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether a person wrote this rather than an agent. Posting as a human is
    /// the dashboard's privilege; the MCP surface never sets it.
    #[serde(default)]
    pub human: bool,
    /// The chat channel a person wrote this from, when they wrote it from one.
    ///
    /// A person on WeChat is not reading the talk board, and never will be, so
    /// an answer left there is an answer nobody receives. This is the id
    /// `send_channel_message` takes, carried the whole way in so the envelope
    /// can hand the reader the one call that reaches back to where the message
    /// came from. Empty for a person typing at a dashboard, where the board
    /// really is what they are looking at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The reader's own earlier message that this one quotes, in the chat
    /// app's numbering — set only when the person answered by quoting it.
    ///
    /// It rides with the return address because that is what it is for: an
    /// answer carrying it is drawn under the same exchange in the app, so the
    /// person reading a day of messages can see what was answered. It used to
    /// be pasted into the message body, which quietly rewrote what the person
    /// had written; the words are theirs and are left alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_quote: Option<String>,
}

impl TalkVoice {
    /// How this voice reads on a board: the label if it has one, else the
    /// session it speaks from, else what kind of thing it is.
    pub fn name(&self) -> String {
        if let Some(label) = self.label.as_ref().filter(|label| !label.is_empty()) {
            return label.clone();
        }
        if let Some(session) = self.session_id.as_ref() {
            return session.clone();
        }
        self.kind.clone().unwrap_or_else(|| "someone".into())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkAuthor {
    /// Origin key of the machine that minted the message.
    pub machine: String,
    /// What that machine was called at the time, for reading.
    #[serde(default)]
    pub machine_label: String,
    #[serde(flatten)]
    pub voice: TalkVoice,
}

/// One session, anywhere: who a direct message is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkAddress {
    pub machine: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkMessage {
    /// `<origin>:<seq>` — unique everywhere, and the key that makes a replayed
    /// message a no-op rather than a duplicate.
    pub id: String,
    pub origin: String,
    pub seq: u64,
    /// Epoch milliseconds on the minting machine. Display and ordering only:
    /// two machines' clocks disagree, and nothing here depends on them.
    pub ts: u64,
    pub scope: TalkScope,
    pub author: TalkAuthor,
    pub kind: TalkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<TalkAddress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub text: String,
}

impl TalkMessage {
    /// Whether a message that arrived from another machine can be filed.
    ///
    /// The origin becomes a file name and the id is the identity everything
    /// else keys on, so both are checked before anything is written.
    fn acceptable(&self) -> bool {
        valid_origin(&self.origin)
            && self.seq > 0
            && self.id == format!("{}:{}", self.origin, self.seq)
            && self.text.len() <= MAX_TEXT
            && self.author.machine.len() <= 128
    }
}

/// What a poster hands to the store. The store mints the rest.
///
/// Every machine field may be left empty, and the daemon that takes the draft
/// fills it in with its own: a poster on this machine does not have to know
/// what this machine is called, and a message sent to a session here is
/// addressed here whatever the sender believes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkDraft {
    pub scope: TalkScope,
    /// Who is speaking. An author from another machine carries its own machine
    /// with it; one from this machine leaves it empty.
    #[serde(default)]
    pub author: TalkAuthor,
    pub kind: TalkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<TalkAddress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub text: String,
}

/// The highest unbroken sequence number held for each origin.
pub type TalkVector = BTreeMap<String, u64>;

/// What one machine holds, as told to another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkState {
    pub origin: String,
    pub label: String,
    pub vector: TalkVector,
    /// The oldest sequence number still held per origin. A peer whose vector
    /// sits below this has missed messages for good, and saying so is kinder
    /// than pretending the gap can be filled.
    #[serde(default)]
    pub low_water: TalkVector,
}

/// Which machines or paths a read reaches over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "select", rename_all = "snake_case")]
pub enum TalkSelector {
    /// Only where the reader is: the point of scopes.
    #[default]
    Mine,
    /// Everywhere, which is how the board is searched.
    All,
    /// A named few, matched against origin keys, machine labels, or paths.
    Only { names: Vec<String> },
}

impl TalkSelector {
    fn admits(&self, mine: Option<&str>, candidate: &str, label: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Mine => mine.is_some_and(|mine| mine.eq_ignore_ascii_case(candidate)),
            Self::Only { names } => names.iter().any(|name| {
                name.eq_ignore_ascii_case(candidate)
                    || label.is_some_and(|label| name.eq_ignore_ascii_case(label))
            }),
        }
    }
}

/// What to read off the board. Everything defaults to "what is in front of
/// me": the reader's own machine, its own directory, and whatever was said to
/// everyone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkFilter {
    /// Only messages past this vector, which is how a caller polls for new
    /// ones without seeing the board again.
    #[serde(default)]
    pub since: TalkVector,
    /// Restrict to one scope: `global`, `machine`, `path`, `task`, or
    /// `direct`.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Match against an author's session id or label.
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub machines: TalkSelector,
    #[serde(default)]
    pub paths: TalkSelector,
    /// Who is reading, so `Mine` and direct messages mean something. Filled in
    /// from the reader's session; the machine is always the daemon's own.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// The task the reader belongs to: the session at the top of the chain of
    /// subagents it was started from, or its own id when nobody started it.
    /// Filled in from the reader's session the same way `session_id` is, and
    /// for the same reason — which task an agent is part of is not something
    /// it should be able to claim.
    #[serde(default)]
    pub task: Option<String>,
    /// Read backwards from here, for paging into the past.
    #[serde(default)]
    pub before: Option<u64>,
    #[serde(default)]
    pub limit: usize,
    /// Read as the person these machines belong to rather than as a session on
    /// one of them: the dashboard shows what the agents said to each other, so
    /// their messages are not a place to hide things from the human watching.
    /// The MCP surface builds its own filter and never sets this.
    #[serde(default)]
    pub owner: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkPage {
    pub messages: Vec<TalkMessage>,
    /// Everything the board held when this was read. Hand it back as `since`
    /// to be told only what has happened since.
    pub cursor: String,
    /// Whether older messages matched than fit in this page.
    pub truncated: bool,
}

/// The newest `limit` messages of however many are offered, in the order a
/// page reads them: oldest first.
///
/// A page is the tail of what matched, so the walk that finds it never needs
/// more than a page in hand. Holding only that is what keeps reading the board
/// costing a page rather than a copy of everything on it.
///
/// This is the right end to keep for someone arriving at the board and asking
/// what is on it. It is the wrong end for someone following a cursor - see
/// [`OldestFirst`], which is what such a read gets instead.
struct NewestFirst {
    limit: usize,
    /// Oldest first, so the one to drop is the one on top.
    heap: BinaryHeap<Oldest>,
    dropped: bool,
}

/// The oldest `limit` messages of however many are offered, oldest first.
///
/// What a caller following a cursor must be given. Such a caller hands the
/// cursor back and is told only what has happened since, so anything a page
/// leaves out is not paged past - it is lost, and the cursor is what loses it.
/// Keeping the newest end is what did that: the page was the tail of a burst,
/// the cursor was everything the board held, and the messages in between were
/// dropped and then skipped. The one they were most likely to be was an answer
/// somebody was waiting on, because an answer arrives before the noise that
/// buries it.
///
/// Keeping the oldest end instead makes the window walk forwards: each read
/// hands back a contiguous run and a cursor that reaches no further than the
/// end of it, so a backlog drains in order over as many reads as it takes and
/// nothing falls between two of them.
struct OldestFirst {
    limit: usize,
    /// Newest first, so the one to drop is the one on top.
    heap: BinaryHeap<Newest>,
    dropped: bool,
}

/// A message ordered so the *oldest* compares greatest, which is what puts it
/// on top of a [`BinaryHeap`] and first out of a full page.
struct Oldest(TalkMessage);

impl Ord for Oldest {
    fn cmp(&self, other: &Self) -> Ordering {
        (other.0.ts, &other.0.origin, other.0.seq).cmp(&(self.0.ts, &self.0.origin, self.0.seq))
    }
}

impl PartialOrd for Oldest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Oldest {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Oldest {}

/// A message ordered the plain way round, so the *newest* is on top of a
/// [`BinaryHeap`] and first out of a full page.
struct Newest(TalkMessage);

impl Ord for Newest {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.0.ts, &self.0.origin, self.0.seq).cmp(&(other.0.ts, &other.0.origin, other.0.seq))
    }
}

impl PartialOrd for Newest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Newest {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Newest {}

impl OldestFirst {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            heap: BinaryHeap::new(),
            dropped: false,
        }
    }

    /// Take `message` if the page has room or if it is older than the newest
    /// the page is holding.
    fn offer(&mut self, message: &TalkMessage) {
        if self.heap.len() == self.limit {
            let newer_than_held = self.heap.peek().is_some_and(|newest| {
                (message.ts, &message.origin, message.seq)
                    >= (newest.0.ts, &newest.0.origin, newest.0.seq)
            });
            self.dropped = true;
            if newer_than_held {
                return;
            }
            self.heap.pop();
        }
        self.heap.push(Newest(message.clone()));
    }

    fn dropped(&self) -> bool {
        self.dropped
    }

    fn into_page(self) -> Vec<TalkMessage> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|held| held.0)
            .collect()
    }
}

impl NewestFirst {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            heap: BinaryHeap::new(),
            dropped: false,
        }
    }

    /// Take `message` if the page has room or if it is newer than the oldest
    /// the page is holding. Only then is it copied: everything older than a
    /// full page is looked at and let go.
    fn offer(&mut self, message: &TalkMessage) {
        if self.heap.len() == self.limit {
            let older_than_held = self.heap.peek().is_some_and(|oldest| {
                (message.ts, &message.origin, message.seq)
                    <= (oldest.0.ts, &oldest.0.origin, oldest.0.seq)
            });
            self.dropped = true;
            if older_than_held {
                return;
            }
            self.heap.pop();
        }
        self.heap.push(Oldest(message.clone()));
    }

    /// Whether anything matched that the page had no room for.
    fn dropped(&self) -> bool {
        self.dropped
    }

    fn into_page(self) -> Vec<TalkMessage> {
        // `into_sorted_vec` ascends by [`Oldest`]'s order, which is newest
        // first; a page reads the other way round.
        let mut messages: Vec<TalkMessage> = self
            .heap
            .into_sorted_vec()
            .into_iter()
            .map(|held| held.0)
            .collect();
        messages.reverse();
        messages
    }
}

/// The board on one machine.
pub struct TalkStore {
    root: PathBuf,
    inner: Mutex<TalkInner>,
}

struct TalkInner {
    identity: TalkIdentity,
    logs: BTreeMap<String, OriginLog>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TalkIdentity {
    origin: String,
    label: String,
}

#[derive(Default)]
struct OriginLog {
    /// Sorted by seq, gaps allowed: a message can arrive before the one in
    /// front of it and must still be filed.
    messages: Vec<TalkMessage>,
    /// The highest seq compacted away, so a hole left by retention is not
    /// mistaken for one replication can still fill.
    low_water: u64,
}

impl OriginLog {
    /// The highest seq held with nothing missing below it.
    fn contiguous(&self) -> u64 {
        let mut reached = self.low_water;
        for message in &self.messages {
            if message.seq == reached + 1 {
                reached = message.seq;
            } else if message.seq > reached + 1 {
                break;
            }
        }
        reached
    }

    fn highest(&self) -> u64 {
        self.messages
            .last()
            .map_or(self.low_water, |message| message.seq)
    }
}

impl TalkStore {
    /// Open the board under a state directory, minting this machine's identity
    /// the first time.
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("log")).with_context(|| {
            format!("failed to create the talk directory at {}", root.display())
        })?;
        let identity = load_identity(&root)?;
        let logs = load_logs(&root.join("log"));
        let store = Self {
            root,
            inner: Mutex::new(TalkInner { identity, logs }),
        };
        store.compact()?;
        Ok(store)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, TalkInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn origin(&self) -> String {
        self.inner().identity.origin.clone()
    }

    pub fn label(&self) -> String {
        self.inner().identity.label.clone()
    }

    /// Record what this machine is called. The controller is the only thing
    /// that knows the name a human uses for a machine, so it sinks it down and
    /// posts made afterwards carry it.
    pub fn set_label(&self, label: &str) -> Result<()> {
        let label = label.trim();
        if label.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner();
        if inner.identity.label == label {
            return Ok(());
        }
        inner.identity.label = label.to_string();
        let identity = inner.identity.clone();
        drop(inner);
        save_identity(&self.root, &identity)
    }

    pub fn state(&self) -> TalkState {
        let inner = self.inner();
        TalkState {
            origin: inner.identity.origin.clone(),
            label: inner.identity.label.clone(),
            vector: inner
                .logs
                .iter()
                .map(|(origin, log)| (origin.clone(), log.contiguous()))
                .collect(),
            low_water: inner
                .logs
                .iter()
                .filter(|(_, log)| log.low_water > 0)
                .map(|(origin, log)| (origin.clone(), log.low_water))
                .collect(),
        }
    }

    /// Mint a message on this machine and file it.
    pub fn post(&self, mut draft: TalkDraft) -> Result<TalkMessage> {
        let text = draft.text.trim();
        if text.is_empty() {
            bail!("a talk message needs something to say");
        }
        if text.len() > MAX_TEXT {
            bail!(
                "a talk message must be shorter than {MAX_TEXT} bytes; post a summary and leave \
                 the rest where it is"
            );
        }
        let (origin, label) = {
            let inner = self.inner();
            (inner.identity.origin.clone(), inner.identity.label.clone())
        };
        draft.scope.anchor(&origin);
        if draft.author.machine.trim().is_empty() {
            draft.author.machine = origin.clone();
            draft.author.machine_label = label;
        }
        if let Some(to) = draft.to.as_mut()
            && to.machine.trim().is_empty()
        {
            to.machine = origin.clone();
        }
        let mut inner = self.inner();
        let seq = inner
            .logs
            .get(&origin)
            .map_or(0, OriginLog::highest)
            .saturating_add(1);
        let message = TalkMessage {
            id: format!("{origin}:{seq}"),
            origin: origin.clone(),
            seq,
            ts: now_ms(),
            scope: draft.scope,
            author: draft.author,
            kind: draft.kind,
            to: draft.to,
            reply_to: draft.reply_to,
            text: text.to_string(),
        };
        self.file(&mut inner, message.clone())?;
        Ok(message)
    }

    /// File messages that came from somewhere else. Returns how many were new;
    /// filing the same message twice is a no-op, which is what lets the
    /// controller push and pull without tracking what it has already carried.
    pub fn merge(&self, messages: Vec<TalkMessage>) -> Result<usize> {
        let mut inner = self.inner();
        let mut added = 0;
        for message in messages {
            if !message.acceptable() {
                continue;
            }
            if self.file(&mut inner, message)? {
                added += 1;
            }
        }
        Ok(added)
    }

    /// Everything held past a peer's vector, oldest first, so a peer that is
    /// far behind catches up over several fetches rather than one huge one.
    ///
    /// A batch is at most [`MAX_PAGE`] and the board may hold tens of
    /// thousands, so the walk keeps only the end it is going to hand back
    /// rather than copying every message it looked at to sort and throw away.
    /// This runs on every sync round against every reachable machine, which is
    /// the one place on the board where the cost of the whole board is paid
    /// over and over.
    pub fn since(&self, from: &TalkVector, limit: usize) -> Vec<TalkMessage> {
        let inner = self.inner();
        let mut page = OldestFirst::new(limit.clamp(1, MAX_PAGE));
        for (origin, log) in &inner.logs {
            let held = from.get(origin).copied().unwrap_or(0);
            // An origin is sorted by seq, so where the peer's knowledge ends
            // is found rather than walked to. The common case by far is a peer
            // that is already up to date, and that case should not cost a pass
            // over everything the origin has ever said.
            let fresh = log.messages.partition_point(|message| message.seq <= held);
            for message in &log.messages[fresh..] {
                page.offer(message);
            }
        }
        page.into_page()
    }

    /// Read the board as someone standing on this machine sees it.
    pub fn read(&self, filter: &TalkFilter) -> TalkPage {
        let inner = self.inner();
        let mine = inner.identity.origin.clone();
        let label = inner.identity.label.clone();
        let query = filter.query.as_ref().map(|query| query.to_lowercase());
        let limit = if filter.limit == 0 {
            50
        } else {
            filter.limit.min(MAX_PAGE)
        };
        // A board holds up to [`RETAIN_PER_ORIGIN`] messages per machine and a
        // page carries at most a few dozen, so the walk keeps only the end it
        // is going to hand back rather than collecting every match to sort and
        // throw away. What that saves is a copy of each message it looked at -
        // and a read of the whole board is what a caller waiting for an answer
        // does every couple of seconds.
        //
        // Which end depends on what the caller is doing. Arriving at the board
        // and asking what is on it wants the newest, with `before` to page
        // back. Following a cursor wants the oldest of what is new, because
        // that caller will hand the cursor back and never be offered anything
        // this page left out.
        let following = !filter.since.is_empty();
        let mut newest = NewestFirst::new(limit);
        let mut oldest = OldestFirst::new(limit);
        for message in inner.logs.values().flat_map(|log| log.messages.iter()) {
            if message.seq <= filter.since.get(&message.origin).copied().unwrap_or(0)
                || !filter.before.is_none_or(|before| message.ts < before)
                || !visible(message, filter, &mine, &label)
                || !(filter.kinds.is_empty()
                    || filter.kinds.iter().any(|kind| kind == message.kind.name()))
                || !(filter.authors.is_empty()
                    || filter.authors.iter().any(|author| {
                        message
                            .author
                            .voice
                            .session_id
                            .as_deref()
                            .is_some_and(|id| id.eq_ignore_ascii_case(author))
                            || message
                                .author
                                .voice
                                .label
                                .as_deref()
                                .is_some_and(|label| label.eq_ignore_ascii_case(author))
                    }))
            {
                continue;
            }
            if query
                .as_ref()
                .is_some_and(|query| !message.text.to_lowercase().contains(query))
            {
                continue;
            }
            if following {
                oldest.offer(message);
            } else {
                newest.offer(message);
            }
        }
        let (truncated, matched) = if following {
            (oldest.dropped(), oldest.into_page())
        } else {
            (newest.dropped(), newest.into_page())
        };
        // Everything the board holds is what the caller has been shown, unless
        // the page had to leave some of it out. Then the cursor may reach no
        // further than the newest message actually in hand: the page is the
        // oldest end of what matched, so every message at or below that point
        // either went out with it or was filtered out on the way, and the ones
        // left behind are all above it and will be offered again next read.
        // An origin this page says nothing about keeps the caller's own mark,
        // which costs a second look at messages that matched nothing and loses
        // none that did.
        let vector = if truncated {
            let mut vector = filter.since.clone();
            for message in &matched {
                let mark = vector.entry(message.origin.clone()).or_default();
                *mark = (*mark).max(message.seq);
            }
            vector
        } else {
            inner
                .logs
                .iter()
                .map(|(origin, log)| (origin.clone(), log.highest()))
                .collect()
        };
        TalkPage {
            messages: matched,
            cursor: encode_cursor(&vector),
            truncated,
        }
    }

    /// Add one message to memory and to its origin's log file.
    fn file(&self, inner: &mut TalkInner, message: TalkMessage) -> Result<bool> {
        let log = inner.logs.entry(message.origin.clone()).or_default();
        if message.seq <= log.low_water {
            return Ok(false);
        }
        let at = match log
            .messages
            .binary_search_by(|held| held.seq.cmp(&message.seq))
        {
            Ok(_) => return Ok(false),
            Err(at) => at,
        };
        let path = self
            .root
            .join("log")
            .join(format!("{}.jsonl", message.origin));
        append_line(&path, &message)?;
        log.messages.insert(at, message);
        if log.messages.len() > RETAIN_PER_ORIGIN + COMPACT_SLACK {
            let cut = log.messages.len() - RETAIN_PER_ORIGIN;
            log.low_water = log.messages[cut - 1].seq;
            log.messages.drain(..cut);
            rewrite_log(&path, &log.messages)?;
        }
        Ok(true)
    }

    /// Drop what has aged out. Called when the board is opened rather than on
    /// a timer: a daemon that nobody talks to should not be doing work.
    pub fn compact(&self) -> Result<()> {
        let mut inner = self.inner();
        let cutoff = now_ms().saturating_sub(RETAIN_MS);
        let root = self.root.join("log");
        for (origin, log) in inner.logs.iter_mut() {
            let keep = log
                .messages
                .iter()
                .position(|message| message.ts >= cutoff)
                .unwrap_or(log.messages.len());
            if keep == 0 {
                continue;
            }
            log.low_water = log.messages[keep - 1].seq;
            log.messages.drain(..keep);
            rewrite_log(&root.join(format!("{origin}.jsonl")), &log.messages)?;
        }
        Ok(())
    }
}

/// Whether one message is in front of the reader the filter describes.
fn visible(message: &TalkMessage, filter: &TalkFilter, mine: &str, label: &str) -> bool {
    if let Some(wanted) = filter.scope.as_deref() {
        let name = match message.kind {
            TalkKind::Direct => "direct",
            _ => message.scope.name(),
        };
        if wanted != name {
            return false;
        }
    }
    // A direct message is between two sessions. It is on the board so the
    // machine it happened on can be read later, not so anyone can be part of
    // it: only its two ends see it unless someone asks for directs by name.
    if message.kind == TalkKind::Direct {
        let addressed = message.to.as_ref().is_some_and(|to| {
            to.machine == mine
                && filter
                    .session_id
                    .as_deref()
                    .is_some_and(|session| session == to.session_id)
        });
        let sent = message.author.machine == mine
            && message.author.voice.session_id == filter.session_id
            && filter.session_id.is_some();
        return addressed || sent || filter.owner || filter.scope.as_deref() == Some("direct");
    }
    match &message.scope {
        TalkScope::Global => true,
        TalkScope::Machine { machine } => filter.machines.admits(
            Some(mine),
            machine,
            machine_label(message, machine, mine, label),
        ),
        TalkScope::Path { machine, path } => {
            filter.machines.admits(
                Some(mine),
                machine,
                machine_label(message, machine, mine, label),
            ) && match &filter.paths {
                // Standing in no particular directory, every directory on
                // an admitted machine is as relevant as any other.
                TalkSelector::Mine if filter.path.is_none() => true,
                selector => selector.admits(filter.path.as_deref(), path, None),
            }
        }
        // A task is the one scope a machine boundary does not cut, so no
        // machine selector applies to it. An agent that starts a subagent on
        // another machine is still doing the same piece of work and the two of
        // them have to hear each other; the machine on the scope only records
        // where a thing was said. Being in the task is the whole of it.
        TalkScope::Task { root_session, .. } => {
            filter.owner || filter.task.as_deref() == Some(root_session.as_str())
        }
    }
}

/// What a human calls the machine a message's board belongs to, so a reader
/// can name another machine the way they know it rather than by its origin
/// key. This machine's own label is authoritative and current; another's is
/// whatever it called itself when it posted.
fn machine_label<'a>(
    message: &'a TalkMessage,
    machine: &str,
    mine: &str,
    label: &'a str,
) -> Option<&'a str> {
    if machine == mine {
        return Some(label);
    }
    Some(message.author.machine_label.as_str())
        .filter(|label| !label.is_empty() && message.author.machine == machine)
}

pub fn encode_cursor(vector: &TalkVector) -> String {
    vector
        .iter()
        .map(|(origin, seq)| format!("{origin}:{seq}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn decode_cursor(text: &str) -> TalkVector {
    text.split(',')
        .filter_map(|entry| {
            let (origin, seq) = entry.trim().rsplit_once(':')?;
            Some((origin.to_string(), seq.parse().ok()?))
        })
        .collect()
}

fn valid_origin(origin: &str) -> bool {
    !origin.is_empty()
        && origin.len() <= 64
        && origin
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn append_line(path: &Path, message: &TalkMessage) -> Result<()> {
    let mut line = serde_json::to_vec(message).context("failed to encode a talk message")?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open the talk log at {}", path.display()))?;
    restrict(&file);
    file.write_all(&line)
        .with_context(|| format!("failed to write to the talk log at {}", path.display()))
}

fn rewrite_log(path: &Path, messages: &[TalkMessage]) -> Result<()> {
    let temporary = path.with_extension("jsonl.tmp");
    let mut file = File::create(&temporary)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    restrict(&file);
    for message in messages {
        let mut line = serde_json::to_vec(message).context("failed to encode a talk message")?;
        line.push(b'\n');
        file.write_all(&line)?;
    }
    file.flush()?;
    drop(file);
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace the talk log at {}", path.display()))
}

fn load_logs(root: &Path) -> BTreeMap<String, OriginLog> {
    let mut logs: BTreeMap<String, OriginLog> = BTreeMap::new();
    let Ok(entries) = fs::read_dir(root) else {
        return logs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut log = OriginLog::default();
        // A half-written line is what a crash leaves behind, and the rest of
        // the log is still perfectly good: skip it rather than lose the file.
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(message) = serde_json::from_str::<TalkMessage>(&line) else {
                continue;
            };
            if !message.acceptable() {
                continue;
            }
            if let Err(at) = log
                .messages
                .binary_search_by(|held| held.seq.cmp(&message.seq))
            {
                log.messages.insert(at, message);
            }
        }
        if let Some(origin) = log.messages.first().map(|message| message.origin.clone()) {
            log.low_water = log.messages.first().map_or(0, |message| message.seq - 1);
            logs.insert(origin, log);
        }
    }
    logs
}

fn load_identity(root: &Path) -> Result<TalkIdentity> {
    let path = root.join("identity.json");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(identity) = serde_json::from_str::<TalkIdentity>(&text)
        && valid_origin(&identity.origin)
    {
        return Ok(identity);
    }
    let host = hostname();
    let identity = TalkIdentity {
        // Readable enough to recognise in a file name, unique enough that two
        // machines with the same hostname never share a log.
        origin: format!("{}-{}", sanitize(&host), fingerprint(root, &host)),
        label: host,
    };
    save_identity(root, &identity)?;
    Ok(identity)
}

fn save_identity(root: &Path, identity: &TalkIdentity) -> Result<()> {
    let path = root.join("identity.json");
    let temporary = path.with_extension("tmp");
    let text = serde_json::to_string_pretty(identity)?;
    fs::write(&temporary, text)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    if let Ok(file) = File::open(&temporary) {
        restrict(&file);
    }
    fs::rename(&temporary, &path).with_context(|| format!("failed to replace {}", path.display()))
}

fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "machine".into()
    } else {
        trimmed.chars().take(40).collect()
    }
}

/// Eight hex characters that no other machine will mint: the state directory
/// this daemon owns, its host name, and the moment it first needed an origin.
fn fingerprint(root: &Path, host: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(root.as_os_str().as_encoded_bytes());
    hasher.update(host.as_bytes());
    hasher.update(now_ms().to_be_bytes());
    hasher.update(std::process::id().to_be_bytes());
    hasher
        .finalize()
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0_i8; 256];
        // SAFETY: the buffer outlives the call and is one byte longer than the
        // length handed to it, so the result is always terminated.
        if unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) } == 0 {
            let bytes: Vec<u8> = buffer
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| *byte as u8)
                .collect();
            if let Ok(host) = String::from_utf8(bytes)
                && !host.is_empty()
            {
                // A `.local` suffix is Bonjour's, not the machine's name.
                return host.trim_end_matches(".local").to_string();
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "machine".into())
}

fn restrict(file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = file;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// When a message may be put in front of an agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TalkDeliver {
    /// Type it in if the session is free, and wait for it to be if it is not —
    /// but not forever: an agent that stays busy is interrupted rather than
    /// left unaware.
    #[default]
    Auto,
    /// Type it in now, whatever the session is in the middle of.
    Now,
    /// Wait for the session to be free, however long that takes.
    WhenIdle,
}

impl TalkDeliver {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "auto" => Self::Auto,
            "now" => Self::Now,
            "when_idle" => Self::WhenIdle,
            other => bail!("unknown deliver {other}: use auto, now, or when_idle"),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Now => "now",
            Self::WhenIdle => "when_idle",
        }
    }
}

/// How long an `auto` delivery waits for a prompt box it can safely use.
///
/// A box that is drawn but holds something has usually been abandoned
/// half-typed, and appending to it is only worth risking once it is clear
/// nobody is coming back. A box that is not drawn at all is a runtime still
/// starting up, or one whose screen muxloom no longer recognises; both resolve
/// sooner, and neither is made better by waiting.
pub const DELIVER_OCCUPIED_MS: u64 = 5 * 60 * 1000;
pub const DELIVER_ABSENT_MS: u64 = 60 * 1000;
/// How long anything waits before delivering it would be stranger than saying
/// it never arrived.
pub const DELIVER_EXPIRY_MS: u64 = 30 * 60 * 1000;

/// A message waiting for the session it is addressed to to be free. Held by
/// the daemon and written down, so a handover does not swallow it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkQueued {
    pub message_id: String,
    pub session_id: String,
    /// The rendered envelope, kept as it will be typed: the message it came
    /// from is on the board and could be edited by compaction underneath it.
    pub body: String,
    pub queued_at: u64,
    pub deliver: TalkDeliver,
    /// Who sent it, so that a message which never arrives can be sent back to
    /// them. A queue written down by an older daemon has nobody to tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<TalkAddress>,
    /// What was said, kept beside the envelope so a bounce can quote it
    /// without the sender having to go and look the message up.
    #[serde(default)]
    pub text: String,
    /// The chat binding this came in through, when a person on a chat app sent
    /// it rather than an agent. Their return address: they have no session to
    /// be written back to, and the chat is the only place they are reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// What the session was called when this was queued, for the bounce that
    /// goes to a person. They aimed at a name and never saw an id, so an id is
    /// not something they can match this against. Kept here rather than looked
    /// up later because the commonest thing to be told is that the session is
    /// gone, and a session that is gone has no name left to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
}

impl TalkQueued {
    /// Whether a queued message goes in now, given what the session's prompt
    /// box says and how long this has been waiting for it.
    ///
    /// A running turn is not in itself a reason to wait: both CLIs hold what
    /// arrives mid-turn in an empty prompt and read it when the turn ends. Only
    /// `when_idle`, which was asked for precisely that, waits one out.
    pub fn due(&self, composer: Composer, working: bool, attention: bool, now: u64) -> bool {
        if composer == Composer::Ready {
            return self.deliver != TalkDeliver::WhenIdle || !working;
        }
        // A question on screen is answered by whatever is typed next, and no
        // length of waiting makes answering it with somebody else's message
        // the right thing to do.
        if attention || self.deliver == TalkDeliver::WhenIdle {
            return false;
        }
        let patience = if composer == Composer::Occupied {
            DELIVER_OCCUPIED_MS
        } else {
            DELIVER_ABSENT_MS
        };
        now.saturating_sub(self.queued_at) >= patience
    }

    /// Whether it has waited so long that delivering it would be stranger than
    /// admitting it never arrived.
    pub fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.queued_at) >= DELIVER_EXPIRY_MS
    }
}

/// Why a queued message never went in. Every one of these is the end of the
/// road for it, and the sender is waiting: an agent that asked a question and
/// hears nothing cannot tell "no" from "never arrived", so each of these is
/// said out loud on the board rather than written to a log nobody reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TalkUndelivered {
    /// The session is not on this machine any more.
    Gone,
    /// The session ended while the message was waiting for it.
    Ended,
    /// It waited out the whole expiry without the session ever showing a
    /// prompt box to put it in.
    NoPrompt,
    /// Writing to the session failed.
    Failed(String),
}

impl TalkUndelivered {
    /// The half-sentence that finishes "never reached that session because …".
    pub fn reason(&self) -> String {
        match self {
            Self::Gone => "the session is no longer on that machine".into(),
            Self::Ended => "the session ended while the message was waiting".into(),
            Self::NoPrompt => format!(
                "in {} minutes the session never showed a prompt box to put it in — it may have \
                 been stopped on a question that whole time, or no longer be running an agent at \
                 all",
                DELIVER_EXPIRY_MS / 60_000
            ),
            Self::Failed(error) => format!("writing to the session failed: {error}"),
        }
    }
}

/// What the sender is told when a message never reached the session it was
/// for.
///
/// Addressed back to the sender as a direct message, which is what a sender
/// waiting on an answer is already reading. It is not typed into their prompt:
/// the machine holding the undelivered message is usually not the one holding
/// the sender, and a bounce that could itself fail to arrive and bounce again
/// is not worth the trouble.
pub fn render_bounce(queued: &TalkQueued, why: &TalkUndelivered) -> String {
    let quoted = queued.text.trim();
    let quoted = if quoted.is_empty() {
        String::new()
    } else {
        format!("\n--- what you sent ---\n{quoted}\n--- end ---")
    };
    format!(
        "[muxloom] Your message to session {} never reached it: {}. Nobody read it, so no answer \
         is coming — stop waiting on it, and either do without or find another agent.{quoted}",
        queued.session_id,
        why.reason()
    )
}

/// How much of a person's own message is quoted back to them on a chat.
///
/// Enough to recognise which one this was about, and no more: they are holding
/// the phone they typed it on and can scroll up to the rest. What they cannot
/// work out for themselves is that it never arrived.
const CHANNEL_BOUNCE_QUOTE: usize = 160;

/// What a person on a chat app is told when the message they sent from it
/// never reached the agent it was for.
///
/// A separate rendering from the board's because the reader is separate: an
/// agent is told to stop waiting and reads a session id as a name, and a person
/// is holding a phone and knows only the name they aimed at. What they both
/// need is the same and comes first — this did not arrive, and here is why.
pub fn render_channel_bounce(queued: &TalkQueued, why: &TalkUndelivered) -> String {
    let who = queued
        .session_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(&queued.session_id);
    let said = queued.text.trim();
    let quoted = if said.is_empty() {
        String::new()
    } else {
        let mut short: String = said.chars().take(CHANNEL_BOUNCE_QUOTE).collect();
        if short.chars().count() < said.chars().count() {
            short.push('…');
        }
        format!("\n\n--- what you sent ---\n{short}")
    };
    format!(
        "Your message to {who} never got there: {}.\n\nNobody read it, so no answer is coming. \
         `/who` lists what is running, and `/select` points this chat at one of them.{quoted}",
        why.reason()
    )
}

/// What a message from another agent looks like when it lands in a session.
///
/// The one place this is rendered, because everything about it is a promise:
/// an agent reading its own terminal must be able to tell at a glance that
/// this is another agent talking and not the person it works for, and must be
/// told how to answer without having to guess.
///
/// `here` is the machine this is being typed on. It is what decides whether the
/// reply has to name a machine at all, and it has to, because a machine is
/// called different things depending on who is asking: the sender knows its own
/// machine by the name its own daemon gave it, while from inside a session on
/// that same machine the only name that answers is `local`. Repeating the
/// sender's name for it would hand the reader an address its own tools reject.
/// Between two agents on one machine — which is most of them — the reply names
/// no machine, and the name is spelled out only when the message really did
/// cross a machine, where what the controller sank into the sender's board is
/// the name it is addressed by from here too.
pub fn render_delivery(message: &TalkMessage, reply_expected: bool, here: &str) -> String {
    let author = &message.author;
    let who = author.voice.name();
    let kind = author.voice.kind.as_deref().unwrap_or("agent");
    let machine = if author.machine_label.is_empty() {
        author.machine.as_str()
    } else {
        author.machine_label.as_str()
    };
    let addressed = if author.machine.is_empty() || author.machine == here {
        String::new()
    } else {
        format!("machine: \"{machine}\", ")
    };
    let session = author
        .voice
        .session_id
        .as_deref()
        .map(|session| format!(" (session {session})"))
        .unwrap_or_default();
    // The whole reply call, `reply_to` and all, so answering is one paste
    // rather than three things to look up. A reply that carries the id is what
    // lets the sender's wait end on the right message instead of on the next
    // thing anyone happens to say.
    let call = |session: &str| {
        format!(
            "message_agent {{ {addressed}session_id: \"{session}\", reply_to: \"{}\", text: \"…\" \
             }}",
            message.id
        )
    };
    let reply = match author.voice.session_id.as_deref() {
        Some(session) if reply_expected => format!(
            "The sender is blocked waiting on this answer and will keep waiting. Reply with the \
             muxloom tool {} as soon as you know something — including if the answer is no, or \
             not yet, or that you are the wrong agent to ask. Silence is the one answer it \
             cannot act on.",
            call(session)
        ),
        Some(session) => format!(
            "Reply with the muxloom tool {}, or ignore this if it does not concern what you are \
             doing.",
            call(session)
        ),
        // A person who wrote in from a chat app reads the answer in that app or
        // not at all. Sending them to the board here is how an answer ends up
        // filed where the one person waiting for it never looks, so the call
        // that reaches them is the one spelled out.
        None if author.voice.human => match author.voice.channel.as_deref() {
            Some(channel) if !channel.is_empty() => {
                // A quote is answered quoting, so the person reading a day of
                // messages can see which of theirs this belongs to.
                let quoting = match author.voice.channel_quote.as_deref() {
                    Some(quoted) if !quoted.is_empty() => format!("reply_to: \"{quoted}\", "),
                    _ => String::new(),
                };
                format!(
                    "A person sent this from their chat app. Answer them there with the muxloom \
                     tool send_channel_message {{ channel: \"{channel}\", {quoting}text: \"…\" }} \
                     — not on the talk board, which they are not reading. Reply first, briefly, \
                     before you go and do the thing they asked for: from their side silence and \
                     refusal look the same."
                )
            }
            _ => "A person wrote this, not an agent. Answer where they are: talk_post puts it on \
                  the board they are watching."
                .into(),
        },
        None => "Say anything back on the talk board with talk_post; the sender is not a muxloom \
                 session, so there is nowhere to reply directly."
            .into(),
    };
    // Who typed it is the first thing the reader has to get right, and the two
    // cases pull opposite ways: another agent's message is a colleague's
    // opinion to weigh, and a person's is the instruction the whole session
    // exists to serve. Telling a reader that the human it works for is "another
    // agent" is the worst of the two mistakes, so the line is written from
    // whether the board says a person spoke.
    let from = match author.voice.human {
        true => format!("[muxloom] Message from \"{who}\" on {machine}{session}"),
        false => format!("[muxloom] Message from {kind} \"{who}\" on {machine}{session}"),
    };
    let provenance = match author.voice.human {
        true => {
            "Delivered by muxloom: a person typed this — the human you work for, writing in from \
             where they are. It is not another agent's suggestion."
        }
        false => {
            "Delivered by muxloom: another agent typed this, not the person you are working for. \
             Judge it on its merits."
        }
    };
    format!(
        "{from}, {}.\n\
         {provenance}\n\
         --- message ---\n\
         {}\n\
         --- end of message ---\n\
         {reply}",
        civil_utc(message.ts),
        message.text
    )
}

/// The bytes that put a whole message into a CLI's prompt as one paste rather
/// than as line after line of typing, each of which would submit.
pub fn paste_bytes(body: &str, submit: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(body.len() + 16);
    bytes.extend_from_slice(b"\x1b[200~");
    // A carriage return inside the paste would be a submission on the CLIs
    // that unwrap it, so newlines travel as newlines.
    bytes.extend(body.replace("\r\n", "\n").replace('\r', "\n").into_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    if submit {
        bytes.push(b'\r');
    }
    bytes
}

/// A message folded onto one line, for a CLI that does not understand
/// bracketed paste and would submit every line of it separately.
pub fn folded(body: &str) -> String {
    body.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ")
}

/// Just the hour and minute of a stamp, for a board that gives each message one
/// line. UTC like [`civil_utc`], and labelled as such where it is drawn.
pub fn clock_utc(ms: u64) -> String {
    let time = (ms / 1000) % 86_400;
    format!("{:02}:{:02}", time / 3600, (time % 3600) / 60)
}

/// Epoch milliseconds as a plain UTC stamp. UTC because the two ends of a
/// message rarely keep the same clock, let alone the same zone, and a stamp
/// that says which is which is worth more than a local one that lies.
pub fn civil_utc(ms: u64) -> String {
    let seconds = ms / 1000;
    let (time, days) = (seconds % 86_400, seconds / 86_400);
    // Howard Hinnant's civil_from_days, with the era shifted to 1970.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// How many messages one round carries in each direction per machine. A round
/// that falls behind catches up on the next one.
const SYNC_BATCH: usize = MAX_PAGE;

/// What one replication round moved.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TalkSyncSummary {
    pub machines: usize,
    pub pulled: usize,
    pub pushed: usize,
    /// Machines that could not be reached or are too old to have a board.
    pub skipped: usize,
}

impl fmt::Display for TalkSyncSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} machines, {} pulled, {} pushed, {} skipped",
            self.machines, self.pulled, self.pushed, self.skipped
        )
    }
}

/// Carry messages between the machines a controller can reach.
///
/// Two daemons never speak to each other: the controller is the only thing
/// that can see both, so it is the only thing that can replicate. Each round
/// diffs a machine's version vector against the local board, pulls what is
/// missing, and pushes what that machine has not got. Both directions are
/// idempotent and additive, so a round that dies halfway is just a round that
/// runs again.
pub fn run_sync(runtime: &Runtime, targets: &[Target]) -> Result<TalkSyncSummary> {
    let pool = runtime.bridge_pool();
    let local = Target::local();
    let mut summary = TalkSyncSummary::default();
    let mut mine = pool
        .talk_status(&local, None)
        .context("the local talk board is unreachable")?
        .vector;
    for target in targets.iter().filter(|target| target.id != local.id) {
        // The name a human gave the machine travels with it: the daemon knows
        // its own hostname and nothing else about how it is spoken of.
        let label = match &target.transport {
            Transport::Ssh { alias } => Some(alias.clone()),
            _ => None,
        };
        let theirs = match pool.talk_status(target, label) {
            Ok(state) => state,
            Err(error) => {
                debug::log("talk", format!("{}: no board ({error})", target.id));
                summary.skipped += 1;
                continue;
            }
        };
        summary.machines += 1;
        match carry(&pool, target, &local, mine.clone(), &theirs.vector) {
            Ok((pulled, pushed)) => {
                summary.pulled += pulled;
                summary.pushed += pushed;
                if pulled > 0 {
                    // The next machine should be told what this one just said,
                    // and not be asked for it a second time.
                    mine = pool.talk_status(&local, None)?.vector;
                }
            }
            Err(error) => {
                debug::log("talk", format!("{}: sync failed ({error})", target.id));
                summary.skipped += 1;
            }
        }
    }
    Ok(summary)
}

/// One machine's half of a round: pull first, so what it minted is on the
/// local board before the next machine is pushed to.
fn carry(
    pool: &BridgePool,
    target: &Target,
    local: &Target,
    mine: TalkVector,
    theirs: &TalkVector,
) -> Result<(usize, usize)> {
    let inbound = pool.talk_fetch(target, mine, SYNC_BATCH)?;
    let pulled = if inbound.is_empty() {
        0
    } else {
        pool.talk_append(local, inbound)?
    };
    let outbound = pool.talk_fetch(local, theirs.clone(), SYNC_BATCH)?;
    let pushed = if outbound.is_empty() {
        0
    } else {
        pool.talk_append(target, outbound)?
    };
    Ok((pulled, pushed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> TalkStore {
        let root = std::env::temp_dir().join(format!(
            "mxl-talk-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        TalkStore::open(root).unwrap()
    }

    fn draft(text: &str, scope: TalkScope) -> TalkDraft {
        TalkDraft {
            scope,
            author: TalkAuthor {
                voice: TalkVoice {
                    session_id: Some("session-1".into()),
                    label: Some("builder".into()),
                    kind: Some("claude".into()),
                    human: false,
                    channel: None,
                    channel_quote: None,
                },
                ..TalkAuthor::default()
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: text.into(),
        }
    }

    #[test]
    fn a_post_is_minted_once_and_reads_back_after_a_restart() {
        let store = store("restart");
        let root = store.root.clone();
        let posted = store
            .post(draft(
                "the build is green",
                TalkScope::Path {
                    machine: String::new(),
                    path: "/work".into(),
                },
            ))
            .unwrap();
        assert_eq!(posted.seq, 1);
        assert_eq!(posted.id, format!("{}:1", store.origin()));
        // An empty machine in the scope means "here", and only the daemon
        // knows what "here" is called.
        assert_eq!(posted.scope.machine(), Some(store.origin().as_str()));

        let reopened = TalkStore::open(root.clone()).unwrap();
        assert_eq!(reopened.origin(), store.origin());
        let page = reopened.read(&TalkFilter {
            path: Some("/work".into()),
            ..TalkFilter::default()
        });
        assert_eq!(page.messages, vec![posted.clone()]);
        // The next post continues the run rather than colliding with it.
        assert_eq!(
            reopened
                .post(draft("and deployed", posted.scope))
                .unwrap()
                .seq,
            2
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn messages_from_elsewhere_merge_out_of_order_and_only_once() {
        let store = store("merge");
        let far = |seq: u64, ts: u64| TalkMessage {
            id: format!("other-1234abcd:{seq}"),
            origin: "other-1234abcd".into(),
            seq,
            ts,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: "other-1234abcd".into(),
                machine_label: "gpu-box".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: format!("message {seq}"),
        };

        assert_eq!(store.merge(vec![far(3, 300), far(1, 100)]).unwrap(), 2);
        // What is held is not yet what can be claimed: 2 is missing, so the
        // vector stops at 1 and replication asks for the hole again.
        assert_eq!(store.state().vector["other-1234abcd"], 1);
        assert_eq!(store.merge(vec![far(1, 100), far(3, 300)]).unwrap(), 0);
        assert_eq!(store.merge(vec![far(2, 200)]).unwrap(), 1);
        assert_eq!(store.state().vector["other-1234abcd"], 3);

        // A message whose id does not match its origin is not filed at all.
        let mut forged = far(4, 400);
        forged.id = "other-1234abcd:9".into();
        assert_eq!(store.merge(vec![forged]).unwrap(), 0);

        let held = store.since(&TalkVector::new(), 10);
        assert_eq!(held.len(), 3);
        let from_two: Vec<u64> = store
            .since(&TalkVector::from([("other-1234abcd".into(), 2)]), 10)
            .iter()
            .map(|message| message.seq)
            .collect();
        assert_eq!(from_two, vec![3]);

        fs::remove_dir_all(&store.root).ok();
    }

    /// A catch-up batch is capped, and what it drops has to be the newest -
    /// otherwise a peer that is far behind is handed a batch, tells us it is
    /// still missing the older half, and is handed the same batch again. The
    /// cap is applied across the whole board rather than per origin, so this
    /// puts two machines' messages in front of it interleaved in time and asks
    /// for fewer than either alone.
    #[test]
    fn a_catch_up_batch_is_the_oldest_the_peer_is_missing_across_every_origin() {
        let store = store("catch-up");
        let far = |origin: &str, seq: u64, ts: u64| TalkMessage {
            id: format!("{origin}:{seq}"),
            origin: origin.into(),
            seq,
            ts,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: origin.into(),
                machine_label: origin.into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: format!("{origin} {seq}"),
        };
        let (alpha, bravo) = ("alpha-1234abcd", "bravo-1234abcd");
        assert_eq!(
            store
                .merge(vec![
                    far(alpha, 1, 100),
                    far(bravo, 1, 150),
                    far(alpha, 2, 200),
                    far(bravo, 2, 250),
                    far(alpha, 3, 300),
                ])
                .unwrap(),
            5
        );

        let named = |from: &TalkVector, limit: usize| -> Vec<String> {
            store
                .since(from, limit)
                .iter()
                .map(|message| message.id.clone())
                .collect()
        };

        // Oldest first and across both origins: taking three from alpha alone
        // would leave bravo's oldest behind forever.
        assert_eq!(
            named(&TalkVector::new(), 3),
            vec![
                format!("{alpha}:1"),
                format!("{bravo}:1"),
                format!("{alpha}:2")
            ]
        );

        // What the peer already holds is skipped per origin, not globally.
        assert_eq!(
            named(&TalkVector::from([(alpha.into(), 2)]), 10),
            vec![
                format!("{bravo}:1"),
                format!("{bravo}:2"),
                format!("{alpha}:3")
            ]
        );

        // A peer that is up to date is handed nothing at all.
        assert!(
            named(
                &TalkVector::from([(alpha.into(), 3), (bravo.into(), 2)]),
                10
            )
            .is_empty()
        );

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_page_is_the_newest_that_matched_however_much_the_board_holds() {
        let store = store("page");
        let far = |origin: &str, seq: u64, ts: u64| TalkMessage {
            id: format!("{origin}:{seq}"),
            origin: origin.into(),
            seq,
            ts,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: origin.into(),
                machine_label: origin.into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: format!("at {ts}"),
        };
        // Two machines talking in turn, so the newest of the two boards
        // together is not the newest of either one on its own.
        let mut held = Vec::new();
        for round in 1..=4 {
            held.push(far("aaaa-1234abcd", round, round * 200 - 100));
            held.push(far("bbbb-5678cdef", round, round * 200));
        }
        assert_eq!(store.merge(held).unwrap(), 8);

        let texts = |limit: usize| -> (Vec<String>, bool) {
            let page = store.read(&TalkFilter {
                limit,
                ..TalkFilter::default()
            });
            (
                page.messages
                    .into_iter()
                    .map(|message| message.text)
                    .collect(),
                page.truncated,
            )
        };

        // A page smaller than what matched is its tail, still oldest first,
        // and says so.
        assert_eq!(
            texts(3),
            (
                vec!["at 600".to_string(), "at 700".into(), "at 800".into()],
                true
            )
        );
        // Room for everything is everything, and nothing was left behind.
        assert_eq!(
            texts(100),
            (
                vec![
                    "at 100".to_string(),
                    "at 200".into(),
                    "at 300".into(),
                    "at 400".into(),
                    "at 500".into(),
                    "at 600".into(),
                    "at 700".into(),
                    "at 800".into(),
                ],
                false
            )
        );
        // Exactly enough room is not a page that dropped anything.
        assert!(!texts(8).1);

        fs::remove_dir_all(&store.root).ok();
    }

    /// A cursor is a promise: hand it back and you are told what has happened
    /// since. Keeping the newest end of an oversized page broke that promise -
    /// the page was the tail of a burst and the cursor was everything the board
    /// held, so the messages in between were dropped and then skipped, and the
    /// one they were most likely to be was the answer somebody was waiting on.
    #[test]
    fn a_cursor_is_given_every_message_it_was_not_shown() {
        let store = store("cursor");
        let far = |seq: u64| TalkMessage {
            id: format!("cccc-9012abcd:{seq}"),
            origin: "cccc-9012abcd".into(),
            seq,
            ts: seq * 100,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: "cccc-9012abcd".into(),
                machine_label: "elsewhere".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: format!("said {seq}"),
        };
        // The answer comes first and the noise that buries it comes after,
        // which is the order that made this lose the answer.
        assert_eq!(store.merge((1..=9).map(far).collect()).unwrap(), 9);

        // Someone arriving at the board still gets the newest end: catching up
        // is what that read is for.
        let arriving = store.read(&TalkFilter {
            limit: 3,
            ..TalkFilter::default()
        });
        assert_eq!(
            arriving
                .messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![7, 8, 9]
        );
        assert!(arriving.truncated);

        // Someone following a cursor gets the oldest end, and the cursor stops
        // where the page stops rather than where the board does.
        let mut cursor = encode_cursor(&BTreeMap::from([("cccc-9012abcd".to_string(), 0)]));
        let mut seen = Vec::new();
        for _ in 0..5 {
            let page = store.read(&TalkFilter {
                since: decode_cursor(&cursor),
                limit: 3,
                ..TalkFilter::default()
            });
            if page.messages.is_empty() {
                break;
            }
            seen.extend(page.messages.iter().map(|message| message.seq));
            cursor = page.cursor;
        }
        assert_eq!(
            seen,
            (1..=9).collect::<Vec<_>>(),
            "a poll that follows its cursor is handed every message once, in order"
        );

        // And the cursor of a truncated page really is held back: reading with
        // the first one again gives the second page, not the board's tail.
        let first = store.read(&TalkFilter {
            since: decode_cursor(&encode_cursor(&BTreeMap::from([(
                "cccc-9012abcd".to_string(),
                0,
            )]))),
            limit: 3,
            ..TalkFilter::default()
        });
        assert_eq!(decode_cursor(&first.cursor).get("cccc-9012abcd"), Some(&3));

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_task_is_read_by_its_members_and_reaches_across_machines() {
        let store = store("task");
        store
            .post(draft(
                "the retry lives in client.rs",
                TalkScope::Task {
                    machine: String::new(),
                    root_session: "lead".into(),
                },
            ))
            .unwrap();
        // A subagent of the same task, running on another machine, filed on
        // its own board and replicated here.
        store
            .merge(vec![TalkMessage {
                id: "other-1234abcd:1".into(),
                origin: "other-1234abcd".into(),
                seq: 1,
                ts: 20,
                scope: TalkScope::Task {
                    machine: "other-1234abcd".into(),
                    root_session: "lead".into(),
                },
                author: TalkAuthor {
                    machine: "other-1234abcd".into(),
                    machine_label: "gpu-box".into(),
                    voice: TalkVoice::default(),
                },
                kind: TalkKind::Message,
                to: None,
                reply_to: None,
                text: "the pool is fine".into(),
            }])
            .unwrap();

        let texts = |filter: &TalkFilter| -> Vec<String> {
            store
                .read(filter)
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        // Both halves of the task reach a member without it having to widen
        // anything: a machine boundary does not cut a piece of work in two.
        assert_eq!(
            texts(&TalkFilter {
                task: Some("lead".into()),
                ..TalkFilter::default()
            }),
            // Oldest first, and the merged one carries a stamp from before
            // this test started.
            ["the pool is fine", "the retry lives in client.rs"]
        );
        // Somebody else's task, on the same machine and in the same
        // directory, is none of this reader's business.
        assert!(
            texts(&TalkFilter {
                task: Some("another-lead".into()),
                path: Some("/work".into()),
                machines: TalkSelector::All,
                paths: TalkSelector::All,
                ..TalkFilter::default()
            })
            .is_empty()
        );
        // Nor is asking for the scope by name a way into it.
        assert!(
            texts(&TalkFilter {
                scope: Some("task".into()),
                machines: TalkSelector::All,
                ..TalkFilter::default()
            })
            .is_empty()
        );
        // The dashboard reads as the person the machines belong to and sees
        // what the agents said, the same as it does for directs.
        assert_eq!(
            texts(&TalkFilter {
                owner: true,
                scope: Some("task".into()),
                ..TalkFilter::default()
            })
            .len(),
            2
        );

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn scopes_decide_who_sees_a_post_and_include_widens_it() {
        let store = store("scopes");
        let mine = store.origin();
        store.post(draft("everyone", TalkScope::Global)).unwrap();
        store
            .post(draft(
                "this machine",
                TalkScope::Machine {
                    machine: String::new(),
                },
            ))
            .unwrap();
        store
            .post(draft(
                "this project",
                TalkScope::Path {
                    machine: String::new(),
                    path: "/work".into(),
                },
            ))
            .unwrap();
        store
            .merge(vec![TalkMessage {
                id: "other-1234abcd:1".into(),
                origin: "other-1234abcd".into(),
                seq: 1,
                ts: 10,
                scope: TalkScope::Machine {
                    machine: "other-1234abcd".into(),
                },
                author: TalkAuthor {
                    machine: "other-1234abcd".into(),
                    machine_label: "gpu-box".into(),
                    voice: TalkVoice::default(),
                },
                kind: TalkKind::Message,
                to: None,
                reply_to: None,
                text: "far away".into(),
            }])
            .unwrap();

        let texts = |filter: &TalkFilter| -> Vec<String> {
            store
                .read(filter)
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        // Standing in /work on this machine: everything here, nothing there.
        assert_eq!(
            texts(&TalkFilter {
                path: Some("/work".into()),
                ..TalkFilter::default()
            }),
            ["everyone", "this machine", "this project"]
        );
        // Standing in another directory, the project channel is not mine.
        assert_eq!(
            texts(&TalkFilter {
                path: Some("/elsewhere".into()),
                ..TalkFilter::default()
            }),
            ["everyone", "this machine"]
        );
        // Reaching for another machine by the name a human uses for it.
        assert_eq!(
            texts(&TalkFilter {
                path: Some("/work".into()),
                machines: TalkSelector::Only {
                    names: vec!["gpu-box".into()],
                },
                ..TalkFilter::default()
            }),
            ["far away", "everyone"]
        );
        assert_eq!(
            texts(&TalkFilter {
                path: Some("/work".into()),
                machines: TalkSelector::All,
                paths: TalkSelector::All,
                scope: Some("machine".into()),
                ..TalkFilter::default()
            }),
            ["far away", "this machine"]
        );
        assert_eq!(
            texts(&TalkFilter {
                query: Some("PROJECT".into()),
                path: Some("/work".into()),
                ..TalkFilter::default()
            }),
            ["this project"]
        );

        // A cursor answers with what arrived after it, and nothing else.
        let page = store.read(&TalkFilter {
            path: Some("/work".into()),
            ..TalkFilter::default()
        });
        let cursor = decode_cursor(&page.cursor);
        assert_eq!(cursor[&mine], 3);
        store.post(draft("later", TalkScope::Global)).unwrap();
        assert_eq!(
            texts(&TalkFilter {
                since: cursor,
                path: Some("/work".into()),
                ..TalkFilter::default()
            }),
            ["later"]
        );

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_direct_message_is_only_between_its_two_ends() {
        let store = store("direct");
        let mine = store.origin();
        let mut sent = draft("look at line 40", TalkScope::Global);
        sent.kind = TalkKind::Direct;
        sent.to = Some(TalkAddress {
            machine: mine.clone(),
            session_id: "session-2".into(),
        });
        store.post(sent).unwrap();

        let texts = |session: &str| -> Vec<String> {
            store
                .read(&TalkFilter {
                    session_id: Some(session.into()),
                    ..TalkFilter::default()
                })
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        assert_eq!(texts("session-2"), ["look at line 40"]);
        assert_eq!(texts("session-1"), ["look at line 40"]);
        assert!(texts("session-3").is_empty());
        // Asking for directs by name is how the board is audited.
        assert_eq!(
            store
                .read(&TalkFilter {
                    session_id: Some("session-3".into()),
                    scope: Some("direct".into()),
                    ..TalkFilter::default()
                })
                .messages
                .len(),
            1
        );
        // And the dashboard, which is the person the machine belongs to, sees
        // it among everything else without having to ask for it by name.
        assert_eq!(
            store
                .read(&TalkFilter {
                    owner: true,
                    ..TalkFilter::default()
                })
                .messages
                .len(),
            1
        );

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_round_between_two_boards_leaves_them_holding_the_same_thing() {
        let here = store("sync-here");
        let there = store("sync-there");
        here.post(draft("starting on the parser", TalkScope::Global))
            .unwrap();
        there
            .post(draft(
                "the parser is mine, take the lexer",
                TalkScope::Global,
            ))
            .unwrap();

        // Exactly what `carry` does, without needing two daemons and a
        // controller between them: diff the vectors, pull, then push.
        let round = |pull_into: &TalkStore, from: &TalkStore| -> (usize, usize) {
            let pulled = pull_into
                .merge(from.since(&pull_into.state().vector, MAX_PAGE))
                .unwrap();
            let pushed = from
                .merge(pull_into.since(&from.state().vector, MAX_PAGE))
                .unwrap();
            (pulled, pushed)
        };

        assert_eq!(round(&here, &there), (1, 1));
        assert_eq!(here.state().vector, there.state().vector);
        // Both boards read the same, in the same order, on either machine.
        let texts = |store: &TalkStore| -> Vec<String> {
            store
                .read(&TalkFilter::default())
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        assert_eq!(texts(&here).len(), 2);
        assert_eq!(texts(&here), texts(&there));
        // Nothing new was said, so the next round carries nothing: replication
        // is a diff, not a resend.
        assert_eq!(round(&here, &there), (0, 0));

        fs::remove_dir_all(&here.root).ok();
        fs::remove_dir_all(&there.root).ok();
    }

    #[test]
    fn an_envelope_says_who_is_talking_and_how_to_answer() {
        let store = store("envelope");
        let mut sent = draft(
            "look at src/talk.rs\nline 40 is wrong",
            TalkScope::Machine {
                machine: String::new(),
            },
        );
        sent.kind = TalkKind::Direct;
        sent.author.machine = "far-away".into();
        sent.author.machine_label = "gpu-box".into();
        sent.to = Some(TalkAddress {
            machine: String::new(),
            session_id: "session-2".into(),
        });
        let message = store.post(sent).unwrap();

        let envelope = render_delivery(&message, false, "nearby");
        assert!(
            envelope.starts_with(
                "[muxloom] Message from claude \"builder\" on gpu-box (session \
                                  session-1), "
            ),
            "{envelope}"
        );
        // The line that keeps an agent from mistaking this for its user.
        assert!(envelope.contains("not the person you are working for"));
        assert!(envelope.contains(
            "--- message ---\nlook at src/talk.rs\nline 40 is wrong\n--- end of message ---"
        ));
        // The reply call is complete enough to paste, `reply_to` included —
        // that id is what ends the sender's wait on this answer rather than on
        // the next thing anyone says.
        assert!(
            envelope.contains(&format!(
                "message_agent {{ machine: \"gpu-box\", session_id: \"session-1\", reply_to: \
                 \"{}\", text: \"…\" }}, or ignore this",
                message.id
            )),
            "{envelope}"
        );
        let awaited = render_delivery(&message, true, "nearby");
        assert!(
            awaited.contains("blocked waiting on this answer"),
            "{awaited}"
        );
        // An answer of no is still an answer, and the envelope says so.
        assert!(
            awaited.contains("including if the answer is no"),
            "{awaited}"
        );

        // The same message read on the machine it was sent from names no
        // machine to reply to. The sender's name for its own machine is not a
        // name the reader's tools answer to — from in here that machine is
        // `local` — so spelling it out would be handing over an address that
        // fails. The prose still says where the sender is sitting.
        let home = render_delivery(&message, false, "far-away");
        assert!(
            home.contains(&format!(
                "message_agent {{ session_id: \"session-1\", reply_to: \"{}\", text: \"…\" }}",
                message.id
            )),
            "{home}"
        );
        assert!(!home.contains("machine: "), "{home}");
        assert!(home.contains("\"builder\" on gpu-box"), "{home}");

        // A message from something that is not a session says so instead of
        // pointing at a reply address that does not exist.
        let mut anonymous = message.clone();
        anonymous.author.voice.session_id = None;
        assert!(render_delivery(&anonymous, true, "nearby").contains("nowhere to reply directly"));

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_person_writing_in_from_a_chat_is_answered_in_that_chat_and_not_on_the_board() {
        let store = store("envelope-human");
        let mut sent = draft(
            "干的怎么样了",
            TalkScope::Machine {
                machine: String::new(),
            },
        );
        sent.kind = TalkKind::Direct;
        sent.author.voice = TalkVoice {
            label: Some("Ming via WeChat / 微信".into()),
            human: true,
            channel: Some("wechat-1".into()),
            ..Default::default()
        };
        sent.to = Some(TalkAddress {
            machine: String::new(),
            session_id: "session-2".into(),
        });
        let message = store.post(sent).unwrap();

        let envelope = render_delivery(&message, true, "nearby");
        // Not "another agent typed this": it is the person the session works
        // for, and the header has to say so before anything else.
        // No "from agent": the kind slot is what says an agent is speaking, and
        // the person the session works for is not one.
        assert!(
            envelope.starts_with("[muxloom] Message from \"Ming via WeChat / 微信\" on "),
            "{envelope}"
        );
        assert!(envelope.contains("a person typed this"), "{envelope}");
        assert!(!envelope.contains("another agent typed this"), "{envelope}");
        // The way back is the chat they wrote from, spelled out, and the board
        // is named as the wrong answer rather than left to be guessed at.
        assert!(
            envelope.contains("send_channel_message { channel: \"wechat-1\", text: \"…\" }"),
            "{envelope}"
        );
        assert!(envelope.contains("not on the talk board"), "{envelope}");
        assert!(!envelope.contains("talk_post"), "{envelope}");
        // The person's words are delivered as they wrote them: nothing muxloom
        // has to say about how to answer is mixed into them.
        assert!(
            envelope.contains("--- message ---\n干的怎么样了\n--- end of message ---"),
            "{envelope}"
        );

        // Answering a quote quotes back, so the exchange stays readable in the
        // app — and the whole call is there to paste.
        let mut quoting = message.clone();
        quoting.author.voice.channel_quote = Some("7499470565387522824".into());
        let envelope = render_delivery(&quoting, true, "nearby");
        assert!(
            envelope.contains(
                "send_channel_message { channel: \"wechat-1\", reply_to: \
                 \"7499470565387522824\", text: \"…\" }"
            ),
            "{envelope}"
        );

        // A person typing at a dashboard is looking at the board, so there the
        // board is where an answer reaches them.
        let mut at_a_desk = message.clone();
        at_a_desk.author.voice.channel = None;
        let envelope = render_delivery(&at_a_desk, false, "nearby");
        assert!(envelope.contains("talk_post"), "{envelope}");
        assert!(envelope.contains("A person wrote this"), "{envelope}");

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_message_is_typed_in_as_one_submission() {
        // Everything between the brackets is one paste, and the only carriage
        // return is the one that submits it.
        assert_eq!(
            String::from_utf8(paste_bytes("first\r\nsecond\rthird", true)).unwrap(),
            "\x1b[200~first\nsecond\nthird\x1b[201~\r"
        );
        assert!(!paste_bytes("draft only", false).ends_with(b"\r"));

        // A CLI that would submit line by line gets one line.
        assert_eq!(folded("a\n\nb   \nc"), "a ⏎ b ⏎ c");

        assert_eq!(TalkDeliver::parse("when_idle").unwrap().name(), "when_idle");
        assert!(TalkDeliver::parse("whenever").is_err());
    }

    #[test]
    fn a_queued_message_goes_in_on_an_empty_prompt_however_busy_the_agent_is() {
        let waiting = TalkQueued {
            message_id: "far-away:1".into(),
            session_id: "session-2".into(),
            body: "[muxloom] Message from …".into(),
            queued_at: 1_000,
            deliver: TalkDeliver::Auto,
            from: Some(TalkAddress {
                machine: "far-away".into(),
                session_id: "session-1".into(),
            }),
            text: "the parser is yours".into(),
            channel: None,
            session_label: None,
        };
        // The turn in progress is beside the point: the prompt is empty, so the
        // message lands whole and is read when the turn ends.
        assert!(waiting.due(Composer::Ready, true, false, 1_000));
        assert!(waiting.due(Composer::Ready, false, false, 1_000));
        // Half a sentence somebody has not sent yet is worth waiting out.
        assert!(!waiting.due(Composer::Occupied, false, false, 1_000));
        assert!(!waiting.due(Composer::Absent, false, false, 1_000));
    }

    #[test]
    fn a_queued_message_waits_out_a_busy_prompt_but_not_forever() {
        let waiting = TalkQueued {
            message_id: "far-away:1".into(),
            session_id: "session-2".into(),
            body: "[muxloom] Message from …".into(),
            queued_at: 1_000,
            deliver: TalkDeliver::Auto,
            from: Some(TalkAddress {
                machine: "far-away".into(),
                session_id: "session-1".into(),
            }),
            text: "the parser is yours".into(),
            channel: None,
            session_label: None,
        };
        // A runtime that has not drawn its prompt is usually still starting up.
        assert!(waiting.due(Composer::Absent, false, false, 1_000 + DELIVER_ABSENT_MS));
        // A half-typed prompt gets longer, since going in means joining it.
        assert!(!waiting.due(Composer::Occupied, false, false, 1_000 + DELIVER_ABSENT_MS));
        assert!(waiting.due(
            Composer::Occupied,
            false,
            false,
            1_000 + DELIVER_OCCUPIED_MS
        ));
        // A question on screen is never answered by somebody else's message.
        assert!(!waiting.due(Composer::Absent, false, true, 1_000 + DELIVER_EXPIRY_MS));

        let patient = TalkQueued {
            deliver: TalkDeliver::WhenIdle,
            ..waiting.clone()
        };
        assert!(!patient.due(Composer::Ready, true, false, 1_000 + DELIVER_OCCUPIED_MS));
        assert!(patient.due(Composer::Ready, false, false, 1_000));
        assert!(!patient.due(Composer::Absent, false, false, 1_000 + DELIVER_OCCUPIED_MS));
        assert!(!patient.expired(1_000 + DELIVER_OCCUPIED_MS));
        assert!(patient.expired(1_000 + DELIVER_EXPIRY_MS));
    }

    #[test]
    fn a_message_that_never_arrives_is_sent_back_saying_so() {
        let queued = TalkQueued {
            message_id: "far-away:1".into(),
            session_id: "session-2".into(),
            body: "[muxloom] Message from …".into(),
            queued_at: 1_000,
            deliver: TalkDeliver::Auto,
            from: Some(TalkAddress {
                machine: "far-away".into(),
                session_id: "session-1".into(),
            }),
            text: "can you take the lexer?".into(),
            channel: None,
            session_label: None,
        };

        let bounce = render_bounce(&queued, &TalkUndelivered::Ended);
        // Which message, what became of it, and the one thing the sender needs
        // to hear: nobody is going to answer.
        assert!(bounce.contains("session session-2"), "{bounce}");
        assert!(
            bounce.contains("the session ended while the message was waiting"),
            "{bounce}"
        );
        assert!(bounce.contains("no answer is coming"), "{bounce}");
        // Quoted back, so the sender knows which question died without having
        // to go and look the message up.
        assert!(bounce.contains("can you take the lexer?"), "{bounce}");

        // The expiry is quoted in the units the sender thinks in.
        let waited = render_bounce(&queued, &TalkUndelivered::NoPrompt);
        assert!(waited.contains("in 30 minutes"), "{waited}");

        // Whatever the write failed with is passed through rather than
        // flattened into "something went wrong".
        let failed = render_bounce(&queued, &TalkUndelivered::Failed("broken pipe".into()));
        assert!(failed.contains("broken pipe"), "{failed}");

        // Nothing to quote is not an empty pair of markers.
        let empty = TalkQueued {
            text: String::new(),
            ..queued
        };
        let bounce = render_bounce(&empty, &TalkUndelivered::Gone);
        assert!(!bounce.contains("what you sent"), "{bounce}");
    }

    #[test]
    fn a_message_a_person_sent_from_a_chat_is_bounced_back_to_the_chat() {
        let queued = TalkQueued {
            message_id: "lark-1:1".into(),
            session_id: "muxloomd-claude-1788002025-2669-0".into(),
            body: "[muxloom] Message from …".into(),
            queued_at: 1_000,
            deliver: TalkDeliver::Auto,
            // A person has no session to be written back to on the board, which
            // is exactly why this path exists.
            from: None,
            text: "can you take the lexer?".into(),
            channel: Some("lark-1".into()),
            session_label: Some("lexer".into()),
        };

        let bounce = render_channel_bounce(&queued, &TalkUndelivered::Ended);
        // The name they aimed at, never the id: an id is not something they
        // have ever seen, let alone something they can match this against.
        assert!(bounce.contains("lexer"), "{bounce}");
        assert!(!bounce.contains("muxloomd-claude"), "{bounce}");
        assert!(
            bounce.contains("the session ended while the message was waiting"),
            "{bounce}"
        );
        // What to do next, in the two commands this chat takes.
        assert!(bounce.contains("/who"), "{bounce}");
        assert!(bounce.contains("/select"), "{bounce}");
        assert!(bounce.contains("can you take the lexer?"), "{bounce}");

        // A session that never had a name is named by the only thing left.
        let nameless = TalkQueued {
            session_label: None,
            ..queued.clone()
        };
        let bounce = render_channel_bounce(&nameless, &TalkUndelivered::Gone);
        assert!(
            bounce.contains("muxloomd-claude-1788002025-2669-0"),
            "{bounce}"
        );

        // Long enough to recognise, short enough to read on a phone: this
        // lands on one, and they can scroll up to the rest themselves.
        let essay = TalkQueued {
            text: "x".repeat(CHANNEL_BOUNCE_QUOTE * 3),
            ..queued.clone()
        };
        let bounce = render_channel_bounce(&essay, &TalkUndelivered::NoPrompt);
        assert!(
            bounce.ends_with(&format!("{}…", "x".repeat(CHANNEL_BOUNCE_QUOTE))),
            "{bounce}"
        );

        // Nothing to quote is not an empty pair of markers here either.
        let empty = TalkQueued {
            text: String::new(),
            ..queued
        };
        let bounce = render_channel_bounce(&empty, &TalkUndelivered::Gone);
        assert!(!bounce.contains("what you sent"), "{bounce}");
    }
}
