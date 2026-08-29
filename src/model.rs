use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

pub const LOCAL_TARGET_ID: &str = "local";

/// Parse a semantic version into comparable numbers, ignoring any pre-release
/// or build suffix (`0.4.3-rc1` -> `(0, 4, 3)`). `None` if it does not look
/// like one.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn version_is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// A daemon generation broken down into what two of them can be compared by.
///
/// The stamp itself is `<version>:protocol-<n>:<commit>:<height>:<file>`, and
/// only the first and fourth fields order anything. The commit names the build
/// without ranking it, and the file identifies the very copy that is running —
/// which differs between a controller and the companion beside it, so nothing
/// that compares two machines may look at it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GenerationRank {
    pub version: (u64, u64, u64),
    /// How many commits are behind the build. `u64::MAX` for one made by hand,
    /// `0` for a build old enough not to say — which is every build from before
    /// this field existed, and which must therefore rank below the one asking
    /// to replace it.
    pub height: u64,
}

pub fn generation_rank(stamp: &str) -> Option<GenerationRank> {
    if stamp.trim().is_empty() {
        return None;
    }
    let mut fields = stamp.trim().split(':');
    let mut numbers = fields.next()?.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    let version = (
        numbers.next()?,
        numbers.next().unwrap_or(0),
        numbers.next().unwrap_or(0),
    );
    // Past the version come the protocol version and the commit, then the
    // height. An absent field is a stamp from before there was one.
    let height = fields
        .nth(2)
        .map_or(0, |height| height.trim().parse().unwrap_or(u64::MAX));
    Some(GenerationRank { version, height })
}

/// Whether a daemon stamped `running` is behind a build stamped `current`.
///
/// Rank only, never the whole stamp: this compares a machine's daemon with the
/// controller watching it, and those are two different files even when they
/// were cut from the same commit. Two builds of one version are told apart by
/// their height, which is the only thing that separates the nightlies a fleet
/// actually runs — comparing package versions alone left every machine between
/// two releases reading as current, however far behind it had fallen, and so
/// never offered the update that would have brought it forward.
///
/// A hand-made build is deliberately not counted as ahead. On one machine it
/// claims the top of the order, so that an installed release never retires the
/// daemon a developer is working on; across two machines that same claim would
/// have a working tree deployed over the entire fleet every half hour.
pub fn generation_is_behind(running: &str, current: &str) -> bool {
    let (Some(running), Some(current)) = (generation_rank(running), generation_rank(current))
    else {
        return false;
    };
    if running.version != current.version {
        return running.version < current.version;
    }
    current.height != u64::MAX && running.height < current.height
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    Claude,
    OpenCode,
    Pi,
    Terminal,
}

impl AgentKind {
    /// Every runtime the dashboard knows, in the order it offers them. A
    /// machine shows the subset it actually has installed, but this order is
    /// what the picker, the settings panel, and the machine row all follow.
    pub const ALL: [Self; 5] = [
        Self::Codex,
        Self::Claude,
        Self::OpenCode,
        Self::Pi,
        Self::Terminal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Terminal => "terminal",
        }
    }

    /// The runtimes an install can provide, in offer order. A terminal is
    /// whatever shell the machine already has, so it is never one of them.
    pub fn agents() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(|kind| *kind != Self::Terminal)
    }

    /// Whether the CLI keeps its own record of the conversations it has had on
    /// the machine, which is what makes a session resumable by its own id and
    /// worth mirroring into the local backup. Three of them write transcripts
    /// and OpenCode keeps a store, but a terminal remembers nothing, so it is
    /// archived and searched through muxloom's own history alone.
    pub fn has_native_history(self) -> bool {
        self != Self::Terminal
    }

    /// Whether muxloom can resolve a published release for this runtime and
    /// hand it to a machine itself. Every agent publishes one; a terminal is
    /// whatever shell the machine already has, so there is nothing to fetch.
    /// The `install` command in the config stays as the fallback for when the
    /// release cannot be reached from here or from there.
    pub fn has_release_download(self) -> bool {
        self != Self::Terminal
    }

    pub fn toggle(self) -> Self {
        self.next()
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|kind| *kind == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|kind| *kind == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| format!("unsupported agent kind: {value}"))
    }
}

