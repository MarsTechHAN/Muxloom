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
/// The path namespace muxloom keeps its own coordination notes under: which
/// machine holds a chat account's lease, what a person said about an approval
/// another machine parked. Reserved — a post from a tool call is refused here,
/// because none of it is anybody's memory.
pub const WIRE_PATH_PREFIX: &str = "/muxloom/";
/// What the log holding that traffic is called, next to the one holding what
/// was written down.
const WIRE_SUFFIX: &str = "_wire";
/// The same two limits for that log. Small and short, because none of it is
/// worth reading tomorrow: a lease expires in forty-five seconds and a
/// delivered message has already been delivered.
const RETAIN_PER_WIRE_ORIGIN: usize = 1_000;
const RETAIN_WIRE_MS: u64 = 12 * 60 * 60 * 1000;
/// How far past the retention limit a log may run before it is rewritten.
/// Compaction rewrites a whole file, so it must not happen on every post.
const COMPACT_SLACK: usize = 512;
/// The most messages one read or one replication fetch carries.
pub const MAX_PAGE: usize = 500;

/// The least a note has to say to be one.
///
/// Not a style rule. A note is read by somebody who was not here and has no
/// idea what anyone was doing, so anything that only makes sense to whoever is
/// looking right now is not a note however it is labelled — and a board of
/// those is the "temporary display board" a memory turns into. Twenty-four
/// characters is about the shortest a fact fits in ("APFS mtime is
/// nanosecond." is twenty-five) and comfortably longer than the status words
/// that used to arrive as notes: done, restarted, still waiting.
const NOTE_MIN_CHARS: usize = 24;

/// Whether a note is written down rather than said to whoever is looking.
///
/// Checked in the store, so it holds for every writer rather than for whichever
/// surface remembered to check, and only for an agent: a person writing on
/// their own board is not somebody to lecture about how to write it.
///
/// Three shapes, each of which has a tool that is actually the right one:
/// a note that opens by naming somebody is a message to them, a note that ends
/// in a question is a question, and a note too short to stand on its own is a
/// remark. All three used to land on the board and stay for thirty days.
fn note_discipline(text: &str) -> Result<()> {
    if text.starts_with(['@', '\u{ff20}']) {
        bail!(
            "a note that opens by naming somebody is a message to them, and the board does not \
             reach anybody: use message_agent, which does. What goes here is written for whoever \
             reads the board next"
        );
    }
    if text.ends_with(['?', '\u{ff1f}']) {
        bail!(
            "a note that ends in a question is a question, and nobody owes the board an answer: \
             ask it with message_agent. A note says what is true"
        );
    }
    let held = text.chars().count();
    if held < NOTE_MIN_CHARS {
        bail!(
            "{held} characters is a remark, not something worth keeping for thirty days. Write it \
             for somebody who was not here: what is true, what it cost to find out, and what to \
             do about it — or say it to somebody with message_agent, or put it in your head name \
             with set_head_name if it is what you are doing right now"
        );
    }
    Ok(())
}

/// Whether a record is muxloom talking to itself rather than something
/// somebody wrote down.
///
/// Two kinds of traffic ride the board because it is the only thing every
/// machine replicates: the coordination notes under [`WIRE_PATH_PREFIX`], and
/// direct messages, which are filed after delivery so a machine can be read
/// later for what its agents said to each other. Neither is memory, and both
/// are written thousands of times more often than memory is — on the board
/// this was measured on, twenty-three thousand lease heartbeats against about
/// two hundred real notes, with the real notes already being evicted by them.
///
/// So they go in a log of their own. Not a filter over one log: the log is
/// append-only and sequence-numbered, and a record dropped from the middle of
/// one leaves a hole that stops the version vector, so peers re-send it and it
/// comes straight back. Separate sequence spaces are the only way one class of
/// record can be dropped on a schedule of its own.
fn is_wire(kind: TalkKind, scope: &TalkScope) -> bool {
    kind == TalkKind::Direct
        || matches!(scope, TalkScope::Path { path, .. } if path.starts_with(WIRE_PATH_PREFIX))
}

/// Which log a machine files that traffic in. Its own, beside the durable one.
fn wire_origin(origin: &str) -> String {
    format!("{origin}{WIRE_SUFFIX}")
}

/// How much of a log is kept, and for how long, by which log it is.
fn retention_for(origin: &str) -> (usize, u64) {
    match origin.ends_with(WIRE_SUFFIX) {
        true => (RETAIN_PER_WIRE_ORIGIN, RETAIN_WIRE_MS),
        false => (RETAIN_PER_ORIGIN, RETAIN_MS),
    }
}

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

    /// What the board calls this room, which is what a person and an agent
    /// read and type: a thread, a folder, a machine, or everyone. [`name`] is
    /// the word the store and the wire keep using, so a record written by one
    /// build reads the same in every other.
    ///
    /// [`name`]: Self::name
    pub fn board_name(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Machine { .. } => "machine",
            Self::Path { .. } => "folder",
            Self::Task { .. } => "thread",
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
    ///
    /// Legacy. Nothing mints one any more: the board is a memory keyed by
    /// task, folder, machine or fleet, and a remark addressed to whoever
    /// happens to be reading is the one thing it is not for. The variant
    /// stays because machines running older builds still mint them and their
    /// records have to replicate and render — dropping the variant would
    /// leave a hole in their sequence space that every round tries to fill
    /// forever.
    Message,
    /// Something written down to be found later, and the only kind anything
    /// files now: the board is the memory a session cannot keep.
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

/// What one board hands another: everything held past a peer's vector, plus
/// the prefixes an origin's owner has dropped for good, as far as this board
/// knows them.
///
/// The floor has to travel with the messages, because it is the only thing
/// that can move a peer past a hole it will never be offered. A receiver
/// adopts the floor it is handed and never lowers it: the run it can claim
/// then starts above the gap, so the messages beyond the gap advance its
/// vector instead of being re-offered behind it forever.
///
/// A high sequence number inside a message is not a floor. Only a board's
/// statement about a dropped prefix is one; reading a hole as a retention
/// would make ordinary out-of-order replication throw records away. And the
/// statement is the owner's, not the carrier's: a board may publish a floor
/// for an origin it owns, and for one whose owner's floor it was told. That is
/// what lets a controller carrying a machine's batch to the next machine relay
/// what the owner said, while a replica clearing its own copy of somebody
/// else's log has nothing to publish. See [`TalkStore::since`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TalkBatch {
    pub messages: Vec<TalkMessage>,
    /// The owner's dropped prefix per origin, as far as this board knows it.
    /// A floor travels whether or not the peer's vector already sits above it:
    /// it is metadata, not payload, and a peer that was running an older build
    /// may hold the records above a gap while covering the gap in its vector,
    /// having never been told the prefix is gone. A receiver adopts a floor
    /// monotonically, so a value it already knows is ignored.
    pub low_water: TalkVector,
}

impl TalkBatch {
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, TalkMessage> {
        self.messages.iter()
    }
}

impl From<Vec<TalkMessage>> for TalkBatch {
    /// A batch with no floors: what a caller that only has messages can say.
    fn from(messages: Vec<TalkMessage>) -> Self {
        Self {
            messages,
            low_water: TalkVector::new(),
        }
    }
}

/// What one machine holds, as told to another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkState {
    pub origin: String,
    pub label: String,
    pub vector: TalkVector,
    /// The highest sequence number this board has dropped per origin, by its
    /// own retention or by adopting an owner's floor. This is the local
    /// compaction mark, not the claim replication carries: for an origin this
    /// machine does not own, clearing its copy says nothing about what the
    /// origin still holds. See [`TalkStore::since`] for what a peer is told.
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
/// [`oldest_page`], which is what such a read gets instead.
struct NewestFirst {
    limit: usize,
    /// Oldest first, so the one to drop is the one on top.
    heap: BinaryHeap<Oldest>,
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

/// One origin's unread messages, walked in sequence order, as a following page
/// takes them.
///
/// A page a caller follows has to be a prefix of every origin it speaks for.
/// Such a caller hands the cursor back and is told only what has happened
/// since, so a message the page stepped over is lost: the cursor is past it,
/// and the next read filters it out. Time alone does not give that prefix - a
/// machine's clock can step backwards between two posts, and a page chosen by
/// time would hand out the later sequence first while the cursor moved past
/// the earlier one. Sequence numbers are the machine's own order of what it
/// said; time only decides which origin's next message the page takes.
struct UnreadRun<'a> {
    /// The origin whose sequences these are, and the tie-break between two
    /// messages stamped at the same millisecond.
    origin: &'a str,
    /// The origin's log after the caller's mark, in sequence order, still to
    /// be walked.
    messages: std::slice::Iter<'a, TalkMessage>,
    /// The next message of it the page may take, if any.
    next: Option<&'a TalkMessage>,
}

impl<'a> UnreadRun<'a> {
    fn new(origin: &'a str, messages: &'a [TalkMessage]) -> Self {
        Self {
            origin,
            messages: messages.iter(),
            next: None,
        }
    }

    /// Walk forward to the next message the page may take, skipping what the
    /// caller's filter leaves out.
    fn advance(&mut self, accept: &impl Fn(&TalkMessage) -> bool) {
        self.next = self.messages.find(|message| accept(message));
    }
}

