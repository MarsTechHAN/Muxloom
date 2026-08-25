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