/// How far a session may write. Ordered from the narrowest outwards, because
/// narrowing is the only direction this ever moves: what a parent hands a
/// child is the smaller of what it has and what it asked for, and `min` is
/// exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// Back to the agent that started it, and down to whatever it starts
    /// itself. A pair of hands, reporting to the one pair of eyes that asked
    /// for the work — and the rest of the fleet is none of its business.
    Parent,
    /// Anyone on the same piece of work: the agent that started it, the ones
    /// started alongside it, and the ones it starts itself. Coordination
    /// within a team, without the rest of the fleet hearing it.
    Task,
    /// Anyone, anywhere, which is what an agent a person started has.
    Fleet,
}

impl Reach {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Task => "task",
            Self::Fleet => "fleet",
        }
    }
}

impl std::str::FromStr for Reach {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "parent" => Ok(Self::Parent),
            "task" => Ok(Self::Task),
            "fleet" => Ok(Self::Fleet),
            other => Err(format!(
                "unknown reach: {other} — one of parent, task, fleet"
            )),
        }
    }
}

/// What a session started on another agent's behalf is allowed to do.
///
/// A subagent is part of somebody else's piece of work, and the agent that
/// handed it out is the one answering for it. Which parts of muxloom that
/// subagent needs is a judgement about the work rather than about the code, so
/// it belongs to the agent making the handoff, at the moment it makes it — and
/// it can only ever be narrowed on the way down. A parent hands over the
/// smaller of what it holds and what it was asked for, so no chain of launches
/// ever ends somewhere with more than it started.
///
/// A session nobody started carries none of this, which is what full powers
/// look like: an agent a person started answers to that person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Powers {
    /// How far its messages may go.
    pub reach: Reach,
    /// The kinds it may start sessions of, in `AgentKind::ALL` order. Empty
    /// means it starts none: the work it was given is the work it does.
    pub launches: Vec<AgentKind>,
    /// Whether it may put a message in front of the person through a bound
    /// chat. Off by default for anything an agent started, because a phone
    /// that five subagents can each write to is a phone the person stops
    /// reading — and their parent is the one that owes them an answer.
    pub may_reach_person: bool,
}

impl Powers {
    /// Everything there is, which is what a session nobody started holds.
    pub fn whole() -> Self {
        Self {
            reach: Reach::Fleet,
            launches: AgentKind::ALL.to_vec(),
            may_reach_person: true,
        }
    }

    /// Nothing but the work: report back to whoever asked, and do it yourself.
    pub fn none() -> Self {
        Self {
            reach: Reach::Parent,
            launches: Vec::new(),
            may_reach_person: false,
        }
    }

    /// What a child gets when the agent starting it says nothing about any of
    /// this — the common case, and the one that has to be sensible on its own.
    ///
    /// It may talk to the team it is on, it may hand work out further in the
    /// same runtime it is itself (which is the runtime the person picked, all
    /// the way up the chain), and it does not write to the person: the agent
    /// that started it does that, because the person asked *it*.
    pub fn default_child_of(parent_kind: Option<AgentKind>, parent: &Powers) -> Self {
        let launches = match parent_kind {
            Some(kind) => vec![kind],
            // A caller that cannot say what runtime it is - a controller, a
            // session from a daemon too old to stamp one - hands on its own
            // list rather than guessing at a narrower one.
            None => parent.launches.clone(),
        };
        Powers {
            reach: Reach::Task,
            launches,
            may_reach_person: false,
        }
    }

    /// What this holder may actually hand over when a child asks for `asked`:
    /// the smaller of the two, dial by dial. Asking for more than the parent
    /// has is not an error — a parent that repeats its own defaults down a
    /// chain that has already been narrowed would fail at the second link —
    /// it simply does not get it.
    pub fn narrowed(&self, asked: &Powers) -> Self {
        Powers {
            reach: self.reach.min(asked.reach),
            launches: AgentKind::ALL
                .into_iter()
                .filter(|kind| self.launches.contains(kind) && asked.launches.contains(kind))
                .collect(),
            may_reach_person: self.may_reach_person && asked.may_reach_person,
        }
    }