/// The oldest `limit` of what the runs offer, oldest first.
///
/// Each run is walked in its own sequence order and contributes only a prefix,
/// with time deciding which origin goes next. Keeping the oldest end is what a
/// caller following a cursor needs: such a caller hands the cursor back and is
/// told only what has happened since, so a page that was the tail of a burst
/// would leave the messages in between dropped and then skipped - and the one
/// they were most likely to be was an answer somebody was waiting on, because
/// an answer arrives before the noise that buries it.
///
/// Taking each origin in sequence order is what makes the cursor of a
/// truncated page safe. Everything this reader still had to see is above the
/// last sequence the page took from that origin, so the cursor the caller is
/// handed cannot step over it. Where time is what orders a page, a clock that
/// stepped backwards could put the later sequence in the page and leave the
/// earlier one below the cursor.
///
/// Returns whether anything matched that the page had no room for.
fn oldest_page(
    mut runs: Vec<UnreadRun<'_>>,
    limit: usize,
    accept: impl Fn(&TalkMessage) -> bool,
) -> (bool, Vec<TalkMessage>) {
    for run in &mut runs {
        run.advance(&accept);
    }
    let mut page = Vec::new();
    while page.len() < limit {
        let Some(at) = runs
            .iter()
            .enumerate()
            .filter_map(|(at, run)| {
                run.next
                    .map(|message| (message.ts, run.origin, message.seq, at))
            })
            .min()
            .map(|(_, _, _, at)| at)
        else {
            break;
        };
        let message = runs[at]
            .next
            .take()
            .expect("a run was chosen for its next message");
        page.push(message.clone());
        runs[at].advance(&accept);
    }
    let dropped = runs.iter().any(|run| run.next.is_some());
    (dropped, page)
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
    /// The highest seq compacted away on this machine. This is local
    /// housekeeping: it starts this log's contiguity above a hole and keeps
    /// re-offered records out, but for an origin this machine does not own it
    /// says nothing about the origin's sequence space.
    low_water: u64,
    /// The highest seq the origin's owner has said is gone for good, as far as
    /// this machine can state it. Only the owner's word raises it, heard
    /// directly, carried by another board, or read back from the header an
    /// explicit clear or retention of a log this machine owns wrote down. A
    /// local clear of a copy never does, which is what stops a replica
    /// poisoning a peer with a prefix the origin still holds, while a hub that
    /// legitimately learned the floor can pass it on.
    owner_low_water: u64,
}

