use std::fmt;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    Claude,
    Terminal,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Terminal => "terminal",
        }
    }

    pub fn toggle(self) -> Self {
        self.next()
    }

    pub fn next(self) -> Self {
        match self {
            Self::Codex => Self::Claude,
            Self::Claude => Self::Terminal,
            Self::Terminal => Self::Codex,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Codex => Self::Terminal,
            Self::Claude => Self::Codex,
            Self::Terminal => Self::Claude,
        }
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
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "terminal" => Ok(Self::Terminal),
            other => Err(format!("unsupported agent kind: {other}")),
        }
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
    pub codex: bool,
    pub claude: bool,
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
}

impl AgentSession {
    pub fn display_label(&self) -> &str {
        if self.label.is_empty() {
            self.path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(&self.path)
        } else {
            &self.label
        }
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