    /// The kinds, as the comma-separated list the environment carries and a
    /// refusal quotes back. Empty string for none, which is why the variable is
    /// always set rather than left out: absent means "nobody said", and that
    /// reads as full powers. Spaced for the sentence it lands in; the reader
    /// trims, so the two uses are the one format.
    pub fn launches_list(&self) -> String {
        self.launches
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Read a comma-separated list of kinds back, in `AgentKind::ALL` order so
    /// the same set always reads the same way. A word that names no runtime is
    /// dropped rather than refused: this is read back out of an environment a
    /// newer muxloom wrote, which may know a runtime this one does not, and a
    /// launch is not the place to find that out.
    pub fn launches_from(list: &str) -> Vec<AgentKind> {
        AgentKind::ALL
            .into_iter()
            .filter(|kind| list.split(',').any(|word| word.trim() == kind.as_str()))
            .collect()
    }
}

/// What an agent's prompt box says about a message typed into it right now.
///
/// This, and not "is the agent working", is the question worth asking before
/// putting something in front of a session. Both CLIs keep an empty prompt box
/// on screen for the whole of a turn and hold what arrives during one until
/// that turn ends, so a message delivered mid-turn is read a moment later, in
/// order, with nothing lost. What loses a message is the box being otherwise
/// engaged: already holding a sentence somebody has not sent yet, or not drawn
/// at all because the CLI is asking a question, still starting up, or no longer
/// the process on the pty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Composer {
    /// Drawn and empty: a paste lands whole, and lands on its own.
    Ready,
    /// Drawn with something already in it. A paste would be appended to that
    /// and submitted together with it, as one message neither party wrote.
    Occupied,
    /// Not drawn where this runtime draws it. Whatever is typed now goes to
    /// whatever is there instead — a dialog's answer, an installer, a shell.
    Absent,
}

impl Composer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Occupied => "occupied",
            Self::Absent => "absent",
        }
    }
}

impl fmt::Display for Composer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disabled,
    Scanning,
    Online,
    Offline,
}

/// Progress for controller-side connection, provisioning, and installation
/// work. A missing total represents a stage whose duration is not measurable;
/// byte transfers report their exact size when the server provides one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProgress {
    pub label: String,
    pub completed: u64,
    pub total: Option<u64>,
}