impl OriginLog {
    /// The floor this board may state to a peer for this origin.
    ///
    /// This is the owner's word, never the local serving mark. For an origin
    /// this machine owns, an explicit clear or retention is its own word and
    /// raises this mark when it writes the header; for a copy, only a floor
    /// learned from the real owner counts. A mark inferred from the oldest
    /// record still held is deliberately not part of it: a truncated or
    /// corrupt owned log has no header saying the missing prefix is gone, and
    /// a peer told otherwise would skip a record the origin might recover.
    fn published_floor(&self) -> u64 {
        self.owner_low_water
    }

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
            .map_or(self.low_water, |message| message.seq.max(self.low_water))
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
        let logs = load_logs(&root.join("log"), &identity.origin);
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
        // Which log this is filed in, which is not the same question as which
        // machine said it: `author.machine` and every address on the record
        // stay this machine's own key, and only the sequence space moves.
        let wire = is_wire(draft.kind, &draft.scope);
        let log = match wire {
            true => wire_origin(&origin),
            false => origin.clone(),
        };
        // What an agent writes down has to be something written down. A person
        // at a dashboard or on a phone is exempt: it is their board, and the
        // rules exist to stop a fleet of agents turning it into a feed.
        if !wire && draft.kind == TalkKind::Note && !draft.author.voice.human {
            note_discipline(text)?;
        }
        let mut inner = self.inner();
        // A memory does not need saying twice. The same voice writing the same
        // thing down again is a session that has forgotten it already did —
        // which is exactly what happens when an agent loses its context and
        // starts over, and what turned a board into hundreds of copies of the
        // same sentence. Only its own voice is checked: two agents reaching
        // the same conclusion is worth having twice, once each.
        if !wire && draft.kind == TalkKind::Note && draft.author.voice.session_id.is_some() {
            if let Some(held) = inner.logs.get(&log).and_then(|held| {
                held.messages.iter().rev().find(|message| {
                    message.kind == TalkKind::Note
                        && message.text == text
                        && message.scope == draft.scope
                        && message.author.voice.session_id == draft.author.voice.session_id
                })
            }) {
                bail!(
                    "you already wrote this down, word for word, as {}. A memory does not need \
                     saying twice; if what you know has changed, say what changed and reply_to \
                     that one",
                    held.id
                );
            }
        }
        let seq = inner
            .logs
            .get(&log)
            .map_or(0, OriginLog::highest)
            .saturating_add(1);
        let message = TalkMessage {
            id: format!("{log}:{seq}"),
            origin: log,
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

    /// File messages that came from somewhere else. Returns how many messages
    /// were new; filing the same message twice is a no-op, which is what lets
    /// the controller push and pull without tracking what it has already
    /// carried.
    ///
    /// A floor carried by the batch is adopted here too, and a batch may carry
    /// one without carrying a message: the floor alone is what moves this
    /// board's run above a gap that no page can fill, so dropping it would
    /// leave the peer behind the gap until a post happened to share a page
    /// with it. A caller that only has messages hands over a batch with no
    /// floors, so nothing is ever inferred from a hole.
    pub fn merge(&self, batch: impl Into<TalkBatch>) -> Result<usize> {
        self.merge_batch(batch.into())
    }

    /// File a batch a board handed over during replication, adopting the owner
    /// floors it carries.
    ///
    /// `sender` names the board that handed the batch over. It stays in the
    /// signature — and on the wire — so controllers and daemons from different
    /// builds keep talking, but receipt no longer judges a floor by it. It
    /// cannot: `run_sync` is a hub, and a batch the controller pushes to the
    /// next machine names the controller, while the floor inside it belongs to
    /// the origin the controller heard from. A sender check made that relay
    /// indistinguishable from a replica speaking for an origin it does not
    /// own, and the peer behind the controller stalled below the gap forever.
    ///
    /// What keeps the statement honest is the publishing side instead: a board
    /// publishes a floor only for an origin it owns or one whose owner it was
    /// told about, and a local clear of a copy publishes nothing. See
    /// [`TalkStore::since`]. Entries are adopted monotonically and only as
    /// stated; a floor is never inferred from a gap or an out-of-order
    /// sequence number.
    pub fn merge_from(&self, _sender: &str, batch: impl Into<TalkBatch>) -> Result<usize> {
        self.merge_batch(batch.into())
    }

    fn merge_batch(&self, batch: TalkBatch) -> Result<usize> {
        let TalkBatch {
            messages,
            low_water,
        } = batch;
        let mut inner = self.inner();
        self.adopt_owner_floors(&mut inner, &low_water)?;
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

    /// Take a board's word for a prefix the origin's owner says is gone.
    ///
    /// Both marks move: the local one because this board will not hold those
    /// records again, and the owner's one because from here on this board may
    /// relay the claim when it syncs another machine. That second mark is what
    /// lets a controller carry a machine's floor to the machines behind it;
    /// without it the hub stalled every peer it served.
    ///
    /// Never lower: a floor says records are gone for good, and the board
    /// saying so may know less than this one does about a prefix it already
    /// dropped. Raising the local mark is what lets [`OriginLog::contiguous`]
    /// start above the hole, so a message past a compacted prefix advances the
    /// vector instead of waiting below a message that will never arrive.
    fn adopt_owner_floors(&self, inner: &mut TalkInner, floors: &TalkVector) -> Result<()> {
        for (origin, floor) in floors {
            if *floor == 0 || !valid_origin(origin) {
                continue;
            }
            let log = inner.logs.entry(origin.clone()).or_default();
            if *floor <= log.low_water && *floor <= log.owner_low_water {
                continue;
            }
            log.low_water = log.low_water.max(*floor);
            log.owner_low_water = log.owner_low_water.max(*floor);
            rewrite_log(
                &self.root.join("log").join(format!("{origin}.jsonl")),
                origin,
                log.low_water,
                log.owner_low_water,
                &log.messages,
            )?;
        }
        Ok(())
    }

    /// Everything held past a peer's vector, oldest first, so a peer that is
    /// far behind catches up over several fetches rather than one huge one.
    ///
    /// Each origin contributes only the run of its own sequence numbers from
    /// the peer's mark, in sequence order; time only interleaves the origins.
    /// A batch that took a later sequence from an origin before an earlier one
    /// would leave a hole below everything it carried: the peer's vector stops
    /// at the hole, so the next diff asks for the same batch again and the
    /// message the peer is missing is never in it. A clock that stepped
    /// backwards is no reason to reorder what a machine said.
    ///
    /// A batch is at most [`MAX_PAGE`] and the board may hold tens of
    /// thousands, so the walk stops as soon as the page is full and copies
    /// only the end it is going to hand back. This runs on every sync round
    /// against every reachable machine, which is the one place on the board
    /// where the cost of the whole board is paid over and over.
    ///
    /// Every batch also carries every owner floor this board can state, for
    /// every origin it knows one — whether or not the peer's vector already
    /// sits at or above it. A floor is metadata, not payload: a machine that
    /// was running an older build can hold every record above a gap and cover
    /// the gap in its vector, having missed the floor completely, and a
    /// publication filtered by that vector would never reach it. A receiver
    /// adopts a floor monotonically and ignores a value it already knows, so
    /// restating one costs a word and is what lets an upgraded hub learn and
    /// relay a floor it missed. Message selection is unchanged and still
    /// starts above the peer's vector.
    ///
    /// The floor is the owner's, not this board's local serving mark. For an
    /// origin this machine owns, an explicit clear or retention is the owner's
    /// statement; for a copy it is not, and only a floor learned from the owner
    /// may travel. Clearing this machine's copy of somebody else's log is
    /// housekeeping, and passing that mark on would make a peer skip records
    /// the origin still holds. A hub that was told the owner's floor may relay
    /// it, which is how a machine synced through the controller hears that an
    /// origin's prefix is gone.
    pub fn since(&self, from: &TalkVector, limit: usize) -> TalkBatch {
        let inner = self.inner();
        let runs: Vec<UnreadRun<'_>> = inner
            .logs
            .iter()
            .filter_map(|(origin, log)| {
                let held = from.get(origin).copied().unwrap_or(0);
                // Nothing at or below this board's own watermark can be filed
                // by a peer that is behind it: those records are gone for
                // good, and a page spent on them is a page the peer cannot
                // use.
                let held = held.max(log.low_water);
                // An origin is sorted by seq, so where the peer's knowledge
                // ends is found rather than walked to. The common case by far
                // is a peer that is already up to date, and that case should
                // not cost a pass over everything the origin has ever said.
                let fresh = log.messages.partition_point(|message| message.seq <= held);
                if fresh < log.messages.len() {
                    Some(UnreadRun::new(origin, &log.messages[fresh..]))
                } else {
                    None
                }
            })
            .collect();
        TalkBatch {
            messages: oldest_page(runs, limit.clamp(1, MAX_PAGE), |_| true).1,
            low_water: inner
                .logs
                .iter()
                .map(|(origin, log)| (origin.clone(), log.published_floor()))
                .filter(|(_, floor)| *floor > 0)
                .collect(),
        }
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
        // page carries at most a few dozen, so neither end collects every
        // match to sort and throw away. What that saves is a copy of each
        // message it looked at - and a read of the whole board is what a caller
        // waiting for an answer does every couple of seconds.
        //
        // Which end depends on what the caller is doing. Arriving at the board
        // and asking what is on it wants the newest, with `before` to page
        // back; the cursor there is only the mark following starts from.
        // Following a cursor wants the oldest of what is new, and it has to be
        // a prefix of every origin it speaks for: that caller will hand the
        // cursor back and never be offered anything this page left out.
        let following = !filter.since.is_empty();
        let accepts = |message: &TalkMessage| -> bool {
            message.seq > filter.since.get(&message.origin).copied().unwrap_or(0)
                && filter.before.is_none_or(|before| message.ts < before)
                && visible(message, filter, &mine, &label)
                && (filter.kinds.is_empty()
                    || filter.kinds.iter().any(|kind| kind == message.kind.name()))
                && (filter.authors.is_empty()
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
                && query
                    .as_ref()
                    .is_none_or(|query| message.text.to_lowercase().contains(query))
        };
        let (truncated, matched) = if following {
            let runs: Vec<UnreadRun<'_>> = inner
                .logs
                .iter()
                .filter_map(|(origin, log)| {
                    let held = filter.since.get(origin).copied().unwrap_or(0);
                    let fresh = log.messages.partition_point(|message| message.seq <= held);
                    if fresh < log.messages.len() {
                        Some(UnreadRun::new(origin, &log.messages[fresh..]))
                    } else {
                        None
                    }
                })
                .collect();
            oldest_page(runs, limit, accepts)
        } else {
            let mut newest = NewestFirst::new(limit);
            for message in inner.logs.values().flat_map(|log| log.messages.iter()) {
                if accepts(message) {
                    newest.offer(message);
                }
            }
            (newest.dropped(), newest.into_page())
        };
        // Everything the board holds is what the caller has been shown, unless
        // the page had to leave some of it out. Then the cursor may reach no
        // further than the newest message actually in hand. A following page
        // takes each origin in sequence order and only a prefix of what that
        // origin still has to say, so everything at or below the mark either
        // went out with the page or was filtered out on the way, and the
        // messages left behind are all above it and will be offered again next
        // read. An origin this page says nothing about keeps the caller's own
        // mark, which costs a second look at messages that matched nothing and
        // loses none that did.
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
        let mine = inner.identity.origin.clone();
        let message_origin = message.origin.clone();
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
            .join(format!("{message_origin}.jsonl"));
        append_line(&path, &message)?;
        log.messages.insert(at, message);
        let (retain, _) = retention_for(&message_origin);
        if log.messages.len() > retain + COMPACT_SLACK {
            let cut = log.messages.len() - retain;
            // A watermark adopted from a peer can sit above records this board
            // still holds; retention must not walk it back down.
            log.low_water = log.low_water.max(log.messages[cut - 1].seq);
            if owns_origin(&mine, &message_origin) {
                // Retention on a log this machine owns is the owner dropping
                // its own prefix, so it may be stated to a peer; a copy's
                // retention is housekeeping and stays local.
                log.owner_low_water = log.owner_low_water.max(log.low_water);
            }
            log.messages.drain(..cut);
            rewrite_log(
                &path,
                &message_origin,
                log.low_water,
                log.owner_low_water,
                &log.messages,
            )?;
        }
        Ok(true)
    }

    /// Drop everything this board holds, keeping only the mark that says it
    /// was held.
    ///
    /// The mark is the whole of why this is not `rm`. A record dropped from
    /// the middle of a log leaves a hole that stops the version vector, so
    /// peers re-send it and it comes straight back; dropping every record and
    /// forgetting where they ended is worse still, because then every peer
    /// re-sends everything. Clearing is therefore the same shape as retention:
    /// a prefix drop that writes down what it dropped, after which a peer
    /// offering any of it again is simply refused.
    ///
    /// It clears this machine's board. Another machine's copy is that
    /// machine's to clear — the board is replicated, not shared, and nothing
    /// here can speak for a daemon it cannot see. Clearing therefore raises
    /// only this machine's local mark for a copied origin, while for an origin
    /// owned here the mark is also the owner's floor and is written down as
    /// such; [`TalkStore::since`] publishes that floor and never a copy's.
    pub fn clear(&self) -> Result<usize> {
        let mut inner = self.inner();
        let mine = inner.identity.origin.clone();
        let root = self.root.join("log");
        let mut dropped = 0;
        for (origin, log) in inner.logs.iter_mut() {
            if log.messages.is_empty() {
                continue;
            }
            dropped += log.messages.len();
            log.low_water = log.highest();
            if owns_origin(&mine, origin) {
                log.owner_low_water = log.owner_low_water.max(log.low_water);
            }
            log.messages.clear();
            rewrite_log(
                &root.join(format!("{origin}.jsonl")),
                origin,
                log.low_water,
                log.owner_low_water,
                &log.messages,
            )?;
        }
        Ok(dropped)
    }

    /// Drop what has aged out. Called when the board is opened rather than on
    /// a timer: a daemon that nobody talks to should not be doing work.
    pub fn compact(&self) -> Result<()> {
        let mut inner = self.inner();
        let mine = inner.identity.origin.clone();
        let now = now_ms();
        let root = self.root.join("log");
        for (origin, log) in inner.logs.iter_mut() {
            let cutoff = now.saturating_sub(retention_for(origin).1);
            let keep = log
                .messages
                .iter()
                .position(|message| message.ts >= cutoff)
                .unwrap_or(log.messages.len());
            if keep == 0 {
                continue;
            }
            // A watermark adopted from a peer can sit above records this board
            // still holds; retention must not walk it back down.
            log.low_water = log.low_water.max(log.messages[keep - 1].seq);
            if owns_origin(&mine, origin) {
                // As in `file`: aging out an owned log is the owner dropping
                // its own prefix, so write down that it may be stated.
                log.owner_low_water = log.owner_low_water.max(log.low_water);
            }
            log.messages.drain(..keep);
            rewrite_log(
                &root.join(format!("{origin}.jsonl")),
                origin,
                log.low_water,
                log.owner_low_water,
                &log.messages,
            )?;
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
    // The board is the fleet's memory, kept in four rooms, and it holds notes.
    // A direct message is the mailbox beside it — two sessions talking, kept
    // hours rather than weeks — and is read by asking for the mailbox, never
    // by reading the board: a board that showed every message one agent sent
    // another was a feed, and the notes worth keeping were the ones a reader
    // scrolled past.
    if message.kind == TalkKind::Direct {
        if filter.scope.as_deref() != Some("direct") {
            return false;
        }
        // And the mailbox is the reader's own: asking for it by name is not
        // the whole of the check, or every session on the machine would be
        // handed every agent-to-agent direct on it. Only the two ends of the
        // conversation read it — and the person whose machines these are,
        // which is what `owner` means. A caller with no session of its own is
        // neither end of anything.
        if filter.owner {
            return true;
        }
        let Some(me) = filter.session_id.as_deref() else {
            return false;
        };
        let addressed_to_me = message
            .to
            .as_ref()
            .is_some_and(|to| to.session_id.as_str() == me);
        let sent_by_me = message
            .author
            .voice
            .session_id
            .as_deref()
            .is_some_and(|sender| sender == me);
        return addressed_to_me || sent_by_me;
    }
    // Nor is the older kind, "message": a person speaking at a dashboard, or a
    // build from before notes. Kept, because a log cannot have holes in it, and
    // shown only to a reader that asks for it by kind.
    if message.kind == TalkKind::Message && !filter.kinds.iter().any(|kind| kind == "message") {
        return false;
    }
    match &message.scope {
        TalkScope::Global => true,
        TalkScope::Machine { machine } => filter.machines.admits(
            Some(mine),
            machine,
            machine_label(message, machine, mine, label),
        ),
        TalkScope::Path { machine, path } => {
            // muxloom's own coordination notes are on the board because every
            // machine replicates it, not because anyone is meant to read them.
            // They surface only for the code that asks for that exact path, so
            // a person or an agent reading the board sees the board.
            if path.starts_with(WIRE_PATH_PREFIX)
                && !matches!(&filter.paths, TalkSelector::Only { names } if names
                    .iter()
                    .any(|name| name == path))
            {
                return false;
            }
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

/// The stored name of the scope a caller named, in the board's words or the
/// older ones: `thread` or `task`, `folder` or `path`, `machine`, `global` — and
/// `direct`, which names the mailbox rather than a room on the board. `None` for
/// a word that is none of them.
pub fn scope_word(word: &str) -> Option<&'static str> {
    match word.trim().to_ascii_lowercase().as_str() {
        "thread" | "task" => Some("task"),
        "folder" | "path" => Some("path"),
        "machine" => Some("machine"),
        "global" => Some("global"),
        "direct" => Some("direct"),
        _ => None,
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

/// Whether this board owns an origin's sequence space: its own log, or the
/// companion log it files wire traffic in. Anything else is a copy.
fn owns_origin(mine: &str, origin: &str) -> bool {
    origin == mine || wire_origin(mine) == origin
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

fn rewrite_log(
    path: &Path,
    origin: &str,
    low_water: u64,
    owner_low_water: u64,
    messages: &[TalkMessage],
) -> Result<()> {
    let temporary = path.with_extension("jsonl.tmp");
    let mut file = File::create(&temporary)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    restrict(&file);
    if low_water > 0 || owner_low_water > 0 {
        let mut header = serde_json::to_vec(&LogHeader {
            log: origin.to_string(),
            dropped_through: low_water,
            owner_dropped_through: owner_low_water,
        })
        .context("failed to encode a talk log header")?;
        header.push(b'\n');
        file.write_all(&header)?;
    }
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

/// The first line of a rewritten log: what that log has already dropped.
///
/// A log knows its own [`OriginLog::low_water`] while a daemon is running, but
/// it was only ever written down inside the records themselves — read back as
/// "one below the oldest record still here". A log compacted down to nothing
/// therefore forgot it completely, the origin vanished from this machine's
/// version vector, and the next replication round asked every peer for the
/// whole thirty days again and filed all of it a second time. Retention that
/// only holds until the next restart is not retention.
///
/// [`OriginLog::owner_low_water`] is written beside it because it cannot be
/// recovered from the records: a floor adopted from an origin belongs to a
/// sequence space this machine may not hold anything of, and losing it on
/// restart would leave every peer that syncs through this machine stalled
/// behind the gap again. The same mark is what lets an owned log's own clear
/// or retention survive a restart as a statement a peer may hear; a mark
/// derived from the oldest record still held is local serving only and is
/// never published as the owner's floor.
///
/// Written only when there is something to say, and shaped so that a build
/// that predates it skips the line as unparseable and falls back to deriving
/// the same number from the records — lower, never higher, so it can cost a
/// re-fetch but can never drop a record on the floor.
#[derive(Debug, Serialize, Deserialize)]
struct LogHeader {
    log: String,
    dropped_through: u64,
    /// The owner's floor as this board knew it. Defaulted so a log written by
    /// a build that predates the distinction still opens: its local mark is
    /// all it has, which the owner of the log may state as its own floor and
    /// a copy may not.
    #[serde(default)]
    owner_dropped_through: u64,
}

fn load_logs(root: &Path, mine: &str) -> BTreeMap<String, OriginLog> {
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
        // The file is named after the log it holds, which is the only thing
        // that still says which log an emptied one was.
        let Some(origin) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
            .filter(|stem| valid_origin(stem))
        else {
            continue;
        };
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut log = OriginLog::default();
        let mut headed = false;
        // A half-written line is what a crash leaves behind, and the rest of
        // the log is still perfectly good: skip it rather than lose the file.
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(message) = serde_json::from_str::<TalkMessage>(&line) else {
                if let Ok(header) = serde_json::from_str::<LogHeader>(&line)
                    && header.log == origin
                {
                    headed = true;
                    log.low_water = log.low_water.max(header.dropped_through);
                    log.owner_low_water = log.owner_low_water.max(header.owner_dropped_through);
                    if owns_origin(mine, &origin) {
                        // The header was written when this machine cleared or
                        // trimmed a log it owns, so its mark is this
                        // machine's own word as the origin. For a copy the
                        // same header records local housekeeping and says
                        // nothing a peer may hear.
                        log.owner_low_water = log.owner_low_water.max(header.dropped_through);
                    }
                }
                continue;
            };
            // A record filed under the wrong log would share a sequence space
            // with records that are not its own, so it is not this log's.
            if !message.acceptable() || message.origin != origin {
                continue;
            }
            if let Err(at) = log
                .messages
                .binary_search_by(|held| held.seq.cmp(&message.seq))
            {
                log.messages.insert(at, message);
            }
        }
        if let Some(first) = log.messages.first() {
            // The run a board serves starts one below its oldest record, which
            // is all a log without a header can say about its own prefix.
            log.low_water = log.low_water.max(first.seq - 1);
            if !headed && first.seq > 1 && owns_origin(mine, &origin) {
                // An owned log that starts above one with nothing on disk
                // saying why is truncated or corrupt, not retention: the
                // missing records may still exist on this machine and may
                // still be recoverable. Keep serving what is here, but say
                // nothing a peer could read as "skip the gap".
                debug::log(
                    "talk",
                    format!(
                        "{origin}: log starts at {} with no header; serving what is held, \
                         publishing no owner floor",
                        first.seq
                    ),
                );
            }
        } else if !headed {
            // Nothing held and nothing dropped: an empty file nobody has
            // written to yet, and no reason to claim an origin over it.
            continue;
        }
        logs.insert(origin, log);
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
            "The sender is blocked waiting on this answer: reply with {} as soon as you know, \
             including if the answer is no, not yet, or wrong agent.",
            call(session)
        ),
        Some(session) => format!(
            "Reply with {}, or ignore this if it is not yours.",
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
                    "Answer them in their chat app, briefly and before you start the work, with \
                     send_channel_message {{ channel: \"{channel}\", {quoting}text: \"…\" }} — \
                     not on the talk board."
                )
            }
            _ => "A person wrote this, not an agent: answer with talk_post, on the board they are \
                  watching."
                .into(),
        },
        None => "The sender is not a muxloom session, so there is nowhere to reply directly; \
                 talk_post if you must answer."
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
        true => "Delivered by muxloom: a person typed this — the human you work for.",
        false => {
            "Delivered by muxloom: another agent typed this, not the person you are working for; \
             weigh it on its merits."
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
    let local_board = pool
        .talk_status(&local, None)
        .context("the local talk board is unreachable")?;
    // The board a batch came from still travels with it, for older peers and
    // for diagnosis. Floors no longer depend on it: a batch names the board
    // that handed it over, and that board may be a hub relaying a floor it
    // learned from the origin, which is exactly how a machine synced through
    // the controller hears about a dropped prefix.
    let local_origin = local_board.origin;
    let mut mine = local_board.vector;
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
        match carry(&pool, target, &local, &local_origin, mine.clone(), &theirs) {
            Ok((pulled, pushed, local_moved)) => {
                summary.pulled += pulled;
                summary.pushed += pushed;
                if local_moved {
                    // The next machine should be told what this one just said,
                    // and not be asked for it a second time. A floor on its
                    // own moves this board's mark, so it counts here even when
                    // every message in the batch was already held.
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
///
/// Both appends name the board that handed the batch over, which is not
/// necessarily the origin of every floor inside it. That is the point: the
/// controller's local board is a hub, and a floor it pulled from a machine has
/// to travel on to the next machine even though the controller does not own
/// the origin. The floor's provenance is settled on the publishing side — see
/// [`TalkStore::since`] — so the receiver adopts it without asking who is
/// carrying it.
///
/// Returns how many messages were pulled, how many were pushed, and whether
/// the local board was handed anything at all — a batch that carries only a
/// dropped prefix still moves the local vector.
fn carry(
    pool: &BridgePool,
    target: &Target,
    local: &Target,
    local_origin: &str,
    mine: TalkVector,
    theirs: &TalkState,
) -> Result<(usize, usize, bool)> {
    let inbound = pool.talk_fetch(target, mine, SYNC_BATCH)?;
    let handed = !inbound.is_empty() || !inbound.low_water.is_empty();
    let pulled = if handed {
        pool.talk_append(local, inbound, &theirs.origin)?
    } else {
        0
    };
    let outbound = pool.talk_fetch(local, theirs.vector.clone(), SYNC_BATCH)?;
    let pushed = if outbound.is_empty() && outbound.low_water.is_empty() {
        0
    } else {
        pool.talk_append(target, outbound, local_origin)?
    };
    Ok((pulled, pushed, handed))
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

    /// A note, which is what the board holds. Signed as a person so these
    /// tests' short sentences are not held to the discipline an agent's notes
    /// are; the tests for that discipline build their own drafts.
    fn draft(text: &str, scope: TalkScope) -> TalkDraft {
        TalkDraft {
            scope,
            author: TalkAuthor {
                voice: TalkVoice {
                    session_id: Some("session-1".into()),
                    label: Some("builder".into()),
                    kind: Some("claude".into()),
                    human: true,
                    channel: None,
                    channel_quote: None,
                },
                ..TalkAuthor::default()
            },
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: text.into(),
        }
    }

    /// A record from another machine, for tests that have to reconstruct a log
    /// file the way a previous build left it.
    fn far_record(origin: &str, seq: u64) -> TalkMessage {
        TalkMessage {
            id: format!("{origin}:{seq}"),
            origin: origin.into(),
            seq,
            ts: now_ms(),
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: origin.into(),
                machine_label: "elsewhere".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: format!("held after the gap {seq}"),
        }
    }

    /// Write a log exactly as given: no header, no rewriting — the shape a
    /// build that did not know about owner floors would have left on disk.
    fn write_log_file(root: &Path, origin: &str, messages: &[TalkMessage]) {
        fs::create_dir_all(root.join("log")).unwrap();
        let mut text = String::new();
        for message in messages {
            text.push_str(&serde_json::to_string(message).unwrap());
            text.push('\n');
        }
        fs::write(root.join("log").join(format!("{origin}.jsonl")), text).unwrap();
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
            kind: TalkKind::Note,
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

    #[test]
    fn a_log_compacted_to_nothing_still_says_what_it_dropped() {
        // Retention that only holds until the next restart is not retention.
        // A log emptied by the age cutoff used to leave an empty file, which
        // read back as a machine this board had never heard of - so the next
        // round asked every peer for the whole thirty days again and filed
        // all of it a second time.
        let store = store("watermark");
        let root = store.root.clone();
        let old = |seq: u64| TalkMessage {
            id: format!("other-1234abcd:{seq}"),
            origin: "other-1234abcd".into(),
            seq,
            ts: 1,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: "other-1234abcd".into(),
                machine_label: "gpu-box".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: format!("message {seq}"),
        };
        assert_eq!(store.merge(vec![old(1), old(2), old(3)]).unwrap(), 3);
        drop(store);

        // Opening compacts, and everything here is older than the window.
        let reopened = TalkStore::open(root.clone()).unwrap();
        assert!(reopened.read(&TalkFilter::default()).messages.is_empty());
        assert_eq!(reopened.state().vector["other-1234abcd"], 3);
        assert_eq!(reopened.state().low_water["other-1234abcd"], 3);
        drop(reopened);

        // And it is still said after the restart that used to forget it.
        let restarted = TalkStore::open(root.clone()).unwrap();
        assert_eq!(restarted.state().vector["other-1234abcd"], 3);
        assert_eq!(
            restarted.merge(vec![old(1), old(2), old(3)]).unwrap(),
            0,
            "a peer re-offering what was dropped must not refill the board"
        );
        // What is genuinely newer is still taken.
        let mut fresh = old(4);
        fresh.ts = now_ms();
        assert_eq!(restarted.merge(vec![fresh]).unwrap(), 1);
        assert_eq!(restarted.state().vector["other-1234abcd"], 4);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn muxlooms_own_traffic_rides_a_log_beside_the_one_holding_memory() {
        let store = store("wire");
        let mine = store.origin();
        let wire = format!("{mine}_wire");

        // What somebody wrote down.
        let note = store
            .post(draft(
                "the lease flood evicts real notes",
                TalkScope::Path {
                    machine: String::new(),
                    path: "/work/repo".into(),
                },
            ))
            .unwrap();
        assert_eq!((note.origin.as_str(), note.seq), (mine.as_str(), 1));

        // A coordination note muxloom writes to itself. It is filed under its
        // own log, and says the same machine said it: everything addressed on
        // the record stays this machine's own key, and only the sequence space
        // moves.
        let lease = store
            .post(draft(
                "muxloom-channel-lease v1\naccount=bot@im\nuntil=9\ncursor=a",
                TalkScope::Path {
                    machine: String::new(),
                    path: format!("{WIRE_PATH_PREFIX}channel-leases"),
                },
            ))
            .unwrap();
        assert_eq!((lease.origin.as_str(), lease.seq), (wire.as_str(), 1));
        assert_eq!(lease.author.machine, mine);
        assert_eq!(lease.scope.machine(), Some(mine.as_str()));

        // A delivered direct is a record of a delivery, not a memory either.
        let mut sent = draft("look at line 40", TalkScope::Global);
        sent.kind = TalkKind::Direct;
        sent.to = Some(TalkAddress {
            machine: String::new(),
            session_id: "session-2".into(),
        });
        let direct = store.post(sent).unwrap();
        assert_eq!((direct.origin.as_str(), direct.seq), (wire.as_str(), 2));
        assert_eq!(direct.to.expect("addressed").machine, mine);

        // So the next thing written down is the second thing written down,
        // however much traffic went by in between. That is the whole point:
        // the durable log's sequence space is not spent on machinery, and its
        // retention window is not either.
        let second = store
            .post(draft(
                "and a second thing worth keeping",
                TalkScope::Path {
                    machine: String::new(),
                    path: "/work/repo".into(),
                },
            ))
            .unwrap();
        assert_eq!((second.origin.as_str(), second.seq), (mine.as_str(), 2));

        // Reading the board shows the board. The coordination note is only
        // handed to the code that asks for that exact path.
        let texts = |filter: TalkFilter| -> Vec<String> {
            store
                .read(&filter)
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        let seen = texts(TalkFilter {
            session_id: Some("session-1".into()),
            paths: TalkSelector::All,
            machines: TalkSelector::All,
            ..TalkFilter::default()
        });
        assert!(
            !seen.iter().any(|text| text.contains("channel-lease")),
            "the board is not muxloom's own traffic: {seen:?}"
        );
        // The direct it sent is in the mailbox, not on the board.
        assert_eq!(seen.len(), 2, "two notes: {seen:?}");
        // Even to the person the machines belong to, who sees everything else.
        let owned = texts(TalkFilter {
            owner: true,
            paths: TalkSelector::All,
            machines: TalkSelector::All,
            ..TalkFilter::default()
        });
        assert!(!owned.iter().any(|text| text.contains("channel-lease")));
        // And the lease reader still finds it, by naming it.
        let asked = texts(TalkFilter {
            scope: Some("path".into()),
            machines: TalkSelector::All,
            paths: TalkSelector::Only {
                names: vec![format!("{WIRE_PATH_PREFIX}channel-leases")],
            },
            ..TalkFilter::default()
        });
        assert_eq!(asked.len(), 1);

        // Each log is kept to its own depth and its own age.
        assert_eq!(
            retention_for(&wire),
            (RETAIN_PER_WIRE_ORIGIN, RETAIN_WIRE_MS)
        );
        assert_eq!(retention_for(&mine), (RETAIN_PER_ORIGIN, RETAIN_MS));

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn a_cleared_board_stays_cleared_when_the_fleet_offers_it_back() {
        let store = store("clear");
        let root = store.root.clone();
        store
            .post(draft("worth keeping", TalkScope::Global))
            .unwrap();
        store
            .post(draft("also worth keeping", TalkScope::Global))
            .unwrap();
        let far = |seq: u64| TalkMessage {
            id: format!("other-1234abcd:{seq}"),
            origin: "other-1234abcd".into(),
            seq,
            ts: now_ms(),
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: "other-1234abcd".into(),
                machine_label: "gpu-box".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: format!("from elsewhere {seq}"),
        };
        store.merge(vec![far(1), far(2)]).unwrap();

        assert_eq!(store.clear().unwrap(), 4);
        assert!(store.read(&TalkFilter::default()).messages.is_empty());
        // Cleared, not forgotten: the marks say the board reached here, so a
        // peer still holding all of it is offering nothing new.
        assert_eq!(store.state().vector["other-1234abcd"], 2);
        assert_eq!(store.merge(vec![far(1), far(2)]).unwrap(), 0);
        // And nothing was said twice: the next post carries on from where the
        // cleared board left off, so no id is ever minted a second time.
        assert_eq!(
            store.post(draft("after", TalkScope::Global)).unwrap().seq,
            3
        );
        drop(store);

        let reopened = TalkStore::open(root.clone()).unwrap();
        assert_eq!(
            reopened
                .read(&TalkFilter::default())
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect::<Vec<_>>(),
            ["after"]
        );
        assert_eq!(reopened.merge(vec![far(1), far(2)]).unwrap(), 0);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_same_voice_does_not_write_the_same_thing_down_twice() {
        let store = store("repeat");
        let scope = TalkScope::Path {
            machine: String::new(),
            path: "/work/repo".into(),
        };
        let note = |text: &str, scope: TalkScope| {
            let mut draft = draft(text, scope);
            draft.kind = TalkKind::Note;
            draft
        };
        store
            .post(note("the lexer keeps its own arena", scope.clone()))
            .unwrap();
        let again = store.post(note("the lexer keeps its own arena", scope.clone()));
        assert!(
            again
                .as_ref()
                .is_err_and(|error| error.to_string().contains("already wrote this")),
            "{again:?}"
        );
        // What actually differs is not a repeat: another scope, other words,
        // and another voice reaching the same conclusion are all worth having.
        store
            .post(note("the lexer keeps its own arena", TalkScope::Global))
            .unwrap();
        store
            .post(note("the parser keeps no arena of its own", scope.clone()))
            .unwrap();
        let mut other = note("the lexer keeps its own arena", scope);
        other.author.voice.session_id = Some("session-2".into());
        store.post(other).unwrap();
        assert_eq!(store.read(&TalkFilter::default()).messages.len(), 4);

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn an_older_machines_message_still_replicates_and_reads() {
        // Nothing mints a message any more, but a machine on an older build
        // does, and its records have to arrive, read back and replicate like
        // anything else. Dropping the kind would leave a hole in that
        // machine's sequence space that every round would try to fill forever.
        let store = store("legacy-kind");
        let far = TalkMessage {
            id: "other-1234abcd:1".into(),
            origin: "other-1234abcd".into(),
            seq: 1,
            ts: now_ms(),
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: "other-1234abcd".into(),
                machine_label: "gpu-box".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: "said on a build that still had the other kind".into(),
        };
        assert_eq!(store.merge(vec![far.clone()]).unwrap(), 1);
        assert_eq!(store.state().vector["other-1234abcd"], 1);
        // Kept, but the board holds notes: an ordinary read does not show it.
        assert!(store.read(&TalkFilter::default()).messages.is_empty());
        // And a reader can still ask for that kind by name.
        assert_eq!(
            store
                .read(&TalkFilter {
                    kinds: vec!["message".into()],
                    ..TalkFilter::default()
                })
                .messages
                .len(),
            1
        );
        assert_eq!(TalkKind::parse("message").unwrap(), TalkKind::Message);

        fs::remove_dir_all(&store.root).ok();
    }

    #[test]
    fn what_an_agent_writes_down_has_to_be_something_written_down() {
        let store = store("discipline");
        let note = |text: &str| {
            let mut draft = draft(text, TalkScope::Global);
            draft.kind = TalkKind::Note;
            draft.author.voice.human = false;
            draft
        };
        let refused = |text: &str| {
            store
                .post(note(text))
                .expect_err("not something the board keeps")
                .to_string()
        };
        // Addressed at somebody: that is a message, and the board reaches
        // nobody. This is the shape that was actually on the board — an agent
        // handing a file to another agent by posting it to everyone.
        assert!(
            refused("@someone (H20): the new mp4 is at /tmp/3.mp4, please switch to it")
                .contains("message_agent")
        );
        // A question nobody owes an answer to.
        assert!(
            refused("Has anybody seen the lexer arena leak before?").contains("says what is true")
        );
        // And a remark that will mean nothing to anyone tomorrow.
        assert!(refused("restarted the daemon").contains("is a remark"));
        assert!(refused("done").contains("is a remark"));

        // What a note is: a fact that stands on its own.
        store
            .post(note(
                "The flaky auth test is clock skew on the CI runner, not the retry logic: it \
                 fails only when the runner is more than two seconds behind NTP.",
            ))
            .unwrap();
        // Short is fine when it is still a fact rather than a fragment.
        store.post(note("APFS mtime is nanosecond.")).unwrap();
        // A person is not lectured about how to write on their own board.
        let mut theirs = note("done");
        theirs.author.voice.human = true;
        theirs.author.voice.session_id = None;
        store.post(theirs).unwrap();
        assert_eq!(store.read(&TalkFilter::default()).messages.len(), 3);

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
            kind: TalkKind::Note,
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
            kind: TalkKind::Note,
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
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: format!("said {seq}"),
        };
        // The answer comes first and the noise that buries it comes after,
        // which is the order that made this lose the answer.
        assert_eq!(
            store.merge((1..=9).map(far).collect::<Vec<_>>()).unwrap(),
            9
        );

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

    /// A machine's clock can step backwards between two posts, and the board
    /// must not reorder what that machine said because of it. A following read
    /// used to choose its page by time and then advance the cursor to the
    /// highest sequence in it, so the later sequence was handed out first and
    /// the earlier one was left below the cursor - never offered again, with
    /// nothing said about it.
    #[test]
    fn a_clock_that_steps_backwards_does_not_hide_a_message_from_a_follower() {
        let store = store("cursor-clock");
        let origin = "cccc-9012abcd";
        let far = |seq: u64, ts: u64| TalkMessage {
            id: format!("{origin}:{seq}"),
            origin: origin.into(),
            seq,
            ts,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: origin.into(),
                machine_label: "elsewhere".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: format!("said {seq}"),
        };
        // Written in sequence order, but the clock went back between the
        // second and the third: the second is the newest by time and the
        // oldest by sequence.
        assert_eq!(
            store
                .merge(vec![far(1, 500), far(2, 1000), far(3, 900)])
                .unwrap(),
            3
        );

        // The reader has had the first post and asks for what follows, one
        // message at a time.
        let mut cursor = encode_cursor(&BTreeMap::from([(origin.to_string(), 1)]));
        let mut seen = Vec::new();
        let mut pages = Vec::new();
        for _ in 0..4 {
            let page = store.read(&TalkFilter {
                since: decode_cursor(&cursor),
                limit: 1,
                ..TalkFilter::default()
            });
            if page.messages.is_empty() {
                break;
            }
            let seqs: Vec<u64> = page.messages.iter().map(|message| message.seq).collect();
            pages.push((
                seqs.clone(),
                decode_cursor(&page.cursor).get(origin).copied(),
            ));
            seen.extend(seqs);
            cursor = page.cursor;
        }

        assert_eq!(
            pages,
            vec![(vec![2], Some(2)), (vec![3], Some(3))],
            "a follower is handed the oldest unread sequence first, and the cursor stops \
             where the page stops"
        );
        assert_eq!(
            seen,
            vec![2, 3],
            "a poll that follows its cursor is handed every unread message once"
        );
        assert!(
            store
                .read(&TalkFilter {
                    since: decode_cursor(&cursor),
                    limit: 1,
                    ..TalkFilter::default()
                })
                .messages
                .is_empty(),
            "the cursor has reached the end of what the board held"
        );

        fs::remove_dir_all(&store.root).ok();
    }

    /// A catch-up batch is the run a peer's vector can advance over. A peer
    /// behind a machine whose clock stepped backwards used to be handed the
    /// later sequence first; the earlier one stayed missing, so the vector
    /// stopped below both and every round re-offered the message the peer
    /// already held while the one it was missing was never in the batch.
    #[test]
    fn a_catch_up_batch_starts_at_the_lowest_sequence_a_peer_is_missing() {
        let store = store("catch-up-clock");
        let origin = "cccc-9012abcd";
        let far = |seq: u64, ts: u64| TalkMessage {
            id: format!("{origin}:{seq}"),
            origin: origin.into(),
            seq,
            ts,
            scope: TalkScope::Global,
            author: TalkAuthor {
                machine: origin.into(),
                machine_label: "elsewhere".into(),
                voice: TalkVoice::default(),
            },
            kind: TalkKind::Note,
            to: None,
            reply_to: None,
            text: format!("said {seq}"),
        };
        assert_eq!(
            store
                .merge(vec![far(1, 500), far(2, 1000), far(3, 900)])
                .unwrap(),
            3
        );

        // The peer holds only the first post. The batch has to begin with
        // sequence 2 even though sequence 3 carries the older timestamp.
        assert_eq!(
            store
                .since(&TalkVector::from([(origin.to_string(), 1)]), 2)
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );

        // One at a time, as a batch smaller than the backlog would: the peer
        // files what it is handed and asks again from the run it holds. It
        // reaches the end of what it is missing rather than being handed
        // sequence 3 twice while 2 stays behind.
        let mut held = TalkVector::from([(origin.to_string(), 1)]);
        let mut carried = Vec::new();
        for _ in 0..3 {
            let batch = store.since(&held, 1);
            if batch.is_empty() {
                break;
            }
            carried.extend(batch.iter().map(|message| message.seq));
            // A peer's vector is the run it holds unbroken, not the highest
            // sequence it has seen: a hole stops it, which is what asks for
            // the missing message again.
            let mut mark = held[origin];
            while carried.contains(&(mark + 1)) {
                mark += 1;
            }
            held.insert(origin.to_string(), mark);
        }
        assert_eq!(
            carried,
            vec![2, 3],
            "every message the peer was missing is carried once"
        );
        assert_eq!(held[origin], 3, "the peer's vector reaches what it holds");
        assert!(store.since(&held, 1).is_empty());

        fs::remove_dir_all(&store.root).ok();
    }

    /// A peer behind a prefix its sender dropped for good has to step over
    /// that prefix, not stall below it. A catch-up page is the oldest
    /// [`MAX_PAGE`] messages a peer is missing, so once a clear leaves a
    /// permanent gap under more held messages than one page, every round
    /// offered the same 500 duplicates: the peer's vector could never advance
    /// past the gap and the newer messages never reached the oldest page. The
    /// sender's floor has to travel with the batch so the receiver can
    /// re-anchor its run above what it will never be sent.
    #[test]
    fn a_peer_steps_over_a_dropped_prefix_instead_of_stalling_below_it() {
        let source = store("floor-source");
        let peer = store("floor-peer");
        let origin = source.origin();

        source
            .post(draft("the board this replaces", TalkScope::Global))
            .unwrap();
        // Clearing keeps the mark: every message up to here is gone for good,
        // and a peer that missed it cannot be sent it again.
        source.clear().unwrap();
        assert_eq!(source.state().low_water[&origin], 1);

        // One full page's worth above the permanent gap, and one more. The
        // page is the oldest MAX_PAGE, so the last post can only arrive after
        // the peer's vector has stepped over the gap.
        for seq in 2..=MAX_PAGE as u64 + 1 {
            source
                .post(draft(
                    &format!("held after the gap {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        assert_eq!(source.state().low_water[&origin], 1);

        let pulled = peer
            .merge_from(&origin, source.since(&peer.state().vector, MAX_PAGE))
            .unwrap();
        assert_eq!(pulled, MAX_PAGE);
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            MAX_PAGE as u64 + 1,
            "the peer stalled below a prefix it will never be sent"
        );

        // And the next post is in the next page rather than behind the same
        // 500 duplicates.
        source
            .post(draft(
                "the first thing said after the stall",
                TalkScope::Global,
            ))
            .unwrap();
        let page = source.since(&peer.state().vector, MAX_PAGE);
        assert_eq!(
            page.len(),
            1,
            "the newest post is behind the same 500 duplicates"
        );
        assert_eq!(
            page.iter().map(|message| message.seq).collect::<Vec<_>>(),
            vec![MAX_PAGE as u64 + 2]
        );

        fs::remove_dir_all(&source.root).ok();
        fs::remove_dir_all(&peer.root).ok();
    }

    /// A floor is a statement about one origin's whole sequence space, so only
    /// the board that owns that origin may make it. A replica that clears its
    /// own copy of somebody else's log has dropped a copy, not the origin; if
    /// it handed that mark on, a fresh board syncing through it would skip
    /// records the origin still holds, and the loss would be permanent and
    /// depend only on the order the machines happened to be visited in.
    #[test]
    fn a_replica_does_not_publish_a_floor_for_an_origin_it_copied() {
        let owner = store("floor-owner");
        let replica = store("floor-replica");
        let fresh = store("floor-fresh");
        let origin = owner.origin();

        for seq in 1..=100u64 {
            owner
                .post(draft(
                    &format!("said before the clear {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        assert_eq!(owner.state().vector[&origin], 100);

        // The replica catches up completely, then clears its own copy. The
        // mark it keeps is about the copy on this machine, not about what the
        // origin still holds.
        let all = owner.since(&replica.state().vector, MAX_PAGE);
        assert_eq!(replica.merge_from(&origin, all).unwrap(), 100);
        assert_eq!(replica.state().vector[&origin], 100);
        replica.clear().unwrap();
        assert_eq!(replica.state().low_water[&origin], 100);

        // A fresh board pulls the replica first, which is a legal sync order.
        // The replica must not pass on a floor for an origin it merely copied:
        // its batch carries no such claim, and the fresh board adopts none.
        let from_replica = replica.since(&fresh.state().vector, MAX_PAGE);
        assert!(
            from_replica.low_water.is_empty(),
            "a replica published a floor for an origin it copied: {:?}",
            from_replica.low_water
        );
        fresh.merge_from(&replica.origin(), from_replica).unwrap();
        assert_eq!(
            fresh.state().low_water.get(&origin).copied().unwrap_or(0),
            0,
            "the fresh board adopted a floor from a board that does not own the origin"
        );
        assert_eq!(
            fresh.state().vector.get(&origin).copied().unwrap_or(0),
            0,
            "an empty board claimed a prefix it was never given"
        );

        // Receipt cannot audit whether the board handing a batch over owns the
        // origins its floors name, and must not: a hub carries a machine's
        // batch to the machines behind it, and a floor the hub learned from
        // the owner is exactly how a peer synced through the controller hears
        // that a prefix is gone. What keeps the claim honest is the
        // publishing side, above, and the relay is covered by
        // `a_hub_relays_the_floor_it_learned_from_the_origin`.

        // Pulling the origin itself is what restores the records, because the
        // origin still holds all 100 and has not dropped anything.
        let from_owner = owner.since(&fresh.state().vector, MAX_PAGE);
        assert!(from_owner.low_water.is_empty());
        assert_eq!(fresh.merge_from(&origin, from_owner).unwrap(), 100);
        assert_eq!(
            fresh.state().vector.get(&origin).copied().unwrap_or(0),
            100,
            "the fresh board never recovered the prefix the replica still held"
        );
        assert_eq!(
            fresh.since(&TalkVector::new(), MAX_PAGE).len(),
            100,
            "the fresh board holds a vector without the records behind it"
        );

        // The original fix still holds, and a floor alone is enough: after a
        // clear the owner's next batch carries no message at all, only the
        // mark that the prefix is gone for good.
        owner.clear().unwrap();
        let behind = store("floor-behind");
        let floor_only = owner.since(&behind.state().vector, MAX_PAGE);
        assert!(floor_only.is_empty());
        assert_eq!(
            floor_only.low_water.get(&origin).copied(),
            Some(100),
            "the owner's own batch lost the floor for its own dropped prefix"
        );
        assert_eq!(behind.merge_from(&origin, floor_only).unwrap(), 0);
        assert_eq!(
            behind.state().vector.get(&origin).copied().unwrap_or(0),
            100,
            "a floor alone did not move the peer above the dropped prefix"
        );

        // And a floor never comes back down, even from the board that set it.
        behind
            .merge_from(
                &origin,
                TalkBatch {
                    messages: Vec::new(),
                    low_water: TalkVector::from([(origin.clone(), 50)]),
                },
            )
            .unwrap();
        assert_eq!(
            behind.state().low_water.get(&origin).copied(),
            Some(100),
            "a later floor walked the mark back down"
        );

        // Records beyond the gap then advance the vector, with no page spent
        // on the prefix that will never be offered again.
        for seq in 101..=150u64 {
            owner
                .post(draft(
                    &format!("said after the clear {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        let after_gap = owner.since(&behind.state().vector, MAX_PAGE);
        assert_eq!(after_gap.len(), 50);
        assert_eq!(behind.merge_from(&origin, after_gap).unwrap(), 50);
        assert_eq!(
            behind.state().vector.get(&origin).copied().unwrap_or(0),
            150,
            "a peer behind the owner's floor stalled below it"
        );

        fs::remove_dir_all(&owner.root).ok();
        fs::remove_dir_all(&replica.root).ok();
        fs::remove_dir_all(&fresh.root).ok();
        fs::remove_dir_all(&behind.root).ok();
    }

    /// `run_sync` is a hub: it pulls a machine's batch into the controller's
    /// local board and then pushes that board to the next machine. A floor the
    /// hub learned from the origin has to survive the second hop, even though
    /// the hub does not own the origin. If a board may state only its own
    /// origin's floor, a peer synced through the controller never hears that
    /// the prefix is gone: it holds every message above the gap, its vector
    /// cannot start above the gap, and once a full page sits above it every
    /// round offers the same page again while the newest posts never arrive.
    #[test]
    fn a_hub_relays_the_floor_it_learned_from_the_origin() {
        let owner = store("hub-owner");
        let hub = store("hub-local");
        let peer = store("hub-peer");
        let origin = owner.origin();

        owner
            .post(draft("the board this replaces", TalkScope::Global))
            .unwrap();
        owner.clear().unwrap();
        // Exactly one page above the permanent gap, and catching up the rest
        // is what has to step over it.
        for seq in 2..=MAX_PAGE as u64 + 1 {
            owner
                .post(draft(
                    &format!("held after the gap {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        assert_eq!(owner.state().low_water[&origin], 1);

        // First hop: the controller's local board pulls from the owner. The
        // controller does not own the origin, but it now knows the owner's
        // floor.
        let pulled = hub
            .merge_from(&origin, owner.since(&hub.state().vector, MAX_PAGE))
            .unwrap();
        assert_eq!(pulled, MAX_PAGE);
        assert_eq!(hub.state().vector[&origin], MAX_PAGE as u64 + 1);

        // Second hop: the controller pushes its board to a machine it does not
        // own, naming itself as the sender, which is what `carry` does.
        peer.merge_from(&hub.origin(), hub.since(&peer.state().vector, MAX_PAGE))
            .unwrap();
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            MAX_PAGE as u64 + 1,
            "a peer synced through the controller stalled below the origin's floor"
        );

        // And the next post crosses both hops rather than being hidden behind
        // the same full page of duplicates.
        owner
            .post(draft(
                "the first thing said after the gap",
                TalkScope::Global,
            ))
            .unwrap();
        assert_eq!(
            hub.merge_from(&origin, owner.since(&hub.state().vector, MAX_PAGE))
                .unwrap(),
            1
        );
        assert_eq!(
            peer.merge_from(&hub.origin(), hub.since(&peer.state().vector, MAX_PAGE))
                .unwrap(),
            1
        );
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            MAX_PAGE as u64 + 2,
            "the newest post stayed behind the duplicates already carried"
        );

        fs::remove_dir_all(&owner.root).ok();
        fs::remove_dir_all(&hub.root).ok();
        fs::remove_dir_all(&peer.root).ok();
    }

    /// A floor adopted from an origin is knowledge about that origin, not a
    /// property of this machine's copy, so reopening the board must not forget
    /// it. Losing it on restart would re-arm the hub stall for every peer that
    /// syncs through this machine after the daemon comes back up.
    #[test]
    fn an_adopted_owner_floor_survives_reopening_the_board() {
        let owner = store("floor-persist-owner");
        let hub = store("floor-persist-hub");
        let origin = owner.origin();

        owner
            .post(draft("the board this replaces", TalkScope::Global))
            .unwrap();
        owner.clear().unwrap();
        for seq in 2..=10u64 {
            owner
                .post(draft(
                    &format!("held after the gap {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        let adopted = hub
            .merge_from(&origin, owner.since(&hub.state().vector, MAX_PAGE))
            .unwrap();
        assert_eq!(adopted, 9);
        assert_eq!(
            hub.since(&TalkVector::new(), MAX_PAGE)
                .low_water
                .get(&origin)
                .copied(),
            Some(1),
            "the hub learned the owner's floor"
        );

        let root = hub.root.clone();
        drop(hub);
        let reopened = TalkStore::open(root.clone()).unwrap();
        let announced = reopened.since(&TalkVector::new(), MAX_PAGE);
        assert_eq!(
            announced.low_water.get(&origin).copied(),
            Some(1),
            "the adopted owner floor was forgotten when the board was reopened"
        );

        // And it is still a usable floor: a peer adopts it and steps over the
        // prefix the origin no longer holds.
        let peer = store("floor-persist-peer");
        assert_eq!(peer.merge_from(&reopened.origin(), announced).unwrap(), 9);
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            10,
            "a peer could not step over the reopened board's floor"
        );

        fs::remove_dir_all(&owner.root).ok();
        fs::remove_dir_all(&peer.root).ok();
        fs::remove_dir_all(&root).ok();
    }

    /// Housekeeping on a copy is not a statement about the origin. A hub that
    /// clears its copy of somebody else's log has dropped records it held, not
    /// records the origin holds, so the batch it hands on says nothing about
    /// that origin's prefix.
    #[test]
    fn a_hub_does_not_publish_a_floor_for_clearing_a_copy() {
        let owner = store("copy-owner");
        let hub = store("copy-hub");
        let origin = owner.origin();

        for seq in 1..=10u64 {
            owner
                .post(draft(
                    &format!("said before the copy is cleared {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        let all = owner.since(&hub.state().vector, MAX_PAGE);
        assert_eq!(hub.merge_from(&origin, all).unwrap(), 10);
        assert_eq!(hub.state().vector[&origin], 10);

        hub.clear().unwrap();
        assert_eq!(
            hub.state().low_water[&origin],
            10,
            "clearing the copy did not raise this machine's own mark"
        );
        let batch = hub.since(&TalkVector::new(), MAX_PAGE);
        assert!(batch.is_empty());
        assert_eq!(
            batch.low_water.get(&origin).copied().unwrap_or(0),
            0,
            "a local clear of a copy was published as the origin's floor"
        );

        // The origin still holds every record, so a peer that never adopted
        // the copy's mark can still get all ten from it.
        let peer = store("copy-peer");
        let from_owner = owner.since(&peer.state().vector, MAX_PAGE);
        assert!(from_owner.low_water.is_empty());
        assert_eq!(peer.merge_from(&origin, from_owner).unwrap(), 10);
        assert_eq!(peer.state().vector[&origin], 10);

        fs::remove_dir_all(&owner.root).ok();
        fs::remove_dir_all(&hub.root).ok();
        fs::remove_dir_all(&peer.root).ok();
    }

    /// A controller-local hub that was running the base build already holds
    /// the records above a cleared prefix, and its vector covers the gap
    /// because the base build inferred the mark from the oldest record it
    /// held. The floor it never learned is therefore older than its vector.
    /// Filtering published floors by the peer's vector made that harmless-
    /// looking state permanent: an ordinary upgrade left the hub unable to
    /// relay, a fresh peer pulled through it filed 500 records below an
    /// unknown gap, and its vector never advanced again.
    #[test]
    fn an_upgraded_hub_learns_and_relays_a_floor_its_vector_already_covers() {
        let owner = store("upgrade-owner");
        let origin = owner.origin();
        owner
            .post(draft("the board this replaces", TalkScope::Global))
            .unwrap();
        owner.clear().unwrap();
        for seq in 2..=MAX_PAGE as u64 + 1 {
            owner
                .post(draft(
                    &format!("held after the gap {seq}"),
                    TalkScope::Global,
                ))
                .unwrap();
        }
        assert_eq!(owner.state().vector[&origin], MAX_PAGE as u64 + 1);

        // The controller's copy as the base build left it: every post-clear
        // record appended, no header, and the base load inferred a mark one
        // below the oldest record. Nothing on disk names the owner's floor.
        let hub = store("upgrade-hub");
        let hub_root = hub.root.clone();
        drop(hub);
        let left = owner.since(&TalkVector::new(), MAX_PAGE);
        assert_eq!(left.len(), MAX_PAGE);
        write_log_file(&hub_root, &origin, &left.messages);
        let hub = TalkStore::open(hub_root.clone()).unwrap();
        assert_eq!(
            hub.state().vector.get(&origin).copied().unwrap_or(0),
            MAX_PAGE as u64 + 1,
            "the base build's copy did not read back the way this test needs"
        );

        // The pull that used to say nothing: no message is missing, and the
        // owner's floor sits below the peer's vector, but a floor is metadata
        // and has to travel anyway.
        let pulled = owner.since(&hub.state().vector, MAX_PAGE);
        assert!(pulled.is_empty(), "the hub was missing a message");
        hub.merge_from(&origin, pulled).unwrap();
        assert_eq!(
            hub.since(&TalkVector::new(), MAX_PAGE)
                .low_water
                .get(&origin)
                .copied(),
            Some(1),
            "the upgraded hub never learned the floor it had missed"
        );

        // A fresh peer synced through the hub now steps over the gap instead
        // of filing everything above an unknown one.
        let peer = store("upgrade-peer");
        peer.merge_from(&hub.origin(), hub.since(&peer.state().vector, MAX_PAGE))
            .unwrap();
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            MAX_PAGE as u64 + 1,
            "a peer synced through the upgraded hub stalled at the gap"
        );

        // And a post made after the upgrade crosses both hops.
        owner
            .post(draft(
                "the first thing said after the upgrade",
                TalkScope::Global,
            ))
            .unwrap();
        assert_eq!(
            hub.merge_from(&origin, owner.since(&hub.state().vector, MAX_PAGE))
                .unwrap(),
            1
        );
        assert_eq!(
            peer.merge_from(&hub.origin(), hub.since(&peer.state().vector, MAX_PAGE))
                .unwrap(),
            1
        );
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            MAX_PAGE as u64 + 2,
            "the first post after the upgrade stayed behind the gap"
        );

        fs::remove_dir_all(&owner.root).ok();
        fs::remove_dir_all(&hub_root).ok();
        fs::remove_dir_all(&peer.root).ok();
    }

    /// A headerless owned log that begins above one is a truncated or corrupt
    /// log, not retention: nothing on disk says the prefix was dropped. The
    /// local serving mark may still start above the oldest record held, but
    /// publishing it would tell a peer to skip a record this machine might
    /// still recover from the origin.
    #[test]
    fn a_headerless_owned_log_does_not_publish_an_owner_floor() {
        let owner = store("corrupt-owner");
        let origin = owner.origin();
        let root = owner.root.clone();
        let first = 5;
        let held: Vec<TalkMessage> = (first..=9).map(|seq| far_record(&origin, seq)).collect();
        drop(owner);
        write_log_file(&root, &origin, &held);
        let reopened = TalkStore::open(root.clone()).unwrap();

        let batch = reopened.since(&TalkVector::new(), MAX_PAGE);
        assert_eq!(
            batch.low_water.get(&origin).copied().unwrap_or(0),
            0,
            "a headerless log published an owner floor it never wrote down"
        );
        assert_eq!(batch.len(), 5, "the local records stopped being served");

        let peer = store("corrupt-peer");
        peer.merge_from(&origin, batch).unwrap();
        assert_eq!(
            peer.state().low_water.get(&origin).copied().unwrap_or(0),
            0,
            "a peer adopted a floor from a headerless log and will skip the gap"
        );
        assert_eq!(
            peer.state().vector.get(&origin).copied().unwrap_or(0),
            0,
            "a peer advanced past a record the origin might still recover"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&peer.root).ok();
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
                kind: TalkKind::Note,
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
                kind: TalkKind::Note,
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

    /// A direct message is the mailbox beside the board, not something on it:
    /// a reader of the board does not see it, whoever they are, and the mailbox
    /// is read by asking for it.
    #[test]
    fn a_direct_message_is_read_from_the_mailbox_and_never_from_the_board() {
        let store = store("direct");
        let mine = store.origin();
        let mut sent = draft("look at line 40", TalkScope::Global);
        sent.kind = TalkKind::Direct;
        sent.author.voice.human = false;
        sent.to = Some(TalkAddress {
            machine: mine.clone(),
            session_id: "session-2".into(),
        });
        store.post(sent).unwrap();

        let read = |session: &str, scope: Option<&str>, owner: bool| -> Vec<String> {
            store
                .read(&TalkFilter {
                    session_id: Some(session.into()),
                    scope: scope.map(Into::into),
                    owner,
                    ..TalkFilter::default()
                })
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        // Reading the board: not there, for either end or for the dashboard.
        for session in ["session-1", "session-2", "session-3"] {
            assert!(read(session, None, false).is_empty(), "{session}");
        }
        assert!(read("session-3", None, true).is_empty());
        // Reading the mailbox: there.
        assert_eq!(
            read("session-2", Some("direct"), false),
            ["look at line 40"]
        );
        assert_eq!(
            read("session-1", Some("direct"), false),
            ["look at line 40"]
        );

        fs::remove_dir_all(&store.root).ok();
    }

    /// The mailbox is not a feed. A session that is neither end of a direct
    /// message is not handed it for asking for the mailbox by name, because
    /// the tool promises "my mailbox" and a wait that ends on a stranger's
    /// answer is a wait that ends on the wrong thing.
    #[test]
    fn a_direct_message_is_not_offered_to_a_stranger_reading_the_mailbox() {
        let store = store("direct-stranger");
        let mine = store.origin();
        let mut sent = draft("look at line 40", TalkScope::Global);
        sent.kind = TalkKind::Direct;
        sent.author.voice.human = false;
        sent.to = Some(TalkAddress {
            machine: mine.clone(),
            session_id: "session-2".into(),
        });
        store.post(sent).unwrap();

        let read = |session: &str, scope: Option<&str>, owner: bool| -> Vec<String> {
            store
                .read(&TalkFilter {
                    session_id: Some(session.into()),
                    scope: scope.map(Into::into),
                    owner,
                    ..TalkFilter::default()
                })
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect()
        };
        // A third session, in nobody's conversation, asked for the mailbox and
        // was handed somebody else's mail.
        assert!(
            read("session-3", Some("direct"), false).is_empty(),
            "a session that is neither end read somebody else's mailbox"
        );
        // The two ends still have it, and so does the person whose machines
        // these are — the dashboard's privilege is the owner flag, not asking
        // for the mailbox.
        assert_eq!(
            read("session-2", Some("direct"), false),
            ["look at line 40"]
        );
        assert_eq!(
            read("session-1", Some("direct"), false),
            ["look at line 40"]
        );
        assert_eq!(read("session-3", Some("direct"), true), ["look at line 40"]);
        // And the mailbox being readable does not put it on the board.
        for owner in [false, true] {
            assert!(read("session-3", None, owner).is_empty(), "owner={owner}");
        }

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
                .merge_from(
                    &from.origin(),
                    from.since(&pull_into.state().vector, MAX_PAGE),
                )
                .unwrap();
            let pushed = from
                .merge_from(
                    &pull_into.origin(),
                    pull_into.since(&from.state().vector, MAX_PAGE),
                )
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
        sent.author.voice.human = false;
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
