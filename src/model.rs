use std::fmt;

use serde::{Deserialize, Serialize};

pub const LOCAL_TARGET_ID: &str = "local";

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
    pub needs_attention: bool,
    pub attention_reason: Option<String>,
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
    pub resume_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectoryListing {
    pub path: String,
    pub directories: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: FileEntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePreviewKind {
    Text,
    Markdown,
    Audio,
    Video,
    Binary,
}

impl fmt::Display for FilePreviewKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Binary => "binary",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryPage {
    pub text: String,
    pub history_size: usize,
    pub pane_height: usize,
    pub pane_width: usize,
    pub offset_from_bottom: usize,
}

impl HistoryPage {
    pub fn total_lines(&self) -> usize {
        self.history_size + self.pane_height
    }

    pub fn has_older(&self) -> bool {
        self.offset_from_bottom < self.history_size
    }
}