impl TaskProgress {
    pub fn pending(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            completed: 0,
            total: None,
        }
    }

    pub fn bytes(label: impl Into<String>, completed: u64, total: Option<u64>) -> Self {
        Self {
            label: label.into(),
            completed,
            total,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub label: String,
    pub transport: Transport,
}

impl Target {
    pub fn local() -> Self {
        Self {
            id: LOCAL_TARGET_ID.into(),
            label: "This machine".into(),
            transport: Transport::Local,
        }
    }

    pub fn ssh(alias: impl Into<String>) -> Self {
        let alias = alias.into();
        Self {
            id: alias.clone(),
            label: alias.clone(),
            transport: Transport::Ssh { alias },
        }
    }

    /// A human-facing name for a machine list: the label if it says something
    /// other than the id, otherwise the id itself.
    pub fn label_or_id(&self) -> &str {
        let label = self.label.trim();
        if label.is_empty() || label == self.id {
            self.id.as_str()
        } else {
            label
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Local,
    Ssh { alias: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub tmux: bool,
    /// The agent runtimes whose executable answered on the machine. A terminal
    /// needs nothing installed, so it never appears here.
    pub runtimes: BTreeSet<AgentKind>,
}

impl Probe {
    pub fn has(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Terminal || self.runtimes.contains(&kind)
    }

    pub fn set(&mut self, kind: AgentKind, present: bool) {
        if present && kind != AgentKind::Terminal {
            self.runtimes.insert(kind);
        } else {
            self.runtimes.remove(&kind);
        }
    }

    /// The runtimes to offer on this machine: everything installed, plus the
    /// terminal, in offer order.
    pub fn available(&self) -> Vec<AgentKind> {
        AgentKind::ALL
            .into_iter()
            .filter(|kind| self.has(*kind))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct TargetStatus {
    pub target: Target,
    pub enabled: bool,
    pub state: ConnectionState,
    pub probe: Probe,
    pub error: Option<String>,
    pub consecutive_failures: u8,
}

impl TargetStatus {
    pub fn new(target: Target, enabled: bool) -> Self {
        Self {
            target,
            enabled,
            state: if enabled {
                ConnectionState::Scanning
            } else {
                ConnectionState::Disabled
            },
            probe: Probe::default(),
            error: None,
            consecutive_failures: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SearchMatchKind {
    History,
    Recap,
    Name,
}

impl fmt::Display for SearchMatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Name => "name",
            Self::Recap => "recap",
            Self::History => "history",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub session_id: String,
    pub target_id: String,
    pub kind: AgentKind,
    pub label: String,
    pub path: String,
    pub match_kind: SearchMatchKind,
    pub snippet: String,
    pub line_number: Option<usize>,
    pub created_at: u64,
    pub dead: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMatch {
    pub recap: bool,
    pub line_number: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub id: String,
    pub target_id: String,
    pub kind: AgentKind,
    pub path: String,
    pub label: String,
    pub created_at: u64,
    /// When this session was archived, in seconds. An archive is read
    /// newest-put-down first, which is a different order from when each
    /// conversation began. `None` on a live session, and on a record archived
    /// by a daemon too old to write it down.
    pub archived_at: Option<u64>,
    pub dead: bool,
    pub pid: Option<u32>,
    pub working: bool,
    pub needs_attention: bool,
    pub attention_reason: Option<String>,
    pub recap: Option<String>,
    /// What the runtime called the conversation, out of the transcript it
    /// keeps. Absent while it has not named one yet, from runtimes that write
    /// no transcript, and from daemons too old to read one.
    pub title: Option<String>,
    /// The agent-native conversation this session is having, as the daemon
    /// matched it. This is what `--resume` has to be given to reopen *this*
    /// session rather than whichever conversation in the folder was touched
    /// last. Absent from runtimes that write no transcript, from a session
    /// whose transcript has not been matched yet, and from daemons too old to
    /// record one.
    pub thread: Option<String>,
    /// The session that started this one, when an agent did. A person's launch
    /// from the dashboard has none, and neither has anything a daemon too old
    /// to record one reports.
    pub parent: Option<String>,
}

impl AgentSession {
    /// What to call this session in a list.
    ///
    /// A name typed by hand is what the user meant it to be called and beats
    /// everything. Failing that, the agent names its own conversation, which
    /// says what is being worked on rather than merely where; only when there
    /// is no name at all does the folder have to stand in for one.
    pub fn display_label(&self) -> &str {
        if !self.label.is_empty() {
            return &self.label;
        }
        if let Some(title) = self.title.as_deref().filter(|title| !title.is_empty()) {
            return title;
        }
        self.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub target: Target,
    pub kind: AgentKind,
    pub path: String,
    pub label: String,
    pub temporary: bool,
    pub resume_id: Option<String>,
    /// Initial prompt for a fresh session. Used when another agent runtime's
    /// history is referenced instead of passed as an incompatible resume id.
    pub initial_prompt: Option<String>,
    /// The session asking for this one, when an agent is. A launch a person
    /// makes from the dashboard has no parent: it is its own piece of work.
    pub parent: Option<String>,
    /// What the asking agent is handing over, already narrowed against what it
    /// holds. `None` for every launch a person makes, which is what full
    /// powers look like.
    pub powers: Option<Powers>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: String,
    pub directories: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: FileEntryKind,
    pub size: u64,
    /// Last modification time in whole seconds since the Unix epoch, or 0 when
    /// the target could not report one. Paired with `size` it is the cheap
    /// change stamp the browser polls to notice edits to an open file.
    #[serde(default)]
    pub mtime: u64,
    /// True when the entry is a symbolic link. `kind` describes what the link
    /// resolves to, so a link to a directory can be opened like any other
    /// directory; this flag only keeps the listing able to say it is a link.
    #[serde(default)]
    pub symlink: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// True when the walk stopped at its own budget rather than at the end of
    /// the tree, so the caller can say the list is a slice rather than the set.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilePreviewKind {
    Text,
    Markdown,
    Image,
    Audio,
    Video,
    Binary,
}

impl fmt::Display for FilePreviewKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Binary => "binary",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePreview {
    pub path: String,
    pub mime: String,
    pub kind: FilePreviewKind,
    pub size: u64,
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCandidate {
    pub id: String,
    pub kind: AgentKind,
    pub source_path: String,
    pub recap: Option<String>,
    pub first_message: Option<String>,
    pub last_message: Option<String>,
    pub updated_at: String,
}

impl ResumeCandidate {
    pub fn summary(&self) -> &str {
        self.recap
            .as_deref()
            .or(self.first_message.as_deref())
            .or(self.last_message.as_deref())
            .unwrap_or("Previous session")
    }
}

/// Where a backed-up transcript landed after being restored onto a machine that
/// had lost it. Plain fields so the daemon build, which has no backup store,
/// still compiles the worker protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredTranscript {
    /// The agent-native id to resume with (`claude --resume <id>`,
    /// `codex resume <id>`).
    pub resume_id: String,
    /// Absolute path the transcript now occupies on the target.
    pub path: String,
    /// Uncompressed transcript size, for the status line.
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryPage {
    pub text: String,
    pub history_size: usize,
    pub pane_height: usize,
    pub pane_width: usize,
    pub offset_from_bottom: usize,
    /// Whether the page holds the rows a terminal would have shown, which is
    /// what makes its offsets count the same thing an attached emulator scrolls
    /// through. Pages of raw log lines leave it false.
    #[serde(default)]
    pub rendered: bool,
    /// Whether rows older than this page are expected to exist. Rendering a
    /// page reaches only as far back as it was asked to, so `history_size`
    /// measures that reach rather than the session, and on its own would read
    /// as the end of the history after a single page.
    #[serde(default)]
    pub more_history: bool,
}

impl HistoryPage {
    pub fn total_lines(&self) -> usize {
        self.history_size + self.pane_height
    }

    pub fn has_older(&self) -> bool {
        self.more_history || self.offset_from_bottom < self.history_size
    }

    /// The oldest offset the page can vouch for, or `None` while older history
    /// is still expected and no limit is known yet.
    pub fn oldest_offset(&self) -> Option<usize> {
        (!self.more_history).then_some(self.history_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one rule the whole thing rests on: whatever a child asks for, it
    /// gets no more than the agent starting it already had. Without it a chain
    /// of launches is a way to climb back to full powers one hop at a time.
    #[test]
    fn a_grant_is_the_smaller_of_what_was_held_and_what_was_asked_for() {
        let held = Powers {
            reach: Reach::Task,
            launches: vec![AgentKind::Claude],
            may_reach_person: false,
        };
        let asked = Powers::whole();
        let granted = held.narrowed(&asked);
        assert_eq!(granted, held, "asking for everything gets what was held");

        // And the other way round: a holder of everything hands over exactly
        // what was asked for, which is how a parent restricts a child.
        let narrow = Powers {
            reach: Reach::Parent,
            launches: Vec::new(),
            may_reach_person: false,
        };
        assert_eq!(Powers::whole().narrowed(&narrow), narrow);

        // Dial by dial, so a child asking wide on one and narrow on another
        // gets the narrow answer on both.
        let mixed = Powers::whole().narrowed(&Powers {
            reach: Reach::Fleet,
            launches: vec![AgentKind::Codex, AgentKind::Terminal],
            may_reach_person: true,
        });
        assert_eq!(mixed.reach, Reach::Fleet);
        assert_eq!(mixed.launches, vec![AgentKind::Codex, AgentKind::Terminal]);
        // Kinds come back in the one order, whatever order they were asked in.
        let unordered = Powers::whole().narrowed(&Powers {
            reach: Reach::Task,
            launches: vec![AgentKind::Terminal, AgentKind::Codex],
            may_reach_person: false,
        });
        assert_eq!(
            unordered.launches,
            vec![AgentKind::Codex, AgentKind::Terminal]
        );
        // Narrowing is idempotent, so re-applying a chain's own grant at every
        // link leaves it where it was rather than eroding it.
        assert_eq!(held.narrowed(&held), held);
    }

    /// What a child gets when nobody says anything, which is most launches.
    #[test]
    fn a_child_nobody_spoke_for_works_within_its_team_in_its_parents_runtime() {
        let child = Powers::default_child_of(Some(AgentKind::Claude), &Powers::whole());
        assert_eq!(child.reach, Reach::Task);
        assert_eq!(child.launches, vec![AgentKind::Claude]);
        assert!(
            !child.may_reach_person,
            "the agent that was asked is the one that answers the person"
        );
        // A grandchild of that child keeps the runtime rather than widening
        // back out to every kind the fleet has.
        let grandchild = Powers::default_child_of(Some(AgentKind::Claude), &child);
        assert_eq!(grandchild.launches, vec![AgentKind::Claude]);
        // A caller that cannot say what runtime it is hands on its own list,
        // which after one narrowing is already the right one.
        assert_eq!(
            Powers::default_child_of(None, &child).launches,
            vec![AgentKind::Claude]
        );
    }

    /// The environment is where a grant is read back, so the round trip has to
    /// be exact — and "no kinds at all" has to survive it, because that is the
    /// setting that stops a subagent starting more of them.
    #[test]
    fn a_set_of_kinds_survives_the_trip_through_an_environment_variable() {
        for launches in [
            AgentKind::ALL.to_vec(),
            vec![AgentKind::Claude],
            vec![AgentKind::Codex, AgentKind::Terminal],
            Vec::new(),
        ] {
            let powers = Powers {
                reach: Reach::Task,
                launches: launches.clone(),
                may_reach_person: false,
            };
            assert_eq!(Powers::launches_from(&powers.launches_list()), launches);
        }
        assert_eq!(Powers::none().launches_list(), "");
        // A runtime this build has never heard of is dropped rather than
        // failing the launch: the variable may have been written by a newer
        // muxloom that knows one more.
        assert_eq!(
            Powers::launches_from("claude,quantum-agent"),
            vec![AgentKind::Claude]
        );
        assert_eq!(Reach::Fleet.as_str().parse(), Ok(Reach::Fleet));
        assert!("everyone".parse::<Reach>().is_err());
    }

    /// The whole point of carrying the stamp across the wire. A fleet on
    /// nightlies reads the same package version everywhere, so before this the
    /// controller had nothing to notice a month-old machine by.
    #[test]
    fn a_daemon_from_an_older_commit_of_one_version_reads_as_behind() {
        let old = "0.5.5:protocol-1:aaac2e0:265:100-1";
        let new = "0.5.5:protocol-1:3af6b11:287:200-2";
        assert!(generation_is_behind(old, new));
        assert!(!generation_is_behind(new, old));
        assert!(!generation_is_behind(new, new));
    }

    /// The file each build is differs between the controller and the daemon it
    /// watches even when both were cut from one commit, so it must not count.
    #[test]
    fn the_same_build_on_two_machines_is_not_behind_itself() {
        let daemon = "0.5.5:protocol-1:3af6b11:287:9000-5";
        let controller = "0.5.5:protocol-1:3af6b11:287:4200-9";
        assert!(!generation_is_behind(daemon, controller));
        assert!(!generation_is_behind(controller, daemon));
    }

    /// A version is still a version. A daemon left behind by a release is
    /// behind whatever the commits say.
    #[test]
    fn an_older_version_is_behind_however_far_along_its_commits_are() {
        let old = "0.5.4:protocol-1:aaac2e0:9999:1-1";
        let new = "0.5.5:protocol-1:3af6b11:2:1-1";
        assert!(generation_is_behind(old, new));
        assert!(!generation_is_behind(new, old));
    }

    /// A build made by hand ranks above every numbered one of its version, so
    /// that an installed release never retires the daemon a developer is
    /// working on. Across two machines that claim would have a working tree
    /// deployed over the whole fleet every half hour, so it stops at the edge.
    #[test]
    fn a_hand_made_controller_does_not_call_the_fleet_outdated() {
        let fleet = "0.5.5:protocol-1:3af6b11:287:1-1";
        let working_tree = "0.5.5:protocol-1:local:local:1-1";
        assert!(!generation_is_behind(fleet, working_tree));
        // Nor the other way about: nothing numbered outranks a build somebody
        // made deliberately.
        assert!(!generation_is_behind(working_tree, fleet));
    }

    /// A daemon old enough not to send a stamp at all sends an empty string,
    /// which orders nothing. The caller falls back to the version there.
    #[test]
    fn a_stamp_that_says_nothing_orders_nothing() {
        let new = "0.5.5:protocol-1:3af6b11:287:1-1";
        assert!(!generation_is_behind("", new));
        assert!(!generation_is_behind(new, ""));
    }
}
