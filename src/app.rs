use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, widgets::ListState};
use unicode_width::UnicodeWidthChar;

use crate::{
    config::{Config, State},
    debug,
    media::{MediaFrame, MediaPlayback, MediaUpdate},
    model::{
        AgentKind, AgentSession, ConnectionState, DirectoryListing, FileEntry, FileEntryKind,
        FileListing, FilePreview, FilePreviewKind, HistoryPage, LOCAL_TARGET_ID, LaunchRequest,
        ResumeCandidate, SearchResult, Target, TargetStatus, TaskProgress,
    },
    port_forward::{PortForwardManager, PortForwardState, PortForwardSummary},
    recap::extract_recap,
    runtime::{Runtime, agent_is_working, attention_reason, is_temporary_session_id},
    ssh_config,
    talk::{TalkAuthor, TalkDraft, TalkKind, TalkMessage, TalkPage, TalkScope, TalkVoice},
    terminal_session::TerminalSession,
    worker::{Event, Request, ScanRequest, TaskKind, Worker},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Machines,
    Agents,
    Recap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileManagerOrigin {
    AgentPane,
    TerminalPane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchField {
    Kind,
    Path,
    Label,
}

/// One row of the machine pane. The moderators row is pinned above the machines
/// and stands for the agents muxloom runs to coordinate the others, so it
/// cannot be an index into `targets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineRow {
    Moderators,
    Machine(usize),
}

#[derive(Debug, Clone)]
pub struct LaunchForm {
    pub target: Target,
    pub kind: AgentKind,
    pub path: String,
    pub label: String,
    pub temporary: bool,
    pub field: LaunchField,
}

/// One line of a moderator's scope: a machine it may drive, or an agent it may
/// hand work to. Checked means in scope.
#[derive(Debug, Clone)]
pub struct ScopeItem {
    /// What the moderator's briefing calls it.
    pub label: String,
    /// The machine it is on, by target id. A machine names itself; an agent
    /// names where it runs, which is what lets the agent list follow the
    /// machines that are still checked.
    pub machine: String,
    pub selected: bool,
}

/// The new-moderator form. No directory field: muxloom makes the folder.
#[derive(Debug, Clone)]
pub struct ModeratorForm {
    pub kind: AgentKind,
    pub name: String,
    pub machines: Vec<ScopeItem>,
    pub agents: Vec<ScopeItem>,
    /// Which row the cursor is on, over the flattened row list.
    pub selected: usize,
    pub error: Option<String>,
}

/// What a row of the moderator form is, once the machine and agent lists have
/// been flattened into one navigable column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeratorRow {
    Kind,
    Name,
    MachinesHeader,
    Machine(usize),
    AgentsHeader,
    Agent(usize),
}

impl ModeratorForm {
    /// Every row in the order they are drawn and navigated. Headers are in the
    /// list so an empty group still says why it is empty.
    pub fn rows(&self) -> Vec<ModeratorRow> {
        let mut rows = vec![
            ModeratorRow::Kind,
            ModeratorRow::Name,
            ModeratorRow::MachinesHeader,
        ];
        rows.extend((0..self.machines.len()).map(ModeratorRow::Machine));
        rows.push(ModeratorRow::AgentsHeader);
        rows.extend(self.visible_agents().into_iter().map(ModeratorRow::Agent));
        rows
    }

    pub fn row(&self) -> ModeratorRow {
        let rows = self.rows();
        rows.get(self.selected)
            .copied()
            .unwrap_or(ModeratorRow::Kind)
    }

    /// The agents worth choosing between: the ones on machines that are still
    /// checked. Unchecking a machine takes its agents out of the list rather
    /// than leaving them there to be handed to a moderator that was told not
    /// to look at that machine at all.
    pub fn visible_agents(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| self.machine_chosen(&agent.machine))
            .map(|(index, _)| index)
            .collect()
    }

    fn machine_chosen(&self, machine: &str) -> bool {
        self.machines
            .iter()
            .any(|item| item.machine == machine && item.selected)
    }

    /// The checked labels, or an empty list when everything is checked — the
    /// briefing says "every machine" rather than listing the fleet back.
    fn chosen<'a>(items: impl IntoIterator<Item = &'a ScopeItem>) -> Vec<String> {
        let items: Vec<&ScopeItem> = items.into_iter().collect();
        if items.iter().all(|item| item.selected) {
            return Vec::new();
        }
        items
            .iter()
            .filter(|item| item.selected)
            .map(|item| item.label.clone())
            .collect()
    }

    pub fn chosen_machines(&self) -> Vec<String> {
        Self::chosen(&self.machines)
    }

    /// Only the agents still on show: one on a machine the moderator was told
    /// to leave alone is out of scope whether or not its box was ever cleared.
    pub fn chosen_agents(&self) -> Vec<String> {
        Self::chosen(
            self.visible_agents()
                .into_iter()
                .map(|index| &self.agents[index]),
        )
    }
}

#[derive(Debug, Clone)]
pub struct TemporalForm {
    pub target: Target,
    pub kind: AgentKind,
    pub path: String,
    /// What to call this chat. Blank keeps the default name.
    pub label: String,
}

impl TemporalForm {
    pub const DEFAULT_LABEL: &'static str = "Temporal Chat";

    pub fn label(&self) -> &str {
        let label = self.label.trim();
        if label.is_empty() {
            Self::DEFAULT_LABEL
        } else {
            label
        }
    }
}

#[derive(Debug, Clone)]
pub struct PortForwardForm {
    pub target: Target,
    pub session_id: String,
    pub folder: String,
    pub remote_host: String,
    pub remote_port: String,
    pub local_port: String,
    pub detected_ports: Vec<u16>,
    pub active: Vec<PortForwardSummary>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub detection_error: Option<String>,
}

impl PortForwardForm {
    pub const FIELD_COUNT: usize = 3;

    pub fn row_count(&self) -> usize {
        Self::FIELD_COUNT + self.active.len()
    }

    pub fn active_index(&self) -> Option<usize> {
        self.selected.checked_sub(Self::FIELD_COUNT)
    }
}

#[derive(Debug, Clone)]
struct ArchivedResume {
    source_session_id: String,
    launch: LaunchForm,
}

#[derive(Debug, Clone)]
struct PendingInstallLaunch {
    launch: LaunchForm,
    resume_id: Option<String>,
    initial_prompt: Option<String>,
    remove_archive_session_id: Option<String>,
}

/// An attach running on a background thread. Attaching dials the daemon bridge
/// (or spawns ssh/tmux), which takes seconds on a poor link, so the render loop
/// hands the work off and picks the result up on a later tick.
struct PendingAttach {
    session_id: String,
    /// Whether the terminal should take keyboard input once it is live. A second
    /// activation request while the attach is in flight upgrades this.
    take_input: bool,
    outcome: mpsc::Receiver<Result<TerminalSession, String>>,
}

/// Result of the background self-update check, handed to the UI thread. Kept in
/// `app` (rather than `update`, which is controller-only) so this module builds
/// for the lean `muxloomd` companion too.
#[derive(Debug, Clone, Default)]
pub struct UpdateNote {
    /// Footer message to show, if any.
    pub message: Option<String>,
    /// The newer version that is now staged / available, if any.
    /// Set only when a new build was downloaded and is waiting on a restart.
    pub staged_version: Option<String>,
    /// Set when a newer release exists but nothing was downloaded, so the
    /// header asks for `muxloom update` rather than promising a restart.
    pub available_version: Option<String>,
    /// Set when the user should decide what to do; opens the update modal.
    pub prompt: Option<UpdatePrompt>,
}

/// What the startup check found and what saying yes would do. The release is
/// carried in full rather than looked up again: the build the user agreed to
/// is the build that gets installed, even if a nightly lands in between.
#[derive(Debug, Clone)]
pub struct UpdatePrompt {
    /// How the build was named to the user: `0.5.5`, or `nightly 0.5.4+142`.
    pub latest: String,
    /// How the running build is named, so the prompt can say what is being
    /// left behind — two nightlies otherwise differ only in a commit count.
    pub current: String,
    /// The release tag its assets hang off: `v0.5.5`, or `nightly`.
    pub tag: String,
    /// The package version its asset names carry.
    pub version: String,
    /// True on an installed release bundle, where yes replaces the bundle in
    /// place. A source build can only refresh the companion cache.
    pub can_self_update: bool,
}

/// One machine's forced daemon update in flight: archive the sessions that
/// hold the old generation, cycle the bridge so the handover completes, then
/// resume every agent from the transcript its runtime recorded.
#[derive(Debug)]
struct ForcedUpdate {
    target: Target,
    phase: ForcedPhase,
    /// Give up and report if a phase stalls past this.
    deadline: Instant,
    /// Agents to bring back once the new daemon serves, in archive order.
    resumes: Vec<PendingResume>,
    /// Sessions whose archive/kill acknowledgement is still pending.
    pending_acks: usize,
    terminals_archived: usize,
    /// Whether the negotiated handover already failed once and the outright
    /// daemon restart was ordered. One escalation, then give up loudly.
    escalated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForcedPhase {
    Archiving,
    Cycling,
    Resuming,
}

#[derive(Debug)]
struct PendingResume {
    session_id: String,
    kind: AgentKind,
    path: String,
    label: String,
}

/// A slot the startup update thread writes once; the UI drains it on the next tick.
pub type UpdateSlot = Arc<Mutex<Option<UpdateNote>>>;

#[derive(Debug, Clone)]
pub struct PathPickerForm {
    pub launch: LaunchForm,
    pub path: String,
    pub directories: Vec<String>,
    pub query: String,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl PathPickerForm {
    pub fn matches(&self) -> Vec<String> {
        matched_directories(self)
    }
}

#[derive(Debug, Clone)]
pub struct ResumeForm {
    pub launch: LaunchForm,
    pub candidates: Vec<ResumeCandidate>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Cross-machine reference panel: search across every machine's backed-up
    /// history and reference one that is not on this machine. Collapsed until
    /// the user types a query.
    pub query: String,
    pub history_hits: Vec<CrossMachineHit>,
    pub history_selected: usize,
    pub searched_query: String,
    pub search_edited_at: Option<Instant>,
}

impl ResumeForm {
    /// True once the user has typed a search query — the cross-machine panel is
    /// expanded and its list takes navigation.
    pub fn history_active(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

/// A backed-up conversation surfaced by the cross-machine reference search.
/// Plain fields only (no backup-crate types) so the modal compiles without the
/// controller feature.
#[derive(Debug, Clone)]
pub struct CrossMachineHit {
    pub target_id: String,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub snippet: String,
    pub created_at: u64,
}

/// A conversation the local backup still holds for a machine that has since
/// forgotten it. Plain fields only, for the same reason as `CrossMachineHit`.
#[derive(Debug, Clone)]
struct RecoverableSession {
    session_id: String,
    kind: String,
    label: String,
    cwd: String,
    title: String,
    recap: String,
    created_at: u64,
    machine_key: String,
    restorable: bool,
}

/// What the local store knows about a listed session its machine has lost.
#[derive(Debug, Clone)]
struct RecoveryInfo {
    /// Backup partition holding the record. Not always the target id: a machine
    /// keeps the key it was first seen under, across later alias churn.
    machine_key: String,
    /// Whether a resumable agent transcript came with the record, or only the
    /// terminal output it printed.
    restorable: bool,
}

#[derive(Debug)]
pub struct FileManagerForm {
    pub origin: FileManagerOrigin,
    pub target: Target,
    /// Agent this browser was opened for, if any; used to remember the last
    /// browsed directory per agent.
    pub session_id: Option<String>,
    pub path: String,
    pub entries: Vec<FileEntry>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub directory_cache: HashMap<String, Vec<FileEntry>>,
    pub return_path: Option<String>,
    pub preview_path: Option<String>,
    pub preview: Option<FilePreview>,
    pub preview_requested_path: Option<String>,
    pub preview_loading: bool,
    pub preview_error: Option<String>,
    pub preview_scroll: usize,
    pub preview_max_scroll: usize,
    pub preview_page_rows: u16,
    /// True while the view sits at the bottom of the preview. A refresh that
    /// makes the file longer then follows the new tail instead of leaving the
    /// reader stranded above the appended lines.
    pub preview_follow_tail: bool,
    /// `(size, mtime)` of the previewed file as of the listing that produced the
    /// preview now on screen. A later listing with a different stamp means the
    /// file changed and the preview needs re-fetching.
    pub preview_stamp: Option<(u64, u64)>,
    /// Styled rows for the preview on screen. Rebuilt only when the content
    /// changes, and kept out of the per-frame path so paging a large file never
    /// re-renders it.
    pub preview_rendered: Option<crate::ui::PreviewRender>,
    pub query: String,
    pub search_request_id: Option<u64>,
    pub searching: bool,
    /// Set when the last recursive search hit its own budget, so the footer can
    /// say the list is a slice of the matches rather than all of them.
    pub search_truncated: bool,
    pub search_edited_at: Option<Instant>,
    pub preview_cache: HashMap<String, FilePreview>,
    pub preload_pending: HashSet<String>,
    pub entry_rows: Vec<(usize, Rect)>,
    pub list_area: Option<Rect>,
    pub preview_area: Option<Rect>,
    pub preview_text_area: Option<Rect>,
    pub preview_visible: Vec<String>,
    pub preview_selection: Option<TerminalSelection>,
    pub media_playback: Option<MediaPlayback>,
    pub media_frame: Option<MediaFrame>,
    pub media_loading: bool,
    pub media_error: Option<String>,
}

#[derive(Debug, Clone)]
struct FileClick {
    key: String,
    at: Instant,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Launch(LaunchForm),
    Moderator(ModeratorForm),
    Temporal(TemporalForm),
    PortForward(PortForwardForm),
    ConfirmKill {
        session_id: String,
        label: String,
        archive: bool,
    },
    ConfirmInstall {
        launch: LaunchForm,
        resume_id: Option<String>,
        initial_prompt: Option<String>,
        remove_archive_session_id: Option<String>,
    },
    ConfirmArchivedResume {
        source_session_id: String,
        launch: LaunchForm,
        resume_id: String,
        remove_archive: bool,
    },
    ConfirmHistoryReference {
        form: ResumeForm,
        candidate: ResumeCandidate,
    },
    LegacyFallback {
        target_id: String,
        detail: String,
    },
    UpdatePrompt(UpdatePrompt),
    /// A forced daemon update would interrupt what it lists; the user decides.
    ConfirmForcedUpdate {
        target: Target,
        working: Vec<String>,
        terminals: Vec<String>,
        resumable: usize,
    },
    Help(HelpForm),
    Settings(SettingsForm),
    Search(SearchForm),
    Board(BoardForm),
    PathPicker(PathPickerForm),
    Resume(ResumeForm),
    RenameAgent {
        session_id: String,
        value: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct HelpForm {
    pub offset: usize,
}

pub const HELP_CONTENT_ROWS: usize = 80;

/// Wall-clock milliseconds each agent-spinner frame is shown. Deriving the
/// frame index from elapsed time divided by this keeps the animation speed
/// constant regardless of how frequently the UI redraws.
const ANIMATION_FRAME_MS: u128 = 180;
const ACTIVITY_REFRESH_INTERVAL: Duration = Duration::from_millis(350);
/// How often the talk board is carried between machines. A conversation this
/// slow is still a conversation; a machine polled faster than this is being
/// asked to answer more often than anyone types.
const TALK_SYNC_INTERVAL: Duration = Duration::from_secs(2);
/// How long one phase of a forced daemon update may take before the
/// orchestration gives up and says so. Generous: the cycle can carry a
/// companion upload over a slow link, and the escalation waits out a
/// deliberate daemon stop.
const FORCED_PHASE_TIMEOUT: Duration = Duration::from_secs(180);
const FILE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// How often the browser re-lists the directory holding the open preview to
/// notice that the file changed. Only runs while a preview is on screen, so an
/// idle browser costs nothing on the wire.
const FILE_MONITOR_INTERVAL: Duration = Duration::from_millis(1500);
/// How long to wait for a monitor listing before assuming it was lost and
/// polling again. Keeps a dropped reply from wedging the monitor for good.
const FILE_MONITOR_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest file the monitor re-reads on its own. A preview always shows the
/// whole file, so following a bigger one would mean hauling it across the link
/// every time it changes; those wait for an explicit refresh.
const AUTO_REFRESH_LIMIT: u64 = 4 * 1024 * 1024;
/// How much of a backed-up capture is read to build one page of history. The
/// blob holds every row the session ever printed, which for a long-running
/// agent is tens of megabytes; a page is then cut from the tail of this.
const RECOVERED_HISTORY_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct SettingsForm {
    pub scope: SettingsScope,
    pub values: Vec<String>,
    /// Text for the read-only [`SettingsRow::Note`] rows, in row order.
    pub notes: Vec<String>,
    /// The runtimes this machine does not have, filled when the panel opens.
    /// Each one gets an install action in its own section.
    pub missing: Vec<AgentKind>,
    pub selected: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Host(String),
}

#[derive(Debug, Clone)]
pub struct SearchForm {
    pub query: String,
    pub submitted_query: String,
    pub results: Vec<SearchResult>,
    pub result_rows: Vec<(usize, Rect)>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub edited_at: Instant,
}

/// The tail of the talk board as the dashboard holds it: what has been said
/// across every machine, and how much of it arrived while nobody was reading.
#[derive(Debug, Clone, Default)]
pub struct Board {
    /// Oldest first. Capped at [`BOARD_MEMORY`]; the rest is on the machines
    /// that minted it, and stays there.
    pub messages: Vec<TalkMessage>,
    /// What the local board held when it was last read. Handed back with the
    /// next read so it only answers with what has been said since.
    pub cursor: String,
    /// Messages that arrived while the overlay was closed.
    pub unread: usize,
}

/// How many messages the dashboard keeps in front of it. A board is read from
/// the bottom, and a thousand lines is already more than anyone scrolls.
const BOARD_MEMORY: usize = 1000;

impl Board {
    /// File a page of messages, newest last, ignoring any already held. Returns
    /// how many were new, which is what the unread mark counts.
    pub fn merge(&mut self, messages: Vec<TalkMessage>) -> usize {
        let mut added = 0;
        for message in messages {
            if self.messages.iter().any(|held| held.id == message.id) {
                continue;
            }
            self.messages.push(message);
            added += 1;
        }
        if added > 0 {
            // Two machines' clocks disagree and replication arrives out of
            // order, so the board is sorted rather than appended to: the origin
            // and sequence break ties the timestamps cannot.
            self.messages.sort_by(|left, right| {
                (left.ts, &left.origin, left.seq).cmp(&(right.ts, &right.origin, right.seq))
            });
            if self.messages.len() > BOARD_MEMORY {
                self.messages.drain(..self.messages.len() - BOARD_MEMORY);
            }
        }
        added
    }
}

/// Which slice of the board is on screen. The tabs are the scopes, plus the
/// directs agents sent each other and the everything view they all fold into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BoardTab {
    #[default]
    All,
    Global,
    Machine,
    Path,
    Task,
    Direct,
}

impl BoardTab {
    pub const ORDER: [BoardTab; 6] = [
        BoardTab::All,
        BoardTab::Global,
        BoardTab::Machine,
        BoardTab::Path,
        BoardTab::Task,
        BoardTab::Direct,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Global => "Global",
            Self::Machine => "Machine",
            Self::Path => "Path",
            Self::Task => "Task",
            Self::Direct => "Direct",
        }
    }

    /// Whether a message belongs on this tab. A direct message answers for its
    /// delivery rather than for the scope it was filed under: it was said to
    /// one session, not to everyone standing in that directory.
    ///
    /// `task` is the task the agent list is standing in, from
    /// [`App::selected_task`]. Only the Task tab reads it, and it is the one
    /// tab a message cannot answer for on its own: whether something belongs
    /// to a piece of work is a fact about who started whom, which lives in the
    /// session list rather than on the message.
    pub fn admits(self, message: &TalkMessage, task: &BTreeMap<String, usize>) -> bool {
        let direct = message.kind == TalkKind::Direct;
        match self {
            Self::All => true,
            Self::Direct => direct,
            Self::Global => !direct && matches!(message.scope, TalkScope::Global),
            Self::Machine => !direct && matches!(message.scope, TalkScope::Machine { .. }),
            Self::Path => !direct && matches!(message.scope, TalkScope::Path { .. }),
            // Everything the task said and everything said to it, whichever
            // board it was filed under: a task is a set of agents, and what
            // they are doing is scattered across the scopes they used. The
            // scope check catches a message from a session the machine has
            // since forgotten, which the author check no longer can.
            Self::Task => {
                let member = |id: Option<&str>| id.is_some_and(|id| task.contains_key(id));
                member(message.author.voice.session_id.as_deref())
                    || member(message.to.as_ref().map(|to| to.session_id.as_str()))
                    || message
                        .scope
                        .task()
                        .is_some_and(|root| task.contains_key(root))
            }
        }
    }

    fn stepped(self, delta: isize) -> Self {
        let at = Self::ORDER.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        let len = Self::ORDER.len() as isize;
        Self::ORDER[((at + delta).rem_euclid(len)) as usize]
    }
}

/// The board overlay: a scope tab, what is being looked for, and whatever the
/// person at the keyboard is in the middle of writing.
#[derive(Debug, Clone, Default)]
pub struct BoardForm {
    pub tab: BoardTab,
    /// The message under the cursor. `None` follows the newest, which is where
    /// a conversation happens; picking one stops the view from moving.
    pub selected: Option<String>,
    /// Substring the visible messages are narrowed to.
    pub query: String,
    /// Whether keys are going into `query` rather than the board.
    pub searching: bool,
    /// What is being written, and what it answers. `Some` means keys go here.
    pub compose: Option<String>,
    pub reply_to: Option<String>,
    /// Whether the selected message is shown in full below the list.
    pub expanded: bool,
    /// Which message each drawn row holds, for clicks.
    pub rows: Vec<(String, Rect)>,
    /// How many rows the list had room for when it was last drawn, so a page
    /// key moves by what the reader can see.
    pub page: usize,
    pub error: Option<String>,
}

impl BoardForm {
    /// Move the cursor through what is on screen. Walking off the bottom goes
    /// back to following the newest message, which is where someone reading a
    /// live conversation wants to be.
    fn step(&mut self, view: &[String], delta: isize) {
        if view.is_empty() {
            self.selected = None;
            return;
        }
        let last = view.len() - 1;
        let at = match self.selected.as_ref() {
            None if delta >= 0 => return,
            // Leaving the bottom starts from the newest message rather than
            // from wherever the last selection happened to be.
            None => last,
            Some(id) => match view.iter().position(|held| held == id) {
                Some(at) => at,
                None => last,
            },
        };
        let moved = at as isize + delta;
        self.selected = if moved > last as isize {
            None
        } else {
            Some(view[moved.max(0) as usize].clone())
        };
    }
}

/// Who a message posted from the dashboard is from. A person has no session to
/// speak from, so they are named by the account running muxloom.
fn human_voice() -> TalkVoice {
    let name = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "human".into());
    TalkVoice {
        session_id: None,
        label: Some(name),
        kind: None,
        human: true,
    }
}

/// One row of the settings form. `values` in [`SettingsForm`] aligns with the
/// `Field` rows in row order and `notes` with the `Note` rows; the selection
/// walks fields and actions alike, so a one-shot operation reads as just
/// another line of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    Section(&'static str),
    Field(&'static str),
    /// Read-only text the app fills in when the form opens.
    Note(&'static str),
    /// Runs when the selection is on it and Enter is pressed, with the hint
    /// shown beside it.
    Action(&'static str, &'static str),
}

/// The action label the daemon row carries, matched when Enter fires.
pub const FORCE_UPDATE_ACTION: &str = "Force update";

/// The section title a runtime's settings live under.
pub const fn agent_section(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Codex => "Codex",
        AgentKind::Claude => "Claude",
        AgentKind::OpenCode => "OpenCode",
        AgentKind::Pi => "Pi",
        AgentKind::Terminal => "Terminal",
    }
}

/// The action label that installs a runtime on the machine the panel is for.
pub const fn install_action(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Codex => "Install Codex",
        AgentKind::Claude => "Install Claude",
        AgentKind::OpenCode => "Install OpenCode",
        AgentKind::Pi => "Install Pi",
        AgentKind::Terminal => "Install Terminal",
    }
}

/// Which runtime an install action names, if it is one.
pub fn install_action_kind(label: &str) -> Option<AgentKind> {
    AgentKind::agents().find(|kind| install_action(*kind) == label)
}

impl SettingsForm {
    /// The settings the dashboard edits. Everything else — tunnels, companion
    /// overrides, install commands, sync files, attention patterns, history
    /// bounds — stays in `config.toml`, where the rare edit belongs.
    ///
    /// The runtime sections are generated, so a machine that is missing one
    /// carries the action that installs it and a machine that has them all
    /// carries none.
    pub fn rows(&self) -> Vec<SettingsRow> {
        let mut rows = Vec::new();
        let host = matches!(self.scope, SettingsScope::Host(_));
        if host {
            rows.extend([
                SettingsRow::Section("Environment"),
                SettingsRow::Field("Environment (A=x B=y)"),
            ]);
        } else {
            rows.extend([
                SettingsRow::Section("General"),
                SettingsRow::Field("Refresh interval (ms)"),
                SettingsRow::Field("SSH config path"),
                SettingsRow::Section("Environment"),
                SettingsRow::Field("Environment (A=x B=y)"),
            ]);
        }
        for kind in AgentKind::agents() {
            rows.push(SettingsRow::Section(agent_section(kind)));
            rows.push(SettingsRow::Field("Command"));
            rows.push(SettingsRow::Field("Args"));
            if host && self.missing.contains(&kind) {
                rows.push(SettingsRow::Action(
                    install_action(kind),
                    "Enter: install it on this machine",
                ));
            }
        }
        rows.extend([
            SettingsRow::Section("Terminal"),
            SettingsRow::Field("Command"),
        ]);
        if host {
            rows.extend([
                SettingsRow::Section("Daemon"),
                SettingsRow::Note("Version"),
                SettingsRow::Action(FORCE_UPDATE_ACTION, "Enter: archive, hand over, resume"),
            ]);
        } else {
            rows.extend([
                SettingsRow::Section("Updates"),
                SettingsRow::Field("Update prompt (ask/auto/never)"),
                SettingsRow::Field("Update channel (auto/nightly/stable)"),
                SettingsRow::Section("Input"),
                SettingsRow::Field("Touch gestures (auto/on/off)"),
            ]);
        }
        rows
    }

    /// The editable field labels in `values` order.
    pub fn field_labels(&self) -> Vec<&'static str> {
        self.rows()
            .into_iter()
            .filter_map(|row| match row {
                SettingsRow::Field(label) => Some(label),
                _ => None,
            })
            .collect()
    }

    /// The rows the selection can land on, in display order.
    fn focusable(&self) -> Vec<SettingsRow> {
        self.rows()
            .into_iter()
            .filter(|row| matches!(row, SettingsRow::Field(_) | SettingsRow::Action(..)))
            .collect()
    }

    pub fn focus_len(&self) -> usize {
        self.focusable().len()
    }

    /// Where the selected row's text lives in `values`, or `None` when the
    /// selection is on an action — which has nothing to type into.
    pub fn selected_value(&self) -> Option<usize> {
        let mut fields = 0usize;
        for (index, row) in self.focusable().into_iter().enumerate() {
            match row {
                SettingsRow::Field(_) if index == self.selected => return Some(fields),
                SettingsRow::Field(_) => fields += 1,
                _ if index == self.selected => return None,
                _ => {}
            }
        }
        None
    }

    /// The action under the selection, when the selection is on one.
    pub fn selected_action(&self) -> Option<&'static str> {
        match self.focusable().get(self.selected) {
            Some(SettingsRow::Action(label, _)) => Some(label),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Action {
    Continue,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragDivider {
    Machines,
    Agents,
    PortraitMachines,
    PortraitTerminal,
}

/// How far a pointer may wander before its press stops being a tap. A
/// fingertip covers more than one cell and lifts with a wobble, so a one-cell
/// slip must still act on the row it landed on.
const TAP_SLOP: u16 = 1;
/// How long a press must sit still before a drag from it means "select from
/// here" instead of "scroll from here" on a touch screen, matching the
/// long-press every phone already teaches.
const LONG_PRESS: Duration = Duration::from_millis(350);
/// A single pointer report this far from the previous one is a finger, not a
/// mouse: pointing devices cross cells one at a time, touch screens deliver
/// most of a flick between two reports.
const TOUCH_JUMP_ROWS: u16 = 3;
/// Columns a swipe must cross, against at most half as many rows, to mean
/// "show me the pane beside this one" in a layout that shows one at a time.
const PANE_SWIPE_COLUMNS: u16 = 8;
/// Most scroll steps one pointer report may produce. A finger cannot cross
/// more than a screen, and a wild report must not walk a list for a second.
const MAX_SWIPE_STEPS: u16 = 32;

/// Terminals that name themselves in `TERM_PROGRAM`, all of which are windows
/// on a desktop with a pointing device in front of them.
const DESKTOP_TERM_PROGRAMS: [&str; 12] = [
    "apple_terminal",
    "ghostty",
    "hyper",
    "iterm.app",
    "kitty",
    "konsole",
    "rio",
    "tabby",
    "vscode",
    "warpterminal",
    "wezterm",
    "windowsterminal",
];
/// Terminals that identify themselves in `TERM` instead, which is the one
/// variable that survives an SSH hop.
const DESKTOP_TERMS: [&str; 7] = [
    "alacritty",
    "contour",
    "foot",
    "rio",
    "wezterm",
    "xterm-ghostty",
    "xterm-kitty",
];

/// What the terminal muxloom is drawn in says about how it is pointed at,
/// before a single report has arrived.
///
/// `Some(true)` is a terminal that can only ever be touched — Termux runs on a
/// phone and nothing else. `Some(false)` is a desktop terminal emulator, where
/// every pointer report comes from a mouse or a trackpad however jumpy it
/// looks, so the motion heuristic must not be allowed to guess otherwise.
/// `None` is everything else — an unknown `TERM` over SSH, a mobile SSH client
/// — which only the pointer's own behavior can settle.
fn terminal_touch_hint() -> Option<bool> {
    let termux = std::env::var_os("TERMUX_VERSION").is_some()
        || std::env::var_os("TERMUX_APP_PID").is_some();
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    touch_hint_from(&term, &program, termux)
}

fn touch_hint_from(term: &str, term_program: &str, termux: bool) -> Option<bool> {
    if termux {
        return Some(true);
    }
    let program = term_program.trim().to_ascii_lowercase();
    let term = term.trim().to_ascii_lowercase();
    if DESKTOP_TERM_PROGRAMS.contains(&program.as_str()) || DESKTOP_TERMS.contains(&term.as_str()) {
        return Some(false);
    }
    None
}

/// The pane a press landed in. A gesture keeps steering that pane until the
/// button lifts, even once the pointer has left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GesturePane {
    Machines,
    Agents,
    Terminal,
    FileList,
    FilePreview,
    Modal,
}

/// A press being followed to its release. A touch screen reports a swipe
/// exactly the way a mouse reports a drag, so a press only counts as a click
/// once it lifts near where it landed; anything longer scrolls the pane it
/// started in.
#[derive(Debug, Clone, Copy)]
struct PointerGesture {
    pane: GesturePane,
    origin_column: u16,
    origin_row: u16,
    /// Row the last scroll step was taken from, so every report scrolls by the
    /// rows crossed since the previous one rather than since the press.
    last_row: u16,
    pressed_at: Instant,
    /// Set once the pointer left the tap tolerance: the release selects
    /// nothing and clicks nothing.
    swiped: bool,
    /// When the pointer first left that tolerance. What makes a press a long
    /// press is how long it sat still before it moved, not how long it has been
    /// held: a finger that rests and then drags is reaching for text even
    /// though it is now moving.
    first_move_at: Option<Instant>,
    /// Set once the press became a text selection, which the selection state
    /// itself then carries.
    selecting: bool,
    /// Set once the swipe changed panes, so one long swipe moves one pane.
    switched: bool,
}

impl PointerGesture {
    fn new(pane: GesturePane, mouse: MouseEvent) -> Self {
        Self {
            pane,
            origin_column: mouse.column,
            origin_row: mouse.row,
            last_row: mouse.row,
            pressed_at: Instant::now(),
            swiped: false,
            first_move_at: None,
            selecting: false,
            switched: false,
        }
    }

    /// How long the press sat still before it started moving. A press that has
    /// not moved yet is still being held, so it counts as still now.
    fn held_still(&self) -> Duration {
        self.first_move_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.pressed_at)
    }

    /// The press position, which is what a tap acts on: the finger landed
    /// there, and the wobble it lifts with is not aim.
    fn origin(&self) -> (u16, u16) {
        (self.origin_column, self.origin_row)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Default)]
pub struct PaneLayout {
    pub machines: Option<Rect>,
    pub agents: Option<Rect>,
    pub recap: Option<Rect>,
    pub machine_divider: Option<Rect>,
    pub agents_divider: Option<Rect>,
    pub portrait_machine_divider: Option<Rect>,
    pub portrait_terminal_divider: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPoint {
    pub row: u16,
    pub column: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelection {
    pub anchor: TerminalPoint,
    pub cursor: TerminalPoint,
    dragging: bool,
}

impl TerminalSelection {
    pub fn contains(self, row: u16, column: u16) -> bool {
        if self.anchor == self.cursor {
            return false;
        }
        let (start, end) = self.normalized();
        (row, column) >= (start.row, start.column) && (row, column) <= (end.row, end.column)
    }

    fn normalized(self) -> (TerminalPoint, TerminalPoint) {
        if (self.anchor.row, self.anchor.column) <= (self.cursor.row, self.cursor.column) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// How deep the agent list indents a chain of subagents before the indentation
/// costs more panel width than the shape of the tree is worth. Deeper sessions
/// are still listed, drawn at this depth.
const MAX_SUBAGENT_DEPTH: usize = 4;

/// Where a session sits in the tree its subagents make, and what a fold under
/// it hides. One of these accompanies every row the agent list draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowShape {
    /// How many agents up the chain started this one, capped for the drawing.
    pub depth: usize,
    /// Sessions this one started, counted at every level under it.
    pub descendants: usize,
    /// Whether they are folded away rather than listed under it.
    pub folded: bool,
    /// Whether anything folded away is waiting for an answer, so that a fold
    /// never hides a prompt. False whenever nothing is folded away — what is on
    /// screen says how it is doing itself.
    pub attention: bool,
    /// Whether anything folded away is working, on the same terms.
    pub working: bool,
}

pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub state: State,
    pub state_path: PathBuf,
    pub targets: Vec<TargetStatus>,
    /// Machines this dashboard knows of but cannot reach: another controller
    /// told a daemon here that it can reach them, and the daemon repeated it.
    /// Shown so the fleet does not look smaller than it is, and never more
    /// than shown — the way there belongs to whoever is named on it.
    pub forwarded: Vec<crate::relay::RelayPeer>,
    pub sessions: Vec<AgentSession>,
    pub focus: Focus,
    pub selected_target: usize,
    /// Whether the cursor is on the moderators row, which is pinned above the
    /// machines and is not one. While it is, `selected_target` still points at
    /// this machine — a moderator runs here, so every machine-scoped action
    /// around it means this machine.
    pub moderators_selected: bool,
    /// Where muxloom keeps the folder it makes for each moderator. A local
    /// session working inside it is a moderator; that is the only marker.
    moderator_state_dir: PathBuf,
    pub selected_session_id: Option<String>,
    /// Last highlighted session for each machine. Machine navigation restores
    /// this before falling back to the first visible session.
    selected_sessions_by_target: HashMap<String, String>,
    pub history: HistoryPage,
    pub history_message: String,
    pub history_loading: bool,
    pub history_offset: usize,
    /// The scrollback offset last exchanged with the attached emulator, so
    /// `sync_terminal_scrollback` can tell an app-driven move from the drift
    /// the emulator applies itself as new output arrives.
    terminal_scrollback_pin: usize,
    pub interactive: bool,
    pub modal: Option<Modal>,
    port_forwards: PortForwardManager,
    /// The last state reported for each live forward, so `poll_port_forwards`
    /// only announces transitions instead of repeating itself every tick.
    port_forward_states: HashMap<u64, PortForwardState>,
    pub file_manager: Option<FileManagerForm>,
    /// File browsers stashed while another machine is selected, keyed by target
    /// id. The active machine's browser lives in `file_manager`; switching
    /// machines parks it here and restores the destination machine's browser.
    stashed_file_managers: HashMap<String, FileManagerForm>,
    /// Last directory browsed in the file view, keyed by session id, so
    /// reopening the browser for an agent returns to where you left off.
    file_dirs: HashMap<String, String>,
    pub status_message: String,
    /// The last message written through `set_error`, with the moment it was
    /// written. The footer colours the line while `status_message` still holds
    /// that text, and background chatter refuses to replace a recent one — an
    /// error the user has not had time to read must not be scrolled away by an
    /// auto-reconnect they never asked for.
    pub status_error: Option<(String, Instant)>,
    pub busy_operations: usize,
    pub pane_layout: PaneLayout,
    /// What every machine has been saying, and where the footer drew the chip
    /// that opens it.
    pub board: Board,
    pub board_chip: Option<Rect>,
    pub attention_banner: Option<Rect>,
    pub terminal_back: Option<Rect>,
    pub layout_debug_signature: Option<(u16, u16, u16, u16, bool, bool)>,
    pub attention_ids: Vec<String>,
    /// Prompts the user has already been shown, keyed by session id and holding
    /// the attention reason that was acknowledged. Opening a session clears its
    /// reminder; the entry is dropped once the agent stops asking, so the next
    /// prompt raises the reminder again even when it reads the same.
    attention_ack: HashMap<String, String>,
    pub machine_list_state: ListState,
    pub agent_list_state: ListState,
    pub machine_rows: Vec<(MachineRow, u16)>,
    pub agent_rows: Vec<(Option<String>, u16)>,
    pub archive_row: Option<usize>,
    pub agent_viewport_width: u16,
    pub agent_viewport_height: u16,
    pub terminal: Option<TerminalSession>,
    pub terminal_session_id: Option<String>,
    /// The text on the attached terminal's live screen, kept here so the reads
    /// that ask what the agent is doing right now stay honest while the user
    /// scrolls: the emulator answers with the rows on display, and those are
    /// the past once the view has moved up into history.
    terminal_screen: String,
    pub pending_terminal: Option<TerminalSession>,
    pub pending_terminal_session_id: Option<String>,
    pending_attach: Option<PendingAttach>,
    pub terminal_selection: Option<TerminalSelection>,
    pub animation_frame: u64,
    animation_epoch: Instant,
    worker: Worker,
    pending_scans: HashSet<String>,
    pending_activity_refreshes: HashSet<String>,
    pending_capture: Option<(String, String, usize)>,
    history_cache: HashMap<String, Vec<HistoryPage>>,
    history_cache_dir: PathBuf,
    dragging: Option<DragDivider>,
    /// The press being followed from button-down to button-up, and the pane it
    /// belongs to.
    pointer: Option<PointerGesture>,
    /// What the terminal itself says about the pointer it carries, which
    /// outranks the motion heuristic in both directions.
    touch_hint: Option<bool>,
    /// Whether this run has seen a pointer move the way only a finger moves.
    /// Consulted when `touch` is left on "auto".
    touch_detected: bool,
    /// Whether a pointer has hovered: moved with no button held. Only a mouse
    /// can do that — nothing hovers over a touch screen — so this is the proof
    /// that the jumpy reports are a mouse being moved quickly.
    pointer_hovered: bool,
    last_refresh: Instant,
    last_activity_refresh: Instant,
    last_backup_sync: Option<Instant>,
    backup_in_flight: bool,
    last_talk_sync: Option<Instant>,
    talk_in_flight: bool,
    /// The aggregated history store, beside the state file. Restoring reads it;
    /// listing a machine's lost sessions reads its index.
    backup_root: PathBuf,
    /// `(target id, session id)` of every listed session that exists only in the
    /// local backup — the machine it ran on no longer has it. These are read
    /// from the store instead of the daemon, and pushed back onto it on demand.
    recoverable: HashMap<(String, String), RecoveryInfo>,
    /// Sessions whose transcript is being transferred back to their machine.
    restoring: HashSet<(String, String)>,
    /// Sessions already put back this run. Their transcript is on the machine
    /// now, so they are no longer listed from the backup — the machine's own
    /// archived-resume path can find them, and the next sync links the record to
    /// whatever session resumed it.
    restored: HashSet<(String, String)>,
    top_up_count: u8,
    last_top_up: Option<Instant>,
    notifications: Vec<String>,
    terminal_retry_at: Option<Instant>,
    terminal_failures: u8,
    pending_terminal_started_at: Option<Instant>,
    pending_terminal_has_output: bool,
    pending_terminal_take_input: bool,
    clipboard_request: Option<String>,
    /// Set when a right-click found nothing to copy: the clipboard is the
    /// outer terminal's, so the loop that owns it reads it and hands the text
    /// back.
    clipboard_paste: bool,
    pending_install_launch: Option<PendingInstallLaunch>,
    pending_archived_resume: Option<ArchivedResume>,
    /// A successful launch is not selectable until the next daemon scan returns
    /// it. Keep the intended target across any older scan already in flight.
    pending_launch_selection: Option<(String, String, u8)>,
    last_file_click: Option<FileClick>,
    last_machine_click: Option<FileClick>,
    /// When the last freshness poll for the open preview was sent, and whether
    /// its listing is still outstanding. Both are needed so a slow link makes
    /// the monitor poll less often rather than queueing a listing per tick.
    file_monitor_sent_at: Option<Instant>,
    file_monitor_in_flight: bool,
    next_file_search_id: u64,
    update_slot: UpdateSlot,
    pub(crate) task_progress: Vec<(String, TaskKind, TaskProgress)>,
    /// A newer version staged (or available) by the background update check;
    /// shown in the header. `None` until the check reports something newer.
    /// Sessions whose current prompt has already been announced, and machines
    /// that have answered at least one scan in this run. Both outlive
    /// `sessions`, which is emptied when a machine is disabled.
    notified_attention: HashSet<String>,
    scanned_targets: HashSet<String>,
    /// Machines whose current refresh the user asked for. Background retries
    /// to a machine that already failed stay quiet — no scanning spinner, no
    /// connect progress — because the steady red mark is the message.
    user_refreshes: HashSet<String>,
    /// When each machine's daemon was last nudged toward the current
    /// generation, for the quiet retry backoff.
    daemon_refreshes: HashMap<String, Instant>,
    /// Forced-update orchestrations in flight, one per machine: archive the
    /// sessions holding the old daemon, cycle the bridge, resume the agents.
    forced_updates: HashMap<String, ForcedUpdate>,
    pub staged_update: Option<String>,
    pub available_update: Option<String>,
}

impl App {
    pub fn new(
        config: Config,
        config_path: PathBuf,
        state: State,
        state_path: PathBuf,
        targets: Vec<Target>,
        worker: Worker,
    ) -> Self {
        let state_dir = state_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        let history_cache_dir = state_dir.join("history");
        let backup_root = state_dir.join("backup");
        let statuses = targets
            .into_iter()
            .map(|target| {
                let enabled = state.enabled_hosts.contains(&target.id);
                TargetStatus::new(target, enabled)
            })
            .collect();
        Self {
            config,
            config_path,
            state,
            state_path,
            targets: statuses,
            forwarded: Vec::new(),
            sessions: Vec::new(),
            focus: Focus::Machines,
            selected_target: 0,
            moderators_selected: false,
            moderator_state_dir: state_dir.clone(),
            selected_session_id: None,
            selected_sessions_by_target: HashMap::new(),
            history: HistoryPage::default(),
            history_message: "Select an agent to load its terminal history.".into(),
            history_loading: false,
            history_offset: 0,
            terminal_scrollback_pin: 0,
            interactive: false,
            modal: None,
            port_forwards: PortForwardManager::default(),
            port_forward_states: HashMap::new(),
            file_manager: None,
            stashed_file_managers: HashMap::new(),
            file_dirs: HashMap::new(),
            status_message: "Space enables a machine; n starts an agent".into(),
            status_error: None,
            busy_operations: 0,
            pane_layout: PaneLayout::default(),
            board: Board::default(),
            board_chip: None,
            attention_banner: None,
            terminal_back: None,
            layout_debug_signature: None,
            attention_ids: Vec::new(),
            attention_ack: HashMap::new(),
            machine_list_state: ListState::default(),
            agent_list_state: ListState::default(),
            machine_rows: Vec::new(),
            agent_rows: Vec::new(),
            archive_row: None,
            agent_viewport_width: 80,
            agent_viewport_height: 20,
            terminal: None,
            terminal_session_id: None,
            terminal_screen: String::new(),
            pending_terminal: None,
            pending_terminal_session_id: None,
            pending_attach: None,
            terminal_selection: None,
            animation_frame: 0,
            animation_epoch: Instant::now(),
            worker,
            pending_scans: HashSet::new(),
            pending_activity_refreshes: HashSet::new(),
            pending_capture: None,
            history_cache: HashMap::new(),
            history_cache_dir,
            dragging: None,
            pointer: None,
            touch_hint: terminal_touch_hint(),
            touch_detected: terminal_touch_hint() == Some(true),
            pointer_hovered: false,
            last_refresh: Instant::now(),
            last_activity_refresh: Instant::now(),
            last_backup_sync: None,
            backup_in_flight: false,
            last_talk_sync: None,
            talk_in_flight: false,
            backup_root,
            recoverable: HashMap::new(),
            restoring: HashSet::new(),
            restored: HashSet::new(),
            top_up_count: 0,
            last_top_up: None,
            notifications: Vec::new(),
            terminal_retry_at: None,
            terminal_failures: 0,
            pending_terminal_started_at: None,
            pending_terminal_has_output: false,
            pending_terminal_take_input: false,
            clipboard_request: None,
            clipboard_paste: false,
            pending_install_launch: None,
            pending_archived_resume: None,
            pending_launch_selection: None,
            last_file_click: None,
            last_machine_click: None,
            file_monitor_sent_at: None,
            file_monitor_in_flight: false,
            next_file_search_id: 0,
            update_slot: Arc::new(Mutex::new(None)),
            task_progress: Vec::new(),
            notified_attention: HashSet::new(),
            scanned_targets: HashSet::new(),
            user_refreshes: HashSet::new(),
            daemon_refreshes: HashMap::new(),
            forced_updates: HashMap::new(),
            staged_update: None,
            available_update: None,
        }
    }

    pub fn start(&mut self) {
        self.ensure_target_visible();
        self.refresh_enabled();
    }

    /// A handle the startup update thread writes its result into.
    pub fn update_slot(&self) -> UpdateSlot {
        Arc::clone(&self.update_slot)
    }

    /// Pull a startup update result (if the background thread posted one) into
    /// the footer message and the header's staged-version indicator.
    fn drain_update_slot(&mut self) {
        let note = self
            .update_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(note) = note {
            if let Some(message) = note.message {
                self.status_message = message;
            }
            if note.staged_version.is_some() {
                self.staged_update = note.staged_version;
            }
            if note.available_version.is_some() {
                self.available_update = note.available_version;
            }
            // The prompt waits its turn rather than replacing whatever form
            // the user already has open.
            if let Some(prompt) = note.prompt
                && self.modal.is_none()
            {
                self.modal = Some(Modal::UpdatePrompt(prompt));
            }
        }
    }

    /// Do what the update prompt was told to: replace an installed bundle in
    /// place, or refresh the companion cache on a source build. Runs on a
    /// thread and reports through the update slot like the check itself.
    #[cfg(feature = "controller")]
    fn start_update_download(&mut self, prompt: UpdatePrompt) {
        let slot = self.update_slot();
        let environment = self
            .config
            .environment_for(crate::model::LOCAL_TARGET_ID)
            .unwrap_or_default();
        self.set_background_status(if prompt.can_self_update {
            format!("Downloading muxloom {}…", prompt.latest)
        } else {
            format!("Fetching {} companions to the cache…", prompt.latest)
        });
        std::thread::spawn(move || {
            let note = if prompt.can_self_update {
                // Exactly the build that was offered, not whatever is newest by
                // the time the thread runs: the user answered about this one.
                let release = crate::update::Release {
                    tag: prompt.tag,
                    version: prompt.version,
                    label: prompt.latest.clone(),
                };
                match crate::update::apply(&release, &environment, |_, _| {}) {
                    Ok(()) => UpdateNote {
                        message: Some(format!(
                            "muxloom {} downloaded — restart to apply",
                            release.label
                        )),
                        staged_version: Some(release.label),
                        available_version: None,
                        prompt: None,
                    },
                    Err(error) => UpdateNote {
                        message: Some(format!("update failed: {error:#}")),
                        staged_version: None,
                        available_version: Some(prompt.latest),
                        prompt: None,
                    },
                }
            } else {
                match crate::bridge::refresh_companion_cache(&environment) {
                    Ok(summary) => UpdateNote {
                        message: Some(summary),
                        staged_version: None,
                        available_version: None,
                        prompt: None,
                    },
                    Err(error) => UpdateNote {
                        message: Some(format!("companion fetch failed: {error:#}")),
                        staged_version: None,
                        available_version: Some(prompt.latest),
                        prompt: None,
                    },
                }
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(note);
            }
        });
    }

    /// Show `message` as a failure: it is coloured in the footer and, for a
    /// few seconds, protected from being overwritten by background chatter.
    fn set_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.status_error = Some((message.clone(), Instant::now()));
        self.status_message = message;
    }

    /// Show `message` unless it would bury a failure the user has not had time
    /// to read. For lines nobody asked for: auto-reconnects, background
    /// attaches, and other tick-driven progress.
    fn set_background_status(&mut self, message: impl Into<String>) {
        const ERROR_GRACE: Duration = Duration::from_secs(6);
        if self
            .status_error
            .as_ref()
            .is_some_and(|(text, at)| *text == self.status_message && at.elapsed() < ERROR_GRACE)
        {
            return;
        }
        self.status_message = message.into();
    }

    /// True while `status_message` still holds the last failure, so the footer
    /// can colour it differently from an ordinary progress line.
    pub fn status_is_error(&self) -> bool {
        self.status_error
            .as_ref()
            .is_some_and(|(text, _)| *text == self.status_message)
    }

    /// The running daemon version behind a machine's live bridge, when it is
    /// older than this controller build. `None` while no bridge is connected,
    /// so a machine only reads as outdated when that is actually observable.
    pub fn daemon_lag_version(&self, target_id: &str) -> Option<String> {
        let version = self.worker.bridges.daemon_version(target_id)?;
        crate::model::version_is_newer(env!("CARGO_PKG_VERSION"), &version).then_some(version)
    }

    /// Ids and running versions of every enabled machine whose daemon lags
    /// this controller build, for the footer indicator.
    pub fn outdated_daemons(&self) -> Vec<(String, String)> {
        self.targets
            .iter()
            .filter(|status| status.enabled && status.state == ConnectionState::Online)
            .filter_map(|status| {
                self.daemon_lag_version(&status.target.id)
                    .map(|version| (status.target.id.clone(), version))
            })
            .collect()
    }

    /// The machine the attached terminal lives on, while one is attached.
    fn attached_target_id(&self) -> Option<&str> {
        let session_id = self.terminal_session_id.as_deref()?;
        self.sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.target_id.as_str())
    }

    /// Quietly cycle the bridge of a machine whose daemon lags this build: the
    /// reconnect re-runs bootstrap, deploys the current companion, and the
    /// handover carries every session across on its keeper. One attempt per
    /// machine per backoff window — a handover deferred by pre-keeper sessions
    /// stays deferred until they end (or `force_daemon_update` steps in) —
    /// and never under an attached terminal, whose PTY stream the cycle would
    /// cut.
    fn maybe_refresh_daemons(&mut self) {
        const DAEMON_REFRESH_BACKOFF: Duration = Duration::from_secs(30 * 60);
        for (target_id, _) in self.outdated_daemons() {
            if Some(target_id.as_str()) == self.attached_target_id()
                || self.forced_updates.contains_key(&target_id)
            {
                continue;
            }
            if self
                .daemon_refreshes
                .get(&target_id)
                .is_some_and(|last| last.elapsed() < DAEMON_REFRESH_BACKOFF)
            {
                continue;
            }
            let Some(target) = self
                .targets
                .iter()
                .find(|status| status.target.id == target_id)
                .map(|status| status.target.clone())
            else {
                continue;
            };
            self.daemon_refreshes.insert(target_id, Instant::now());
            let _ = self.worker.requests.send(Request::RefreshDaemon { target });
        }
    }

    /// The Daemon section's action: show what the forced update would do —
    /// archive, hand over, resume — and let the user pull the trigger.
    fn force_update_machine(&mut self, target_id: &str) {
        let target_id = target_id.to_string();
        if self.forced_updates.contains_key(&target_id) {
            self.set_background_status(format!("{target_id}: forced update already running"));
            return;
        }
        if self.daemon_lag_version(&target_id).is_none() {
            self.set_background_status(format!(
                "{target_id}: daemon is already current (or not connected)"
            ));
            return;
        }
        self.propose_forced_update(&target_id);
    }

    /// Open the confirmation for a one-shot forced update: archive the
    /// sessions holding the old daemon, hand over, resume the agents.
    fn propose_forced_update(&mut self, target_id: &str) {
        let Some(target) = self.target(target_id).cloned() else {
            return;
        };
        let working: Vec<String> = self
            .sessions
            .iter()
            .filter(|session| {
                session.target_id == target_id
                    && !session.dead
                    && session.working
                    && session.kind != AgentKind::Terminal
            })
            .map(|session| session.display_label().to_string())
            .collect();
        let terminals: Vec<String> = self
            .sessions
            .iter()
            .filter(|session| {
                session.target_id == target_id
                    && !session.dead
                    && (session.kind == AgentKind::Terminal || is_temporary_session_id(&session.id))
            })
            .map(|session| session.display_label().to_string())
            .collect();
        let resumable = self
            .sessions
            .iter()
            .filter(|session| {
                session.target_id == target_id
                    && !session.dead
                    && session.kind != AgentKind::Terminal
                    && !is_temporary_session_id(&session.id)
            })
            .count();
        // A one-shot action always shows its plan before pulling the trigger.
        self.modal = Some(Modal::ConfirmForcedUpdate {
            target,
            working,
            terminals,
            resumable,
        });
    }

    /// Archive everything holding the old daemon. The acknowledgements drive
    /// the next phase.
    fn begin_forced_update(&mut self, target: Target) {
        let target_id = target.id.clone();
        if self.forced_updates.contains_key(&target_id) {
            return;
        }
        let sessions: Vec<AgentSession> = self
            .sessions
            .iter()
            .filter(|session| session.target_id == target_id && !session.dead)
            .cloned()
            .collect();
        let mut resumes = Vec::new();
        let mut pending_acks = 0usize;
        let mut terminals_archived = 0usize;
        for session in sessions {
            let request = if is_temporary_session_id(&session.id) {
                terminals_archived += 1;
                Request::Kill {
                    target: target.clone(),
                    session_id: session.id.clone(),
                }
            } else {
                if session.kind == AgentKind::Terminal {
                    terminals_archived += 1;
                } else {
                    resumes.push(PendingResume {
                        session_id: session.id.clone(),
                        kind: session.kind,
                        path: session.path.clone(),
                        label: session.label.clone(),
                    });
                }
                Request::Archive {
                    target: target.clone(),
                    session_id: session.id.clone(),
                }
            };
            if self.worker.requests.send(request).is_ok() {
                self.busy_operations += 1;
                pending_acks += 1;
            }
        }
        self.set_background_status(format!(
            "{target_id}: forcing the daemon update — archiving {pending_acks} sessions"
        ));
        let mut update = ForcedUpdate {
            target,
            phase: ForcedPhase::Archiving,
            deadline: Instant::now() + FORCED_PHASE_TIMEOUT,
            resumes,
            pending_acks,
            terminals_archived,
            escalated: false,
        };
        if update.pending_acks == 0 {
            update.phase = ForcedPhase::Cycling;
            self.daemon_refreshes
                .insert(target_id.clone(), Instant::now());
            let _ = self.worker.requests.send(Request::RefreshDaemon {
                target: update.target.clone(),
            });
        }
        self.forced_updates.insert(target_id, update);
    }

    /// One of the sessions being archived for a forced update answered.
    fn forced_update_ack(&mut self, target_id: &str) {
        let Some(update) = self.forced_updates.get_mut(target_id) else {
            return;
        };
        if update.phase != ForcedPhase::Archiving {
            return;
        }
        update.pending_acks = update.pending_acks.saturating_sub(1);
        if update.pending_acks == 0 {
            update.phase = ForcedPhase::Cycling;
            update.deadline = Instant::now() + FORCED_PHASE_TIMEOUT;
            let target = update.target.clone();
            self.daemon_refreshes
                .insert(target_id.to_string(), Instant::now());
            let _ = self.worker.requests.send(Request::RefreshDaemon { target });
        }
    }

    /// The forced cycle reached the new generation: bring the agents back
    /// from the transcripts their runtimes recorded.
    fn forced_update_resume_phase(&mut self, target_id: &str) {
        let Some(update) = self.forced_updates.get_mut(target_id) else {
            return;
        };
        update.phase = ForcedPhase::Resuming;
        update.deadline = Instant::now() + FORCED_PHASE_TIMEOUT;
        let mut paths: Vec<String> = update
            .resumes
            .iter()
            .map(|pending| pending.path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        update.pending_acks = paths.len();
        if paths.is_empty() {
            self.finish_forced_update(target_id);
            return;
        }
        let target = update.target.clone();
        for path in paths {
            // The worker scans both runtimes' histories whatever kind says.
            let _ = self.worker.requests.send(Request::ScanResumes {
                target: target.clone(),
                kind: AgentKind::Codex,
                path,
            });
        }
    }

    /// Resume candidates for one folder arrived during a forced update.
    /// Newest transcripts map onto the newest archived sessions of the same
    /// kind; consumes the event when a forced update owns it.
    fn forced_update_handle_resumes(
        &mut self,
        target_id: &str,
        path: &str,
        result: &Result<Vec<ResumeCandidate>, String>,
    ) -> bool {
        let Some(update) = self.forced_updates.get_mut(target_id) else {
            return false;
        };
        if update.phase != ForcedPhase::Resuming {
            return false;
        }
        update.pending_acks = update.pending_acks.saturating_sub(1);
        let mut candidates: Vec<ResumeCandidate> = match result {
            Ok(candidates) => candidates.clone(),
            Err(_) => Vec::new(),
        };
        let mut launches = Vec::new();
        let mut remaining = Vec::new();
        for pending in std::mem::take(&mut update.resumes) {
            if pending.path != path {
                remaining.push(pending);
                continue;
            }
            let Some(slot) = candidates
                .iter()
                .position(|candidate| candidate.kind == pending.kind)
            else {
                // No transcript to resume from; the session stays archived.
                remaining.push(pending);
                continue;
            };
            let candidate = candidates.remove(slot);
            launches.push((pending, candidate.id));
        }
        update.resumes = remaining;
        let target = update.target.clone();
        let done = update.pending_acks == 0;
        for (pending, resume_id) in launches {
            let command = self.config.command_for(&target.id, pending.kind).clone();
            let environment = self.config.environment_for(&target.id).unwrap_or_default();
            let request = LaunchRequest {
                target: target.clone(),
                kind: pending.kind,
                path: pending.path,
                label: pending.label,
                temporary: false,
                resume_id: Some(resume_id),
                initial_prompt: None,
                // A person at the dashboard is nobody's subagent.
                parent: None,
            };
            let remove_archive_session_id = self
                .state
                .remove_archive_after_resume
                .then_some(pending.session_id);
            if self
                .worker
                .requests
                .send(Request::Launch {
                    request,
                    command,
                    environment,
                    remove_archive_session_id,
                })
                .is_ok()
            {
                self.busy_operations += 1;
            }
        }
        if done {
            self.finish_forced_update(target_id);
        }
        true
    }

    fn finish_forced_update(&mut self, target_id: &str) {
        let Some(update) = self.forced_updates.remove(target_id) else {
            return;
        };
        let unresumed = update.resumes.len();
        let mut summary = format!("{target_id}: daemon updated — sessions resumed");
        if update.terminals_archived > 0 {
            summary.push_str(&format!(
                "; {} terminal(s) archived",
                update.terminals_archived
            ));
        }
        if unresumed > 0 {
            summary.push_str(&format!(
                "; {unresumed} agent(s) had no transcript and stay archived"
            ));
        }
        self.set_background_status(summary);
    }

    /// Abandon forced updates whose current phase stalled.
    fn sweep_forced_updates(&mut self) {
        let expired: Vec<String> = self
            .forced_updates
            .iter()
            .filter(|(_, update)| Instant::now() > update.deadline)
            .map(|(target_id, _)| target_id.clone())
            .collect();
        for target_id in expired {
            self.forced_updates.remove(&target_id);
            self.set_error(format!("{target_id}: forced daemon update timed out"));
        }
    }

    /// Forwards come up on a background thread, so the modal is not the only
    /// place their outcome matters: a tunnel that fails after the modal closes
    /// would otherwise leave the footer claiming it is forwarding. Report every
    /// state transition here, and keep the modal rows fresh while it is open.
    fn poll_port_forwards(&mut self) {
        let summaries = self.port_forwards.summaries();
        self.port_forward_states
            .retain(|id, _| summaries.iter().any(|summary| summary.id == *id));
        for summary in &summaries {
            let previous = self
                .port_forward_states
                .insert(summary.id, summary.state.clone());
            if previous.as_ref() == Some(&summary.state) {
                continue;
            }
            match &summary.state {
                PortForwardState::Starting => {}
                PortForwardState::Active => {
                    self.status_message = format!(
                        "Forwarding 127.0.0.1:{} to {}:{} on {}",
                        summary.local_port,
                        summary.remote_host,
                        summary.remote_port,
                        summary.target_id
                    );
                }
                PortForwardState::Error(error) => {
                    self.status_message = format!(
                        "Forward 127.0.0.1:{} to {}:{} failed: {}",
                        summary.local_port,
                        summary.remote_host,
                        summary.remote_port,
                        short_error(error)
                    );
                }
            }
        }
        if let Some(Modal::PortForward(form)) = self.modal.as_mut() {
            form.active = summaries
                .into_iter()
                .filter(|summary| summary.target_id == form.target.id)
                .collect();
            form.selected = form.selected.min(form.row_count().saturating_sub(1));
        }
    }

    pub fn on_tick(&mut self) {
        // Advance the spinner from wall-clock time, not per-iteration, so its
        // speed stays constant no matter how often the loop redraws (e.g. a
        // stream of mouse-move events must not make the animation race).
        self.animation_frame =
            (self.animation_epoch.elapsed().as_millis() / ANIMATION_FRAME_MS) as u64;
        self.drain_worker();
        self.poll_port_forwards();
        self.drain_update_slot();
        self.poll_media();
        self.poll_attach();
        self.poll_terminal();
        self.maybe_auto_submit_search();
        self.maybe_submit_file_search();
        self.maybe_monitor_open_file();
        self.maybe_search_resume_history();
        if self.last_activity_refresh.elapsed() >= ACTIVITY_REFRESH_INTERVAL {
            self.refresh_daemon_activity();
        }
        self.maybe_refresh_daemons();
        self.sweep_forced_updates();
        self.maybe_backup_sync();
        self.maybe_talk_sync();
        if !self.has_terminal_for_selected()
            && self
                .terminal_retry_at
                .is_some_and(|retry_at| Instant::now() >= retry_at)
            && self.selected_session().is_some_and(|session| !session.dead)
        {
            self.connect_terminal(false);
        }
        if self.last_refresh.elapsed()
            >= Duration::from_millis(self.config.refresh_interval_ms.max(500))
        {
            self.refresh_enabled();
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if is_copy_shortcut(key)
            && (self.copy_preview_selection() || self.copy_terminal_selection())
        {
            return Action::Continue;
        }
        if let Some(modal) = self.modal.take() {
            return self.handle_modal(key, modal);
        }
        if self.handle_pane_number_shortcut(key) {
            return Action::Continue;
        }
        if self.file_manager.is_some() {
            // Ctrl+F toggles the browser closed regardless of which pane is focused.
            if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.open_file_manager();
                return Action::Continue;
            }
            // Pane-focus shortcuts must still move focus between the browser and
            // the other panes; otherwise the browser would trap every key.
            if let Some(direction) = self.focus_direction_for_key(key) {
                self.move_focus(direction);
                return Action::Continue;
            }
            // The browser is modal only while its own pane (the agents column) is
            // focused. When another pane holds focus, fall through so it can
            // handle the key normally.
            if self.focus == Focus::Agents
                || (self.focus == Focus::Recap
                    && self
                        .file_manager
                        .as_ref()
                        .is_some_and(|form| form.preview_path.is_some()))
            {
                self.handle_file_key(key);
                return Action::Continue;
            }
        }
        if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_file_manager();
            return Action::Continue;
        }
        if let Some(direction) = self.focus_direction_for_key(key) {
            self.move_focus(direction);
            return Action::Continue;
        }
        if self.interactive {
            return self.handle_interactive_key(key);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('n') => {
                    self.open_launch();
                    return Action::Continue;
                }
                KeyCode::Char('r') => {
                    self.refresh_enabled_manual();
                    return Action::Continue;
                }
                KeyCode::Char('h') => {
                    self.toggle_hide_disabled();
                    return Action::Continue;
                }
                KeyCode::Char(',') => {
                    self.open_global_settings();
                    return Action::Continue;
                }
                KeyCode::Char('p') => {
                    self.open_search();
                    return Action::Continue;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('?') => {
                self.modal = Some(Modal::Help(HelpForm::default()));
                Action::Continue
            }
            KeyCode::Char('/') => {
                self.open_search();
                Action::Continue
            }
            KeyCode::Char('b') => {
                self.open_board();
                Action::Continue
            }
            KeyCode::Char(',') => {
                self.open_machine_settings();
                Action::Continue
            }
            KeyCode::Char('f') => {
                self.toggle_flatten();
                Action::Continue
            }
            KeyCode::Char('v') => {
                self.toggle_hide_disabled();
                Action::Continue
            }
            KeyCode::Char('r') => {
                self.refresh_enabled_manual();
                Action::Continue
            }
            KeyCode::Char('a') if self.focus == Focus::Agents => {
                self.toggle_archived();
                Action::Continue
            }
            KeyCode::Char('t') if self.focus == Focus::Agents => {
                self.open_temporary_agent();
                Action::Continue
            }
            KeyCode::Char('p') if self.focus == Focus::Agents => {
                self.open_port_forward();
                Action::Continue
            }
            KeyCode::Char('n') => {
                self.open_launch();
                Action::Continue
            }
            KeyCode::Char('x') if self.focus == Focus::Agents => {
                self.open_kill_confirmation();
                Action::Continue
            }
            KeyCode::Char('e') if self.focus == Focus::Agents => {
                self.open_rename_agent();
                Action::Continue
            }
            KeyCode::Enter if matches!(self.focus, Focus::Agents | Focus::Recap) => {
                self.focus = Focus::Recap;
                self.activate_terminal();
                Action::Continue
            }
            KeyCode::Char(' ') if self.focus == Focus::Agents => {
                self.toggle_task_fold();
                Action::Continue
            }
            KeyCode::Char(' ') if self.focus == Focus::Machines => {
                if self.showing_moderators() {
                    self.status_message =
                        "Moderators are not a machine — press n to start one".into();
                } else {
                    self.toggle_target(self.selected_target);
                }
                Action::Continue
            }
            KeyCode::Up => {
                if !self.handle_top_up() {
                    self.move_selection(-1);
                }
                Action::Continue
            }
            KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::Continue
            }
            KeyCode::PageUp if self.focus == Focus::Recap => {
                self.page_history(true);
                Action::Continue
            }
            KeyCode::PageDown if self.focus == Focus::Recap => {
                self.page_history(false);
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    fn handle_pane_number_shortcut(&mut self, key: KeyEvent) -> bool {
        if !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Char('1') if !self.state.flatten => {
                self.focus = Focus::Machines;
                self.release_terminal_input("Machine pane focused");
                true
            }
            KeyCode::Char('2') => {
                self.focus = Focus::Agents;
                self.release_terminal_input("Agent pane focused");
                true
            }
            KeyCode::Char('3') => {
                self.focus = Focus::Recap;
                if self
                    .file_manager
                    .as_ref()
                    .is_some_and(|form| form.preview_path.is_some())
                {
                    self.release_terminal_input("File preview focused");
                } else {
                    self.activate_terminal();
                }
                true
            }
            _ => false,
        }
    }

    /// How far PageUp/PageDown moves through the listing. This used to read
    /// `preview_page_rows`, which is only set while a preview is on screen and
    /// is reset to 1 when it closes, so paging the list moved a single row.
    fn file_list_page(form: &FileManagerForm) -> isize {
        form.list_area
            .map_or(10, |area| area.height as isize - 1)
            .max(1)
    }

    fn handle_file_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut form) = self.file_manager.take() else {
            return false;
        };
        self.last_file_click = None;
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            self.file_manager = Some(form);
            return true;
        }
        // Plain letters type into the filter, so the browser's actions live on
        // control chords instead. They work the same in the list and in a
        // preview, and every other chord is swallowed to keep the browser modal
        // over the pane underneath it.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => self.download_selected_file(&form),
                KeyCode::Char('y') => {
                    if let Some(entry) = form.entries.get(form.selected) {
                        self.clipboard_request = Some(entry.path.clone());
                        self.status_message = format!("Copied path: {}", entry.path);
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(path) = form.preview_path.clone() {
                        self.request_preview_refresh(&mut form, path);
                        self.status_message = "Re-reading the open file".into();
                    } else {
                        self.request_file_listing(form);
                        return true;
                    }
                }
                _ => {}
            }
            self.file_manager = Some(form);
            return true;
        }

        if form.preview_path.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    Self::clear_file_preview(&mut form);
                    self.focus = Focus::Agents;
                    self.status_message = "File preview closed; terminal restored".into();
                }
                KeyCode::Up | KeyCode::Left | KeyCode::PageUp | KeyCode::Char('k') => {
                    Self::page_file_preview(&mut form, false);
                }
                KeyCode::Down
                | KeyCode::Right
                | KeyCode::PageDown
                | KeyCode::Char('j')
                | KeyCode::Char(' ') => Self::page_file_preview(&mut form, true),
                KeyCode::Home | KeyCode::Char('g') => {
                    form.preview_scroll = 0;
                    form.preview_follow_tail = false;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    form.preview_scroll = form.preview_max_scroll;
                    // Follow even a file that currently fits the pane, so jumping
                    // to the end of a log keeps showing its newest lines.
                    form.preview_follow_tail = true;
                }
                KeyCode::Char('c') => {
                    if let Some(entry) = form.entries.get(form.selected) {
                        self.clipboard_request = Some(entry.path.clone());
                        self.status_message = format!("Copied path: {}", entry.path);
                    }
                }
                KeyCode::Char('d') => self.download_selected_file(&form),
                // Large files are not watched automatically, so the reader keeps
                // an explicit way to pull the current bytes.
                KeyCode::Char('r') | KeyCode::F(5) => {
                    if let Some(path) = form.preview_path.clone() {
                        self.request_preview_refresh(&mut form, path);
                        self.status_message = "Re-reading the open file".into();
                    }
                }
                _ => {}
            }
            self.file_manager = Some(form);
            return true;
        }

        match key.code {
            KeyCode::Esc => {
                if form.query.is_empty() {
                    // Ctrl-f remembers where the user was; Esc has to as well,
                    // or closing one way loses the directory and the other does
                    // not.
                    self.remember_file_dir(&form);
                    self.status_message = "File browser closed".into();
                } else {
                    form.query.clear();
                    Self::restore_file_directory_entries(&mut form);
                    self.file_manager = Some(form);
                }
            }
            KeyCode::Up => {
                Self::move_file_selection(&mut form, -1);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::Down => {
                Self::move_file_selection(&mut form, 1);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::Home if !form.entries.is_empty() => {
                form.selected = 0;
                form.return_path = None;
                Self::clear_file_preview(&mut form);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::End if !form.entries.is_empty() => {
                form.selected = form.entries.len() - 1;
                form.return_path = None;
                Self::clear_file_preview(&mut form);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::Left => {
                let child = form.path.clone();
                let parent = parent_path(&child);
                if parent == child {
                    self.file_manager = Some(form);
                } else {
                    self.navigate_file_form(form, parent, Some(child));
                }
            }
            KeyCode::Right | KeyCode::Enter if form.query.starts_with('/') && form.searching => {
                if form.search_request_id.is_none() {
                    self.submit_file_search(&mut form);
                }
                self.file_manager = Some(form);
            }
            KeyCode::Right | KeyCode::Enter => self.open_file_entry(form),
            KeyCode::PageUp => {
                let page = Self::file_list_page(&form);
                Self::move_file_selection(&mut form, -page);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::PageDown => {
                let page = Self::file_list_page(&form);
                Self::move_file_selection(&mut form, page);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::F(5) => self.request_file_listing(form),
            KeyCode::Backspace => {
                let was_recursive = form.query.starts_with('/');
                form.query.pop();
                self.update_file_query(form, was_recursive);
            }
            KeyCode::Char(character) => {
                let was_recursive = form.query.starts_with('/');
                form.query.push(character);
                self.update_file_query(form, was_recursive);
            }
            _ => {
                self.file_manager = Some(form);
            }
        }
        true
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        // Noted wherever the pointer is, and before any pane can consume the
        // report: hovering is what settles mouse against finger, and a hover
        // over the file list proves as much as one over the terminal.
        if mouse.kind == MouseEventKind::Moved {
            self.note_pointer_hover();
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && !self
                .pane_layout
                .machines
                .is_some_and(|area| inside(area, mouse.column, mouse.row))
        {
            self.last_machine_click = None;
        }
        if self.modal.is_some() {
            return self.handle_modal_mouse(mouse);
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pointer = None;
                if self.on_divider(mouse.column, mouse.row) {
                    return Action::Continue;
                }
                // A press on a pane is only a click once it lifts nearby: the
                // same report a mouse sends to start a drag is what a finger
                // sends to start a swipe.
                if let Some(pane) = self.gesture_pane_at(mouse) {
                    self.begin_gesture(pane, mouse);
                    return Action::Continue;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.pointer.is_some() => {
                self.advance_gesture(mouse);
                return Action::Continue;
            }
            MouseEventKind::Up(MouseButton::Left) if self.pointer.is_some() => {
                self.finish_gesture(mouse);
                return Action::Continue;
            }
            _ => {}
        }
        if self.dragging.is_none() && self.handle_file_mouse(mouse) {
            return Action::Continue;
        }
        match mouse.kind {
            MouseEventKind::Down(button) => {
                if button == MouseButton::Left
                    && self
                        .attention_banner
                        .is_some_and(|area| inside(area, mouse.column, mouse.row))
                {
                    self.jump_to_attention();
                    return Action::Continue;
                }
                if button == MouseButton::Left
                    && self
                        .board_chip
                        .is_some_and(|area| inside(area, mouse.column, mouse.row))
                {
                    self.open_board();
                    return Action::Continue;
                }
                // Right-click is the clipboard button over the terminal:
                // copy what is selected, paste when nothing is. Alt-right-click
                // stays the way through to an application that wants the
                // button for itself.
                if button == MouseButton::Right
                    && !mouse.modifiers.contains(KeyModifiers::ALT)
                    && self.terminal_cell_at(mouse.column, mouse.row).is_some()
                {
                    self.right_click_terminal();
                    return Action::Continue;
                }
                if button == MouseButton::Left {
                    self.terminal_selection = None;
                }
                if self.forward_terminal_mouse(mouse) {
                    return Action::Continue;
                }
                if button == MouseButton::Left {
                    self.click_pane(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Drag(button) => {
                if button == MouseButton::Left && self.dragging.is_some() {
                    self.drag_divider(mouse.column, mouse.row);
                } else {
                    self.forward_terminal_mouse(mouse);
                }
            }
            MouseEventKind::Up(button) => {
                if button == MouseButton::Left && self.dragging.take().is_some() {
                    self.sync_terminal_size();
                    self.persist_state();
                } else {
                    self.forward_terminal_mouse(mouse);
                }
            }
            // An application that asked for mouse reporting owns the wheel, the
            // way it does in any terminal emulator; otherwise the wheel moves
            // Muxloom's scrollback. PageUp reaches the history either way.
            MouseEventKind::ScrollUp => {
                if !self.forward_terminal_mouse(mouse) {
                    self.scroll_at(mouse.column, mouse.row, true);
                }
            }
            MouseEventKind::ScrollDown => {
                if !self.forward_terminal_mouse(mouse) {
                    self.scroll_at(mouse.column, mouse.row, false);
                }
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                self.forward_terminal_mouse(mouse);
            }
            MouseEventKind::Moved => {
                self.forward_terminal_mouse(mouse);
            }
        }
        Action::Continue
    }

    /// Mouse input while a modal is up. Clicks must never reach the panes
    /// behind the overlay, so every event ends here.
    fn handle_modal_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.last_machine_click = None;
                self.pointer = Some(PointerGesture::new(GesturePane::Modal, mouse));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (steps, up) = self.track_swipe(mouse);
                for _ in 0..steps {
                    self.scroll_modal(up);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(gesture) = self.pointer.take()
                    && gesture.pane == GesturePane::Modal
                    && !gesture.swiped
                {
                    let (column, row) = gesture.origin();
                    self.click_modal(column, row);
                }
            }
            MouseEventKind::ScrollUp => self.scroll_modal(true),
            MouseEventKind::ScrollDown => self.scroll_modal(false),
            _ => {}
        }
        Action::Continue
    }

    /// One wheel notch, or one row of a swipe, inside the open modal.
    fn scroll_modal(&mut self, up: bool) {
        let delta = if up { -1 } else { 1 };
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        match modal {
            Modal::Help(form) => {
                form.offset = if up {
                    form.offset.saturating_sub(1)
                } else {
                    form.offset.saturating_add(1).min(HELP_CONTENT_ROWS - 1)
                };
            }
            Modal::Settings(form) => {
                form.selected = clamped_index(form.selected, form.values.len(), delta)
            }
            Modal::Search(form) => {
                if form.results.is_empty() {
                    return;
                }
                form.selected = if up {
                    form.selected.saturating_sub(1)
                } else {
                    (form.selected + 1).min(form.results.len() - 1)
                };
            }
            Modal::PathPicker(form) => {
                if form.loading {
                    return;
                }
                form.selected =
                    clamped_index(form.selected, matched_directories(form).len(), delta);
            }
            Modal::Resume(form) => {
                if form.history_active() {
                    form.history_selected =
                        clamped_index(form.history_selected, form.history_hits.len(), delta);
                } else if !form.loading {
                    form.selected = clamped_index(form.selected, form.candidates.len() + 1, delta);
                }
            }
            Modal::PortForward(form) => {
                form.selected = clamped_index(form.selected, form.row_count(), delta)
            }
            Modal::Board(_) => {
                let Some(Modal::Board(mut form)) = self.modal.take() else {
                    return;
                };
                let view: Vec<String> = self
                    .board_view(form.tab, &form.query)
                    .into_iter()
                    .map(|message| message.id.clone())
                    .collect();
                form.step(&view, delta);
                self.modal = Some(Modal::Board(form));
            }
            _ => {}
        }
    }

    /// A tap inside the open modal.
    fn click_modal(&mut self, column: u16, row: u16) {
        if let Some(Modal::Search(form)) = self.modal.as_mut()
            && let Some((index, _)) = form
                .result_rows
                .iter()
                .find(|(_, area)| inside(*area, column, row))
        {
            form.selected = *index;
        }
        if let Some(Modal::Board(form)) = self.modal.as_mut()
            && let Some((id, _)) = form
                .rows
                .iter()
                .find(|(_, area)| inside(*area, column, row))
        {
            form.selected = Some(id.clone());
        }
    }

    /// Which pane a press belongs to, in the order the panes overlap.
    fn gesture_pane_at(&self, mouse: MouseEvent) -> Option<GesturePane> {
        if let Some(form) = self.file_manager.as_ref() {
            if form
                .list_area
                .is_some_and(|area| inside(area, mouse.column, mouse.row))
            {
                return Some(GesturePane::FileList);
            }
            if form
                .preview_area
                .is_some_and(|area| inside(area, mouse.column, mouse.row))
            {
                return Some(GesturePane::FilePreview);
            }
        }
        // Alt+click is how a pointer reaches an application that asked for
        // mouse reporting, so it must keep landing there directly.
        if !mouse.modifiers.contains(KeyModifiers::ALT)
            && self.terminal_cell_at(mouse.column, mouse.row).is_some()
        {
            return Some(GesturePane::Terminal);
        }
        if self
            .pane_layout
            .machines
            .is_some_and(|area| inside(area, mouse.column, mouse.row))
        {
            return Some(GesturePane::Machines);
        }
        if self
            .pane_layout
            .agents
            .is_some_and(|area| inside(area, mouse.column, mouse.row))
        {
            return Some(GesturePane::Agents);
        }
        None
    }

    /// Whether a drag in the terminal pane or the file preview scrolls instead
    /// of selecting: configured on, or left on auto with a touch screen seen.
    fn touch_gestures_active(&self) -> bool {
        match self.config.touch.as_str() {
            "on" => true,
            "off" => false,
            _ => self.touch_detected,
        }
    }

    /// A pointer that moved the way only a finger moves reveals a touch
    /// screen. Say so once: it changes what a drag means in the terminal pane,
    /// and changing that silently would read as lost text selection.
    ///
    /// Motion alone is weak evidence — a mouse flicked across a trackpad
    /// reports the same jump a flick does — so it only counts where nothing
    /// better is known. A terminal that names itself as a desktop emulator, and
    /// a pointer that has hovered over the screen without a button held, both
    /// settle the question the other way for good.
    fn note_touch_pointer(&mut self) {
        if self.touch_detected
            || self.config.touch != "auto"
            || self.touch_hint == Some(false)
            || self.pointer_hovered
        {
            return;
        }
        self.touch_detected = true;
        self.status_message =
            "Touch screen detected: swipe scrolls, long-press starts a selection".into();
    }

    /// A pointer moved with no button held. Nothing hovers over a touch screen
    /// — a finger is either on the glass or off it — so this is the one report
    /// that proves a pointing device, and it takes back a touch screen the
    /// motion heuristic guessed at.
    fn note_pointer_hover(&mut self) {
        if self.pointer_hovered {
            return;
        }
        self.pointer_hovered = true;
        if self.config.touch == "auto" && self.touch_detected && self.touch_hint != Some(true) {
            self.touch_detected = false;
            self.status_message = "Mouse detected: drag selects text again".into();
        }
    }

    fn begin_gesture(&mut self, pane: GesturePane, mouse: MouseEvent) {
        let mut gesture = PointerGesture::new(pane, mouse);
        // Without touch gestures a drag still means selection, so the press
        // starts one right away and the gesture only watches for the tap.
        if !self.touch_gestures_active() {
            match pane {
                GesturePane::Terminal => {
                    gesture.selecting = self.begin_terminal_selection(mouse.column, mouse.row);
                }
                GesturePane::FilePreview => {
                    self.handle_file_mouse(mouse);
                    gesture.selecting = true;
                }
                _ => {}
            }
        }
        self.pointer = Some(gesture);
    }

    /// Fold one drag report into the live gesture: mark it a swipe once it
    /// leaves the tap tolerance, notice a finger, and return the scroll steps
    /// it owes its pane. Content follows the pointer, so a drag downward walks
    /// back toward older rows.
    fn track_swipe(&mut self, mouse: MouseEvent) -> (u16, bool) {
        let Some(gesture) = self.pointer.as_mut() else {
            return (0, false);
        };
        let jump = mouse.row.abs_diff(gesture.last_row);
        let touch_like = jump >= TOUCH_JUMP_ROWS;
        if mouse.row.abs_diff(gesture.origin_row) > TAP_SLOP
            || mouse.column.abs_diff(gesture.origin_column) > TAP_SLOP
        {
            gesture.swiped = true;
            gesture.first_move_at.get_or_insert_with(Instant::now);
        }
        let up = mouse.row > gesture.last_row;
        gesture.last_row = mouse.row;
        if touch_like {
            self.note_touch_pointer();
        }
        (jump.min(MAX_SWIPE_STEPS), up)
    }

    fn advance_gesture(&mut self, mouse: MouseEvent) {
        let Some(before) = self.pointer else {
            return;
        };
        if before.pane == GesturePane::Modal {
            self.pointer = None;
            return;
        }
        let was_touch = self.touch_detected;
        let (steps, up) = self.track_swipe(mouse);
        let revealed = self.touch_detected && !was_touch;
        let touch = self.touch_gestures_active();
        let mut selecting = before.selecting;
        // The flick that reveals the screen is also the one the user meant to
        // scroll with, so the selection it started is dropped rather than
        // copied.
        if selecting && revealed && before.held_still() < LONG_PRESS {
            self.abandon_selection(before.pane);
            selecting = false;
            if let Some(gesture) = self.pointer.as_mut() {
                gesture.selecting = false;
            }
        }
        if selecting {
            match before.pane {
                GesturePane::Terminal => self.update_terminal_selection(mouse.column, mouse.row),
                GesturePane::FilePreview => {
                    self.handle_file_mouse(mouse);
                }
                _ => {}
            }
            return;
        }
        // A press that sat still before it moved reaches for text, the way a
        // long press does on any phone. What counts is how long it rested, not
        // whether it has moved since: a finger that settles and then drags is
        // still selecting, and demanding it never move made the selection
        // unreachable for anyone whose hand is not perfectly steady.
        if touch
            && matches!(
                before.pane,
                GesturePane::Terminal | GesturePane::FilePreview
            )
            && before.held_still() >= LONG_PRESS
        {
            let (column, row) = before.origin();
            let started = if before.pane == GesturePane::Terminal {
                let started = self.begin_terminal_selection(column, row);
                if started {
                    self.update_terminal_selection(mouse.column, mouse.row);
                }
                started
            } else {
                let press = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers: mouse.modifiers,
                };
                self.handle_file_mouse(press);
                self.handle_file_mouse(mouse);
                true
            };
            if let Some(gesture) = self.pointer.as_mut() {
                gesture.selecting = started;
            }
            return;
        }
        if self.swipe_switched_pane(mouse) {
            return;
        }
        let (column, row) = before.origin();
        for _ in 0..steps {
            self.wheel_at(column, row, up);
        }
    }

    fn finish_gesture(&mut self, mouse: MouseEvent) {
        let Some(gesture) = self.pointer.take() else {
            return;
        };
        if gesture.pane == GesturePane::Modal {
            return;
        }
        if gesture.selecting {
            match gesture.pane {
                GesturePane::Terminal => {
                    self.update_terminal_selection(mouse.column, mouse.row);
                    self.finish_terminal_selection(mouse);
                }
                GesturePane::FilePreview => {
                    self.handle_file_mouse(mouse);
                }
                _ => {}
            }
            return;
        }
        if gesture.swiped {
            return;
        }
        // A tap acts where the pointer landed: the wobble it lifts with is not
        // aim.
        let (column, row) = gesture.origin();
        match gesture.pane {
            GesturePane::FileList | GesturePane::FilePreview => {
                let press = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers: mouse.modifiers,
                };
                self.handle_file_mouse(press);
                self.handle_file_mouse(MouseEvent {
                    column,
                    row,
                    ..mouse
                });
            }
            GesturePane::Terminal => self.tap_terminal(MouseEvent {
                column,
                row,
                ..mouse
            }),
            GesturePane::Machines | GesturePane::Agents => {
                self.terminal_selection = None;
                self.click_pane(column, row);
            }
            GesturePane::Modal => {}
        }
    }

    /// Undo the selection a press started, for when the pointer turns out to
    /// have been a finger.
    fn abandon_selection(&mut self, pane: GesturePane) {
        match pane {
            GesturePane::Terminal => self.terminal_selection = None,
            GesturePane::FilePreview => {
                if let Some(form) = self.file_manager.as_mut() {
                    form.preview_selection = None;
                }
            }
            _ => {}
        }
    }

    /// What a press that lifts where it landed means in the terminal pane: the
    /// application below gets the click, and if it wants no mouse, the pane
    /// takes the focus.
    fn tap_terminal(&mut self, mouse: MouseEvent) {
        self.terminal_selection = None;
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..mouse
        };
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..mouse
        };
        let forwarded = self.forward_terminal_mouse(press);
        let released = self.forward_terminal_mouse(release);
        if !forwarded && !released {
            self.click_pane(mouse.column, mouse.row);
        }
    }

    /// A sideways swipe in a layout that shows one pane at a time means "show
    /// me the pane beside this one", the way a phone moves between tabs.
    fn swipe_switched_pane(&mut self, mouse: MouseEvent) -> bool {
        let Some(gesture) = self.pointer else {
            return false;
        };
        let compact = self
            .layout_debug_signature
            .is_some_and(|(_, _, _, _, _, compact)| compact);
        if gesture.switched || !compact {
            return false;
        }
        let sideways = mouse.column.abs_diff(gesture.origin_column);
        let vertical = mouse.row.abs_diff(gesture.origin_row);
        if sideways < PANE_SWIPE_COLUMNS || sideways < vertical.saturating_mul(2) {
            return false;
        }
        // The panes follow the finger: dragging right reveals the pane to the
        // left of the one on screen.
        let direction = if mouse.column > gesture.origin_column {
            FocusDirection::Left
        } else {
            FocusDirection::Right
        };
        self.move_focus(direction);
        if let Some(gesture) = self.pointer.as_mut() {
            gesture.switched = true;
            gesture.swiped = true;
        }
        true
    }

    /// Replay the wheel at a point, so a swipe scrolls exactly what a notch
    /// there would: the file browser first, then an application that asked for
    /// mouse reporting, then Muxloom's own panes.
    fn wheel_at(&mut self, column: u16, row: u16, up: bool) {
        let event = MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        if self.handle_file_mouse(event) {
            return;
        }
        if !self.forward_terminal_mouse(event) {
            self.scroll_at(column, row, up);
        }
    }

    fn handle_file_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(form) = self.file_manager.as_ref() else {
            return false;
        };
        let in_list = form
            .list_area
            .is_some_and(|area| inside(area, mouse.column, mouse.row));
        let in_preview = form
            .preview_area
            .is_some_and(|area| inside(area, mouse.column, mouse.row));
        let preview_dragging = form
            .preview_selection
            .is_some_and(|selection| selection.dragging);
        if !in_list && !in_preview && !preview_dragging {
            return false;
        }
        let mut form = self.file_manager.take().expect("file form disappeared");
        self.focus = if in_preview || preview_dragging {
            Focus::Recap
        } else {
            Focus::Agents
        };
        if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
            self.last_file_click = None;
            // Right-click is the copy button over selected preview text, the
            // same as over the terminal; with nothing selected it still walks
            // up a directory.
            if in_preview {
                self.file_manager = Some(form);
                if self.copy_preview_selection() {
                    if let Some(form) = self.file_manager.as_mut() {
                        form.preview_selection = None;
                    }
                    return true;
                }
                form = self.file_manager.take().expect("file form disappeared");
            }
            let child = form.path.clone();
            let parent = parent_path(&child);
            if parent == child {
                self.file_manager = Some(form);
            } else {
                self.navigate_file_form(form, parent, Some(child));
            }
            return true;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp if in_preview => {
                form.preview_scroll = form.preview_scroll.saturating_sub(1);
                Self::sync_preview_follow(&mut form);
            }
            MouseEventKind::ScrollDown if in_preview => {
                form.preview_scroll = form
                    .preview_scroll
                    .saturating_add(1)
                    .min(form.preview_max_scroll);
                Self::sync_preview_follow(&mut form);
            }
            MouseEventKind::ScrollUp if in_list => Self::move_file_selection(&mut form, -1),
            MouseEventKind::ScrollDown if in_list => Self::move_file_selection(&mut form, 1),
            MouseEventKind::Down(MouseButton::Left) if in_list => {
                if let Some((index, _)) = form
                    .entry_rows
                    .iter()
                    .find(|(_, area)| inside(*area, mouse.column, mouse.row))
                {
                    let index = *index;
                    let Some(path) = form.entries.get(index).map(|entry| entry.path.clone()) else {
                        self.file_manager = Some(form);
                        return true;
                    };
                    let key = format!("entry:{path}");
                    let double_click = self.is_file_double_click(&key);
                    if form.selected != index {
                        Self::clear_file_preview(&mut form);
                    }
                    form.selected = index;
                    form.return_path = None;
                    if double_click {
                        self.last_file_click = None;
                        self.open_file_entry(form);
                        return true;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_preview => {
                let mut closed = false;
                if let Some(path) = form.preview_path.clone() {
                    let key = format!("preview:{path}");
                    if self.is_file_double_click(&key) {
                        self.last_file_click = None;
                        Self::clear_file_preview(&mut form);
                        self.focus = Focus::Agents;
                        self.status_message = "File preview closed; terminal restored".into();
                        closed = true;
                    }
                }
                if !closed && let Some(point) = Self::preview_cell(&form, mouse) {
                    form.preview_selection = Some(TerminalSelection {
                        anchor: point,
                        cursor: point,
                        dragging: true,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if preview_dragging => {
                if let Some(point) = Self::preview_cell(&form, mouse)
                    && let Some(selection) = form.preview_selection.as_mut()
                {
                    selection.cursor = point;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if preview_dragging => {
                if let Some(selection) = form.preview_selection.as_mut() {
                    selection.dragging = false;
                }
                // Held, not taken: the click that copies is the right one.
                if Self::selected_preview_text(&form).is_some() {
                    self.status_message = "Selected; right-click to copy".into();
                }
                self.file_manager = Some(form);
                return true;
            }
            _ => {}
        }
        if in_list {
            self.queue_file_preloads(&mut form);
        }
        self.file_manager = Some(form);
        true
    }

    fn is_file_double_click(&mut self, key: &str) -> bool {
        const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);
        let now = Instant::now();
        let double_click = self.last_file_click.as_ref().is_some_and(|click| {
            click.key == key && now.saturating_duration_since(click.at) <= DOUBLE_CLICK_WINDOW
        });
        self.last_file_click = Some(FileClick {
            key: key.into(),
            at: now,
        });
        double_click
    }

    /// Map a mouse position to a cell within the preview text area, clamped so a
    /// drag that leaves the pane still resolves to an edge cell.
    fn preview_cell(form: &FileManagerForm, mouse: MouseEvent) -> Option<TerminalPoint> {
        let inner = form.preview_text_area?;
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        Some(TerminalPoint {
            row: mouse.row.saturating_sub(inner.y).min(inner.height - 1),
            column: mouse.column.saturating_sub(inner.x).min(inner.width - 1),
        })
    }

    /// Extract the text covered by the preview selection from the rows currently
    /// on screen.
    fn selected_preview_text(form: &FileManagerForm) -> Option<String> {
        selection_text(&form.preview_visible, form.preview_selection?)
    }

    fn copy_preview_selection(&mut self) -> bool {
        let Some(text) = self
            .file_manager
            .as_ref()
            .and_then(Self::selected_preview_text)
        else {
            return false;
        };
        let characters = text.chars().count();
        self.clipboard_request = Some(text);
        self.status_message = format!("Copied {characters} characters to clipboard");
        true
    }

    pub fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.focus == Focus::Agents && self.file_manager.is_some() {
            let form = self.file_manager.take().expect("file manager present");
            self.upload_dropped_files(&form, &text);
            self.file_manager = Some(form);
            return;
        }
        if let Some(modal) = self.modal.as_mut() {
            let text = single_line_paste(&text);
            if text.is_empty() {
                return;
            }
            match modal {
                Modal::Launch(form) => {
                    if let Some(field) = active_text(form) {
                        field.push_str(&text);
                    }
                }
                Modal::PathPicker(form) => {
                    form.query.push_str(&text);
                    form.selected = 0;
                }
                Modal::Search(form) => {
                    form.query.push_str(&text);
                    mark_search_edited(form);
                }
                Modal::Settings(form) => {
                    if let Some(value) = form.values.get_mut(form.selected) {
                        value.push_str(&text);
                        form.error = None;
                    }
                }
                Modal::PortForward(form) if form.selected < PortForwardForm::FIELD_COUNT => {
                    port_forward_value(form).push_str(&text);
                    form.error = None;
                }
                _ => {
                    self.status_message = "Select a text field before pasting".into();
                }
            }
            return;
        }
        // Nothing below here can deliver the text, so say where it went rather
        // than swallow a paste the user watched themselves make.
        if !self.interactive {
            self.status_message = if self.selected_session().is_some_and(|session| session.dead) {
                "This session is archived and read-only, so there is nowhere to paste".into()
            } else {
                "Press Enter or click the terminal to take input, then paste again".into()
            };
            return;
        }
        let Some(terminal) = self.terminal.as_mut() else {
            self.status_message = "Terminal is still connecting; paste again in a moment".into();
            return;
        };
        // A paste is input like any keystroke, so return to the live tail the
        // way handle_interactive_key does: pasting into a scrolled-back view
        // otherwise lands the text somewhere off screen.
        if let Err(error) = terminal.write_paste(&text) {
            self.set_error(format!("Paste failed: {}", short_error(&error.to_string())));
        }
        self.history_offset = 0;
    }

    pub fn resize_agent_viewport(&mut self, width: u16, height: u16) {
        let width = width.max(20);
        let height = height.max(5);
        self.agent_viewport_width = width;
        self.agent_viewport_height = height;
        if self.dragging.is_none() {
            let mut resize_error = None;
            if let Some(terminal) = self.terminal.as_mut()
                && let Err(error) = terminal.resize(width, height)
            {
                resize_error = Some(error.to_string());
            }
            if let Some(terminal) = self.pending_terminal.as_mut()
                && let Err(error) = terminal.resize(width, height)
            {
                resize_error = Some(error.to_string());
            }
            if let Some(error) = resize_error {
                self.set_error(format!("Terminal resize failed: {}", short_error(&error)));
            }
        }
    }

    pub fn visible_target_indices(&self) -> Vec<usize> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, target)| !self.state.hide_disabled || target.enabled)
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether a session is one of the coordinating agents muxloom runs: a
    /// local session working inside the folder muxloom made for it.
    pub fn is_moderator_session(&self, session: &AgentSession) -> bool {
        session.target_id == LOCAL_TARGET_ID
            && crate::moderator::is_moderator_path(&self.moderator_state_dir, &session.path)
    }

    /// Whether the machine pane's current row shows moderators rather than one
    /// machine's agents. Never true in flattened mode, which has no machine
    /// pane to pin a row above.
    pub fn showing_moderators(&self) -> bool {
        self.moderators_selected && !self.state.flatten
    }

    /// Which sessions belong under the row the machine pane is on. Moderators
    /// are gathered under their own row and kept out of the machine they run
    /// on, so this machine's list stays the work rather than the coordination.
    fn belongs_to_selected_row(&self, session: &AgentSession) -> bool {
        if self.state.flatten {
            return true;
        }
        if self.showing_moderators() {
            return self.is_moderator_session(session);
        }
        let selected_target = self
            .targets
            .get(self.selected_target)
            .map(|target| target.target.id.as_str());
        selected_target == Some(session.target_id.as_str()) && !self.is_moderator_session(session)
    }

    pub fn visible_sessions(&self) -> Vec<&AgentSession> {
        self.visible_session_rows()
            .into_iter()
            .map(|(session, _)| session)
            .collect()
    }

    /// The visible sessions in the order the agent list draws them: each
    /// session followed by the ones it started, indented under it, with a
    /// folded task's subagents left out entirely.
    ///
    /// A subagent hangs off its parent rather than off its own folder, because
    /// what an agent started is part of that agent's work wherever it happens
    /// to run. A parent this view does not show — one on another machine, or
    /// archived while its subagent is still going — cannot be indented under,
    /// so the subagent stands on its own instead of disappearing with it.
    pub fn visible_session_rows(&self) -> Vec<(&AgentSession, RowShape)> {
        let mut sessions: Vec<&AgentSession> = self
            .sessions
            .iter()
            .filter(|session| {
                self.belongs_to_selected_row(session)
                    && !(session.dead && session.kind == AgentKind::Terminal)
                    && !(session.dead && is_temporary_session_id(&session.id))
                    && (!session.dead || self.state.show_archived)
            })
            .collect();
        sessions.sort_by(|left, right| {
            left.dead
                .cmp(&right.dead)
                // A Temporal Chat is the scratch pad you reach for right now,
                // not one folder among many, so it stays above them all.
                .then_with(|| {
                    is_temporary_session_id(&right.id).cmp(&is_temporary_session_id(&left.id))
                })
                .then_with(|| left.target_id.cmp(&right.target_id))
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| right.created_at.cmp(&left.created_at))
        });

        let by_id: HashMap<&str, &AgentSession> = sessions
            .iter()
            .map(|session| (session.id.as_str(), *session))
            .collect();
        let mut children: HashMap<&str, Vec<&AgentSession>> = HashMap::new();
        let mut roots: Vec<&AgentSession> = Vec::new();
        for session in &sessions {
            // An archived subagent belongs with the archive rather than
            // indented under a session that is still running, so a parent only
            // counts while the two are on the same side of that line.
            let parent = session.parent.as_deref().filter(|parent| {
                *parent != session.id
                    && by_id
                        .get(parent)
                        .is_some_and(|parent| parent.dead == session.dead)
            });
            match parent {
                Some(parent) => children.entry(parent).or_default().push(session),
                None => roots.push(session),
            }
        }
        for siblings in children.values_mut() {
            // Inside a task the folder has stopped deciding anything: these are
            // the agents one agent started, so they go newest first the way
            // everything else in this list does.
            siblings.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }

        let mut rows = Vec::with_capacity(sessions.len());
        let mut seen = HashSet::new();
        for root in &roots {
            push_session_row(
                root,
                0,
                true,
                &children,
                &self.state.folded_tasks,
                &mut rows,
                &mut seen,
            );
        }
        // Two sessions naming each other as parent belong to no root and would
        // otherwise be listed nowhere. Whatever the chain says, they are still
        // sessions somebody has to be able to reach, so they go in flat.
        for session in &sessions {
            if !seen.contains(session.id.as_str()) {
                rows.push((*session, RowShape::default()));
            }
        }
        rows
    }

    /// Whether anything in the agent list has subagents under it, which is the
    /// only time the fold key has anything to do.
    pub fn has_subagents(&self) -> bool {
        self.visible_session_rows()
            .iter()
            .any(|(_, shape)| shape.descendants > 0)
    }

    /// The task the agent list is standing in: the session at the top of the
    /// chain the selected one hangs off, and everything under it, each with
    /// how deep it sits. Empty when nothing is selected — a task nobody is
    /// standing in has nothing to show.
    ///
    /// Every session counts here, not just the visible ones. A folded subagent
    /// or one filtered out of the list is still doing the work, and the board
    /// is about what was said rather than about what is on screen.
    pub fn selected_task(&self) -> BTreeMap<String, usize> {
        let Some(selected) = self.selected_session_id.as_deref() else {
            return BTreeMap::new();
        };
        let by_id: HashMap<&str, &AgentSession> = self
            .sessions
            .iter()
            .map(|session| (session.id.as_str(), session))
            .collect();
        if !by_id.contains_key(selected) {
            return BTreeMap::new();
        }
        // Up to the top of the chain first. A parent naming a session this
        // machine has never heard of ends the walk here for the same reason it
        // does in the daemon: there is nothing to resolve it against.
        let mut root = selected;
        let mut climbed: HashSet<&str> = HashSet::from([root]);
        while let Some(parent) = by_id
            .get(root)
            .and_then(|session| session.parent.as_deref())
            .filter(|parent| by_id.contains_key(*parent) && climbed.insert(parent))
        {
            root = parent;
        }
        let mut task = BTreeMap::from([(root.to_string(), 0usize)]);
        let mut frontier = vec![(root, 0usize)];
        while let Some((id, depth)) = frontier.pop() {
            for child in self
                .sessions
                .iter()
                .filter(|session| session.parent.as_deref() == Some(id))
            {
                if task.insert(child.id.clone(), depth + 1).is_none() {
                    frontier.push((child.id.as_str(), depth + 1));
                }
            }
        }
        task
    }

    pub fn archived_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| {
                session.dead
                    && session.kind != AgentKind::Terminal
                    && !is_temporary_session_id(&session.id)
                    && self.belongs_to_selected_row(session)
            })
            .count()
    }

    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
    }

    pub fn take_clipboard_request(&mut self) -> Option<String> {
        self.clipboard_request.take()
    }

    /// Whether a right-click asked for the clipboard's contents. The clipboard
    /// belongs to the terminal muxloom is drawn in, so only the loop that owns
    /// that terminal can answer.
    pub fn take_clipboard_paste_request(&mut self) -> bool {
        std::mem::take(&mut self.clipboard_paste)
    }

    /// Hand back what the clipboard held. `None` means nothing on this machine
    /// could read it, which is worth saying: the click did nothing, and the
    /// user is owed the reason rather than left to wonder.
    pub fn deliver_clipboard_paste(&mut self, text: Option<String>) {
        match text {
            Some(text) if !text.is_empty() => {
                let characters = text.chars().count();
                self.status_message = format!("Pasted {characters} characters");
                // After the note, so a paste that fails replaces it with why.
                self.handle_paste(text);
            }
            Some(_) => self.status_message = "The clipboard is empty".into(),
            None => {
                self.status_message =
                    "No clipboard tool here; paste with the terminal's own shortcut".into();
            }
        }
    }

    /// Temper the "copied" message when nothing confirmed the copy. Without a
    /// clipboard tool the text goes out as an OSC 52 request, and a terminal
    /// that quietly drops those left the user holding a receipt for nothing.
    pub fn note_clipboard_delivery(&mut self, confirmed: bool) {
        if confirmed || self.status_message.is_empty() {
            return;
        }
        self.status_message = format!(
            "{} - asked the terminal to take it",
            self.status_message.trim_end()
        );
    }

    pub fn selected_session(&self) -> Option<&AgentSession> {
        let id = self.selected_session_id.as_deref()?;
        self.sessions.iter().find(|session| session.id == id)
    }

    pub fn recap_for(&self, session: &AgentSession) -> String {
        // A daemon reads the runtime's own transcript and falls back to the
        // same screen scrape this would do, so its answer is never the worse
        // of the two. Only a session reached some other way — a tmux pane,
        // where the recap was scraped once at scan time — is better served by
        // looking at the screen again here.
        if crate::runtime::is_daemon_session_id(&session.id)
            && let Some(recap) = session.recap.as_ref().filter(|recap| !recap.is_empty())
        {
            return recap.clone();
        }
        let source = if self.terminal_session_id.as_deref() == Some(&session.id) {
            self.terminal.as_ref().map(|_| self.terminal_screen.clone())
        } else if self.selected_session_id.as_deref() == Some(&session.id)
            && !self.history.text.is_empty()
        {
            Some(self.history.text.clone())
        } else {
            None
        };
        source
            .as_deref()
            .and_then(|output| extract_recap(session.kind, output))
            .or_else(|| session.recap.clone())
            .unwrap_or_else(|| "No recap yet".into())
    }

    /// Whether a session's prompt still deserves a reminder. The session list
    /// keeps showing `needs_attention` as the plain state it is; this is the
    /// narrower question the banner, the jump list and the toasts ask, and it
    /// goes quiet as soon as the user is looking at the prompt themselves.
    pub fn attention_pending(&self, session: &AgentSession) -> bool {
        session.needs_attention && !self.attention_acknowledged(session)
    }

    fn attention_acknowledged(&self, session: &AgentSession) -> bool {
        if self.engaged_with(&session.id) {
            return true;
        }
        self.attention_ack.get(&session.id).map(String::as_str)
            == Some(session.attention_reason.as_deref().unwrap_or_default())
    }

    /// Whether the user is typing into this session right now. Its prompt is on
    /// screen in front of them, so nothing about it needs announcing.
    fn engaged_with(&self, session_id: &str) -> bool {
        self.interactive && self.terminal_session_id.as_deref() == Some(session_id)
    }

    /// Records that the user has seen whatever the session is waiting on.
    fn acknowledge_attention(&mut self, session_id: &str) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id && session.needs_attention)
        else {
            return;
        };
        let reason = session.attention_reason.clone().unwrap_or_default();
        self.attention_ack.insert(session_id.to_string(), reason);
    }

    /// Keeps the acknowledgement map in step with the prompts that exist right
    /// now. Call this wherever attention state is refreshed.
    fn sync_attention_acks(&mut self) {
        if self.interactive
            && let Some(session_id) = self.terminal_session_id.clone()
        {
            self.acknowledge_attention(&session_id);
        }
        self.prune_attention_acks();
    }

    /// Forgets acknowledgements whose prompt is gone — the agent stopped asking,
    /// asked something else, or the session itself went away.
    fn prune_attention_acks(&mut self) {
        let live: HashMap<&str, &str> = self
            .sessions
            .iter()
            .filter(|session| session.needs_attention)
            .map(|session| {
                (
                    session.id.as_str(),
                    session.attention_reason.as_deref().unwrap_or_default(),
                )
            })
            .collect();
        self.attention_ack
            .retain(|id, reason| live.get(id.as_str()) == Some(&reason.as_str()));
    }

    pub fn attention_sessions(&self) -> Vec<&AgentSession> {
        let mut sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|session| self.attention_pending(session))
            .collect();
        sessions.sort_by(|left, right| {
            left.target_id
                .cmp(&right.target_id)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.created_at.cmp(&right.created_at))
        });
        sessions
    }

    pub fn target(&self, id: &str) -> Option<&Target> {
        self.targets
            .iter()
            .find(|status| status.target.id == id)
            .map(|status| &status.target)
    }

    fn handle_interactive_key(&mut self, key: KeyEvent) -> Action {
        // PageUp/PageDown drive muxloom's own scrollback whenever the emulator
        // has buffered lines to move through. Agents that flow their transcript
        // off the top of the screen — Claude Code and Codex both do — fill that
        // buffer, so paging shows their history. An agent painting a
        // self-contained view on the alternate screen (Codex's Ctrl+T
        // transcript) leaves it empty and pages that view itself, so forward
        // the keys to it instead.
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            && self.attached_scrollback_can_move(key.code == KeyCode::PageUp)
        {
            self.page_history(key.code == KeyCode::PageUp);
            return Action::Continue;
        }
        if self.history_offset > 0 {
            self.history_offset = 0;
        }
        if let Some(terminal) = self.terminal.as_mut()
            && let Err(error) = terminal.write_key(key)
        {
            self.status_message =
                format!("Agent input failed: {}", short_error(&error.to_string()));
        }
        Action::Continue
    }

    /// Whether muxloom's own scrollback can move in the requested direction for
    /// the attached terminal. An empty buffer means the agent owns its screen
    /// and its paging — an alternate-screen overlay keeps no scrollback — so we
    /// decline and let the key pass through to it. `older` is PageUp (toward
    /// history).
    fn attached_scrollback_can_move(&mut self, older: bool) -> bool {
        if !self.attached_terminal_for_selected() {
            // Read-only / archived history view: muxloom always owns paging.
            return true;
        }
        let Some(terminal) = self.terminal.as_mut() else {
            return false;
        };
        if terminal.max_scrollback() == 0 {
            // No buffered history: the agent is paging its own view.
            return false;
        }
        if older {
            true
        } else {
            // Only reclaim PageDown while we are actually scrolled up.
            self.history_offset > 0
        }
    }

    fn activate_terminal(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        // Opening the session is how the user answers the prompt, so stop
        // reminding them about it the moment they step in.
        self.acknowledge_attention(&session.id);
        if self.is_recoverable(&session.target_id, &session.id) {
            // Nothing on the machine to resume from yet; put the transcript back
            // first and let the restore finish the launch.
            self.restore_recoverable_session(&session);
        } else if session.dead {
            self.resume_archived_session(session);
        } else {
            self.pending_archived_resume = None;
            self.connect_terminal(true);
        }
    }

    fn resume_archived_session(&mut self, session: AgentSession) {
        if session.kind == AgentKind::Terminal {
            self.status_message = "Exited terminals are removed automatically".into();
            return;
        }
        if self
            .pending_archived_resume
            .as_ref()
            .is_some_and(|pending| pending.source_session_id == session.id)
        {
            self.status_message = format!("Finding {} history to resume...", session.kind);
            return;
        }
        let Some(target) = self.target(&session.target_id).cloned() else {
            self.set_error("Archived session machine is no longer available");
            return;
        };
        self.close_terminal();
        let launch = LaunchForm {
            target: target.clone(),
            kind: session.kind,
            path: session.path.clone(),
            label: session.label.clone(),
            temporary: false,
            field: LaunchField::Kind,
        };
        let request = Request::ScanResumes {
            target,
            kind: session.kind,
            path: session.path,
        };
        if self.worker.requests.send(request).is_err() {
            self.set_error("Resume scanner is unavailable");
            return;
        }
        debug::log(
            "resume",
            format!(
                "archived scan target={} session={} kind={} path={}",
                session.target_id, session.id, session.kind, launch.path
            ),
        );
        self.pending_archived_resume = Some(ArchivedResume {
            source_session_id: session.id,
            launch,
        });
        self.status_message = format!("Finding {} history to resume...", session.kind);
    }

    fn connect_terminal(&mut self, take_input: bool) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.dead {
            self.close_terminal();
            self.history_offset = 0;
            self.request_history();
            self.status_message = "Archived session opened read-only".into();
            return;
        }
        if !take_input
            && self
                .terminal_retry_at
                .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return;
        }
        if self.terminal_session_id.as_deref() == Some(&session.id) && self.terminal.is_some() {
            self.clear_pending_terminal();
            self.interactive = take_input;
            self.history_offset = 0;
            if take_input {
                self.focus = Focus::Recap;
                self.status_message =
                    "Agent terminal input active; Left returns to agent list".into();
            }
            return;
        }
        if self.pending_terminal_session_id.as_deref() == Some(&session.id)
            && self.pending_terminal.is_some()
        {
            self.pending_terminal_take_input |= take_input;
            self.history_offset = 0;
            if take_input {
                self.focus = Focus::Recap;
                self.status_message =
                    "Terminal is connecting; input will activate with its first frame".into();
            }
            return;
        }
        if let Some(pending) = self.pending_attach.as_mut()
            && pending.session_id == session.id
        {
            pending.take_input |= take_input;
            self.history_offset = 0;
            if take_input {
                self.focus = Focus::Recap;
                self.status_message =
                    "Terminal is connecting; input will activate with its first frame".into();
            }
            return;
        }
        let Some(target) = self.target(&session.target_id).cloned() else {
            return;
        };
        self.clear_pending_terminal();
        self.interactive = false;
        debug::log(
            "app",
            format!(
                "prepare terminal target={} session={} viewport={}x{}",
                target.id, session.id, self.agent_viewport_width, self.agent_viewport_height
            ),
        );
        // Attaching dials the bridge (or spawns ssh), which stalls for seconds on
        // a slow link. Run it on its own thread so the UI keeps redrawing and
        // stays responsive while the connection comes up.
        let (sender, outcome) = mpsc::channel();
        let bridges = self.worker.bridges.clone();
        let session_id = session.id.clone();
        let width = self.agent_viewport_width;
        let height = self.agent_viewport_height;
        let spawned = std::thread::Builder::new()
            .name("muxloom-attach".into())
            .spawn(move || {
                let terminal = if crate::runtime::is_daemon_session_id(&session_id) {
                    TerminalSession::attach_daemon(bridges, &target, &session_id, width, height)
                } else {
                    TerminalSession::attach(&target, &session_id, width, height)
                };
                let result = terminal.map_err(|error| {
                    debug::log(
                        "app",
                        format!(
                            "attach failed target={} session={session_id}: {error:#}",
                            target.id
                        ),
                    );
                    short_error(&error.to_string())
                });
                let _ = sender.send(result);
            });
        if let Err(error) = spawned {
            self.set_error(format!(
                "Attach failed: {}",
                short_error(&error.to_string())
            ));
            self.defer_terminal_retry();
            return;
        }
        self.pending_attach = Some(PendingAttach {
            session_id: session.id,
            take_input,
            outcome,
        });
        self.history_offset = 0;
        if take_input {
            self.focus = Focus::Recap;
            self.status_message =
                "Terminal is connecting; input activates with its first frame".into();
        } else {
            self.set_background_status("Switching terminal in background");
        }
    }

    /// Installs a background attach once it finishes. Results for a session the
    /// user has already navigated away from are dropped, which closes the
    /// half-built terminal instead of letting it replace the live one.
    fn poll_attach(&mut self) {
        let Some(pending) = self.pending_attach.as_ref() else {
            return;
        };
        let outcome = match pending.outcome.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("attach thread stopped".into()),
        };
        let Some(pending) = self.pending_attach.take() else {
            return;
        };
        if self.selected_session_id.as_deref() != Some(pending.session_id.as_str()) {
            debug::log(
                "app",
                format!(
                    "discarding attach for deselected session={}",
                    pending.session_id
                ),
            );
            return;
        }
        match outcome {
            Ok(terminal) => {
                self.pending_terminal = Some(terminal);
                self.pending_terminal_session_id = Some(pending.session_id);
                self.pending_terminal_started_at = Some(Instant::now());
                self.pending_terminal_has_output = false;
                self.pending_terminal_take_input = pending.take_input;
                // The layout may have changed while the attach was dialing.
                self.sync_terminal_size();
            }
            Err(error) => {
                self.set_error(format!("Attach failed: {error}"));
                self.defer_terminal_retry();
            }
        }
    }

    fn release_terminal_input(&mut self, message: &str) {
        self.interactive = false;
        self.status_message = message.into();
    }

    fn close_terminal(&mut self) {
        self.interactive = false;
        self.terminal_selection = None;
        self.terminal = None;
        self.terminal_session_id = None;
        self.terminal_screen.clear();
        self.clear_pending_terminal();
    }

    /// Reads the attached terminal's live screen into [`Self::terminal_screen`].
    fn refresh_terminal_screen(&mut self) {
        self.terminal_screen = self
            .terminal
            .as_mut()
            .map(TerminalSession::live_contents)
            .unwrap_or_default();
    }

    fn clear_pending_terminal(&mut self) {
        self.pending_terminal = None;
        self.pending_terminal_session_id = None;
        self.pending_terminal_started_at = None;
        self.pending_terminal_has_output = false;
        self.pending_terminal_take_input = false;
        // Dropping the receiver abandons any in-flight attach; its result is
        // discarded when the thread finishes.
        self.pending_attach = None;
    }

    fn has_terminal_for_selected(&self) -> bool {
        let selected = self.selected_session_id.as_deref();
        selected.is_some()
            && (self.terminal_session_id.as_deref() == selected
                || self.pending_terminal_session_id.as_deref() == selected
                || self
                    .pending_attach
                    .as_ref()
                    .map(|pending| pending.session_id.as_str())
                    == selected)
    }

    /// True when a live emulator is attached for the currently selected session,
    /// so scrolling and copying should read its rendered scrollback rather than
    /// the linearized raw output log.
    pub(crate) fn attached_terminal_for_selected(&self) -> bool {
        self.terminal.is_some()
            && self.selected_session_id.is_some()
            && self.terminal_session_id.as_deref() == self.selected_session_id.as_deref()
    }

    /// True when the selected history position is still represented by the
    /// attached emulator. Older positions are rendered from daemon history.
    /// Settle the attached emulator on the row the app asked for, and report
    /// where it ended up.
    ///
    /// The emulator anchors a scrolled-back view to its content: every line the
    /// session prints lifts its own offset by one so the same rows stay on
    /// screen. Handing it `history_offset` again on every frame would undo
    /// that, sliding the page down a line for each line of output. So write
    /// only when the app moved the view itself, and otherwise take the
    /// emulator's answer as the truth.
    pub(crate) fn sync_terminal_scrollback(&mut self) -> usize {
        if !self.attached_terminal_for_selected() {
            return self.history_offset;
        }
        let desired = self.history_offset;
        let pinned = self.terminal_scrollback_pin;
        let Some(terminal) = self.terminal.as_mut() else {
            return desired;
        };
        if desired != pinned {
            terminal.set_scrollback(desired);
        }
        let settled = terminal.scrollback();
        self.history_offset = settled;
        self.terminal_scrollback_pin = settled;
        settled
    }

    pub(crate) fn attached_history_is_buffered(&mut self) -> bool {
        if !self.attached_terminal_for_selected() {
            return false;
        }
        let offset = self.history_offset;
        self.terminal
            .as_mut()
            .is_some_and(|terminal| offset <= terminal.max_scrollback())
    }

    fn sync_terminal_size(&mut self) {
        let mut resize_error = None;
        if let Some(terminal) = self.terminal.as_mut()
            && let Err(error) = terminal.resize(
                self.agent_viewport_width.max(20),
                self.agent_viewport_height.max(5),
            )
        {
            resize_error = Some(error.to_string());
        }
        if let Some(terminal) = self.pending_terminal.as_mut()
            && let Err(error) = terminal.resize(
                self.agent_viewport_width.max(20),
                self.agent_viewport_height.max(5),
            )
        {
            resize_error = Some(error.to_string());
        }
        if let Some(error) = resize_error {
            self.set_error(format!("Terminal resize failed: {}", short_error(&error)));
        }
    }

    fn poll_terminal(&mut self) {
        let (changed, closed, codex_working_hint) = self
            .terminal
            .as_mut()
            .map(|terminal| {
                let changed = terminal.drain();
                (changed, terminal.is_closed(), terminal.codex_working_hint())
            })
            .unwrap_or((false, false, None));
        if changed && !closed {
            self.refresh_terminal_screen();
            if let Some(session_id) = self.terminal_session_id.clone() {
                let screen = std::mem::take(&mut self.terminal_screen);
                self.sync_live_agent_activity(&session_id, &screen, codex_working_hint);
                self.terminal_screen = screen;
            }
        }
        if changed
            && !closed
            && self.terminal_session_id.as_deref() == self.selected_session_id.as_deref()
        {
            self.terminal_retry_at = None;
            self.terminal_failures = 0;
        }
        if closed {
            debug::log("app", "attached terminal reported closed");
            let closed_selected =
                self.terminal_session_id.as_deref() == self.selected_session_id.as_deref();
            self.terminal = None;
            self.terminal_session_id = None;
            self.terminal_screen.clear();
            self.interactive = false;
            if closed_selected && self.pending_terminal.is_none() && self.pending_attach.is_none() {
                self.handle_selected_terminal_closed();
            }
        }

        let (pending_changed, pending_closed, pending_visible) = self
            .pending_terminal
            .as_mut()
            .map(|terminal| {
                let changed = terminal.drain();
                let closed = terminal.is_closed();
                let visible = !terminal.screen().contents().trim().is_empty();
                (changed, closed, visible)
            })
            .unwrap_or((false, false, false));
        self.pending_terminal_has_output |= pending_changed;
        if pending_closed {
            debug::log("app", "pending terminal reported closed before first frame");
            let closed_selected =
                self.pending_terminal_session_id.as_deref() == self.selected_session_id.as_deref();
            self.clear_pending_terminal();
            if closed_selected {
                self.handle_selected_terminal_closed();
            }
            return;
        }
        let pending_elapsed = self
            .pending_terminal_started_at
            .map(|started| started.elapsed())
            .unwrap_or_default();
        if self.pending_terminal_has_output
            && (pending_visible || pending_elapsed >= Duration::from_millis(120))
        {
            let terminal = self.pending_terminal.take();
            let session_id = self.pending_terminal_session_id.take();
            let take_input = self.pending_terminal_take_input;
            self.pending_terminal_started_at = None;
            self.pending_terminal_has_output = false;
            self.pending_terminal_take_input = false;
            self.terminal = terminal;
            self.terminal_session_id = session_id;
            self.refresh_terminal_screen();
            self.interactive = take_input;
            self.terminal_retry_at = None;
            self.terminal_failures = 0;
            debug::log(
                "app",
                format!(
                    "terminal first frame ready session={}",
                    self.terminal_session_id.as_deref().unwrap_or("unknown")
                ),
            );
            if take_input {
                self.status_message =
                    "Agent terminal input active; Left returns to agent list".into();
            } else {
                self.set_background_status("Live terminal connected in background");
            }
        }
    }

    fn sync_live_agent_activity(
        &mut self,
        session_id: &str,
        screen: &str,
        codex_working_hint: Option<bool>,
    ) {
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        else {
            return;
        };
        let (kind, target_id, dead) = {
            let session = &self.sessions[index];
            (session.kind, session.target_id.clone(), session.dead)
        };
        if dead || kind == AgentKind::Terminal {
            return;
        }

        let attention =
            attention_reason(kind, screen, self.config.attention_patterns_for(&target_id));
        let working = attention.is_none()
            && if kind == AgentKind::Codex {
                codex_working_hint.unwrap_or_else(|| agent_is_working(kind, screen))
            } else {
                agent_is_working(kind, screen)
            };
        let session = &mut self.sessions[index];
        let changed = session.working != working
            || session.needs_attention != attention.is_some()
            || session.attention_reason != attention;
        session.working = working;
        session.needs_attention = attention.is_some();
        session.attention_reason = attention;
        let (working, needs_attention) = (session.working, session.needs_attention);
        if changed {
            debug::log(
                "activity",
                format!(
                    "source=live-terminal target={target_id} session={session_id} kind={kind} working={working} attention={needs_attention}"
                ),
            );
        }
        self.sync_attention_acks();
    }

    fn handle_selected_terminal_closed(&mut self) {
        self.defer_terminal_retry();
        let retry_secs = self
            .terminal_retry_at
            .map(|retry_at| retry_at.saturating_duration_since(Instant::now()).as_secs() + 1)
            .unwrap_or(1);
        self.status_message = format!(
            "Terminal connection closed; retrying in about {retry_secs}s while agent keeps running"
        );
        if let Some(target_id) = self
            .selected_session()
            .map(|session| session.target_id.clone())
        {
            self.refresh_target(&target_id);
        }
        self.request_history();
    }

    fn defer_terminal_retry(&mut self) {
        self.terminal_failures = self.terminal_failures.saturating_add(1).min(5);
        let delay = 1u64 << self.terminal_failures;
        self.terminal_retry_at = Some(Instant::now() + Duration::from_secs(delay.min(30)));
    }

    fn drain_worker(&mut self) {
        while let Ok(event) = self.worker.events.try_recv() {
            self.handle_worker_event(event);
        }
    }

    pub fn visible_task_progress(&self) -> Option<(&str, &TaskProgress)> {
        self.task_progress
            .last()
            .map(|(target_id, _, progress)| (target_id.as_str(), progress))
    }

    fn set_task_progress(
        &mut self,
        target_id: String,
        operation: TaskKind,
        progress: TaskProgress,
    ) {
        self.clear_task_progress(&target_id, operation);
        self.task_progress.push((target_id, operation, progress));
    }

    fn clear_task_progress(&mut self, target_id: &str, operation: TaskKind) {
        self.task_progress
            .retain(|(active_target, active_operation, _)| {
                active_target != target_id || *active_operation != operation
            });
    }

    fn handle_worker_event(&mut self, event: Event) {
        match event {
            Event::TaskProgress {
                target_id,
                operation,
                progress,
            } => {
                // Background reconnects to a machine already marked offline
                // stay out of the footer; the red mark is the message.
                let quiet = operation == TaskKind::Connect
                    && !self.user_refreshes.contains(&target_id)
                    && self
                        .targets
                        .iter()
                        .find(|target| target.target.id == target_id)
                        .is_some_and(|target| target.consecutive_failures > 0);
                if !quiet {
                    self.set_task_progress(target_id, operation, progress);
                }
            }
            Event::Scanned { target_id, result } => {
                let scan_succeeded = result.is_ok();
                self.clear_task_progress(&target_id, TaskKind::Connect);
                self.pending_scans.remove(&target_id);
                self.user_refreshes.remove(&target_id);
                let engaged = self
                    .terminal_session_id
                    .clone()
                    .filter(|_| self.interactive);
                let mut forgot_folds = false;
                if let Some(target) = self
                    .targets
                    .iter_mut()
                    .find(|target| target.target.id == target_id)
                {
                    match result {
                        Ok((probe, sessions)) => {
                            target.state = ConnectionState::Online;
                            target.probe = probe;
                            target.error = None;
                            target.consecutive_failures = 0;
                            // Alerts follow the prompt, not this list: reading
                            // "already alerted" out of `self.sessions` made a
                            // fresh start -- or re-enabling a machine, which
                            // empties it -- ring the bell for every prompt that
                            // had been waiting there all along.
                            let here: HashSet<&str> =
                                sessions.iter().map(|session| session.id.as_str()).collect();
                            let asking: HashSet<&str> = sessions
                                .iter()
                                .filter(|session| session.needs_attention)
                                .map(|session| session.id.as_str())
                                .collect();
                            self.notified_attention.retain(|id| {
                                !here.contains(id.as_str()) || asking.contains(id.as_str())
                            });
                            let first_scan = self.scanned_targets.insert(target_id.clone());
                            for session in sessions.iter().filter(|session| session.needs_attention)
                            {
                                if !self.notified_attention.insert(session.id.clone()) {
                                    continue;
                                }
                                let reason = session
                                    .attention_reason
                                    .as_deref()
                                    .unwrap_or("input required");
                                // No toast for the session the user is already
                                // typing into, nor for prompts that were already
                                // waiting when this machine first answered.
                                if !first_scan && engaged.as_deref() != Some(session.id.as_str()) {
                                    self.notifications.push(format!(
                                        "{} / {} needs input ({reason})",
                                        session.target_id,
                                        session.display_label()
                                    ));
                                }
                                debug::log(
                                    "attention",
                                    format!(
                                        "new prompt target={} session={} reason={reason} first_scan={first_scan}",
                                        session.target_id, session.id
                                    ),
                                );
                            }
                            for session in sessions.iter().filter(|session| session.dead) {
                                let just_exited = self.sessions.iter().any(|previous| {
                                    previous.id == session.id
                                        && previous.target_id == session.target_id
                                        && !previous.dead
                                });
                                if just_exited {
                                    self.history_cache.remove(&history_cache_key(
                                        &session.target_id,
                                        &session.id,
                                    ));
                                    if self.selected_session_id.as_deref() == Some(&session.id) {
                                        self.history = HistoryPage::default();
                                    }
                                }
                            }
                            // A fold means nothing once the task it folds is
                            // gone. The machine has just said everything it
                            // has, so whatever it used to have and no longer
                            // lists will not be back, and the fold goes with
                            // it rather than sitting in the state file for ever.
                            let gone: Vec<String> = self
                                .sessions
                                .iter()
                                .filter(|session| {
                                    session.target_id == target_id
                                        && !here.contains(session.id.as_str())
                                        && self.state.folded_tasks.contains(&session.id)
                                })
                                .map(|session| session.id.clone())
                                .collect();
                            forgot_folds |= !gone.is_empty();
                            self.state
                                .folded_tasks
                                .retain(|folded| !gone.contains(folded));
                            self.sessions
                                .retain(|session| session.target_id != target_id);
                            self.sessions.extend(sessions);
                            self.apply_session_labels();
                            self.sync_attention_acks();
                        }
                        Err(error) => {
                            target.consecutive_failures =
                                target.consecutive_failures.saturating_add(1);
                            if target.state != ConnectionState::Online
                                || target.consecutive_failures >= 3
                            {
                                target.state = ConnectionState::Offline;
                            }
                            target.error = Some(format!(
                                "refresh failed {}/3: {}",
                                target.consecutive_failures.min(3),
                                short_error(&error)
                            ));
                        }
                    }
                }
                if forgot_folds {
                    self.persist_state();
                }
                if scan_succeeded {
                    // A machine that comes back without its sessions has not lost
                    // the conversations themselves: whatever the local store still
                    // holds for it is listed alongside, so the history stays
                    // readable and can be put back.
                    self.merge_recoverable_sessions(&target_id);
                    self.apply_session_labels();
                }
                let launched = self
                    .pending_launch_selection
                    .as_ref()
                    .filter(|(pending_target, _, _)| pending_target == &target_id)
                    .cloned();
                let launched_available = launched.as_ref().is_some_and(|(_, session_id, _)| {
                    self.sessions
                        .iter()
                        .any(|session| session.id == *session_id)
                });
                if let Some((_, session_id, _)) = launched.filter(|_| launched_available) {
                    if let Some(target_index) = self
                        .targets
                        .iter()
                        .position(|target| target.target.id == target_id)
                    {
                        self.set_selected_target(target_index);
                    }
                    // The point of starting an agent is to talk to it, so open
                    // the terminal and take input rather than stopping on the
                    // list beside it. Until now the session has been elsewhere:
                    // the launch form, a machine pane, an archived entry being
                    // resumed -- and every one of them left the first keystroke
                    // going somewhere other than the agent that was just asked
                    // for.
                    self.focus = Focus::Recap;
                    self.select_session(session_id);
                    // A session that is already gone is left alone: opening one
                    // of those means resuming it, and answering a launch with
                    // another launch is not what was asked for.
                    if self.selected_session().is_some_and(|session| !session.dead) {
                        self.activate_terminal();
                    }
                    self.pending_launch_selection = None;
                } else {
                    self.ensure_session_selection();
                }
                if self
                    .selected_session()
                    .is_some_and(|session| session.target_id == target_id)
                    && (!self.has_terminal_for_selected() || self.history_offset > 0)
                {
                    self.request_history();
                }
                let rescan_for_launch = if scan_succeeded {
                    match self.pending_launch_selection.as_mut() {
                        Some((pending_target, _, completed_scans))
                            if pending_target == &target_id && *completed_scans == 0 =>
                        {
                            *completed_scans = 1;
                            true
                        }
                        Some((pending_target, session_id, _)) if pending_target == &target_id => {
                            self.status_message = format!(
                                "Launched session {session_id} exited before it could be selected"
                            );
                            self.pending_launch_selection = None;
                            false
                        }
                        _ => false,
                    }
                } else {
                    false
                };
                if rescan_for_launch {
                    self.refresh_target(&target_id);
                }
            }
            Event::ActivityRefreshed { target_id, result } => {
                self.pending_activity_refreshes.remove(&target_id);
                if let Ok(sessions) = result {
                    self.apply_activity_refresh(&target_id, &sessions);
                }
            }
            Event::DaemonRefreshed { target_id, result } => {
                let forced_cycling = self
                    .forced_updates
                    .get(&target_id)
                    .is_some_and(|update| update.phase == ForcedPhase::Cycling);
                match result {
                    Ok(Some(version)) if version == env!("CARGO_PKG_VERSION") => {
                        if forced_cycling {
                            self.forced_update_resume_phase(&target_id);
                        } else {
                            self.set_background_status(format!(
                                "{target_id} daemon updated to {version} — sessions kept running"
                            ));
                        }
                    }
                    // Still lagging: an old generation kept serving because
                    // it holds pre-keeper sessions, or the companion could
                    // not be updated. The `⟳` marker stays up; `u` on the
                    // machine forces the update when the user decides to.
                    Ok(other) => {
                        if forced_cycling {
                            let still = other.unwrap_or_else(|| "unknown".into());
                            let escalate = self
                                .forced_updates
                                .get_mut(&target_id)
                                .filter(|update| !update.escalated)
                                .map(|update| {
                                    // The polite handover keeps being
                                    // deferred — a drifted client count on an
                                    // old daemon defers it forever. Every
                                    // session is already archived, so stop
                                    // the daemon outright.
                                    update.escalated = true;
                                    update.deadline = Instant::now() + FORCED_PHASE_TIMEOUT;
                                    update.target.clone()
                                });
                            if let Some(target) = escalate {
                                self.set_background_status(format!(
                                    "{target_id}: handover deferred (daemon still {still}); restarting the daemon outright"
                                ));
                                let _ = self
                                    .worker
                                    .requests
                                    .send(Request::ForceDaemonRestart { target });
                            } else {
                                self.forced_updates.remove(&target_id);
                                self.set_error(format!(
                                    "{target_id}: forced update failed — daemon still {still}"
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        if forced_cycling {
                            self.forced_updates.remove(&target_id);
                            self.set_error(format!(
                                "{target_id}: forced update failed — {}",
                                short_error(&error)
                            ));
                        } else {
                            debug::log(
                                "app",
                                format!("daemon refresh failed target={target_id}: {error}"),
                            );
                        }
                    }
                }
            }
            Event::PortsDetected { target_id, result } => {
                if let Some(Modal::PortForward(form)) = self.modal.as_mut()
                    && form.target.id == target_id
                {
                    form.loading = false;
                    match result {
                        Ok(ports) => {
                            let mut detected: std::collections::BTreeSet<_> =
                                form.detected_ports.iter().copied().collect();
                            detected.extend(ports);
                            form.detected_ports = detected.into_iter().collect();
                            form.detection_error = None;
                            if form.remote_port.trim().is_empty()
                                && let Some(port) = form.detected_ports.first()
                            {
                                form.remote_port = port.to_string();
                                form.local_port = port.to_string();
                            }
                        }
                        Err(error) => {
                            form.detection_error = Some(short_error(&error));
                        }
                    }
                }
            }
            Event::Captured {
                target_id,
                session_id,
                result,
            } => {
                if self
                    .pending_capture
                    .as_ref()
                    .is_some_and(|(pending_target, pending_id, _)| {
                        pending_target == &target_id && pending_id == &session_id
                    })
                {
                    self.pending_capture = None;
                    match result {
                        Ok(mut page) => {
                            page.text = sanitize_terminal_text(&page.text);
                            if self.selected_session_id.as_deref() == Some(&session_id)
                                && !page.rendered
                            {
                                // The daemon answered in raw log lines, so this
                                // page cannot continue the emulator's rows.
                                self.clamp_history_to_buffered_rows();
                            }
                            if self.selected_session_id.as_deref() == Some(&session_id)
                                && let Some(oldest) = page.oldest_offset()
                                && self.history_offset > oldest
                            {
                                self.history_offset = oldest;
                                self.status_message = if oldest == 0 {
                                    "This terminal has no older scrollback".into()
                                } else {
                                    format!("Reached the oldest available history ({oldest} lines)")
                                };
                            }
                            self.store_history_page(&target_id, &session_id, page);
                            if self.selected_session_id.as_deref() == Some(&session_id) {
                                self.request_history();
                            }
                        }
                        Err(error) => {
                            if self.selected_session_id.as_deref() == Some(&session_id) {
                                self.history_loading = false;
                                self.history_message =
                                    format!("History unavailable: {}", short_error(&error));
                            }
                        }
                    }
                    if self.selected_session_id.as_deref() != Some(&session_id)
                        && (self.history_offset > 0
                            || self.selected_session().is_some_and(|session| session.dead))
                    {
                        self.request_history();
                    }
                }
            }
            Event::Launched {
                target_id,
                notice,
                result,
                remove_archive_session_id,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(session_id) => {
                        let legacy_tmux = session_id.starts_with("muxloom-");
                        self.selected_sessions_by_target
                            .insert(target_id.clone(), session_id.clone());
                        self.pending_launch_selection =
                            Some((target_id.clone(), session_id.clone(), 0));
                        // Wait on the agent list rather than the sidebar the
                        // launch came from. The session is not in it yet, so
                        // focusing the terminal here would aim input at
                        // whichever agent was open before; the refresh that
                        // finds the new one steps into its terminal.
                        self.focus = Focus::Agents;
                        self.status_message = if legacy_tmux {
                            let detail = notice.unwrap_or_else(|| {
                                "muxloomd was unavailable; compatibility mode was selected".into()
                            });
                            self.notifications.push(format!(
                                "Muxloom warning: {target_id} is using legacy tmux fallback"
                            ));
                            self.modal = Some(Modal::LegacyFallback {
                                target_id: target_id.clone(),
                                detail: short_error(&detail),
                            });
                            format!(
                                "Agent launched on {target_id} using legacy tmux fallback (muxloomd unavailable)"
                            )
                        } else if let Some(notice) = notice {
                            format!("Agent launched on {target_id} with muxloomd; {notice}")
                        } else {
                            format!("Agent launched on {target_id} with muxloomd")
                        };
                        if let Some(session_id) = remove_archive_session_id {
                            self.remove_resumed_archive(&target_id, session_id);
                        }
                        self.refresh_target(&target_id);
                    }
                    Err(error) => self.set_error(format!("Launch failed: {}", short_error(&error))),
                }
            }
            Event::Installed {
                target_id,
                kind,
                result,
            } => {
                self.clear_task_progress(&target_id, TaskKind::Install);
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(message) => {
                        self.status_message = message;
                        self.refresh_target(&target_id);
                        if let Some(pending) = self.pending_install_launch.take()
                            && pending.launch.target.id == target_id
                            && pending.launch.kind == kind
                        {
                            self.submit_launch(
                                pending.launch,
                                pending.resume_id,
                                pending.initial_prompt,
                                pending.remove_archive_session_id,
                            );
                        }
                    }
                    Err(error) => {
                        self.pending_install_launch = None;
                        self.set_error(format!("Install failed: {}", short_error(&error)));
                    }
                }
            }
            Event::Killed { target_id, result } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                self.forced_update_ack(&target_id);
                match result {
                    Ok(()) => {
                        self.status_message = "Agent session closed".into();
                        if self.selected_session_id.as_deref().is_some_and(|id| {
                            self.terminal_session_id.as_deref() == Some(id)
                                || self.pending_terminal_session_id.as_deref() == Some(id)
                        }) {
                            self.close_terminal();
                        }
                        self.selected_session_id = None;
                        self.refresh_target(&target_id);
                    }
                    Err(error) => self.set_error(format!("Close failed: {}", short_error(&error))),
                }
            }
            Event::Archived {
                target_id,
                session_id,
                result,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                self.forced_update_ack(&target_id);
                match result {
                    Ok(()) => {
                        if self.selected_session_id.as_deref() == Some(&session_id) {
                            self.close_terminal();
                            self.history = HistoryPage::default();
                        }
                        // The archive stays as the user left it: archiving is
                        // how a session gets out of the way, so unfolding the
                        // whole archive to show where it went undoes that.
                        self.status_message =
                            "Agent stopped and moved to Archived; a opens it, x there removes it"
                                .into();
                        self.refresh_target(&target_id);
                    }
                    Err(error) => {
                        self.set_error(format!("Archive failed: {}", short_error(&error)));
                    }
                }
            }
            Event::ResumedArchiveRemoved {
                target_id,
                session_id,
                result,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(()) => {
                        self.sessions.retain(|session| session.id != session_id);
                        if self.selected_session_id.as_deref() == Some(&session_id) {
                            self.selected_session_id = None;
                        }
                        if self
                            .selected_sessions_by_target
                            .get(&target_id)
                            .is_some_and(|selected| selected == &session_id)
                        {
                            self.selected_sessions_by_target.remove(&target_id);
                        }
                        self.history_cache
                            .remove(&history_cache_key(&target_id, &session_id));
                        self.state.session_labels.remove(&session_id);
                        self.status_message =
                            "Agent resumed; the previous Archived entry was removed".into();
                        self.persist_state();
                        self.refresh_target(&target_id);
                    }
                    Err(error) => {
                        self.status_message = format!(
                            "Agent resumed, but the previous Archived entry was kept: {}",
                            short_error(&error)
                        );
                    }
                }
            }
            Event::Searched {
                query,
                results,
                unreachable,
            } => {
                if let Some(Modal::Search(form)) = self.modal.as_mut()
                    && form.submitted_query == query
                {
                    form.loading = false;
                    form.results = results;
                    form.result_rows.clear();
                    form.selected = 0;
                    // A machine that could not be reached is not a machine that
                    // holds no match, and saying otherwise sends the user
                    // looking for a session that is really still there.
                    let skipped = match unreachable.as_slice() {
                        [] => None,
                        [one] => Some(format!("{one} could not be searched")),
                        many => Some(format!("{} machines could not be searched", many.len())),
                    };
                    form.error = match (form.results.is_empty(), skipped) {
                        (true, Some(skipped)) => Some(format!("No matches so far; {skipped}")),
                        (true, None) => Some("No matching agent name, recap, or history".into()),
                        (false, Some(skipped)) => {
                            Some(format!("{} matches; {skipped}", form.results.len()))
                        }
                        (false, None) => None,
                    };
                }
            }
            Event::DirectoryListed {
                target_id,
                requested_path,
                result,
            } => {
                if let Some(Modal::PathPicker(form)) = self.modal.as_mut()
                    && form.launch.target.id == target_id
                    && form.path == requested_path
                {
                    form.loading = false;
                    match result {
                        Ok(DirectoryListing { path, directories }) => {
                            form.path = path;
                            form.directories = directories;
                            form.selected = 0;
                            form.error = None;
                        }
                        Err(error) => {
                            form.error = Some(short_error(&error));
                        }
                    }
                }
            }
            Event::ResumesScanned {
                target_id,
                kind,
                path,
                result,
                warning,
            } => {
                // A forced update in its resume phase owns these candidates;
                // they never reach the resume modal.
                if self.forced_update_handle_resumes(&target_id, &path, &result) {
                    return;
                }
                // One runtime's history can fail while the other's succeeds. The
                // candidates that were found are still worth showing, but an
                // empty list must not be reported as a settled "nothing here".
                let unread = warning.filter(|warning| warning.contains(kind.as_str()));
                if let Some(Modal::Resume(form)) = self.modal.as_mut()
                    && form.launch.target.id == target_id
                    && form.launch.kind == kind
                    && form.launch.path == path
                {
                    form.loading = false;
                    match &result {
                        Ok(candidates) => {
                            form.candidates = candidates.clone();
                            form.selected = 0;
                            form.error = unread.clone();
                        }
                        Err(error) => {
                            form.error = Some(short_error(error));
                        }
                    }
                }
                let pending_matches =
                    self.pending_archived_resume
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.launch.target.id == target_id
                                && pending.launch.kind == kind
                                && pending.launch.path == path
                                && self.selected_session_id.as_deref()
                                    == Some(&pending.source_session_id)
                        });
                if pending_matches {
                    let pending = self
                        .pending_archived_resume
                        .take()
                        .expect("matched pending archived resume");
                    match result {
                        Ok(candidates) => {
                            if let Some(candidate) =
                                candidates.iter().find(|candidate| candidate.kind == kind)
                            {
                                debug::log(
                                    "resume",
                                    format!(
                                        "archived match source={} resume_id={} candidates={}",
                                        pending.source_session_id,
                                        candidate.id,
                                        candidates.len()
                                    ),
                                );
                                self.modal = Some(Modal::ConfirmArchivedResume {
                                    source_session_id: pending.source_session_id,
                                    launch: pending.launch,
                                    resume_id: candidate.id.clone(),
                                    remove_archive: self.state.remove_archive_after_resume,
                                });
                            } else if let Some(unread) = unread {
                                self.request_history();
                                self.set_error(unread);
                            } else {
                                self.request_history();
                                self.status_message = format!(
                                    "No resumable {kind} history found; archived output is read-only"
                                );
                            }
                        }
                        Err(error) => {
                            self.request_history();
                            self.set_error(format!(
                                "Could not find resumable {kind} history: {}",
                                short_error(&error)
                            ));
                        }
                    }
                }
            }
            Event::FilesListed {
                target_id,
                requested_path,
                result,
            } => {
                if let Some(mut form) = self.file_manager.take() {
                    if form.target.id != target_id || form.path != requested_path {
                        debug::log(
                            "files",
                            format!(
                                "ignored stale listing target={target_id} requested={requested_path} current_target={} current_path={}",
                                form.target.id, form.path
                            ),
                        );
                        self.file_manager = Some(form);
                        return;
                    }
                    form.loading = false;
                    self.file_monitor_in_flight = false;
                    match result {
                        Ok(FileListing { path, entries, .. }) => {
                            form.directory_cache.insert(path.clone(), entries.clone());
                            form.path = path;
                            // Every listing doubles as a freshness check for the
                            // open preview, so an edit lands on screen without
                            // re-reading the file on a timer.
                            self.refresh_stale_preview(&mut form, &entries);
                            if form.query.starts_with('/') {
                                form.error = None;
                                self.file_manager = Some(form);
                                return;
                            }
                            let selected_path = form.return_path.clone().or_else(|| {
                                form.entries
                                    .get(form.selected)
                                    .map(|entry| entry.path.clone())
                            });
                            let preview_still_exists = form
                                .preview_path
                                .as_ref()
                                .is_none_or(|path| entries.iter().any(|entry| &entry.path == path));
                            if !preview_still_exists {
                                Self::clear_file_preview(&mut form);
                                if self.focus == Focus::Recap {
                                    self.focus = Focus::Agents;
                                }
                            }
                            form.entries = entries;
                            form.selected = selected_path
                                .as_ref()
                                .and_then(|selected_path| {
                                    form.entries
                                        .iter()
                                        .position(|entry| &entry.path == selected_path)
                                })
                                .unwrap_or(0);
                            form.error = None;
                            debug::log(
                                "files",
                                format!(
                                    "list completed target={target_id} path={} entries={} selected={}",
                                    form.path,
                                    form.entries.len(),
                                    form.selected
                                ),
                            );
                        }
                        Err(error) => form.error = Some(short_error(&error)),
                    }
                    self.queue_file_preloads(&mut form);
                    self.file_manager = Some(form);
                }
            }
            Event::FilesSearched {
                target_id,
                root,
                pattern,
                request_id,
                result,
            } => {
                let active_matches = self.file_manager.as_ref().is_some_and(|form| {
                    form.target.id == target_id
                        && form.search_request_id == Some(request_id)
                        && form.query.strip_prefix('/') == Some(pattern.as_str())
                });
                if active_matches {
                    let form = self.file_manager.as_mut().expect("matched file manager");
                    Self::apply_file_search_result(form, result);
                } else if let Some(form) = self.stashed_file_managers.values_mut().find(|form| {
                    form.target.id == target_id
                        && form.search_request_id == Some(request_id)
                        && form.query.strip_prefix('/') == Some(pattern.as_str())
                }) {
                    Self::apply_file_search_result(form, result);
                } else {
                    debug::log(
                        "files",
                        format!(
                            "ignored stale search target={target_id} root={root} pattern={pattern} request={request_id}"
                        ),
                    );
                }
            }
            Event::FilePreviewed {
                target_id,
                path,
                result,
            } => {
                let mut media_request = None;
                if let Some(form) = self.file_manager.as_mut()
                    && form.target.id == target_id
                    && form.preview_path.as_deref() == Some(path.as_str())
                    && form.preview_requested_path.as_deref() == Some(path.as_str())
                {
                    form.preview_loading = false;
                    form.preview_requested_path = None;
                    match result {
                        Ok(preview) => {
                            if matches!(
                                preview.kind,
                                FilePreviewKind::Image | FilePreviewKind::Video
                            ) {
                                media_request =
                                    Some((form.target.clone(), path.clone(), preview.kind));
                            }
                            Self::cache_preview(form, &path, &preview);
                            // A refresh that read back the same bytes must not
                            // throw away the highlighted text; re-rendering it is
                            // the expensive part of showing a preview.
                            if form.preview.as_ref() != Some(&preview) {
                                form.preview = Some(preview);
                                form.preview_rendered = None;
                            }
                            form.preview_error = None;
                        }
                        // A failed refresh must not blank a file the user is
                        // reading; keep what is on screen and try again on the
                        // next change. Only a first read reports the error.
                        Err(error) if form.preview.is_some() => debug::log(
                            "files",
                            format!("preview refresh failed path={path}: {error}"),
                        ),
                        Err(error) => {
                            form.preview = None;
                            form.preview_error = Some(short_error(&error));
                        }
                    }
                }
                if let Some((target, path, kind)) = media_request {
                    self.request_media_preview(target, path, kind);
                }
            }
            Event::MediaOpened {
                target_id,
                path,
                result,
            } => {
                if let Some(form) = self.file_manager.as_mut()
                    && form.target.id == target_id
                    && form.preview_path.as_deref() == Some(path.as_str())
                {
                    match result {
                        Ok(playback) => {
                            form.media_playback = Some(playback);
                            form.media_error = None;
                        }
                        Err(error) => {
                            form.media_loading = false;
                            form.media_error = Some(short_error(&error));
                        }
                    }
                }
            }
            Event::DirectoryPreloaded {
                target_id,
                path,
                result,
            } => {
                if let Some(form) = self.file_manager.as_mut()
                    && form.target.id == target_id
                {
                    form.preload_pending.remove(&path);
                    if let Ok(listing) = result {
                        form.directory_cache.insert(listing.path, listing.entries);
                    }
                }
            }
            Event::PreviewPreloaded {
                target_id,
                path,
                result,
            } => {
                if let Some(form) = self.file_manager.as_mut()
                    && form.target.id == target_id
                {
                    form.preload_pending.remove(&path);
                    if let Ok(preview) = result {
                        Self::cache_preview(form, &path, &preview);
                    }
                }
            }
            Event::FileDownloadProgress {
                remote_path,
                transferred,
                total_size,
                bytes_per_second,
            } => {
                let name = remote_path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(remote_path.as_str());
                self.set_background_status(format!(
                    "Downloading {name}  {}",
                    transfer_progress(transferred, total_size, bytes_per_second)
                ));
            }
            Event::FileUploadProgress {
                name,
                transferred,
                total_size,
                bytes_per_second,
            } => {
                self.set_background_status(format!(
                    "Uploading {name}  {}",
                    transfer_progress(transferred, total_size, bytes_per_second)
                ));
            }
            Event::FileDownloaded { result } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                self.status_message = match result {
                    Ok(path) => format!("Downloaded to {}", path.display()),
                    Err(error) => format!("Download failed: {}", short_error(&error)),
                };
            }
            Event::FilesUploaded {
                target_id,
                remote_directory,
                result,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(names) => {
                        self.status_message = match names.as_slice() {
                            [name] => format!("Uploaded {name}"),
                            names => format!("Uploaded {} files", names.len()),
                        };
                        let refresh = matches!(self.file_manager.as_ref(), Some(form)
                            if form.target.id == target_id && form.path == remote_directory);
                        if refresh && let Some(form) = self.file_manager.take() {
                            self.request_file_listing(form);
                        }
                    }
                    Err(error) => {
                        self.set_error(format!("Upload failed: {}", short_error(&error)));
                    }
                }
            }
            Event::TalkSynced {
                result,
                board,
                forwarded,
            } => {
                self.talk_in_flight = false;
                match result {
                    Ok(summary) => debug::log("talk", summary),
                    Err(error) => debug::log("talk", format!("sync failed: {error}")),
                }
                // A machine this dashboard has of its own is not forwarded,
                // even while it is disabled: the row for it is already there,
                // and it says what it is.
                self.forwarded = forwarded
                    .into_iter()
                    .filter(|peer| {
                        !self
                            .targets
                            .iter()
                            .any(|status| status.target.id == peer.id)
                    })
                    .collect();
                if let Some(page) = board {
                    self.absorb_board(page);
                }
            }
            Event::TalkPosted { result } => match *result {
                Ok(message) => {
                    // It is on the board already; showing it now rather than on
                    // the next round is what makes the overlay feel like a
                    // conversation instead of a form.
                    self.board.merge(vec![message]);
                    self.status_message = "Posted to the board".into();
                }
                Err(error) => {
                    let error = short_error(&error);
                    if let Some(Modal::Board(form)) = self.modal.as_mut() {
                        form.error = Some(error.clone());
                    }
                    self.set_error(format!("Could not post: {error}"));
                }
            },
            Event::BackupSynced { result } => {
                self.backup_in_flight = false;
                match result {
                    Ok(summary) => debug::log("backup", summary),
                    Err(error) => {
                        debug::log("backup", format!("sync failed: {error}"));
                    }
                }
            }
            Event::BackupRestored {
                target_id,
                session_id,
                result,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                let key = (target_id.clone(), session_id.clone());
                self.restoring.remove(&key);
                match result {
                    Ok(restored) => {
                        debug::log(
                            "backup",
                            format!(
                                "restored {session_id} to {target_id} at {} ({} bytes)",
                                restored.path, restored.bytes
                            ),
                        );
                        let session = self
                            .sessions
                            .iter()
                            .find(|session| {
                                session.target_id == target_id && session.id == session_id
                            })
                            .cloned();
                        // The transcript is on the machine now, so the entry stops
                        // being a backup-only ghost whatever happens next.
                        self.restored.insert(key.clone());
                        self.recoverable.remove(&key);
                        self.sessions.retain(|session| {
                            session.target_id != target_id || session.id != session_id
                        });
                        let launch = session.as_ref().and_then(|session| {
                            self.target(&target_id).cloned().map(|target| LaunchForm {
                                target,
                                kind: session.kind,
                                path: session.path.clone(),
                                label: session.label.clone(),
                                temporary: false,
                                field: LaunchField::Kind,
                            })
                        });
                        match launch {
                            Some(launch) => {
                                let kind = launch.kind;
                                self.submit_launch(launch, Some(restored.resume_id), None, None);
                                self.status_message = format!(
                                    "Restored {} of history to {target_id} - resuming {kind}...",
                                    crate::ui::format_bytes(restored.bytes)
                                );
                            }
                            None => {
                                self.status_message = format!(
                                    "Restored {} of history to {target_id}",
                                    crate::ui::format_bytes(restored.bytes)
                                );
                            }
                        }
                    }
                    Err(error) => {
                        debug::log(
                            "backup",
                            format!("restore of {session_id} to {target_id} failed: {error}"),
                        );
                        self.set_error(format!("Restore failed: {error}"));
                    }
                }
            }
        }
    }

    /// Whether a listed session exists only in the local backup, because the
    /// machine it ran on no longer knows about it.
    pub fn is_recoverable(&self, target_id: &str, session_id: &str) -> bool {
        self.recoverable
            .contains_key(&(target_id.to_string(), session_id.to_string()))
    }

    /// Whether a recoverable session came with a transcript its agent can resume
    /// from, as opposed to only the terminal output it printed.
    pub fn is_restorable(&self, target_id: &str, session_id: &str) -> bool {
        self.recovery_info(target_id, session_id)
            .is_some_and(|info| info.restorable)
    }

    fn recovery_info(&self, target_id: &str, session_id: &str) -> Option<&RecoveryInfo> {
        self.recoverable
            .get(&(target_id.to_string(), session_id.to_string()))
    }

    /// Whether a recoverable session's transcript is being pushed back to its
    /// machine right now.
    pub fn is_restoring(&self, target_id: &str, session_id: &str) -> bool {
        self.restoring
            .contains(&(target_id.to_string(), session_id.to_string()))
    }

    /// Add a machine's backed-up sessions that the machine itself no longer has
    /// to the session list. A box that was reimaged, recycled or simply had its
    /// state directory cleared comes back reporting nothing, and until now that
    /// silently emptied the list even though every conversation was still in the
    /// local store. Listing them keeps history reachable and gives the restore
    /// something to act on.
    fn merge_recoverable_sessions(&mut self, target_id: &str) {
        self.recoverable
            .retain(|(target, _), _| target != target_id);
        if !self.config.backup.enabled {
            return;
        }
        let live: HashSet<String> = self
            .sessions
            .iter()
            .filter(|session| session.target_id == target_id)
            .map(|session| session.id.clone())
            .collect();
        for record in recoverable_backup_records(&self.backup_root, target_id, &live) {
            let Ok(kind) = record.kind.parse::<AgentKind>() else {
                continue;
            };
            if self
                .restored
                .contains(&(target_id.to_string(), record.session_id.clone()))
            {
                continue;
            }
            self.recoverable.insert(
                (target_id.to_string(), record.session_id.clone()),
                RecoveryInfo {
                    machine_key: record.machine_key,
                    restorable: record.restorable,
                },
            );
            // The backup keeps both, and they are different things: the name
            // the agent gave the conversation, and the last thing it said.
            let recap = Some(record.recap).filter(|text| !text.trim().is_empty());
            let title = Some(record.title).filter(|text| !text.trim().is_empty());
            self.sessions.push(AgentSession {
                id: record.session_id,
                target_id: target_id.to_string(),
                kind,
                path: record.cwd,
                label: record.label,
                created_at: record.created_at,
                // Nothing is running, so it belongs with the archived entries:
                // read-only until it is put back on the machine.
                dead: true,
                pid: None,
                working: false,
                needs_attention: false,
                attention_reason: None,
                recap,
                title,
                // The backup mirrors a transcript, not the machine's session
                // list, so it cannot say who started this one.
                parent: None,
            });
        }
    }

    /// Show a recoverable session's history out of the local backup: the machine
    /// cannot be asked for it, and the raw capture is the same terminal stream a
    /// live daemon would have replayed.
    fn show_recoverable_history(&mut self, session: &AgentSession) {
        self.pending_capture = None;
        self.history_loading = false;
        let machine = match self.recovery_info(&session.target_id, &session.id) {
            Some(info) => info.machine_key.clone(),
            None => return,
        };
        let (capture, mut clipped) = backup_session_capture(
            &self.backup_root,
            &machine,
            &session.id,
            RECOVERED_HISTORY_BYTES,
        );
        let mut text = if capture.trim().is_empty() {
            clipped = false;
            let transcript = backup_session_transcript(&machine, &session.id, 200_000);
            if transcript.trim().is_empty() {
                String::new()
            } else {
                format!("--- backed-up conversation ---\n\n{transcript}")
            }
        } else {
            capture
        };
        if text.is_empty() {
            self.history = HistoryPage::default();
            self.history_message =
                "This session is only in the local backup, and nothing readable came with it."
                    .into();
            return;
        }
        // Only the newest rows go on the page. Every redraw re-parses whatever
        // sits in `history.text`, and a whole capture is orders of magnitude
        // more than a pane can show: a live page is bounded the same way, by
        // the daemon, so bound this one here.
        let viewport_lines = self.agent_viewport_height.max(1) as usize;
        let chunk_lines = self.config.history_chunk_lines.max(viewport_lines + 50);
        if let Some(cut) = cut_to_last_lines(&text, chunk_lines) {
            text.drain(..cut);
            clipped = true;
        }
        if clipped {
            text.insert_str(0, "--- older output stays in the local backup ---\n\n");
        }
        self.history_message.clear();
        let lines = text.lines().count();
        self.history = HistoryPage {
            text: sanitize_terminal_text(&text),
            history_size: lines,
            pane_height: 0,
            pane_width: self.agent_viewport_width as usize,
            offset_from_bottom: 0,
            rendered: false,
            more_history: false,
        };
        self.history_offset = 0;
        self.status_message = format!(
            "{} is only in the local backup - Enter restores it to {}",
            session.display_label(),
            session.target_id
        );
    }

    /// Push a recoverable session's transcript back onto its machine in the
    /// background, then resume it there. The local copy stays where it is.
    fn restore_recoverable_session(&mut self, session: &AgentSession) {
        let Some(info) = self.recovery_info(&session.target_id, &session.id) else {
            return;
        };
        let machine_key = info.machine_key.clone();
        if !info.restorable {
            self.status_message = format!(
                "Only the terminal output of {} was backed up - it can be read here, but not resumed",
                session.display_label()
            );
            return;
        }
        let Some(target) = self.target(&session.target_id).cloned() else {
            self.set_error("That machine is no longer configured");
            return;
        };
        if !self
            .targets
            .iter()
            .any(|status| status.target.id == session.target_id && status.enabled)
        {
            self.status_message = format!(
                "Enable {} before restoring history to it",
                session.target_id
            );
            return;
        }
        let key = (session.target_id.clone(), session.id.clone());
        if !self.restoring.insert(key) {
            self.status_message = "That history is already being transferred".into();
            return;
        }
        if self
            .worker
            .requests
            .send(Request::BackupRestore {
                target,
                machine_key,
                session_id: session.id.clone(),
            })
            .is_err()
        {
            self.restoring
                .remove(&(session.target_id.clone(), session.id.clone()));
            self.set_error("Restore worker is unavailable");
            return;
        }
        self.busy_operations = self.busy_operations.saturating_add(1);
        self.status_message = format!(
            "Sending {} history back to {}...",
            session.kind, session.target_id
        );
    }

    /// Enqueue a backup pass over the enabled targets when one is due and none
    /// is already running. The very first pass fires as soon as targets exist.
    fn maybe_backup_sync(&mut self) {
        if !self.config.backup.enabled || self.backup_in_flight {
            return;
        }
        let interval = Duration::from_secs(self.config.backup.interval_secs.max(10));
        let due = self
            .last_backup_sync
            .map(|at| at.elapsed() >= interval)
            .unwrap_or(true);
        if !due {
            return;
        }
        self.dispatch_backup_sync();
    }

    /// Send a backup pass to the worker now (used by the timer and the manual
    /// command). No-op when nothing is enabled yet.
    fn dispatch_backup_sync(&mut self) {
        let targets: Vec<Target> = self
            .targets
            .iter()
            .filter(|status| status.enabled)
            .map(|status| status.target.clone())
            .collect();
        if targets.is_empty() {
            return; // nothing enabled yet; retry next tick without arming the timer
        }
        self.last_backup_sync = Some(Instant::now());
        self.backup_in_flight = true;
        let _ = self.worker.requests.send(Request::BackupSync {
            targets,
            include_ansi: self.config.backup.include_ansi,
            ansi_max_bytes: self.config.backup.ansi_max_bytes,
        });
    }

    /// Carry the talk board between machines when a round is due and none is
    /// running. People and agents are waiting on each other's messages here,
    /// so this runs far more often than a backup — it moves what has been
    /// said, not what has been recorded.
    fn maybe_talk_sync(&mut self) {
        if self.talk_in_flight {
            return;
        }
        let due = self
            .last_talk_sync
            .is_none_or(|at| at.elapsed() >= TALK_SYNC_INTERVAL);
        if !due {
            return;
        }
        let targets: Vec<Target> = self
            .targets
            .iter()
            .filter(|status| status.enabled && status.target.id != LOCAL_TARGET_ID)
            .map(|status| status.target.clone())
            .collect();
        self.last_talk_sync = Some(Instant::now());
        self.talk_in_flight = true;
        // This runs even with nowhere to carry anything to: the round is also
        // when agents on this machine get the errands they cannot run
        // themselves, and an agent asking for another machine while nothing
        // polls for work is told so rather than left waiting.
        let _ = self.worker.requests.send(Request::TalkSync {
            targets,
            config: Box::new(self.config.clone()),
            board_since: self.board.cursor.clone(),
        });
    }

    /// File what the round read off the local board. Everything anyone said
    /// anywhere passes through here, whether the overlay is open or not: the
    /// unread mark is the only difference.
    fn absorb_board(&mut self, page: TalkPage) {
        self.board.cursor = page.cursor;
        let added = self.board.merge(page.messages);
        if added == 0 {
            return;
        }
        if matches!(self.modal, Some(Modal::Board(_))) {
            self.board.unread = 0;
        } else {
            self.board.unread += added;
        }
    }

    pub fn open_board(&mut self) {
        self.board.unread = 0;
        self.modal = Some(Modal::Board(BoardForm::default()));
        self.status_message =
            "Board: Tab scope  p post  r reply  Enter expand  / find  Esc close".into();
    }

    /// The messages the open tab and filter leave on screen, oldest last —
    /// which is the order they were said in, and the order a board is read in.
    pub fn board_view(&self, tab: BoardTab, query: &str) -> Vec<&TalkMessage> {
        let needle = query.trim().to_lowercase();
        // Only the Task tab needs to know who is working with whom, and
        // working it out costs a pass over the session list, so the other
        // tabs do not pay for it.
        let task = if tab == BoardTab::Task {
            self.selected_task()
        } else {
            BTreeMap::new()
        };
        self.board
            .messages
            .iter()
            .filter(|message| tab.admits(message, &task))
            .filter(|message| {
                needle.is_empty()
                    || message.text.to_lowercase().contains(&needle)
                    || message.author.voice.name().to_lowercase().contains(&needle)
                    || message
                        .author
                        .machine_label
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect()
    }

    /// Say something on the board as the person at the keyboard. It is minted
    /// by the local daemon like any other post and replicated from there, so a
    /// human's message and an agent's are the same thing on every machine that
    /// receives it.
    fn post_board(&mut self, text: String, reply_to: Option<String>, scope: TalkScope) {
        let draft = TalkDraft {
            scope,
            author: TalkAuthor {
                voice: human_voice(),
                ..TalkAuthor::default()
            },
            kind: TalkKind::Message,
            to: None,
            reply_to,
            text,
        };
        if self
            .worker
            .requests
            .send(Request::TalkPost {
                draft: Box::new(draft),
            })
            .is_err()
        {
            self.set_error("The board is unreachable from here");
        } else {
            self.status_message = "Posting to the board...".into();
        }
    }

    /// Where a message written on this tab belongs. The tab is the channel
    /// being read, so it is also the one being spoken into.
    fn board_scope(&self, tab: BoardTab) -> Result<TalkScope, String> {
        Ok(match tab {
            // Nothing on the everything view says which channel a new message
            // belongs to, and the one everybody reads is the honest default.
            BoardTab::All | BoardTab::Global => TalkScope::Global,
            // The machine is left empty on purpose: the daemon that mints the
            // message is the one that knows what this machine is called.
            BoardTab::Machine => TalkScope::Machine {
                machine: String::new(),
            },
            BoardTab::Path => TalkScope::Path {
                machine: String::new(),
                path: env::current_dir()
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(|error| {
                        format!("this directory has no name to post under: {error}")
                    })?,
            },
            // Both of these are views rather than channels a person can speak
            // into. A task's channel belongs to the agents doing the work, and
            // this tab gathers what they said across every scope they used, so
            // there is no one board a new message here would land on. Replying
            // to something still works: a reply goes where the message it
            // answers went, which for a task message is that task.
            BoardTab::Task => {
                return Err(
                    "A task is the agents doing one piece of work — reply to something \
                            they said, or message one of them directly"
                        .into(),
                );
            }
            BoardTab::Direct => {
                return Err(
                    "A direct message is between two sessions — open the session to answer it"
                        .into(),
                );
            }
        })
    }

    /// Keys inside the board overlay. Answers whether it stays open.
    fn handle_board_key(&mut self, key: KeyEvent, form: &mut BoardForm) -> bool {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let Some(mut text) = form.compose.take() {
            match key.code {
                KeyCode::Esc => {
                    form.reply_to = None;
                    form.error = None;
                }
                KeyCode::Enter => {
                    let said = text.trim().to_string();
                    if said.is_empty() {
                        form.error = Some("Nothing to say yet".into());
                        form.compose = Some(text);
                        return true;
                    }
                    // A reply belongs in the conversation it answers, wherever
                    // that was; anything else belongs to the tab being read.
                    let scope = match form
                        .reply_to
                        .as_ref()
                        .and_then(|id| self.board.messages.iter().find(|held| held.id == *id))
                        .map(|answered| Ok(answered.scope.clone()))
                        .unwrap_or_else(|| self.board_scope(form.tab))
                    {
                        Ok(scope) => scope,
                        Err(error) => {
                            form.error = Some(error);
                            form.compose = Some(text);
                            return true;
                        }
                    };
                    let reply_to = form.reply_to.take();
                    form.error = None;
                    self.post_board(said, reply_to, scope);
                }
                KeyCode::Backspace => {
                    text.pop();
                    form.compose = Some(text);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    text.clear();
                    form.compose = Some(text);
                }
                KeyCode::Char(character) if plain => {
                    text.push(character);
                    form.compose = Some(text);
                }
                _ => form.compose = Some(text),
            }
            return true;
        }
        if form.searching {
            match key.code {
                KeyCode::Esc => {
                    form.query.clear();
                    form.searching = false;
                }
                KeyCode::Enter => form.searching = false,
                KeyCode::Backspace => {
                    form.query.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.query.clear();
                }
                KeyCode::Char(character) if plain => form.query.push(character),
                _ => {}
            }
            form.selected = None;
            return true;
        }
        let view: Vec<String> = self
            .board_view(form.tab, &form.query)
            .into_iter()
            .map(|message| message.id.clone())
            .collect();
        let page = form.page.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('b') => return false,
            KeyCode::Tab | KeyCode::Right => {
                form.tab = form.tab.stepped(1);
                form.selected = None;
                form.expanded = false;
            }
            KeyCode::BackTab | KeyCode::Left => {
                form.tab = form.tab.stepped(-1);
                form.selected = None;
                form.expanded = false;
            }
            KeyCode::Up | KeyCode::Char('k') => form.step(&view, -1),
            KeyCode::Down | KeyCode::Char('j') => form.step(&view, 1),
            KeyCode::PageUp => form.step(&view, -(page as isize)),
            KeyCode::PageDown => form.step(&view, page as isize),
            KeyCode::Home | KeyCode::Char('g') => form.selected = view.first().cloned(),
            KeyCode::End | KeyCode::Char('G') => form.selected = None,
            KeyCode::Enter => form.expanded = !form.expanded,
            KeyCode::Char('/') => {
                form.searching = true;
                form.error = None;
            }
            KeyCode::Char('p') => {
                form.error = self.board_scope(form.tab).err();
                if form.error.is_none() {
                    form.reply_to = None;
                    form.compose = Some(String::new());
                }
            }
            KeyCode::Char('r') => {
                let answered = form
                    .selected
                    .clone()
                    .or_else(|| view.last().cloned())
                    .and_then(|id| self.board.messages.iter().find(|held| held.id == id));
                match answered {
                    None => form.error = Some("Nothing here to reply to".into()),
                    Some(message) if message.kind == TalkKind::Direct => {
                        form.error =
                            Some("That was said to one session — open it and answer there".into());
                    }
                    Some(message) => {
                        form.reply_to = Some(message.id.clone());
                        form.compose = Some(String::new());
                        form.error = None;
                    }
                }
            }
            _ => {}
        }
        true
    }

    fn refresh_enabled(&mut self) {
        let ids: Vec<_> = self
            .targets
            .iter()
            .filter(|target| target.enabled)
            .map(|target| target.target.id.clone())
            .collect();
        for id in ids {
            self.refresh_target(&id);
        }
        self.last_refresh = Instant::now();
    }

    /// A refresh the user asked for: unlike the background timer, it shows
    /// the scanning spinner and connect progress even for machines that are
    /// already marked offline.
    fn refresh_enabled_manual(&mut self) {
        self.user_refreshes.extend(
            self.targets
                .iter()
                .filter(|target| target.enabled)
                .map(|target| target.target.id.clone()),
        );
        self.refresh_enabled();
    }

    fn refresh_daemon_activity(&mut self) {
        let targets: Vec<_> = self
            .targets
            .iter()
            .filter(|status| {
                status.enabled
                    && status.state == ConnectionState::Online
                    && !self.pending_scans.contains(&status.target.id)
                    && !self.pending_activity_refreshes.contains(&status.target.id)
                    && self.sessions.iter().any(|session| {
                        session.target_id == status.target.id
                            && !session.dead
                            && crate::runtime::is_daemon_session_id(&session.id)
                    })
            })
            .map(|status| status.target.clone())
            .collect();
        for target in targets {
            let target_id = target.id.clone();
            if self
                .worker
                .requests
                .send(Request::RefreshActivity { target })
                .is_ok()
            {
                self.pending_activity_refreshes.insert(target_id);
            }
        }
        self.last_activity_refresh = Instant::now();
    }

    fn apply_activity_refresh(&mut self, target_id: &str, refreshed: &[AgentSession]) {
        let by_id: HashMap<_, _> = refreshed
            .iter()
            .map(|session| (session.id.as_str(), session))
            .collect();
        let mut exited = Vec::new();
        let mut notifications = Vec::new();
        let mut changed = 0usize;
        for session in self
            .sessions
            .iter_mut()
            .filter(|session| session.target_id == target_id)
        {
            let Some(latest) = by_id.get(session.id.as_str()) else {
                continue;
            };
            let was_dead = session.dead;
            let needed_attention = session.needs_attention;
            let status_changed = session.dead != latest.dead
                || session.pid != latest.pid
                || session.working != latest.working
                || session.needs_attention != latest.needs_attention
                || session.attention_reason != latest.attention_reason;
            session.dead = latest.dead;
            session.pid = latest.pid;
            session.working = latest.working;
            session.needs_attention = latest.needs_attention;
            session
                .attention_reason
                .clone_from(&latest.attention_reason);
            if latest.recap.is_some() {
                session.recap.clone_from(&latest.recap);
            }
            if latest.title.is_some() {
                session.title.clone_from(&latest.title);
            }
            if status_changed {
                changed += 1;
            }
            if !was_dead && session.dead {
                exited.push(session.id.clone());
            }
            if !needed_attention && session.needs_attention {
                notifications.push((
                    session.id.clone(),
                    session.display_label().to_string(),
                    session
                        .attention_reason
                        .clone()
                        .unwrap_or_else(|| "input required".into()),
                ));
            }
        }
        for session_id in exited {
            self.history_cache
                .remove(&history_cache_key(target_id, &session_id));
            if self.selected_session_id.as_deref() == Some(session_id.as_str()) {
                self.history = HistoryPage::default();
            }
        }
        for (session_id, label, reason) in notifications {
            // No toast for the session the user is already typing into.
            if !self.engaged_with(&session_id) {
                self.notifications
                    .push(format!("{target_id} / {label} needs input ({reason})"));
            }
            debug::log(
                "attention",
                format!("new prompt target={target_id} session={session_id} reason={reason}"),
            );
        }
        self.sync_attention_acks();
        if changed > 0 {
            debug::log(
                "activity",
                format!("refreshed target={target_id} changed_sessions={changed}"),
            );
        }
    }

    fn refresh_target(&mut self, id: &str) {
        if self.pending_scans.contains(id) {
            return;
        }
        let Some(status) = self
            .targets
            .iter_mut()
            .find(|status| status.target.id == id)
        else {
            return;
        };
        if !status.enabled {
            return;
        }
        // A machine that already failed keeps its steady offline mark through
        // background retries; the scanning spinner is for first contact and
        // for refreshes the user asked for.
        if status.state != ConnectionState::Online
            && (status.consecutive_failures == 0 || self.user_refreshes.contains(id))
        {
            status.state = ConnectionState::Scanning;
        }
        let request = ScanRequest {
            target: status.target.clone(),
            commands: AgentKind::agents()
                .map(|kind| (kind, self.config.command_for(id, kind).command.clone()))
                .collect(),
            environment: self.config.environment_for(id).unwrap_or_default(),
            attention_patterns: self.config.attention_patterns_for(id).to_vec(),
        };
        if self.worker.requests.send(Request::Scan(request)).is_ok() {
            self.pending_scans.insert(id.into());
        }
    }

    fn toggle_target(&mut self, index: usize) {
        let Some(status) = self.targets.get_mut(index) else {
            return;
        };
        status.enabled = !status.enabled;
        if status.enabled {
            status.state = ConnectionState::Scanning;
            self.state.enabled_hosts.insert(status.target.id.clone());
            let id = status.target.id.clone();
            self.persist_state();
            self.refresh_target(&id);
        } else {
            status.state = ConnectionState::Disabled;
            self.state.enabled_hosts.remove(&status.target.id);
            let id = status.target.id.clone();
            self.sessions.retain(|session| session.target_id != id);
            self.persist_state();
            self.ensure_target_visible();
            self.ensure_session_selection();
        }
    }

    fn toggle_flatten(&mut self) {
        self.state.flatten = !self.state.flatten;
        if self.state.flatten && self.focus == Focus::Machines {
            self.focus = Focus::Agents;
        }
        self.persist_state();
        self.ensure_session_selection();
    }

    fn toggle_hide_disabled(&mut self) {
        self.state.hide_disabled = !self.state.hide_disabled;
        self.ensure_target_visible();
        self.persist_state();
        self.status_message = if self.state.hide_disabled {
            "Disabled machines hidden; Ctrl-h or v shows all".into()
        } else {
            "All SSH machines visible".into()
        };
    }

    /// Fold the subagents of the highlighted task away, or bring them back.
    ///
    /// A subagent has none of its own to fold, so the key folds the task it is
    /// part of instead and leaves the cursor on the parent: pressing it on a
    /// row of the tree means "put this away" wherever in the tree that row is.
    fn toggle_task_fold(&mut self) {
        let Some(selected) = self.selected_session_id.clone() else {
            return;
        };
        let rows = self.visible_session_rows();
        let Some(position) = rows.iter().position(|(session, _)| session.id == selected) else {
            return;
        };
        let target = if rows[position].1.descendants > 0 {
            selected
        } else {
            // Walk back to the row this one is indented under.
            let depth = rows[position].1.depth;
            let parent = rows[..position]
                .iter()
                .rev()
                .find(|(_, shape)| shape.depth < depth)
                .map(|(session, _)| session.id.clone());
            match parent {
                Some(parent) => parent,
                None => {
                    self.status_message = "No subagents under this one to fold".into();
                    return;
                }
            }
        };
        let folded = if self.state.folded_tasks.remove(&target) {
            false
        } else {
            self.state.folded_tasks.insert(target.clone());
            true
        };
        self.persist_state();
        // Whatever was highlighted may have just been folded away, and the
        // task it belonged to is the honest place to leave the cursor.
        self.select_session(target);
        self.ensure_session_selection();
        self.status_message = if folded {
            "Subagents folded away; space brings them back".into()
        } else {
            "Subagents listed; space folds them away".into()
        };
    }

    fn toggle_archived(&mut self) {
        self.state.show_archived = !self.state.show_archived;
        self.persist_state();
        self.ensure_session_selection();
        self.status_message = if self.state.show_archived {
            "Archived sessions expanded; a collapses them".into()
        } else {
            "Archived sessions collapsed; a expands them".into()
        };
    }

    fn ensure_target_visible(&mut self) {
        let visible = self.visible_target_indices();
        if !visible.contains(&self.selected_target) {
            let fallback = visible.first().copied().unwrap_or(0);
            self.set_selected_target(fallback);
        }
    }

    /// Select a machine by index, rebinding the file browser so it follows the
    /// active machine: the previous machine's browser is parked and the
    /// destination machine's browser (if any) is restored.
    /// The machine pane top to bottom: the pinned moderators row, then the
    /// machines the view is showing.
    pub fn machine_column(&self) -> Vec<MachineRow> {
        let mut rows = vec![MachineRow::Moderators];
        rows.extend(
            self.visible_target_indices()
                .into_iter()
                .map(MachineRow::Machine),
        );
        rows
    }

    pub fn selected_machine_row(&self) -> MachineRow {
        if self.moderators_selected {
            MachineRow::Moderators
        } else {
            MachineRow::Machine(self.selected_target)
        }
    }

    /// Move the machine pane's cursor. The moderators row keeps `selected_target`
    /// on this machine rather than clearing it: a moderator runs here, so the
    /// file browser, the settings panel and a launch all still mean this
    /// machine while the row is highlighted.
    pub fn select_machine_row(&mut self, row: MachineRow) {
        match row {
            MachineRow::Machine(index) => {
                let leaving_moderators = self.moderators_selected;
                self.moderators_selected = false;
                self.set_selected_target(index);
                // Stepping off the moderators row back onto this machine is not
                // a change of machine, so nothing there restores what was
                // selected here before. Do it, or the cursor lands on whichever
                // agent happens to be first.
                if leaving_moderators {
                    self.selected_session_id = self
                        .targets
                        .get(index)
                        .and_then(|status| self.selected_sessions_by_target.get(&status.target.id))
                        .cloned();
                }
            }
            MachineRow::Moderators => {
                if let Some(local) = self
                    .targets
                    .iter()
                    .position(|status| status.target.id == LOCAL_TARGET_ID)
                {
                    self.set_selected_target(local);
                }
                self.moderators_selected = true;
                self.selected_session_id = None;
            }
        }
    }

    fn set_selected_target(&mut self, index: usize) {
        let previous = self
            .targets
            .get(self.selected_target)
            .map(|status| status.target.id.clone());
        if let Some(session) = self.selected_session() {
            self.selected_sessions_by_target
                .insert(session.target_id.clone(), session.id.clone());
        }
        self.selected_target = index;
        let current = self
            .targets
            .get(index)
            .map(|status| status.target.id.clone());
        if previous != current {
            self.rebind_file_manager(previous.as_deref(), current.as_deref());
            self.selected_session_id = current
                .as_ref()
                .and_then(|target_id| self.selected_sessions_by_target.get(target_id))
                .cloned();
        }
    }

    /// Park the active file browser under the machine it belongs to and restore
    /// the destination machine's parked browser, so the file view is per-machine.
    fn rebind_file_manager(&mut self, previous: Option<&str>, current: Option<&str>) {
        if let Some(form) = self.file_manager.take() {
            self.remember_file_dir(&form);
            // Only machine-scoped (agent-pane) browsers survive a switch; a
            // browser opened from an attached terminal is tied to that session.
            if form.origin == FileManagerOrigin::AgentPane
                && let Some(previous) = previous
            {
                self.stashed_file_managers
                    .insert(previous.to_string(), form);
            }
        }
        if let Some(current) = current
            && let Some(mut form) = self.stashed_file_managers.remove(current)
        {
            // The stash can be minutes old, so re-list instead of showing what
            // the directory looked like when the user last switched away. The
            // cached entries stay on screen until the fresh listing lands.
            if !form.query.starts_with('/') {
                let request = Request::ListFiles {
                    target: form.target.clone(),
                    path: form.path.clone(),
                };
                form.loading = self.worker.requests.send(request).is_ok();
            }
            self.file_manager = Some(form);
        }
    }

    /// Remember where a browser was last pointed for its agent, so reopening it
    /// returns to that directory.
    fn remember_file_dir(&mut self, form: &FileManagerForm) {
        if let Some(session_id) = &form.session_id {
            self.file_dirs.insert(session_id.clone(), form.path.clone());
        }
    }

    fn move_focus(&mut self, direction: FocusDirection) {
        let previous = self.focus;
        let next = self
            .geometric_focus(direction)
            .or_else(|| self.compact_focus(direction));
        let Some(next) = next else {
            debug::log(
                "focus",
                format!(
                    "no neighbor direction={direction:?} from={previous:?} machines={:?} agents={:?} terminal={:?}",
                    self.pane_layout.machines, self.pane_layout.agents, self.pane_layout.recap
                ),
            );
            self.status_message = if self.state.flatten
                && self
                    .layout_debug_signature
                    .is_some_and(|(_, _, _, _, portrait, _)| portrait)
                && matches!(direction, FocusDirection::Left | FocusDirection::Right)
            {
                "Flatten mode has no Machine pane; press f to restore grouped panes".into()
            } else {
                "No pane in that direction; follow the visible layout".into()
            };
            return;
        };
        debug::log(
            "focus",
            format!("moved direction={direction:?} from={previous:?} to={next:?}"),
        );
        self.focus = next;
        if next == Focus::Recap {
            if self
                .file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_some())
            {
                self.release_terminal_input("File preview focused");
            } else {
                self.activate_terminal();
            }
        } else {
            self.release_terminal_input("Terminal remains attached; focus moved to a sidebar");
        }
    }

    fn focus_direction_for_key(&mut self, key: KeyEvent) -> Option<FocusDirection> {
        focus_navigation_direction(key)
    }

    fn geometric_focus(&self, direction: FocusDirection) -> Option<Focus> {
        let current = self.focus_area(self.focus)?;
        [Focus::Machines, Focus::Agents, Focus::Recap]
            .into_iter()
            .filter(|candidate| *candidate != self.focus)
            .filter_map(|candidate| {
                let area = self.focus_area(candidate)?;
                focus_distance(current, area, direction).map(|score| (score, candidate))
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, focus)| focus)
    }

    fn focus_area(&self, focus: Focus) -> Option<Rect> {
        match focus {
            Focus::Machines => self.pane_layout.machines,
            Focus::Agents => self.pane_layout.agents,
            Focus::Recap => self.pane_layout.recap,
        }
    }

    fn compact_focus(&self, direction: FocusDirection) -> Option<Focus> {
        let (_, _, _, _, portrait, compact) = self.layout_debug_signature?;
        if !compact {
            return None;
        }
        match (portrait, self.focus, direction) {
            (true, Focus::Recap, FocusDirection::Down) => Some(Focus::Agents),
            (true, Focus::Agents, FocusDirection::Up) => Some(Focus::Recap),
            (true, Focus::Agents, FocusDirection::Left) if !self.state.flatten => {
                Some(Focus::Machines)
            }
            (true, Focus::Machines, FocusDirection::Up) => Some(Focus::Recap),
            (true, Focus::Machines, FocusDirection::Right) => Some(Focus::Agents),
            (false, Focus::Machines, FocusDirection::Right) => Some(Focus::Agents),
            (false, Focus::Agents, FocusDirection::Left) if !self.state.flatten => {
                Some(Focus::Machines)
            }
            (false, Focus::Agents, FocusDirection::Right) => Some(Focus::Recap),
            (false, Focus::Recap, FocusDirection::Left) => Some(Focus::Agents),
            _ => None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Machines => {
                let rows = self.machine_column();
                if rows.is_empty() {
                    return;
                }
                let current = rows
                    .iter()
                    .position(|row| *row == self.selected_machine_row())
                    .unwrap_or(0);
                self.select_machine_row(rows[clamped_index(current, rows.len(), delta)]);
                self.release_terminal_input("Machine selected");
                self.history_offset = 0;
                self.ensure_session_selection();
            }
            Focus::Agents => {
                let ids: Vec<_> = self
                    .visible_sessions()
                    .iter()
                    .map(|session| session.id.clone())
                    .collect();
                if ids.is_empty() {
                    self.selected_session_id = None;
                    return;
                }
                let current = self
                    .selected_session_id
                    .as_ref()
                    .and_then(|selected| ids.iter().position(|id| id == selected))
                    .unwrap_or(0);
                let next = clamped_index(current, ids.len(), delta);
                self.select_session(ids[next].clone());
            }
            Focus::Recap => self.page_history(delta < 0),
        }
    }

    fn handle_top_up(&mut self) -> bool {
        if self.focus != Focus::Agents || self.attention_ids.is_empty() {
            self.top_up_count = 0;
            return false;
        }
        let ids: Vec<_> = self
            .visible_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        if ids.first().copied() != self.selected_session_id.as_deref() {
            self.top_up_count = 0;
            return false;
        }
        let now = Instant::now();
        let consecutive = self
            .last_top_up
            .is_some_and(|last| now.duration_since(last) <= Duration::from_millis(800));
        self.last_top_up = Some(now);
        if consecutive && self.top_up_count == 1 {
            self.top_up_count = 0;
            self.jump_to_attention();
        } else {
            self.top_up_count = 1;
            self.status_message = "Press Up again to open the agent waiting for input".into();
        }
        true
    }

    fn jump_to_attention(&mut self) {
        let Some(session_id) = self.attention_ids.first().cloned() else {
            return;
        };
        let Some(target_id) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.target_id.clone())
        else {
            return;
        };
        if let Some(index) = self
            .targets
            .iter()
            .position(|target| target.target.id == target_id)
        {
            self.set_selected_target(index);
        }
        self.select_session(session_id);
        self.focus = Focus::Recap;
        self.activate_terminal();
        self.status_message = "Opened agent waiting for input".into();
    }

    fn select_session(&mut self, id: String) {
        if self.selected_session_id.as_deref() == Some(&id) {
            if let Some(session) = self.selected_session() {
                self.selected_sessions_by_target
                    .insert(session.target_id.clone(), session.id.clone());
            }
            return;
        }
        self.pending_archived_resume = None;
        self.interactive = false;
        self.terminal_selection = None;
        self.clear_pending_terminal();
        self.terminal_retry_at = None;
        self.terminal_failures = 0;
        self.selected_session_id = Some(id);
        if let Some(session) = self.selected_session() {
            self.selected_sessions_by_target
                .insert(session.target_id.clone(), session.id.clone());
        }
        self.history_offset = 0;
        self.history = HistoryPage::default();
        // The pane falls back to this text until the new session's capture
        // lands, and the last session's failure must not be read as this one's.
        self.history_message.clear();
        if self.selected_session().is_some_and(|session| session.dead) {
            self.request_history();
        } else {
            self.connect_terminal(false);
        }
        if self.focus == Focus::Recap {
            self.activate_terminal();
        }
    }

    fn ensure_session_selection(&mut self) {
        let visible_ids: Vec<_> = self
            .visible_sessions()
            .iter()
            .map(|session| session.id.clone())
            .collect();
        if visible_ids.is_empty() {
            self.selected_session_id = None;
            self.close_terminal();
            self.history = HistoryPage::default();
            self.history_message = if self.showing_moderators() {
                "No moderators yet. Press n to start one.".into()
            } else {
                "No agents on this machine.".into()
            };
            self.history_loading = false;
            return;
        }
        let still_visible = self
            .selected_session_id
            .as_ref()
            .is_some_and(|selected| visible_ids.contains(selected));
        if !still_visible {
            self.select_session(visible_ids[0].clone());
        } else if !self.has_terminal_for_selected()
            && self.selected_session().is_some_and(|session| !session.dead)
        {
            self.connect_terminal(false);
        }
    }

    fn request_history(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if is_temporary_session_id(&session.id) {
            self.history = HistoryPage::default();
            self.history_loading = false;
            self.pending_capture = None;
            self.set_error("Temporal Chat does not retain history");
            return;
        }
        if self.is_recoverable(&session.target_id, &session.id) {
            // Its machine has no record of it, so there is nobody to ask for a
            // page: the local store is the only copy.
            self.show_recoverable_history(&session);
            return;
        }
        let desired_offset = self.history_offset;
        let viewport_lines = self.agent_viewport_height.max(1) as usize;
        let chunk_lines = self.config.history_chunk_lines.max(viewport_lines + 50);
        let capture_offset = history_capture_offset(desired_offset, chunk_lines, viewport_lines);
        if let Some(page) = self.cached_history_page(
            &session.target_id,
            &session.id,
            desired_offset,
            viewport_lines,
        ) {
            self.show_history_page(page);
            let stride = history_capture_stride(chunk_lines, viewport_lines);
            let next_capture = capture_offset.saturating_add(stride);
            if desired_offset.saturating_sub(capture_offset) > stride / 2
                && self.pending_capture.is_none()
                && self
                    .history
                    .oldest_offset()
                    .is_none_or(|oldest| next_capture <= oldest)
            {
                self.send_history_capture(&session, next_capture, chunk_lines, false);
            }
            return;
        }
        let can_use_disk_cache = self
            .targets
            .iter()
            .find(|target| target.target.id == session.target_id)
            .is_none_or(|target| target.state != ConnectionState::Online);
        if can_use_disk_cache
            && self.load_history_page(&session.target_id, &session.id, capture_offset)
            && let Some(page) = self.cached_history_page(
                &session.target_id,
                &session.id,
                desired_offset,
                viewport_lines,
            )
        {
            self.show_history_page(page);
            return;
        }
        if self.pending_capture.is_some() {
            self.history_loading = true;
            return;
        }
        self.send_history_capture(&session, capture_offset, chunk_lines, true);
    }

    fn send_history_capture(
        &mut self,
        session: &AgentSession,
        offset: usize,
        lines: usize,
        loading: bool,
    ) {
        let Some(target) = self.target(&session.target_id).cloned() else {
            return;
        };
        if self
            .worker
            .requests
            .send(Request::Capture {
                target,
                session_id: session.id.clone(),
                offset_from_bottom: offset,
                lines,
                width: self.agent_viewport_width,
                height: self.agent_viewport_height,
            })
            .is_ok()
        {
            self.pending_capture = Some((session.target_id.clone(), session.id.clone(), offset));
            self.history_loading = loading;
        }
    }

    fn cached_history_page(
        &self,
        target_id: &str,
        session_id: &str,
        desired_offset: usize,
        viewport_lines: usize,
    ) -> Option<HistoryPage> {
        self.history_cache
            .get(&history_cache_key(target_id, session_id))?
            .iter()
            .filter(|page| {
                if self.history.total_lines() == 0 {
                    return true;
                }
                // Rendered pages reach back as far as the offset they were
                // asked for, so how much history they report differs from page
                // to page and says nothing about whether one has gone stale.
                // Raw pages count the whole log, where a change in the count is
                // exactly what makes the older ones stale.
                page.rendered == self.history.rendered
                    && (page.rendered || page.total_lines() == self.history.total_lines())
            })
            .filter_map(|page| materialize_history_page(page, desired_offset, viewport_lines))
            .max_by_key(|page| page.offset_from_bottom)
    }

    fn show_history_page(&mut self, page: HistoryPage) {
        self.history = page;
        self.history_message = if self.history.text.is_empty() {
            "No terminal output yet.".into()
        } else {
            String::new()
        };
        self.history_loading = false;
    }

    fn store_history_page(&mut self, target_id: &str, session_id: &str, page: HistoryPage) {
        let pages = self
            .history_cache
            .entry(history_cache_key(target_id, session_id))
            .or_default();
        if let Some(existing) = pages
            .iter_mut()
            .find(|existing| existing.offset_from_bottom == page.offset_from_bottom)
        {
            *existing = page.clone();
        } else {
            pages.push(page.clone());
        }
        let path = self.history_cache_path(
            target_id,
            session_id,
            page.offset_from_bottom,
            page.rendered,
        );
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            debug::log("history", format!("cache directory failed: {error}"));
            return;
        }
        match serde_json::to_vec(&page) {
            Ok(data) => {
                if let Err(error) = fs::write(&path, data) {
                    debug::log(
                        "history",
                        format!("cache write {}: {error}", path.display()),
                    );
                }
            }
            Err(error) => debug::log("history", format!("cache encode failed: {error}")),
        }
    }

    fn load_history_page(&mut self, target_id: &str, session_id: &str, offset: usize) -> bool {
        // Rendered and raw pages at the same offset are different reads of the
        // session, so they are cached apart. Rendered ones are what the daemon
        // is asked for now; the raw file is what an earlier release left.
        let rendered = self.history_cache_path(target_id, session_id, offset, true);
        let path = if rendered.exists() {
            rendered
        } else {
            self.history_cache_path(target_id, session_id, offset, false)
        };
        let Ok(data) = fs::read(&path) else {
            return false;
        };
        match serde_json::from_slice::<HistoryPage>(&data) {
            Ok(page) => {
                let pages = self
                    .history_cache
                    .entry(history_cache_key(target_id, session_id))
                    .or_default();
                if !pages
                    .iter()
                    .any(|cached| cached.offset_from_bottom == page.offset_from_bottom)
                {
                    pages.push(page);
                }
                true
            }
            Err(error) => {
                debug::log("history", format!("cache read {}: {error}", path.display()));
                false
            }
        }
    }

    fn history_cache_path(
        &self,
        target_id: &str,
        session_id: &str,
        offset: usize,
        rendered: bool,
    ) -> PathBuf {
        let suffix = if rendered { "r" } else { "" };
        self.history_cache_dir
            .join(cache_path_component(target_id))
            .join(session_id)
            .join(format!("{offset}{suffix}.json"))
    }

    fn page_history(&mut self, older: bool) {
        if self.selected_session().is_none() {
            return;
        }
        let page = self.agent_viewport_height.saturating_sub(2).max(1) as usize;
        self.scroll_history(older, page);
    }

    /// Drop a finished selection when the view scrolls out from under it, but
    /// leave a drag in progress alone: the button is still down, and rolling
    /// the wheel mid-drag is how a selection is carried past the pane's edge.
    fn release_selection_for_scroll(&mut self) {
        if !self
            .terminal_selection
            .is_some_and(|selection| selection.dragging)
        {
            self.terminal_selection = None;
        }
    }

    fn scroll_history(&mut self, older: bool, lines: usize) {
        if self.attached_terminal_for_selected() {
            self.scroll_attached_terminal(older, lines);
            return;
        }
        if older
            && self.history_offset == 0
            && let Some(session) = self.selected_session().cloned()
            && !session.dead
        {
            self.history_cache
                .remove(&history_cache_key(&session.target_id, &session.id));
            self.history = HistoryPage::default();
        }
        if older {
            let next = self.history_offset.saturating_add(lines.max(1));
            self.history_offset = match self.history_reach() {
                Some(maximum) => {
                    if next > maximum {
                        self.status_message = if maximum == 0 {
                            "This terminal has no older scrollback".into()
                        } else {
                            format!("Reached the oldest available history ({maximum} lines)")
                        };
                    }
                    next.min(maximum)
                }
                None => next,
            };
        } else {
            self.history_offset = self.history_offset.saturating_sub(lines.max(1));
        }
        self.release_selection_for_scroll();
        if self.history_offset == 0 && self.selected_session().is_some_and(|session| !session.dead)
        {
            self.history_loading = false;
            self.history_message.clear();
        } else {
            self.request_history();
        }
    }

    /// How far back the page on screen can vouch for.
    ///
    /// `None` while nothing has been measured yet, or while older rows are
    /// still expected: a rendered page replays the log only as deep as it was
    /// asked to reach, so until one says it read the log whole its size is the
    /// reach of that page rather than a boundary to stop a scroll at.
    fn history_reach(&self) -> Option<usize> {
        (self.history.total_lines() > 0)
            .then(|| self.history.oldest_offset())
            .flatten()
    }

    /// Scroll recent history through the attached emulator, then continue into
    /// the daemon's history pages once the emulator buffer is exhausted.
    fn scroll_attached_terminal(&mut self, older: bool, lines: usize) {
        let step = lines.max(1);
        let mut desired = if older {
            self.history_offset.saturating_add(step)
        } else {
            self.history_offset.saturating_sub(step)
        };
        let boundary = self
            .terminal
            .as_mut()
            .map_or(0, TerminalSession::max_scrollback);
        // Once the daemon has read the session whole there is nothing above its
        // oldest row to ask for, so stop there rather than send the view -- and
        // a capture request per step -- past the top of the history. The
        // emulator's own buffer still counts: it wraps at the pane's width
        // rather than the session's, so it can hold rows the daemon does not.
        if older
            && let Some(oldest) = self.history_reach().map(|oldest| oldest.max(boundary))
            && desired > oldest
        {
            desired = oldest;
            self.status_message = if oldest == 0 {
                "This terminal has no older scrollback".into()
            } else {
                format!("Reached the oldest available history ({oldest} lines)")
            };
        }
        let mut buffered = false;
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.set_scrollback(desired.min(boundary));
            if desired <= boundary {
                self.history_offset = terminal.scrollback();
                self.terminal_scrollback_pin = self.history_offset;
                buffered = true;
            } else {
                self.history_offset = desired;
            }
        }
        self.release_selection_for_scroll();
        if buffered {
            self.history_loading = false;
            self.history_message.clear();
            return;
        }
        if self.daemon_history_continues_terminal() {
            self.request_history();
            return;
        }
        self.clamp_history_to_buffered_rows();
    }

    /// Whether daemon history pages carry on where the attached emulator's
    /// buffer ends. They do once the daemon renders them into rows, which is
    /// what its pages are asked for; a daemon too old to do that answers in raw
    /// log lines and its offsets count something else entirely.
    fn daemon_history_continues_terminal(&self) -> bool {
        self.history.rendered || self.history.total_lines() == 0
    }

    /// Pull the view back to the oldest row the attached emulator buffers.
    ///
    /// A page of raw log lines is neither the same unit as those rows nor the
    /// same picture — an agent's redraws are whole screens of paint, and a
    /// slice of them lands somewhere unrelated — so stop at the buffer's edge
    /// rather than jump there.
    fn clamp_history_to_buffered_rows(&mut self) {
        if !self.attached_terminal_for_selected() {
            return;
        }
        let boundary = self
            .terminal
            .as_mut()
            .map_or(0, TerminalSession::max_scrollback);
        if self.history_offset <= boundary {
            return;
        }
        self.history_offset = boundary;
        self.status_message = if boundary == 0 {
            "This terminal has no scrollback yet".into()
        } else {
            format!("Reached the oldest buffered line ({boundary} up)")
        };
    }

    fn open_launch(&mut self) {
        // The moderators row is not a machine, so the one key that starts an
        // agent starts the kind of agent that row holds.
        if self.showing_moderators() {
            self.open_moderator_launch();
            return;
        }
        let Some(target) = self.launch_target() else {
            self.status_message = "Enable a machine before launching an agent".into();
            return;
        };
        let selected_path = self
            .selected_session()
            .filter(|session| session.target_id == target.id)
            .map(|session| session.path.clone());
        // Normal agents keep the existing preference for the machine's last
        // launch directory before the highlighted agent's folder.
        let path = self
            .state
            .last_launch_dirs
            .get(&target.id)
            .cloned()
            .or(selected_path)
            .unwrap_or_else(|| {
                if target.id == LOCAL_TARGET_ID {
                    env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .display()
                        .to_string()
                } else {
                    ".".into()
                }
            });
        let kind = self.default_kind(&target.id, false);
        self.modal = Some(Modal::Launch(LaunchForm {
            target,
            kind,
            path,
            label: String::new(),
            temporary: false,
            field: LaunchField::Kind,
        }));
    }

    /// The runtimes a machine offers: the ones its probe found installed, plus
    /// a terminal, which needs nothing installed. A machine muxloom has not
    /// reached yet, or one with no agent at all, offers every runtime instead
    /// of dead-ending — picking a missing one there still opens the install
    /// prompt.
    pub fn offered_kinds(&self, target_id: &str) -> Vec<AgentKind> {
        let installed = self
            .targets
            .iter()
            .find(|status| status.target.id == target_id)
            .filter(|status| status.state == ConnectionState::Online)
            .map(|status| status.probe.available())
            .unwrap_or_else(|| AgentKind::ALL.to_vec());
        if installed.iter().any(|kind| *kind != AgentKind::Terminal) {
            installed
        } else {
            AgentKind::ALL.to_vec()
        }
    }

    /// The same list without the terminal: a temporal chat is a conversation
    /// with an agent.
    pub fn offered_agent_kinds(&self, target_id: &str) -> Vec<AgentKind> {
        self.offered_kinds(target_id)
            .into_iter()
            .filter(|kind| *kind != AgentKind::Terminal)
            .collect()
    }

    /// What a fresh launch form starts on: the runtime last launched on that
    /// machine, as long as it is still on offer, and otherwise the first one
    /// the machine offers.
    fn default_kind(&self, target_id: &str, agents_only: bool) -> AgentKind {
        let kinds = if agents_only {
            self.offered_agent_kinds(target_id)
        } else {
            self.offered_kinds(target_id)
        };
        self.state
            .last_launch_kinds
            .get(target_id)
            .copied()
            .filter(|kind| kinds.contains(kind))
            .or_else(|| kinds.first().copied())
            .unwrap_or(AgentKind::Codex)
    }

    fn step_kind(&self, target_id: &str, current: AgentKind, forward: bool) -> AgentKind {
        step_within(&self.offered_kinds(target_id), current, forward)
    }

    fn step_agent_kind(&self, target_id: &str, current: AgentKind, forward: bool) -> AgentKind {
        step_within(&self.offered_agent_kinds(target_id), current, forward)
    }

    fn launch_target(&self) -> Option<Target> {
        // In grouped mode the machine sidebar is authoritative, even if an old
        // session selection still points at a different host.
        (if !self.state.flatten {
            self.targets
                .get(self.selected_target)
                .filter(|status| status.enabled)
                .map(|status| status.target.clone())
        } else {
            self.selected_session()
                .and_then(|session| self.target(&session.target_id))
                .cloned()
                .or_else(|| {
                    self.targets
                        .get(self.selected_target)
                        .filter(|status| status.enabled)
                        .map(|status| status.target.clone())
                })
        })
        .or_else(|| {
            self.targets
                .iter()
                .find(|status| status.enabled)
                .map(|status| status.target.clone())
        })
    }

    fn user_folder(&self, target: &Target) -> String {
        if target.id == LOCAL_TARGET_ID {
            env::var("HOME").unwrap_or_else(|_| {
                env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .display()
                    .to_string()
            })
        } else {
            "~".into()
        }
    }

    fn open_temporary_agent(&mut self) {
        let Some(target) = self.launch_target() else {
            self.status_message = "Enable a machine before starting a Temporal Chat".into();
            return;
        };
        // A scratch chat gets a scratch folder: the daemon makes one of its own
        // for every temporary session and runs it there, so this path is only
        // what a daemon too old to do that falls back to. It must not inherit
        // whichever project happened to be selected — a throwaway agent left
        // loose in a repository is exactly what this avoids.
        let path = self.user_folder(&target);
        let kind = self.default_kind(&target.id, true);
        self.modal = Some(Modal::Temporal(TemporalForm {
            target,
            kind,
            path,
            label: String::new(),
        }));
    }

    /// The new-moderator form, with everything muxloom can currently see
    /// already in scope. Starting from "the whole fleet" and unchecking is the
    /// usual shape of the answer, and it also means a form submitted untouched
    /// says "everything" rather than "nothing".
    fn open_moderator_launch(&mut self) {
        let machines = self
            .targets
            .iter()
            .filter(|status| status.enabled)
            .map(|status| ScopeItem {
                label: status.target.label.clone(),
                machine: status.target.id.clone(),
                selected: true,
            })
            .collect::<Vec<_>>();
        // Every agent the fleet has, not this machine's: a moderator that can
        // only hand work to what happens to be running beside it is no use on
        // a fleet. They are grouped by machine in the order the machines are
        // listed, so the column reads the way the one above it does.
        let mut agents = self
            .sessions
            .iter()
            .filter(|session| {
                !session.dead
                    && session.kind != AgentKind::Terminal
                    && !self.is_moderator_session(session)
                    && machines
                        .iter()
                        .any(|machine| machine.machine == session.target_id)
            })
            .map(|session| ScopeItem {
                label: self.scope_line(session),
                machine: session.target_id.clone(),
                selected: true,
            })
            .collect::<Vec<_>>();
        agents.sort_by_cached_key(|agent| {
            let machine = machines
                .iter()
                .position(|item| item.machine == agent.machine)
                .unwrap_or(usize::MAX);
            (machine, agent.label.to_lowercase())
        });
        let kinds = self.moderator_kinds();
        let kind = self
            .state
            .last_launch_kinds
            .get(LOCAL_TARGET_ID)
            .copied()
            .filter(|kind| kinds.contains(kind))
            .or_else(|| kinds.first().copied())
            .unwrap_or(AgentKind::Codex);
        self.modal = Some(Modal::Moderator(ModeratorForm {
            kind,
            name: String::new(),
            machines,
            agents,
            selected: 1,
            error: None,
        }));
    }

    /// The runtimes a moderator can be. A moderator that cannot call the
    /// muxloom tools has nothing to moderate with, and the daemon writes its
    /// MCP entry into exactly two agents' configuration, so the picker offers
    /// only those — this machine's, or both when the probe found neither and
    /// the install prompt will handle it.
    pub fn moderator_kinds(&self) -> Vec<AgentKind> {
        const MODERATES: [AgentKind; 2] = [AgentKind::Codex, AgentKind::Claude];
        let installed: Vec<_> = self
            .offered_agent_kinds(LOCAL_TARGET_ID)
            .into_iter()
            .filter(|kind| MODERATES.contains(kind))
            .collect();
        if installed.is_empty() {
            MODERATES.to_vec()
        } else {
            installed
        }
    }

    /// How one agent reads in a moderator's brief. The moderator finds sessions
    /// by id through `list_sessions`, so this is written for the person filling
    /// the form in and for the moderator to recognise later, not to be parsed.
    fn scope_line(&self, session: &AgentSession) -> String {
        let machine = self
            .target(&session.target_id)
            .map(|target| target.label.clone())
            .unwrap_or_else(|| session.target_id.clone());
        format!(
            "{} · {} on {machine} ({})",
            session.kind,
            session.display_label(),
            session.id
        )
    }

    /// Start a moderator: make it a folder, leave the brief in it, make sure
    /// the tools it coordinates with are registered, and then launch it like
    /// any other local agent.
    fn launch_moderator(&mut self, mut form: ModeratorForm) {
        let name = form.name.trim().to_string();
        if name.is_empty() {
            form.error =
                Some("A moderator needs a name — it is how you and the others call it".into());
            form.selected = 1;
            self.modal = Some(Modal::Moderator(form));
            return;
        }
        let Some(target) = self.target(LOCAL_TARGET_ID).cloned() else {
            self.set_error("This machine is unavailable, and a moderator runs here");
            return;
        };
        let moderator = crate::moderator::Moderator {
            name: name.clone(),
            kind: form.kind,
            machines: form.chosen_machines(),
            agents: form.chosen_agents(),
        };
        let folder = match crate::moderator::prepare(&self.moderator_state_dir, &moderator) {
            Ok(folder) => folder,
            Err(error) => {
                form.error = Some(format!("{error:#}"));
                self.modal = Some(Modal::Moderator(form));
                return;
            }
        };
        // The MCP entry and the skill are already in place: the local daemon
        // writes both into this user's agent configuration when it starts, and
        // it has to be running for anything to launch at all. A moderator gets
        // them the way every other agent on this machine does.
        self.status_message = format!("Starting moderator {name}...");
        self.confirm_or_submit_launch(
            LaunchForm {
                target,
                kind: form.kind,
                path: folder.display().to_string(),
                label: name,
                temporary: false,
                field: LaunchField::Kind,
            },
            None,
            None,
        );
    }

    fn launch_temporary_agent(&mut self, form: TemporalForm) {
        let label = form.label().to_string();
        self.confirm_or_submit_launch(
            LaunchForm {
                target: form.target,
                kind: form.kind,
                path: form.path,
                label: label.clone(),
                temporary: true,
                field: LaunchField::Kind,
            },
            None,
            None,
        );
        self.status_message = format!("Starting {label}...");
    }

    fn open_port_forward(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            self.status_message = "Select an agent before configuring port forwarding".into();
            return;
        };
        let Some(target) = self.target(&session.target_id).cloned() else {
            self.set_error("The selected agent's machine is unavailable");
            return;
        };
        let visible = if self.attached_terminal_for_selected() {
            self.terminal
                .as_mut()
                .map(TerminalSession::live_contents)
                .unwrap_or_default()
        } else {
            self.history.text.clone()
        };
        let detected_ports = detected_ports_in_text(&visible);
        let remote_port = detected_ports
            .first()
            .map(u16::to_string)
            .unwrap_or_default();
        let mut form = PortForwardForm {
            target: target.clone(),
            session_id: session.id,
            folder: session.path,
            remote_host: "127.0.0.1".into(),
            local_port: remote_port.clone(),
            remote_port,
            detected_ports,
            active: self.port_forwards.summaries_for(&target.id),
            selected: 1,
            loading: true,
            error: None,
            detection_error: None,
        };
        if self
            .worker
            .requests
            .send(Request::DetectPorts { target })
            .is_err()
        {
            form.loading = false;
            form.detection_error = Some("Port detector is unavailable".into());
        }
        self.modal = Some(Modal::PortForward(form));
    }

    fn start_port_forward(&mut self, mut form: PortForwardForm) {
        let result = (|| -> Result<PortForwardSummary, String> {
            let remote_host = form.remote_host.trim();
            if remote_host.is_empty() {
                return Err("Remote host is required".into());
            }
            let remote_port: u16 = form
                .remote_port
                .trim()
                .parse()
                .map_err(|_| "Remote port must be 1-65535".to_string())?;
            if remote_port == 0 {
                return Err("Remote port must be 1-65535".into());
            }
            let local_port = if form.local_port.trim().is_empty() {
                remote_port
            } else {
                form.local_port
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| "Local port must be 0-65535".to_string())?
            };
            self.port_forwards
                .start(
                    self.worker.bridges.clone(),
                    form.target.clone(),
                    form.session_id.clone(),
                    form.folder.clone(),
                    remote_host.into(),
                    remote_port,
                    local_port,
                )
                .map_err(|error| error.to_string())
        })();
        match result {
            Ok(forward) => {
                form.error = None;
                form.active = self.port_forwards.summaries_for(&form.target.id);
                // The tunnel is still opening on a worker thread; poll_port_forwards
                // reports whether it reached Active or failed.
                self.status_message = format!(
                    "Starting forward 127.0.0.1:{} to {}:{} on {}...",
                    forward.local_port, forward.remote_host, forward.remote_port, forward.target_id
                );
            }
            Err(error) => form.error = Some(short_error(&error)),
        }
        self.modal = Some(Modal::PortForward(form));
    }

    fn stop_port_forward(&mut self, mut form: PortForwardForm) {
        if let Some(index) = form.active_index()
            && let Some(forward) = form.active.get(index)
            && self.port_forwards.stop(forward.id)
        {
            self.status_message = format!("Stopped local port {}", forward.local_port);
        }
        form.active = self.port_forwards.summaries_for(&form.target.id);
        form.selected = form.selected.min(form.row_count().saturating_sub(1));
        self.modal = Some(Modal::PortForward(form));
    }

    fn open_path_picker(&mut self, launch: LaunchForm) {
        let path = if launch.path.trim().is_empty() {
            ".".into()
        } else {
            launch.path.clone()
        };
        self.request_directory(PathPickerForm {
            launch,
            path,
            directories: Vec::new(),
            query: String::new(),
            selected: 0,
            loading: false,
            error: None,
        });
    }

    fn request_directory(&mut self, mut form: PathPickerForm) {
        form.loading = true;
        form.error = None;
        let request = Request::ListDirectory {
            target: form.launch.target.clone(),
            path: form.path.clone(),
        };
        if self.worker.requests.send(request).is_err() {
            form.loading = false;
            form.error = Some("Directory worker is unavailable".into());
        }
        self.modal = Some(Modal::PathPicker(form));
    }

    fn prepare_launch(&mut self, launch: LaunchForm) {
        if launch.path.trim().is_empty() {
            self.status_message = "Launch cancelled: working directory is required".into();
            return;
        }
        // A shell has no conversation to resume, so the picker it would open
        // offers one row - "New" - and a keypress that means nothing. Open the
        // terminal where the folder was chosen instead.
        if launch.kind == AgentKind::Terminal {
            self.confirm_or_submit_launch(launch, None, None);
            return;
        }
        let mut form = ResumeForm {
            launch,
            candidates: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            query: String::new(),
            history_hits: Vec::new(),
            history_selected: 0,
            searched_query: String::new(),
            search_edited_at: None,
        };
        form.loading = true;
        let request = Request::ScanResumes {
            target: form.launch.target.clone(),
            kind: form.launch.kind,
            path: form.launch.path.clone(),
        };
        if self.worker.requests.send(request).is_err() {
            form.loading = false;
            form.error = Some("Resume scanner is unavailable".into());
        }
        self.modal = Some(Modal::Resume(form));
    }

    fn open_global_settings(&mut self) {
        let mut values = vec![
            self.config.refresh_interval_ms.to_string(),
            self.config.ssh_config.clone(),
            self.config.environment.clone(),
        ];
        for kind in AgentKind::agents() {
            let command = self.config.agents.get(kind);
            values.push(command.command.clone());
            values.push(format_shell_list(&command.args));
        }
        values.push(self.config.agents.terminal.command.clone());
        values.push(self.config.update_prompt.clone());
        values.push(self.config.update_channel.clone());
        values.push(self.config.touch.clone());
        self.modal = Some(Modal::Settings(SettingsForm {
            scope: SettingsScope::Global,
            values,
            notes: Vec::new(),
            missing: Vec::new(),
            selected: 0,
            error: None,
        }));
    }

    /// What the Daemon section reports for a machine: the version actually
    /// serving it, next to the build this controller would hand over to.
    fn daemon_version_note(&self, target_id: &str) -> String {
        let build = env!("CARGO_PKG_VERSION");
        match self.worker.bridges.daemon_version(target_id) {
            Some(running) if crate::model::version_is_newer(build, &running) => {
                format!("muxloomd {running} running · {build} available")
            }
            Some(running) => format!("muxloomd {running} running · current"),
            None => "not connected".to_string(),
        }
    }

    fn open_machine_settings(&mut self) {
        let target_id = if self.state.flatten {
            self.selected_session()
                .map(|session| session.target_id.clone())
        } else {
            self.targets
                .get(self.selected_target)
                .map(|target| target.target.id.clone())
        };
        let Some(target_id) = target_id else {
            self.status_message = "Select a machine before editing its configuration".into();
            return;
        };
        let mut values = vec![
            self.config
                .hosts
                .get(&target_id)
                .and_then(|host| host.environment.clone())
                .unwrap_or_else(|| self.config.environment.clone()),
        ];
        for kind in AgentKind::agents() {
            let command = self.config.command_for(&target_id, kind).clone();
            values.push(command.command);
            values.push(format_shell_list(&command.args));
        }
        values.push(
            self.config
                .command_for(&target_id, AgentKind::Terminal)
                .command
                .clone(),
        );
        // Only a machine muxloom has actually reached can say what it is
        // missing; an offline one gets no install actions rather than a panel
        // full of them.
        let missing = self
            .targets
            .iter()
            .find(|status| status.target.id == target_id)
            .filter(|status| status.state == ConnectionState::Online)
            .map(|status| {
                AgentKind::agents()
                    .filter(|kind| !status.probe.has(*kind))
                    .collect()
            })
            .unwrap_or_default();
        self.modal = Some(Modal::Settings(SettingsForm {
            scope: SettingsScope::Host(target_id.clone()),
            values,
            notes: vec![self.daemon_version_note(&target_id)],
            missing,
            selected: 0,
            error: None,
        }));
    }

    fn open_search(&mut self) {
        self.modal = Some(Modal::Search(SearchForm {
            query: String::new(),
            submitted_query: String::new(),
            results: Vec::new(),
            result_rows: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            edited_at: Instant::now(),
        }));
    }

    fn open_file_manager(&mut self) {
        self.last_file_click = None;
        if let Some(form) = self.file_manager.take() {
            self.remember_file_dir(&form);
            self.status_message = "File browser closed".into();
            return;
        }
        let origin = if self.focus == Focus::Recap {
            FileManagerOrigin::TerminalPane
        } else {
            FileManagerOrigin::AgentPane
        };
        let selected = self.selected_session().cloned();
        let target = selected
            .as_ref()
            .and_then(|session| self.target(&session.target_id))
            .cloned()
            .or_else(|| {
                self.targets
                    .get(self.selected_target)
                    .map(|status| status.target.clone())
            });
        let Some(target) = target else {
            self.status_message = "No machine is available for file browsing".into();
            return;
        };
        let session_id = selected.as_ref().map(|session| session.id.clone());
        // Start where this agent's browser was last pointed, else the agent's
        // own working directory, else the machine root.
        let path = session_id
            .as_ref()
            .and_then(|id| self.file_dirs.get(id).cloned())
            .or_else(|| {
                selected
                    .filter(|session| session.target_id == target.id)
                    .map(|session| session.path)
            })
            .unwrap_or_else(|| ".".into());
        self.release_terminal_input("File manager opened");
        self.focus = Focus::Agents;
        self.request_file_listing(FileManagerForm {
            origin,
            target,
            session_id,
            path,
            entries: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            directory_cache: HashMap::new(),
            return_path: None,
            preview_path: None,
            preview: None,
            preview_requested_path: None,
            preview_loading: false,
            preview_error: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_page_rows: 1,
            preview_follow_tail: false,
            preview_stamp: None,
            preview_rendered: None,
            query: String::new(),
            search_request_id: None,
            searching: false,
            search_truncated: false,
            search_edited_at: None,
            preview_cache: HashMap::new(),
            preload_pending: HashSet::new(),
            entry_rows: Vec::new(),
            list_area: None,
            preview_area: None,
            preview_text_area: None,
            preview_visible: Vec::new(),
            preview_selection: None,
            media_playback: None,
            media_frame: None,
            media_loading: false,
            media_error: None,
        });
    }

    fn request_file_listing(&mut self, mut form: FileManagerForm) {
        form.loading = true;
        form.error = None;
        // This listing supersedes anything the monitor has outstanding, which
        // would otherwise be answered for the directory we are leaving.
        self.file_monitor_sent_at = None;
        self.file_monitor_in_flight = false;
        Self::clear_file_preview(&mut form);
        let request = Request::ListFiles {
            target: form.target.clone(),
            path: form.path.clone(),
        };
        debug::log(
            "files",
            format!(
                "list requested target={} path={} cached_entries={}",
                form.target.id,
                form.path,
                form.entries.len()
            ),
        );
        if self.worker.requests.send(request).is_err() {
            form.loading = false;
            form.error = Some("File browser worker is unavailable".into());
        }
        self.file_manager = Some(form);
    }

    fn update_file_query(&mut self, mut form: FileManagerForm, was_recursive: bool) {
        if form.query.starts_with('/') {
            if form.query.len() == 1 {
                Self::restore_file_directory_entries(&mut form);
                self.file_manager = Some(form);
                return;
            }
            form.search_request_id = None;
            form.searching = true;
            form.search_edited_at = Some(Instant::now());
            form.error = None;
            form.selected = 0;
            Self::clear_file_preview(&mut form);
        } else {
            if was_recursive {
                Self::restore_file_directory_entries(&mut form);
            }
            Self::select_file_query_match(&mut form);
            self.queue_file_preloads(&mut form);
        }
        self.file_manager = Some(form);
    }

    fn maybe_submit_file_search(&mut self) {
        let ready = self.file_manager.as_ref().is_some_and(|form| {
            form.query.starts_with('/')
                && form.query.len() > 1
                && form.search_request_id.is_none()
                && form
                    .search_edited_at
                    .is_some_and(|edited| edited.elapsed() >= FILE_SEARCH_DEBOUNCE)
        });
        if !ready {
            return;
        }
        let mut form = self.file_manager.take().expect("matched file manager");
        self.submit_file_search(&mut form);
        self.file_manager = Some(form);
    }

    /// Re-fetches the open preview when a fresh listing shows its file changed.
    /// Media previews are skipped: their bytes stream separately, and re-opening
    /// the decoder would restart playback under the viewer.
    fn refresh_stale_preview(&self, form: &mut FileManagerForm, entries: &[FileEntry]) {
        let Some(path) = form.preview_path.clone() else {
            return;
        };
        if form.preview_requested_path.is_some() {
            return; // A fetch is already on its way.
        }
        if form.preview.as_ref().is_some_and(|preview| {
            matches!(
                preview.kind,
                FilePreviewKind::Image | FilePreviewKind::Video
            )
        }) {
            return;
        }
        let Some(entry) = entries.iter().find(|entry| entry.path == path) else {
            return;
        };
        let stamp = (entry.size, entry.mtime);
        if form.preview_stamp == Some(stamp) {
            return;
        }
        form.preview_stamp = Some(stamp);
        // Watching a large file means pulling all of it across the link on every
        // change, which costs far more than the update is worth. Those refresh
        // on demand instead, with r or F5.
        if entry.size > AUTO_REFRESH_LIMIT {
            return;
        }
        self.request_preview_refresh(form, path);
    }

    /// Asks for a fresh copy of the open preview without disturbing what is on
    /// screen: no spinner, no cleared body, and the scroll position is kept so
    /// the reader stays where they were until the new content arrives.
    fn request_preview_refresh(&self, form: &mut FileManagerForm, path: String) {
        let request = Request::PreviewFile {
            target: form.target.clone(),
            path: path.clone(),
        };
        if self.worker.requests.send(request).is_ok() {
            form.preview_requested_path = Some(path);
        }
    }

    /// Watches the file behind the open preview. Re-listing the directory is far
    /// cheaper than re-reading the file, so the poll only carries the entry
    /// metadata; the preview itself is re-fetched from `Event::FilesListed` and
    /// only when `(size, mtime)` actually moved.
    fn maybe_monitor_open_file(&mut self) {
        let Some(form) = self.file_manager.as_ref() else {
            self.file_monitor_sent_at = None;
            self.file_monitor_in_flight = false;
            return;
        };
        // A loading listing is already on its way, and a search shows entries
        // from elsewhere in the tree, so neither needs a poll.
        if form.preview_path.is_none() || form.loading || form.query.starts_with('/') {
            return;
        }
        let elapsed = self
            .file_monitor_sent_at
            .map(|sent| sent.elapsed())
            .unwrap_or(FILE_MONITOR_TIMEOUT);
        let due = if self.file_monitor_in_flight {
            elapsed >= FILE_MONITOR_TIMEOUT
        } else {
            elapsed >= FILE_MONITOR_INTERVAL
        };
        if !due {
            return;
        }
        let request = Request::ListFiles {
            target: form.target.clone(),
            path: form.path.clone(),
        };
        self.file_monitor_sent_at = Some(Instant::now());
        self.file_monitor_in_flight = self.worker.requests.send(request).is_ok();
    }

    fn submit_file_search(&mut self, form: &mut FileManagerForm) {
        let Some(pattern) = form
            .query
            .strip_prefix('/')
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        self.next_file_search_id = self.next_file_search_id.wrapping_add(1).max(1);
        let request_id = self.next_file_search_id;
        form.search_request_id = Some(request_id);
        form.search_edited_at = None;
        form.searching = true;
        let request = Request::SearchFiles {
            target: form.target.clone(),
            root: form.path.clone(),
            pattern,
            request_id,
        };
        if self.worker.requests.send(request).is_err() {
            form.search_request_id = None;
            form.searching = false;
            form.error = Some("File search worker is unavailable".into());
        }
    }

    fn restore_file_directory_entries(form: &mut FileManagerForm) {
        form.search_request_id = None;
        form.searching = false;
        form.search_edited_at = None;
        form.error = None;
        form.entries = form
            .directory_cache
            .get(&form.path)
            .cloned()
            .unwrap_or_default();
        form.selected = form.selected.min(form.entries.len().saturating_sub(1));
        Self::clear_file_preview(form);
    }

    fn apply_file_search_result(form: &mut FileManagerForm, result: Result<FileListing, String>) {
        form.searching = false;
        form.search_edited_at = None;
        match result {
            Ok(listing) => {
                form.search_truncated = listing.truncated;
                form.entries = listing.entries;
                form.selected = 0;
                form.return_path = None;
                form.error = None;
            }
            Err(error) => {
                form.search_truncated = false;
                form.entries.clear();
                form.selected = 0;
                form.error = Some(short_error(&error));
            }
        }
        Self::clear_file_preview(form);
    }

    fn clear_file_preview(form: &mut FileManagerForm) {
        form.preview_path = None;
        form.preview = None;
        form.preview_requested_path = None;
        form.preview_loading = false;
        form.preview_error = None;
        form.preview_scroll = 0;
        form.preview_max_scroll = 0;
        form.preview_page_rows = 1;
        form.preview_follow_tail = false;
        form.preview_stamp = None;
        form.preview_rendered = None;
        form.preview_area = None;
        form.preview_text_area = None;
        form.preview_visible.clear();
        form.preview_selection = None;
        form.media_playback = None;
        form.media_frame = None;
        form.media_loading = false;
        form.media_error = None;
    }

    fn request_media_preview(&mut self, target: Target, path: String, kind: FilePreviewKind) {
        let area = self.pane_layout.recap;
        let width = area
            .map(|area| area.width.saturating_sub(2))
            .unwrap_or(self.agent_viewport_width)
            .clamp(1, 240);
        let height = area
            .map(|area| area.height.saturating_sub(4).saturating_mul(2))
            .unwrap_or_else(|| self.agent_viewport_height.saturating_mul(2))
            .clamp(2, 240);
        let Some(form) = self.file_manager.as_mut() else {
            return;
        };
        if form.target.id != target.id || form.preview_path.as_deref() != Some(path.as_str()) {
            return;
        }
        form.media_playback = None;
        form.media_frame = None;
        form.media_error = None;
        form.media_loading = true;
        if self
            .worker
            .requests
            .send(Request::OpenMedia {
                target,
                path,
                kind,
                width,
                height,
            })
            .is_err()
        {
            form.media_loading = false;
            form.media_error = Some("Media preview worker is unavailable".into());
        }
    }

    fn poll_media(&mut self) {
        let Some(form) = self.file_manager.as_mut() else {
            return;
        };
        let mut close_playback = false;
        while let Some(playback) = form.media_playback.as_ref() {
            let update = playback.try_update();
            match update {
                Ok(MediaUpdate::Frame(frame)) => {
                    form.media_frame = Some(frame);
                    form.media_loading = false;
                    form.media_error = None;
                }
                Ok(MediaUpdate::Finished) => {
                    form.media_loading = false;
                    close_playback = true;
                    break;
                }
                Ok(MediaUpdate::Failed(error)) => {
                    form.media_loading = false;
                    form.media_error = Some(short_error(&error));
                    close_playback = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if form.media_loading && form.media_frame.is_none() {
                        form.media_error =
                            Some("Media decoder stopped before the first frame".into());
                    }
                    form.media_loading = false;
                    close_playback = true;
                    break;
                }
            }
        }
        if close_playback {
            form.media_playback = None;
        }
    }

    fn navigate_file_form(
        &mut self,
        mut form: FileManagerForm,
        path: String,
        return_path: Option<String>,
    ) {
        // Entering or leaving a directory resets the filter so the query does not
        // silently follow the user into a folder where it no longer matches.
        let was_recursive = form.query.starts_with('/');
        form.query.clear();
        form.search_request_id = None;
        form.searching = false;
        form.search_edited_at = None;
        if !was_recursive && !form.entries.is_empty() {
            form.directory_cache
                .insert(form.path.clone(), form.entries.clone());
        }
        form.path = path;
        form.entries = form
            .directory_cache
            .get(&form.path)
            .cloned()
            .unwrap_or_default();
        form.return_path = return_path;
        form.selected = form
            .return_path
            .as_ref()
            .and_then(|return_path| {
                form.entries
                    .iter()
                    .position(|entry| &entry.path == return_path)
            })
            .unwrap_or(0);
        Self::clear_file_preview(&mut form);
        self.request_file_listing(form);
    }

    fn move_file_selection(form: &mut FileManagerForm, delta: isize) {
        if form.entries.is_empty() {
            form.selected = 0;
            return;
        }
        form.selected = clamped_index(form.selected, form.entries.len(), delta);
        form.return_path = None;
        Self::clear_file_preview(form);
    }

    fn select_file_query_match(form: &mut FileManagerForm) {
        if let Some((index, _)) = form
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                folder_match_rank(&entry.name, &form.query).map(|rank| (index, rank))
            })
            .min_by_key(|(_, rank)| *rank)
        {
            form.selected = index;
            form.return_path = None;
        }
    }

    fn page_file_preview(form: &mut FileManagerForm, forward: bool) {
        let step = usize::from(form.preview_page_rows.max(1));
        if form.preview_max_scroll == 0 {
            form.preview_scroll = 0;
        } else if forward {
            form.preview_scroll = form
                .preview_scroll
                .saturating_add(step)
                .min(form.preview_max_scroll);
        } else {
            form.preview_scroll = form.preview_scroll.saturating_sub(step);
        }
        Self::sync_preview_follow(form);
    }

    /// A preview parked on its last row is treated as "following": later
    /// refreshes scroll on to the new end of the file, the way a tail does.
    /// Paging or scrolling anywhere else drops the follow again.
    fn sync_preview_follow(form: &mut FileManagerForm) {
        form.preview_follow_tail =
            form.preview_max_scroll > 0 && form.preview_scroll >= form.preview_max_scroll;
    }

    /// Keep a preview for reopening only while it is small. Opening a file
    /// still reads it whole; caching megabytes per neighbour would grow the
    /// browser's footprint without saving a visible amount of work.
    fn cache_preview(form: &mut FileManagerForm, path: &str, preview: &FilePreview) {
        const CACHE_LIMIT: usize = 256 * 1024;
        if preview.content.len() > CACHE_LIMIT {
            form.preview_cache.remove(path);
            return;
        }
        form.preview_cache.insert(path.to_string(), preview.clone());
    }

    fn queue_file_preloads(&self, form: &mut FileManagerForm) {
        // Neighbours are read ahead only when they are small: a preload is a
        // guess, and guessing wrong on a large file costs the link dearly.
        const PREVIEW_LIMIT: u64 = 256 * 1024;
        const MAX_PENDING_PRELOADS: usize = 2;
        if form.entries.is_empty() {
            return;
        }
        let start = form.selected.saturating_sub(1);
        let end = (form.selected + 2).min(form.entries.len());
        for (index, entry) in form.entries[start..end].iter().enumerate() {
            if form.preload_pending.len() >= MAX_PENDING_PRELOADS {
                break;
            }
            if start + index == form.selected || form.preload_pending.contains(&entry.path) {
                continue;
            }
            let request = match entry.kind {
                FileEntryKind::File
                    if entry.size <= PREVIEW_LIMIT
                        && !form.preview_cache.contains_key(&entry.path) =>
                {
                    Some(Request::PreloadPreview {
                        target: form.target.clone(),
                        path: entry.path.clone(),
                    })
                }
                _ => None,
            };
            if let Some(request) = request
                && self.worker.requests.send(request).is_ok()
            {
                form.preload_pending.insert(entry.path.clone());
            }
        }
    }

    fn open_file_entry(&mut self, mut form: FileManagerForm) {
        let entry = form.entries.get(form.selected).cloned();
        let Some(entry) = entry else {
            if let Some(path) = form.return_path.clone() {
                self.navigate_file_form(form, path, None);
            } else {
                self.file_manager = Some(form);
            }
            return;
        };
        if entry.kind == FileEntryKind::Directory {
            self.navigate_file_form(form, entry.path, None);
        } else if form.preview_path.as_deref() == Some(entry.path.as_str()) {
            Self::clear_file_preview(&mut form);
            self.focus = Focus::Agents;
            self.status_message = "File preview closed; terminal restored".into();
            self.file_manager = Some(form);
        } else {
            Self::clear_file_preview(&mut form);
            form.preview_path = Some(entry.path.clone());
            form.preview_stamp = Some((entry.size, entry.mtime));
            let mut media_kind = None;
            if let Some(preview) = form.preview_cache.get(&entry.path).cloned() {
                let media = matches!(
                    preview.kind,
                    FilePreviewKind::Image | FilePreviewKind::Video
                );
                if media {
                    media_kind = Some(preview.kind);
                }
                form.preview = Some(preview);
                form.preview_loading = false;
                self.status_message = "Opened preloaded preview".into();
                // The cache is only a head start: the file may well have changed
                // since it was read, so confirm it against the target right away
                // and swap the body in if the copy on screen is out of date.
                if !media {
                    self.request_preview_refresh(&mut form, entry.path.clone());
                }
            } else {
                form.preview_requested_path = Some(entry.path.clone());
                form.preview_loading = true;
                let request = Request::PreviewFile {
                    target: form.target.clone(),
                    path: entry.path.clone(),
                };
                if self.worker.requests.send(request).is_err() {
                    form.preview_loading = false;
                    form.preview_error = Some("Preview worker is unavailable".into());
                }
            }
            let media_request =
                media_kind.map(|kind| (form.target.clone(), entry.path.clone(), kind));
            self.focus = Focus::Recap;
            self.file_manager = Some(form);
            if let Some((target, path, kind)) = media_request {
                self.request_media_preview(target, path, kind);
            }
        }
    }

    fn download_selected_file(&mut self, form: &FileManagerForm) {
        let Some(entry) = form.entries.get(form.selected) else {
            return;
        };
        if entry.kind == FileEntryKind::Directory {
            self.status_message = "Select a regular file to download".into();
            return;
        }
        let request = Request::DownloadFile {
            target: form.target.clone(),
            remote_path: entry.path.clone(),
            local_directory: default_download_directory(),
            total_size: entry.size,
        };
        if self.worker.requests.send(request).is_ok() {
            self.busy_operations += 1;
            self.status_message = format!("Downloading {}...", entry.name);
        }
    }

    fn upload_dropped_files(&mut self, form: &FileManagerForm, text: &str) {
        let local_paths = dropped_file_paths(text);
        if local_paths.is_empty() {
            self.status_message = "Drop or paste one or more local file paths".into();
            return;
        }
        let request = Request::UploadFiles {
            target: form.target.clone(),
            local_paths,
            remote_directory: form.path.clone(),
        };
        if self.worker.requests.send(request).is_ok() {
            self.busy_operations += 1;
            self.status_message = "Uploading dropped files...".into();
        }
    }

    fn submit_search(&mut self, mut form: SearchForm) {
        let query = form.query.trim().to_string();
        if query.is_empty() {
            form.error = Some("Enter text to search all agent history".into());
            self.modal = Some(Modal::Search(form));
            return;
        }
        let sessions: Vec<_> = self
            .sessions
            .iter()
            // Backup-only entries have no history on their machine to grep; the
            // same text is reachable through the cross-machine hits below.
            .filter(|session| !self.is_recoverable(&session.target_id, &session.id))
            .filter_map(|session| {
                self.target(&session.target_id)
                    .cloned()
                    .map(|target| (target, session.clone()))
            })
            .collect();
        form.query = query.clone();
        form.submitted_query = query.clone();
        form.results.clear();
        form.result_rows.clear();
        form.selected = 0;
        form.loading = true;
        form.error = None;
        if self
            .worker
            .requests
            .send(Request::Search { query, sessions })
            .is_err()
        {
            form.loading = false;
            form.error = Some("Search worker is unavailable".into());
        }
        self.modal = Some(Modal::Search(form));
    }

    fn maybe_auto_submit_search(&mut self) {
        let should_submit = matches!(self.modal.as_ref(), Some(Modal::Search(form))
            if !form.loading
                && form.query.trim().chars().count() >= 2
                && form.submitted_query != form.query.trim()
                && form.edited_at.elapsed() >= Duration::from_millis(350));
        if !should_submit {
            return;
        }
        if let Some(Modal::Search(form)) = self.modal.take() {
            self.submit_search(form);
        }
    }

    /// Debounced cross-machine history search feeding the resume modal's panel.
    /// Runs against the local backup, so it is synchronous and needs no worker.
    fn maybe_search_resume_history(&mut self) {
        let ready = matches!(self.modal.as_ref(), Some(Modal::Resume(form))
            if form.history_active()
                && form.searched_query != form.query.trim()
                && form
                    .search_edited_at
                    .is_some_and(|at| at.elapsed() >= Duration::from_millis(250)));
        if !ready {
            return;
        }
        let Some(Modal::Resume(form)) = self.modal.as_mut() else {
            return;
        };
        let query = form.query.trim().to_string();
        form.searched_query = query.clone();
        form.history_hits = backup_search_hits(&query, 50);
        form.history_selected = 0;
    }

    /// Build the initial prompt that references a backed-up conversation from
    /// (possibly) another machine. Embeds a transcript excerpt — the source file
    /// is not on this machine — and warns the agent it did not run here.
    fn cross_machine_reference_prompt(&self, launch: &LaunchForm, hit: &CrossMachineHit) -> String {
        // Compare machines, not raw aliases: hit.target_id is a stable machine
        // key, so canonicalise the launch target's alias the same way before
        // deciding whether the referenced conversation ran on a different box.
        let launch_machine = backup_machine_key_for_alias(&launch.target.id);
        let same_machine = hit.target_id == launch_machine;
        let mut prompt = String::new();
        if same_machine {
            prompt.push_str(&format!(
                "Reference a previous {} conversation from this machine as context for the work below.",
                hit.kind
            ));
        } else {
            prompt.push_str(&format!(
                "IMPORTANT: the conversation referenced below ran on a DIFFERENT machine ({}), \
                 not the machine you are running on now ({}). Its files, paths, and environment \
                 may not exist here — treat it purely as reference context, verify anything before \
                 relying on it, and do not assume the referenced workspace is present.",
                hit.target_id, launch_machine
            ));
        }
        if !hit.title.trim().is_empty() {
            prompt.push_str(&format!(
                "\n\nReferenced conversation: {}",
                hit.title.trim()
            ));
        }
        let transcript = backup_session_transcript(&hit.target_id, &hit.session_id, 24_000);
        if !transcript.is_empty() {
            prompt.push_str("\n\n--- referenced transcript (excerpt) ---\n");
            prompt.push_str(&transcript);
            prompt.push_str("\n--- end of referenced transcript ---");
        }
        prompt.push_str(
            "\n\nUse the above as reference, then continue the task in the current workspace.",
        );
        prompt
    }

    fn open_search_result(&mut self, result: SearchResult) {
        let Some(target_index) = self
            .targets
            .iter()
            .position(|target| target.target.id == result.target_id)
        else {
            self.set_error("Search result machine is no longer available");
            return;
        };
        if !self
            .sessions
            .iter()
            .any(|session| session.id == result.session_id)
        {
            self.set_error("Search result session is no longer available");
            return;
        }
        self.set_selected_target(target_index);
        if result.dead && !self.state.show_archived {
            self.state.show_archived = true;
            self.persist_state();
        }
        self.select_session(result.session_id);
        self.focus = Focus::Recap;
        if !result.dead {
            self.activate_terminal();
        }
        self.status_message = format!("Opened {} search match", result.match_kind);
    }

    fn open_kill_confirmation(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        self.modal = Some(Modal::ConfirmKill {
            session_id: session.id.clone(),
            label: session.display_label().into(),
            archive: !session.dead
                && session.kind != AgentKind::Terminal
                && !is_temporary_session_id(&session.id),
        });
    }

    fn open_rename_agent(&mut self) {
        let Some(session) = self.selected_session() else {
            self.status_message = "Select an agent to rename".into();
            return;
        };
        self.modal = Some(Modal::RenameAgent {
            session_id: session.id.clone(),
            value: session.label.clone(),
        });
    }

    fn submit_rename_agent(&mut self, session_id: String, value: String) {
        let name = value.trim().to_string();
        if name.is_empty() {
            self.state.session_labels.remove(&session_id);
        } else {
            self.state
                .session_labels
                .insert(session_id.clone(), name.clone());
        }
        self.persist_state();
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.label = name.clone();
        }
        self.status_message = if name.is_empty() {
            "Agent name cleared".into()
        } else {
            format!("Agent renamed to '{name}'")
        };
    }

    /// Overlay custom agent names (keyed by session id) onto the live sessions.
    fn apply_session_labels(&mut self) {
        for session in &mut self.sessions {
            if let Some(custom) = self.state.session_labels.get(&session.id) {
                session.label = custom.clone();
            }
        }
    }

    fn handle_modal(&mut self, key: KeyEvent, modal: Modal) -> Action {
        match modal {
            Modal::Help(mut form) => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    form.offset = form.offset.saturating_sub(1);
                    self.modal = Some(Modal::Help(form));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    form.offset = form.offset.saturating_add(1).min(HELP_CONTENT_ROWS - 1);
                    self.modal = Some(Modal::Help(form));
                }
                KeyCode::PageUp => {
                    form.offset = form.offset.saturating_sub(8);
                    self.modal = Some(Modal::Help(form));
                }
                KeyCode::PageDown => {
                    form.offset = form.offset.saturating_add(8).min(HELP_CONTENT_ROWS - 1);
                    self.modal = Some(Modal::Help(form));
                }
                KeyCode::Home => {
                    form.offset = 0;
                    self.modal = Some(Modal::Help(form));
                }
                KeyCode::End => {
                    form.offset = HELP_CONTENT_ROWS - 1;
                    self.modal = Some(Modal::Help(form));
                }
                _ => self.modal = Some(Modal::Help(form)),
            },
            Modal::Settings(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Tab | KeyCode::Down => {
                    form.selected = clamped_index(form.selected, form.focus_len(), 1);
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.selected = clamped_index(form.selected, form.focus_len(), -1);
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                // Enter on an action runs it and leaves the panel; on a field
                // it saves, as it always has.
                KeyCode::Enter | KeyCode::Char('s')
                    if key.code == KeyCode::Enter
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    match (form.selected_action(), form.scope.clone()) {
                        (Some(FORCE_UPDATE_ACTION), SettingsScope::Host(target_id)) => {
                            self.force_update_machine(&target_id);
                        }
                        (Some(action), SettingsScope::Host(target_id)) => {
                            match install_action_kind(action) {
                                Some(kind) => self.install_runtime(&target_id, kind),
                                None => self.apply_settings(form),
                            }
                        }
                        _ => self.apply_settings(form),
                    }
                }
                KeyCode::Backspace => {
                    if let Some(index) = form.selected_value() {
                        form.values[index].pop();
                    }
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(index) = form.selected_value() {
                        form.values[index].clear();
                    }
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(index) = form.selected_value() {
                        form.values[index].push(character);
                    }
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                _ => self.modal = Some(Modal::Settings(form)),
            },
            Modal::Board(mut form) => {
                if self.handle_board_key(key, &mut form) {
                    self.modal = Some(Modal::Board(form));
                }
            }
            Modal::Search(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::BackTab if !form.results.is_empty() => {
                    form.selected = clamped_index(form.selected, form.results.len(), -1);
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Down | KeyCode::Tab if !form.results.is_empty() => {
                    form.selected = clamped_index(form.selected, form.results.len(), 1);
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Enter
                    if !form.loading
                        && !form.results.is_empty()
                        && form.submitted_query == form.query =>
                {
                    let result = form.results[form.selected].clone();
                    self.open_search_result(result);
                }
                KeyCode::Enter => self.submit_search(form),
                KeyCode::Backspace => {
                    form.query.pop();
                    mark_search_edited(&mut form);
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.query.clear();
                    mark_search_edited(&mut form);
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.query.push(character);
                    mark_search_edited(&mut form);
                    self.modal = Some(Modal::Search(form));
                }
                _ => self.modal = Some(Modal::Search(form)),
            },
            Modal::PathPicker(mut form) => match key.code {
                KeyCode::Esc => self.modal = Some(Modal::Launch(form.launch)),
                KeyCode::Up if !matched_directories(&form).is_empty() => {
                    form.selected =
                        clamped_index(form.selected, matched_directories(&form).len(), -1);
                    self.modal = Some(Modal::PathPicker(form));
                }
                KeyCode::Down if !matched_directories(&form).is_empty() => {
                    form.selected =
                        clamped_index(form.selected, matched_directories(&form).len(), 1);
                    self.modal = Some(Modal::PathPicker(form));
                }
                KeyCode::Left if !form.loading => {
                    form.path = parent_path(&form.path);
                    form.directories.clear();
                    form.query.clear();
                    form.selected = 0;
                    self.request_directory(form);
                }
                KeyCode::Right if !form.loading && !matched_directories(&form).is_empty() => {
                    let directories = matched_directories(&form);
                    form.path = child_path(&form.path, &directories[form.selected]);
                    form.directories.clear();
                    form.query.clear();
                    form.selected = 0;
                    self.request_directory(form);
                }
                KeyCode::F(5) if !form.loading => self.request_directory(form),
                KeyCode::Backspace => {
                    form.query.pop();
                    form.selected = 0;
                    self.modal = Some(Modal::PathPicker(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.query.clear();
                    form.selected = 0;
                    self.modal = Some(Modal::PathPicker(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.query.push(character);
                    form.selected = 0;
                    self.modal = Some(Modal::PathPicker(form));
                }
                KeyCode::Enter if !form.loading && form.error.is_none() => {
                    form.launch.path = form.path;
                    self.prepare_launch(form.launch);
                }
                _ => self.modal = Some(Modal::PathPicker(form)),
            },
            // Typing a query expands the cross-machine reference panel and its
            // list takes navigation; otherwise the same-machine candidate list
            // behaves as before. Arrow keys navigate (j/k route into the search
            // box so they can be typed).
            Modal::Resume(mut form) => match key.code {
                KeyCode::Esc => {
                    if form.history_active() {
                        form.query.clear();
                        form.searched_query.clear();
                        form.history_hits.clear();
                        form.history_selected = 0;
                        self.modal = Some(Modal::Resume(form));
                    } else {
                        self.modal = Some(Modal::Launch(form.launch));
                    }
                }
                KeyCode::Left if !form.history_active() => {
                    self.modal = Some(Modal::Launch(form.launch))
                }
                KeyCode::Backspace => {
                    form.query.pop();
                    form.search_edited_at = Some(Instant::now());
                    self.modal = Some(Modal::Resume(form));
                }
                KeyCode::Up | KeyCode::Down => {
                    let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                    if form.history_active() {
                        if !form.history_hits.is_empty() {
                            form.history_selected = clamped_index(
                                form.history_selected,
                                form.history_hits.len(),
                                delta,
                            );
                        }
                    } else if !form.loading {
                        form.selected =
                            clamped_index(form.selected, form.candidates.len() + 1, delta);
                    }
                    self.modal = Some(Modal::Resume(form));
                }
                KeyCode::Enter if form.history_active() => {
                    match form.history_hits.get(form.history_selected).cloned() {
                        Some(hit) => {
                            let prompt = self.cross_machine_reference_prompt(&form.launch, &hit);
                            self.confirm_or_submit_launch(form.launch, None, Some(prompt));
                        }
                        None => self.modal = Some(Modal::Resume(form)),
                    }
                }
                KeyCode::Enter if form.selected == 0 => {
                    self.confirm_or_submit_launch(form.launch, None, None)
                }
                KeyCode::Enter if !form.loading => {
                    let candidate = form
                        .selected
                        .checked_sub(1)
                        .and_then(|index| form.candidates.get(index))
                        .cloned();
                    match candidate {
                        Some(candidate) if candidate.kind != form.launch.kind => {
                            self.modal = Some(Modal::ConfirmHistoryReference { form, candidate });
                        }
                        Some(candidate) => {
                            self.confirm_or_submit_launch(form.launch, Some(candidate.id), None)
                        }
                        None => self.confirm_or_submit_launch(form.launch, None, None),
                    }
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.query.push(character);
                    form.search_edited_at = Some(Instant::now());
                    self.modal = Some(Modal::Resume(form));
                }
                _ => self.modal = Some(Modal::Resume(form)),
            },
            Modal::ConfirmKill {
                session_id,
                label,
                archive,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter if archive => self.archive_session(&session_id),
                KeyCode::Char('y') | KeyCode::Enter => self.delete_session(&session_id),
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => {
                    self.modal = Some(Modal::ConfirmKill {
                        session_id,
                        label,
                        archive,
                    })
                }
            },
            Modal::ConfirmInstall {
                launch,
                resume_id,
                initial_prompt,
                remove_archive_session_id,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.install_and_launch(
                    launch,
                    resume_id,
                    initial_prompt,
                    remove_archive_session_id,
                ),
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => {
                    self.modal = Some(Modal::ConfirmInstall {
                        launch,
                        resume_id,
                        initial_prompt,
                        remove_archive_session_id,
                    })
                }
            },
            Modal::ConfirmArchivedResume {
                source_session_id,
                launch,
                resume_id,
                mut remove_archive,
            } => match key.code {
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                    remove_archive = !remove_archive;
                    self.modal = Some(Modal::ConfirmArchivedResume {
                        source_session_id,
                        launch,
                        resume_id,
                        remove_archive,
                    });
                }
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.state.remove_archive_after_resume = remove_archive;
                    self.persist_state();
                    let archive = remove_archive.then_some(source_session_id);
                    self.confirm_or_submit_launch_with_archive(
                        launch,
                        Some(resume_id),
                        None,
                        archive,
                    );
                }
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => {
                    self.modal = Some(Modal::ConfirmArchivedResume {
                        source_session_id,
                        launch,
                        resume_id,
                        remove_archive,
                    });
                }
            },
            Modal::ConfirmHistoryReference { form, candidate } => match key.code {
                KeyCode::Char('r') | KeyCode::Enter => {
                    let prompt = history_reference_prompt(&form.launch, &candidate);
                    self.confirm_or_submit_launch(form.launch, None, Some(prompt));
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.modal = Some(Modal::Resume(form));
                }
                _ => self.modal = Some(Modal::ConfirmHistoryReference { form, candidate }),
            },
            Modal::LegacyFallback { target_id, detail } => match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.modal = Some(Modal::LegacyFallback { target_id, detail }),
            },
            Modal::UpdatePrompt(prompt) => match key.code {
                #[cfg(feature = "controller")]
                KeyCode::Char('y') | KeyCode::Enter => self.start_update_download(prompt),
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.set_background_status(format!(
                        "muxloom {} available — run `muxloom update` when ready",
                        prompt.latest
                    ));
                }
                _ => self.modal = Some(Modal::UpdatePrompt(prompt)),
            },
            Modal::ConfirmForcedUpdate {
                target,
                working,
                terminals,
                resumable,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.begin_forced_update(target),
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.set_background_status(format!(
                        "{}: forced daemon update declined; it will ask again later",
                        target.id
                    ));
                }
                _ => {
                    self.modal = Some(Modal::ConfirmForcedUpdate {
                        target,
                        working,
                        terminals,
                        resumable,
                    })
                }
            },
            Modal::Launch(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Tab | KeyCode::Down => {
                    form.field = next_field(form.field);
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.field = previous_field(form.field);
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Left if form.field == LaunchField::Kind => {
                    form.kind = self.step_kind(&form.target.id, form.kind, false);
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Right | KeyCode::Char(' ') if form.field == LaunchField::Kind => {
                    form.kind = self.step_kind(&form.target.id, form.kind, true);
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Enter => match form.field {
                    LaunchField::Kind => {
                        form.field = LaunchField::Path;
                        self.modal = Some(Modal::Launch(form));
                    }
                    LaunchField::Path => self.open_path_picker(form),
                    LaunchField::Label => self.prepare_launch(form),
                },
                KeyCode::Backspace => {
                    if let Some(field) = active_text(&mut form) {
                        field.pop();
                    }
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(field) = active_text(&mut form) {
                        field.clear();
                    }
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(field) = active_text(&mut form) {
                        field.push(character);
                    }
                    self.modal = Some(Modal::Launch(form));
                }
                _ => self.modal = Some(Modal::Launch(form)),
            },
            // The name field takes every printable key, space included, so the
            // scope checkboxes answer to Enter as well as to space and the
            // runtime answers to the arrows alone.
            Modal::Moderator(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Tab | KeyCode::Down => {
                    form.selected = clamped_index(form.selected, form.rows().len(), 1);
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.selected = clamped_index(form.selected, form.rows().len(), -1);
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::Left if form.row() == ModeratorRow::Kind => {
                    form.kind = step_within(&self.moderator_kinds(), form.kind, false);
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::Right | KeyCode::Char(' ') if form.row() == ModeratorRow::Kind => {
                    form.kind = step_within(&self.moderator_kinds(), form.kind, true);
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::Enter if form.row() == ModeratorRow::Kind => {
                    form.selected = 1;
                    self.modal = Some(Modal::Moderator(form));
                }
                // A header toggles its whole group, which is how "all but two"
                // gets entered without walking the list.
                KeyCode::Enter | KeyCode::Char(' ')
                    if !matches!(form.row(), ModeratorRow::Kind | ModeratorRow::Name) =>
                {
                    match form.row() {
                        ModeratorRow::Machine(index) => {
                            form.machines[index].selected = !form.machines[index].selected;
                        }
                        ModeratorRow::Agent(index) => {
                            form.agents[index].selected = !form.agents[index].selected;
                        }
                        ModeratorRow::MachinesHeader => toggle_all(&mut form.machines),
                        ModeratorRow::AgentsHeader => {
                            let visible = form.visible_agents();
                            let all = visible.iter().all(|&index| form.agents[index].selected);
                            for index in visible {
                                form.agents[index].selected = !all;
                            }
                        }
                        ModeratorRow::Kind | ModeratorRow::Name => {}
                    }
                    // A machine that just went out of scope took its agents
                    // off the column with it, so the cursor may be past the
                    // end of a list that is shorter than it was a key ago.
                    form.selected = form.selected.min(form.rows().len().saturating_sub(1));
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::Enter => self.launch_moderator(form),
                KeyCode::Backspace if form.row() == ModeratorRow::Name => {
                    form.name.pop();
                    form.error = None;
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::Char('u')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && form.row() == ModeratorRow::Name =>
                {
                    form.name.clear();
                    self.modal = Some(Modal::Moderator(form));
                }
                KeyCode::Char(character)
                    if form.row() == ModeratorRow::Name
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.name.push(character);
                    form.error = None;
                    self.modal = Some(Modal::Moderator(form));
                }
                _ => self.modal = Some(Modal::Moderator(form)),
            },
            // Every printable key names the chat, so the runtime moved off the
            // letters it used to answer to and onto the arrows alone.
            Modal::Temporal(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Left | KeyCode::BackTab => {
                    form.kind = self.step_agent_kind(&form.target.id, form.kind, false);
                    self.modal = Some(Modal::Temporal(form));
                }
                KeyCode::Right | KeyCode::Tab => {
                    form.kind = self.step_agent_kind(&form.target.id, form.kind, true);
                    self.modal = Some(Modal::Temporal(form));
                }
                KeyCode::Backspace => {
                    form.label.pop();
                    self.modal = Some(Modal::Temporal(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.label.clear();
                    self.modal = Some(Modal::Temporal(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.label.push(character);
                    self.modal = Some(Modal::Temporal(form));
                }
                KeyCode::Enter => self.launch_temporary_agent(form),
                _ => self.modal = Some(Modal::Temporal(form)),
            },
            Modal::PortForward(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('p') if form.selected >= PortForwardForm::FIELD_COUNT => {}
                KeyCode::Tab | KeyCode::Down => {
                    form.selected = clamped_index(form.selected, form.row_count(), 1);
                    form.error = None;
                    self.modal = Some(Modal::PortForward(form));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.selected = clamped_index(form.selected, form.row_count(), -1);
                    form.error = None;
                    self.modal = Some(Modal::PortForward(form));
                }
                KeyCode::Left | KeyCode::Right
                    if form.selected == 1 && !form.detected_ports.is_empty() =>
                {
                    let old_remote = form.remote_port.clone();
                    let current = old_remote.parse::<u16>().ok();
                    let index = current
                        .and_then(|port| {
                            form.detected_ports
                                .iter()
                                .position(|candidate| *candidate == port)
                        })
                        .unwrap_or(0);
                    let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                    let next = clamped_index(index, form.detected_ports.len(), delta);
                    form.remote_port = form.detected_ports[next].to_string();
                    if form.local_port.trim().is_empty() || form.local_port == old_remote {
                        form.local_port = form.remote_port.clone();
                    }
                    form.error = None;
                    self.modal = Some(Modal::PortForward(form));
                }
                KeyCode::Char('d') if form.active_index().is_some() => self.stop_port_forward(form),
                KeyCode::Enter if form.active_index().is_none() => self.start_port_forward(form),
                KeyCode::Backspace if form.selected < PortForwardForm::FIELD_COUNT => {
                    port_forward_value(&mut form).pop();
                    form.error = None;
                    self.modal = Some(Modal::PortForward(form));
                }
                KeyCode::Char('u')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && form.selected < PortForwardForm::FIELD_COUNT =>
                {
                    port_forward_value(&mut form).clear();
                    form.error = None;
                    self.modal = Some(Modal::PortForward(form));
                }
                KeyCode::Char(character)
                    if form.selected < PortForwardForm::FIELD_COUNT
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    port_forward_value(&mut form).push(character);
                    form.error = None;
                    self.modal = Some(Modal::PortForward(form));
                }
                _ => self.modal = Some(Modal::PortForward(form)),
            },
            Modal::RenameAgent {
                session_id,
                mut value,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_rename_agent(session_id, value),
                KeyCode::Backspace => {
                    value.pop();
                    self.modal = Some(Modal::RenameAgent { session_id, value });
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    value.clear();
                    self.modal = Some(Modal::RenameAgent { session_id, value });
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    value.push(character);
                    self.modal = Some(Modal::RenameAgent { session_id, value });
                }
                _ => self.modal = Some(Modal::RenameAgent { session_id, value }),
            },
        }
        Action::Continue
    }

    fn apply_settings(&mut self, mut form: SettingsForm) {
        let parsed = (|| -> Result<Config, String> {
            let mut config = self.config.clone();
            match &form.scope {
                SettingsScope::Global => {
                    config.refresh_interval_ms =
                        parse_setting(&form.values[0], "Refresh interval (ms)")?;
                    if config.refresh_interval_ms < 500 {
                        return Err("Refresh interval must be at least 500 ms".into());
                    }
                    config.ssh_config = form.values[1].clone();
                    config.environment = form.values[2].clone();
                    let mut index = 3;
                    for kind in AgentKind::agents() {
                        let command = form.values[index].clone();
                        let args = parse_shell_list(
                            &form.values[index + 1],
                            &format!("{} args", agent_section(kind)),
                        )?;
                        let entry = config.agents.get_mut(kind);
                        entry.command = command;
                        entry.args = args;
                        index += 2;
                    }
                    config.agents.terminal.command = form.values[index].clone();
                    config.update_prompt = form.values[index + 1].trim().to_string();
                    config.update_channel = form.values[index + 2].trim().to_string();
                    config.touch = form.values[index + 3].trim().to_string();
                }
                SettingsScope::Host(target_id) => {
                    // The form edits the common fields; everything it no
                    // longer shows — tunnels, companion overrides, installs,
                    // sync files, attention patterns — keeps whatever the
                    // config file says for this host. Command overrides are
                    // seeded from the effective config so those hidden parts
                    // ride along unchanged.
                    let mut host = config.hosts.get(target_id).cloned().unwrap_or_default();
                    host.environment = Some(form.values[0].clone());
                    let mut index = 1;
                    for kind in AgentKind::agents() {
                        let mut entry = config.command_for(target_id, kind).clone();
                        entry.command = form.values[index].clone();
                        entry.args = parse_shell_list(
                            &form.values[index + 1],
                            &format!("{} args", agent_section(kind)),
                        )?;
                        *host.slot_mut(kind) = Some(entry);
                        index += 2;
                    }
                    let mut terminal = config.command_for(target_id, AgentKind::Terminal).clone();
                    terminal.command = form.values[index].clone();
                    host.terminal = Some(terminal);
                    config.hosts.insert(target_id.clone(), host);
                }
            }
            config
                .environment_for(match &form.scope {
                    SettingsScope::Global => LOCAL_TARGET_ID,
                    SettingsScope::Host(target_id) => target_id,
                })
                .map_err(|error| error.to_string())?;
            config.validate().map_err(|error| error.to_string())?;
            let effective_host = match &form.scope {
                SettingsScope::Global => None,
                SettingsScope::Host(target_id) => Some(target_id.as_str()),
            };
            // A runtime with no command has nothing to probe for and nothing
            // to launch; only the terminal may be blank, where it means the
            // user's own shell.
            for kind in AgentKind::agents() {
                let empty = effective_host
                    .map(|host| config.command_for(host, kind))
                    .unwrap_or(config.agents.get(kind))
                    .command
                    .trim()
                    .is_empty();
                if empty {
                    return Err(format!("{} command cannot be empty", agent_section(kind)));
                }
            }
            Ok(config)
        })();

        let config = match parsed {
            Ok(config) => config,
            Err(error) => {
                form.error = Some(error);
                self.modal = Some(Modal::Settings(form));
                return;
            }
        };
        let ssh_hosts = match ssh_config::load_hosts(&config.ssh_config_path()) {
            Ok(hosts) => hosts,
            Err(error) => {
                form.error = Some(short_error(&error.to_string()));
                self.modal = Some(Modal::Settings(form));
                return;
            }
        };
        if let Err(error) = config.save(&self.config_path) {
            form.error = Some(short_error(&error.to_string()));
            self.modal = Some(Modal::Settings(form));
            return;
        }
        self.config = config;
        let mut target_defs = vec![Target::local()];
        target_defs.extend(
            ssh_hosts
                .into_iter()
                .filter(|alias| alias != LOCAL_TARGET_ID)
                .map(Target::ssh),
        );
        self.targets = target_defs
            .into_iter()
            .map(|target| {
                self.targets
                    .iter()
                    .find(|existing| existing.target.id == target.id)
                    .cloned()
                    .unwrap_or_else(|| {
                        let enabled = self.state.enabled_hosts.contains(&target.id);
                        TargetStatus::new(target, enabled)
                    })
            })
            .collect();
        let known_targets: HashSet<_> = self
            .targets
            .iter()
            .map(|target| target.target.id.clone())
            .collect();
        self.sessions
            .retain(|session| known_targets.contains(&session.target_id));
        self.ensure_target_visible();
        self.ensure_session_selection();
        self.worker = Worker::start(Runtime::new(&self.config));
        self.pending_scans.clear();
        self.pending_activity_refreshes.clear();
        self.pending_capture = None;
        self.status_message = match &form.scope {
            SettingsScope::Global => {
                format!(
                    "Global configuration saved to {}",
                    self.config_path.display()
                )
            }
            SettingsScope::Host(target_id) => format!(
                "Configuration for {target_id} saved to {}",
                self.config_path.display()
            ),
        };
        debug::log("config", format!("saved {}", self.config_path.display()));
        self.refresh_enabled_manual();
    }

    fn submit_launch(
        &mut self,
        form: LaunchForm,
        resume_id: Option<String>,
        initial_prompt: Option<String>,
        remove_archive_session_id: Option<String>,
    ) {
        if form.path.trim().is_empty() {
            self.status_message = "Launch cancelled: working directory is required".into();
            return;
        }
        let command = self.config.command_for(&form.target.id, form.kind).clone();
        let environment = self
            .config
            .environment_for(&form.target.id)
            .unwrap_or_default();
        // Remember this directory and runtime as the machine's defaults for the
        // next launch: the pair you used last is the pair you usually want next.
        // A moderator's folder and a temporary chat's scratch folder are
        // muxloom's own and belong to no workflow, so starting one must not aim
        // the machine's next ordinary launch at it.
        if !form.temporary
            && !crate::moderator::is_moderator_path(&self.moderator_state_dir, &form.path)
        {
            self.state
                .last_launch_dirs
                .insert(form.target.id.clone(), form.path.clone());
        }
        self.state
            .last_launch_kinds
            .insert(form.target.id.clone(), form.kind);
        self.persist_state();
        let request = LaunchRequest {
            target: form.target,
            kind: form.kind,
            path: form.path,
            label: form.label,
            temporary: form.temporary,
            resume_id,
            initial_prompt,
            parent: None,
        };
        if self
            .worker
            .requests
            .send(Request::Launch {
                request,
                command,
                environment,
                remove_archive_session_id,
            })
            .is_ok()
        {
            self.busy_operations += 1;
            self.status_message = "Launching agent...".into();
        }
    }

    fn confirm_or_submit_launch(
        &mut self,
        form: LaunchForm,
        resume_id: Option<String>,
        initial_prompt: Option<String>,
    ) {
        self.confirm_or_submit_launch_with_archive(form, resume_id, initial_prompt, None);
    }

    fn confirm_or_submit_launch_with_archive(
        &mut self,
        form: LaunchForm,
        resume_id: Option<String>,
        initial_prompt: Option<String>,
        remove_archive_session_id: Option<String>,
    ) {
        let available = self
            .targets
            .iter()
            .find(|target| target.target.id == form.target.id)
            .is_some_and(|target| target.probe.has(form.kind));
        if available || form.kind == AgentKind::Terminal {
            self.submit_launch(form, resume_id, initial_prompt, remove_archive_session_id);
        } else {
            self.modal = Some(Modal::ConfirmInstall {
                launch: form,
                resume_id,
                initial_prompt,
                remove_archive_session_id,
            });
        }
    }

    /// Put one runtime on a machine from its settings panel. The panel closes
    /// so the footer gauge, which every install already reports into, is
    /// visible; when it lands the machine is rescanned and the runtime joins
    /// the launch picker.
    fn install_runtime(&mut self, target_id: &str, kind: AgentKind) {
        let Some(target) = self.target(target_id).cloned() else {
            return;
        };
        let command = self.config.command_for(target_id, kind).clone();
        if !kind.has_release_download() && command.install.trim().is_empty() {
            self.set_error(format!(
                "No install command configured for {kind} - set agents.{}.install",
                kind.as_str()
            ));
            return;
        }
        let environment = self.config.environment_for(target_id).unwrap_or_default();
        let request = Request::Install {
            target: target.clone(),
            kind,
            command,
            environment,
        };
        if self.worker.requests.send(request).is_ok() {
            self.busy_operations += 1;
            self.status_message = format!("Installing {kind} on {}...", target.label);
        }
    }

    fn install_and_launch(
        &mut self,
        launch: LaunchForm,
        resume_id: Option<String>,
        initial_prompt: Option<String>,
        remove_archive_session_id: Option<String>,
    ) {
        let command = self
            .config
            .command_for(&launch.target.id, launch.kind)
            .clone();
        let environment = self
            .config
            .environment_for(&launch.target.id)
            .unwrap_or_default();
        let request = Request::Install {
            target: launch.target.clone(),
            kind: launch.kind,
            command,
            environment,
        };
        if self.worker.requests.send(request).is_ok() {
            self.pending_install_launch = Some(PendingInstallLaunch {
                launch: launch.clone(),
                resume_id,
                initial_prompt,
                remove_archive_session_id,
            });
            self.busy_operations += 1;
            self.status_message =
                format!("Installing {} on {}...", launch.kind, launch.target.label);
        }
    }

    fn archive_session(&mut self, session_id: &str) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if self.is_recoverable(&session.target_id, session_id) {
            self.status_message = "That history is already only in the local backup".into();
            return;
        }
        let Some(target) = self.target(&session.target_id).cloned() else {
            return;
        };
        if self
            .worker
            .requests
            .send(Request::Archive {
                target,
                session_id: session_id.into(),
            })
            .is_ok()
        {
            self.busy_operations += 1;
            self.status_message = "Stopping agent and preserving it in Archived...".into();
        }
    }

    fn delete_session(&mut self, session_id: &str) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if self.is_recoverable(&session.target_id, session_id) {
            // The machine has nothing to close, and the backup is the last copy
            // of this conversation: it is not something a keystroke should drop.
            self.status_message =
                "Kept: this history exists only in the local backup, so it is not deleted here"
                    .into();
            return;
        }
        let Some(target) = self.target(&session.target_id).cloned() else {
            return;
        };
        if self
            .worker
            .requests
            .send(Request::Kill {
                target,
                session_id: session_id.into(),
            })
            .is_ok()
        {
            self.busy_operations += 1;
            self.status_message = "Closing agent session...".into();
        }
    }

    fn remove_resumed_archive(&mut self, target_id: &str, session_id: String) {
        let Some(target) = self.target(target_id).cloned() else {
            self.status_message =
                "Agent resumed, but its previous Archived entry could not be located".into();
            return;
        };
        if self
            .worker
            .requests
            .send(Request::RemoveResumedArchive { target, session_id })
            .is_ok()
        {
            self.busy_operations += 1;
            self.status_message = "Agent resumed; removing the previous Archived entry...".into();
        } else {
            self.status_message =
                "Agent resumed, but the cleanup worker was unavailable; the previous Archived entry was kept"
                    .into();
        }
    }

    fn on_divider(&mut self, column: u16, row: u16) -> bool {
        if self
            .pane_layout
            .portrait_terminal_divider
            .is_some_and(|area| near_horizontal_divider(area, column, row))
        {
            self.dragging = Some(DragDivider::PortraitTerminal);
            return true;
        }
        if self
            .pane_layout
            .portrait_machine_divider
            .is_some_and(|area| near_divider(area, column, row))
        {
            self.dragging = Some(DragDivider::PortraitMachines);
            return true;
        }
        if self
            .pane_layout
            .machine_divider
            .is_some_and(|area| near_divider(area, column, row))
        {
            self.dragging = Some(DragDivider::Machines);
            return true;
        }
        if self
            .pane_layout
            .agents_divider
            .is_some_and(|area| near_divider(area, column, row))
        {
            self.dragging = Some(DragDivider::Agents);
            return true;
        }
        false
    }

    /// True when the file browser has taken over the terminal pane, which is
    /// the one layout that sizes the browser against the whole window.
    pub(crate) fn file_manager_fills_the_terminal_pane(&self) -> bool {
        self.file_manager
            .as_ref()
            .is_some_and(|form| form.origin == FileManagerOrigin::TerminalPane)
    }

    fn drag_divider(&mut self, column: u16, row: u16) {
        match self.dragging {
            Some(DragDivider::Machines) => {
                let Some(area) = self.pane_layout.machines else {
                    return;
                };
                self.state.machine_width = column
                    .saturating_sub(area.x)
                    .saturating_add(1)
                    .clamp(16, 52);
            }
            Some(DragDivider::Agents) => {
                let Some(area) = self.pane_layout.agents else {
                    return;
                };
                let width = column.saturating_sub(area.x).saturating_add(1);
                // Each layout clamps this width differently, and a drag that
                // clamps to a range the layout does not use leaves the divider
                // stuck somewhere the pointer never was.
                if self.file_manager_fills_the_terminal_pane() {
                    let row = area
                        .width
                        .saturating_add(self.pane_layout.recap.map_or(0, |recap| recap.width));
                    self.state.file_width = width.clamp(12, row.saturating_sub(24).max(12));
                } else if self.file_manager.is_some() {
                    self.state.file_width = width.clamp(22, 72);
                } else {
                    self.state.agents_width = width.clamp(24, 72);
                }
            }
            Some(DragDivider::PortraitMachines) => {
                let (Some(machines), Some(agents)) =
                    (self.pane_layout.machines, self.pane_layout.agents)
                else {
                    return;
                };
                let total = machines.width.saturating_add(agents.width).max(1);
                let display_percent = column
                    .saturating_sub(machines.x)
                    .saturating_add(1)
                    .saturating_mul(100)
                    / total;
                self.state.portrait_machine_percent = display_percent.clamp(25, 75);
            }
            Some(DragDivider::PortraitTerminal) => {
                let Some(recap) = self.pane_layout.recap else {
                    return;
                };
                let lower_height = self
                    .pane_layout
                    .machines
                    .or(self.pane_layout.agents)
                    .map_or(0, |area| area.height);
                let total = recap.height.saturating_add(lower_height).max(1);
                self.state.portrait_terminal_percent = row
                    .saturating_sub(recap.y)
                    .saturating_add(1)
                    .saturating_mul(100)
                    .checked_div(total)
                    .unwrap_or(65)
                    .clamp(45, 82);
            }
            None => {}
        }
    }

    fn click_pane(&mut self, column: u16, row: u16) {
        if self
            .terminal_back
            .is_some_and(|area| inside(area, column, row))
        {
            self.release_terminal_input("Returned to agent list");
            self.focus = Focus::Agents;
            return;
        }
        if let Some(area) = self
            .pane_layout
            .machines
            .filter(|area| inside(*area, column, row))
        {
            self.release_terminal_input("Machine selected");
            self.focus = Focus::Machines;
            let mut line = row.saturating_sub(area.y.saturating_add(1));
            let mut hit = None;
            for (machine_row, height) in self
                .machine_rows
                .iter()
                .skip(self.machine_list_state.offset())
            {
                if line < *height {
                    hit = Some((*machine_row, line));
                    break;
                }
                line = line.saturating_sub(*height);
            }
            match hit {
                Some((MachineRow::Machine(target_index), item_line)) => {
                    self.select_machine_row(MachineRow::Machine(target_index));
                    self.ensure_session_selection();
                    let target_id = self.targets[target_index].target.id.clone();
                    // Border + list highlight + two-character state marker put
                    // the rendered `[x]` checkbox in these three columns.
                    let checkbox_start = area.x.saturating_add(5);
                    let checkbox_end = checkbox_start.saturating_add(3);
                    let checkbox_hit =
                        item_line == 0 && (checkbox_start..checkbox_end).contains(&column);
                    if checkbox_hit && self.is_machine_double_click(&target_id) {
                        self.last_machine_click = None;
                        self.toggle_target(target_index);
                    } else if !checkbox_hit {
                        self.last_machine_click = None;
                    }
                }
                // The moderators row has no checkbox to hit: it is not a
                // machine and there is nothing about it to enable.
                Some((MachineRow::Moderators, _)) => {
                    self.select_machine_row(MachineRow::Moderators);
                    self.ensure_session_selection();
                    self.last_machine_click = None;
                }
                None => self.last_machine_click = None,
            }
            return;
        }
        if let Some(area) = self
            .pane_layout
            .agents
            .filter(|area| inside(*area, column, row))
        {
            self.release_terminal_input("Agent selected");
            self.focus = Focus::Agents;
            let mut line = row.saturating_sub(area.y.saturating_add(1));
            let mut hit = None;
            for (row_index, (id, height)) in self
                .agent_rows
                .iter()
                .enumerate()
                .skip(self.agent_list_state.offset())
            {
                if line < *height {
                    hit = Some((row_index, id.clone()));
                    break;
                }
                line = line.saturating_sub(*height);
            }
            if let Some((row_index, id)) = hit {
                if self.archive_row == Some(row_index) {
                    self.toggle_archived();
                } else if let Some(id) = id {
                    self.select_session(id);
                }
            }
            return;
        }
        if self
            .pane_layout
            .recap
            .is_some_and(|area| inside(area, column, row))
        {
            self.focus = Focus::Recap;
            self.activate_terminal();
        }
    }

    fn is_machine_double_click(&mut self, target_id: &str) -> bool {
        const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);
        let now = Instant::now();
        let double_click = self.last_machine_click.as_ref().is_some_and(|click| {
            click.key == target_id && now.saturating_duration_since(click.at) <= DOUBLE_CLICK_WINDOW
        });
        self.last_machine_click = Some(FileClick {
            key: target_id.into(),
            at: now,
        });
        double_click
    }

    fn forward_terminal_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !self.interactive || self.history_offset > 0 {
            return false;
        }
        let Some(area) = self
            .pane_layout
            .recap
            .filter(|area| inside(*area, mouse.column, mouse.row))
        else {
            return false;
        };
        if mouse.column <= area.x
            || mouse.column >= area.x + area.width.saturating_sub(1)
            || mouse.row <= area.y
            || mouse.row >= area.y + area.height.saturating_sub(1)
        {
            return false;
        }
        self.focus = Focus::Recap;
        let column = mouse.column.saturating_sub(area.x + 1);
        let row = mouse.row.saturating_sub(area.y + 1);
        if let Some(terminal) = self.terminal.as_mut() {
            match terminal.write_mouse(mouse, column, row) {
                Ok(forwarded) => forwarded,
                Err(error) => {
                    self.status_message =
                        format!("Mouse input failed: {}", short_error(&error.to_string()));
                    false
                }
            }
        } else {
            false
        }
    }

    fn terminal_cell_at(&self, column: u16, row: u16) -> Option<TerminalPoint> {
        let area = self.pane_layout.recap?;
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        inside(inner, column, row).then_some(TerminalPoint {
            row: row.saturating_sub(inner.y),
            column: column.saturating_sub(inner.x),
        })
    }

    fn begin_terminal_selection(&mut self, column: u16, row: u16) -> bool {
        let Some(point) = self.terminal_cell_at(column, row) else {
            return false;
        };
        self.focus = Focus::Recap;
        if !self.interactive
            && self.history_offset == 0
            && self.selected_session().is_some_and(|session| !session.dead)
        {
            self.activate_terminal();
        }
        self.terminal_selection = Some(TerminalSelection {
            anchor: point,
            cursor: point,
            dragging: true,
        });
        true
    }

    fn update_terminal_selection(&mut self, column: u16, row: u16) {
        let Some(point) = self.terminal_cell_at(column, row) else {
            return;
        };
        if let Some(selection) = self.terminal_selection.as_mut() {
            selection.cursor = point;
            // Announced here rather than on button-down: a plain click selects
            // nothing, and the note used to sit in the footer for good.
            if selection.anchor != point {
                self.status_message = "Selecting terminal text...".into();
            }
        }
    }

    fn finish_terminal_selection(&mut self, mouse: MouseEvent) {
        if let Some(selection) = self.terminal_selection.as_mut() {
            selection.dragging = false;
        }
        // Lifting the button ends the selection, not the copy. Copying here
        // meant a selection could never be looked at before it was taken, and
        // every stray drag overwrote whatever the clipboard was holding.
        if self
            .terminal_selection
            .is_some_and(|selection| selection.anchor != selection.cursor)
        {
            self.status_message = "Selected; right-click to copy, again to paste".into();
            return;
        }
        if self.status_message == "Selecting terminal text..." {
            self.status_message = "Nothing selected".into();
        }

        self.terminal_selection = None;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..mouse
        };
        let forwarded = self.forward_terminal_mouse(down);
        let released = self.forward_terminal_mouse(mouse);
        if !forwarded && !released {
            self.click_pane(mouse.column, mouse.row);
        }
    }

    /// What a right-click over the terminal means. One button carries both
    /// halves of the clipboard, the way a Windows console does, and which half
    /// it took is never in doubt: text on screen is highlighted or it is not.
    /// The copy clears the highlight, so the click after it pastes.
    fn right_click_terminal(&mut self) {
        self.focus = Focus::Recap;
        if self.copy_terminal_selection() {
            self.terminal_selection = None;
            return;
        }
        self.terminal_selection = None;
        self.clipboard_paste = true;
    }

    fn copy_terminal_selection(&mut self) -> bool {
        // Make sure the emulator is at the same scrollback position that is on
        // screen, so a selection made while scrolled back copies what is shown.
        if self.attached_history_is_buffered() {
            let offset = self.history_offset;
            if let Some(terminal) = self.terminal.as_mut() {
                terminal.set_scrollback(offset);
            }
        }
        let Some(text) = self.selected_terminal_text() else {
            return false;
        };
        let characters = text.chars().count();
        self.clipboard_request = Some(text);
        self.status_message = format!("Copied {characters} characters to clipboard");
        true
    }

    fn selected_terminal_text(&mut self) -> Option<String> {
        let selection = self.terminal_selection?;
        if selection.anchor == selection.cursor {
            return None;
        }
        let (start, end) = selection.normalized();
        let text = if self.attached_history_is_buffered() {
            let screen = self.terminal.as_ref()?.screen();
            let (rows, columns) = screen.size();
            if rows == 0 || columns == 0 {
                return None;
            }
            screen.contents_between(
                start.row.min(rows - 1),
                start.column.min(columns - 1),
                end.row.min(rows - 1),
                end.column.saturating_add(1).min(columns),
            )
        } else {
            self.selected_history_text(start, end)
        };
        let text = text.trim_end_matches([' ', '\n', '\r']).to_string();
        (!text.is_empty()).then_some(text)
    }

    fn selected_history_text(&self, start: TerminalPoint, end: TerminalPoint) -> String {
        let body = if self.history_message.is_empty() {
            self.history.text.as_str()
        } else {
            self.history_message.as_str()
        };
        let lines: Vec<_> = body.lines().collect();
        let height = usize::from(self.agent_viewport_height);
        let scroll = lines.len().saturating_sub(height);
        let mut selected = Vec::new();
        for row in start.row..=end.row {
            let Some(line) = lines.get(scroll + usize::from(row)) else {
                continue;
            };
            let range_start = if row == start.row { start.column } else { 0 };
            let range_end = if row == end.row {
                end.column.saturating_add(1)
            } else {
                self.agent_viewport_width
            };
            selected.push(display_column_slice(
                &strip_terminal_styles(line),
                range_start,
                range_end,
            ));
        }
        selected.join("\n")
    }

    fn scroll_at(&mut self, column: u16, row: u16, up: bool) {
        if self
            .pane_layout
            .recap
            .is_some_and(|area| inside(area, column, row))
        {
            // Scrolling is a read gesture: it moves focus but never attaches.
            // Attaching here would silently route the next keystroke into the
            // agent, and on an archived session it would start a resume.
            self.focus = Focus::Recap;
            self.scroll_history(up, 1);
            return;
        }
        if let Some(_area) = self
            .pane_layout
            .machines
            .filter(|area| inside(*area, column, row))
        {
            self.focus = Focus::Machines;
            self.move_selection(if up { -1 } else { 1 });
            return;
        }
        if let Some(_area) = self
            .pane_layout
            .agents
            .filter(|area| inside(*area, column, row))
        {
            self.focus = Focus::Agents;
            self.move_selection(if up { -1 } else { 1 });
        }
    }

    fn persist_state(&mut self) {
        if let Err(error) = self.state.save(&self.state_path) {
            self.status_message =
                format!("Could not save state: {}", short_error(&error.to_string()));
        }
    }
}

fn clamped_index(current: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        return 0;
    }
    let current = current.min(length - 1);
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(length - 1)
    }
}

fn port_forward_value(form: &mut PortForwardForm) -> &mut String {
    match form.selected {
        0 => &mut form.remote_host,
        1 => &mut form.remote_port,
        2 => &mut form.local_port,
        _ => unreachable!("active forward rows are not editable"),
    }
}

fn detected_ports_in_text(text: &str) -> Vec<u16> {
    let text = text.to_ascii_lowercase();
    let mut ports = std::collections::BTreeSet::new();
    for marker in ["localhost:", "127.0.0.1:", "0.0.0.0:", "[::1]:"] {
        let mut rest = text.as_str();
        while let Some(position) = rest.find(marker) {
            let after = &rest[position + marker.len()..];
            let digits: String = after
                .chars()
                .take_while(char::is_ascii_digit)
                .take(5)
                .collect();
            if let Ok(port) = digits.parse::<u16>()
                && port >= 1024
            {
                ports.insert(port);
            }
            rest = after;
        }
    }
    ports.into_iter().collect()
}

fn focus_navigation_direction(key: KeyEvent) -> Option<FocusDirection> {
    if !has_pane_focus_modifier(key.modifiers)
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        return None;
    }
    if cfg!(target_os = "macos") && key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            // macOS Terminal commonly translates physical Option+Left/Right
            // into the readline word-navigation sequences Esc-b / Esc-f.
            KeyCode::Char('b') => return Some(FocusDirection::Left),
            KeyCode::Char('f') => return Some(FocusDirection::Right),
            _ => {}
        }
    }
    arrow_direction(key.code)
}

fn arrow_direction(code: KeyCode) -> Option<FocusDirection> {
    match code {
        KeyCode::Left => Some(FocusDirection::Left),
        KeyCode::Right => Some(FocusDirection::Right),
        KeyCode::Up => Some(FocusDirection::Up),
        KeyCode::Down => Some(FocusDirection::Down),
        _ => None,
    }
}

fn has_pane_focus_modifier(modifiers: KeyModifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::ALT)
    } else {
        modifiers.contains(KeyModifiers::ALT)
    }
}

#[cfg(test)]
fn pane_focus_modifier() -> KeyModifiers {
    if cfg!(target_os = "macos") {
        KeyModifiers::SUPER
    } else {
        KeyModifiers::ALT
    }
}

fn focus_distance(current: Rect, candidate: Rect, direction: FocusDirection) -> Option<(u32, u32)> {
    let current_x = u32::from(current.x) * 2 + u32::from(current.width);
    let current_y = u32::from(current.y) * 2 + u32::from(current.height);
    let candidate_x = u32::from(candidate.x) * 2 + u32::from(candidate.width);
    let candidate_y = u32::from(candidate.y) * 2 + u32::from(candidate.height);
    match direction {
        FocusDirection::Left
            if candidate_x < current_x
                && ranges_overlap(current.y, current.height, candidate.y, candidate.height) =>
        {
            Some((current_x - candidate_x, current_y.abs_diff(candidate_y)))
        }
        FocusDirection::Right
            if candidate_x > current_x
                && ranges_overlap(current.y, current.height, candidate.y, candidate.height) =>
        {
            Some((candidate_x - current_x, current_y.abs_diff(candidate_y)))
        }
        FocusDirection::Up
            if candidate_y < current_y
                && ranges_overlap(current.x, current.width, candidate.x, candidate.width) =>
        {
            Some((current_y - candidate_y, current_x.abs_diff(candidate_x)))
        }
        FocusDirection::Down
            if candidate_y > current_y
                && ranges_overlap(current.x, current.width, candidate.x, candidate.width) =>
        {
            Some((candidate_y - current_y, current_x.abs_diff(candidate_x)))
        }
        _ => None,
    }
}

fn ranges_overlap(
    first_start: u16,
    first_length: u16,
    second_start: u16,
    second_length: u16,
) -> bool {
    let first_end = first_start.saturating_add(first_length);
    let second_end = second_start.saturating_add(second_length);
    first_start < second_end && second_start < first_end
}

fn history_capture_stride(chunk_lines: usize, viewport_lines: usize) -> usize {
    chunk_lines
        .saturating_sub(viewport_lines)
        .saturating_sub(20)
        .max(1)
}

fn history_cache_key(target_id: &str, session_id: &str) -> String {
    format!("{target_id}\0{session_id}")
}

fn cache_path_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn history_capture_offset(
    desired_offset: usize,
    chunk_lines: usize,
    viewport_lines: usize,
) -> usize {
    let stride = history_capture_stride(chunk_lines, viewport_lines);
    desired_offset / stride * stride
}

fn materialize_history_page(
    source: &HistoryPage,
    desired_offset: usize,
    viewport_lines: usize,
) -> Option<HistoryPage> {
    // `history_size` bounds the page only once it has stopped expecting older
    // history; until then it measures how far this page reached, and the chunk
    // it holds is what limits the slice.
    if let Some(oldest) = source.oldest_offset()
        && (source.offset_from_bottom > oldest || desired_offset > oldest)
    {
        return None;
    }
    let delta = desired_offset.checked_sub(source.offset_from_bottom)?;
    let lines: Vec<_> = source.text.lines().collect();
    if delta > lines.len() {
        return None;
    }
    let end = lines.len().saturating_sub(delta);
    if desired_offset > source.offset_from_bottom && end < viewport_lines.min(lines.len()) {
        return None;
    }
    Some(HistoryPage {
        text: lines[..end].join("\n"),
        history_size: source.history_size,
        pane_height: source.pane_height,
        pane_width: source.pane_width,
        offset_from_bottom: desired_offset,
        rendered: source.rendered,
        more_history: source.more_history,
    })
}

/// Step one runtime along the list a machine offers, wrapping at both ends. A
/// current runtime the machine no longer offers lands on the first one.
fn step_within(kinds: &[AgentKind], current: AgentKind, forward: bool) -> AgentKind {
    let Some(first) = kinds.first().copied() else {
        return current;
    };
    let Some(index) = kinds.iter().position(|kind| *kind == current) else {
        return first;
    };
    let step = if forward { 1 } else { kinds.len() - 1 };
    kinds[(index + step) % kinds.len()]
}

fn next_field(field: LaunchField) -> LaunchField {
    match field {
        LaunchField::Kind => LaunchField::Path,
        LaunchField::Path => LaunchField::Label,
        LaunchField::Label => LaunchField::Kind,
    }
}

fn previous_field(field: LaunchField) -> LaunchField {
    match field {
        LaunchField::Kind => LaunchField::Label,
        LaunchField::Path => LaunchField::Kind,
        LaunchField::Label => LaunchField::Path,
    }
}

/// Check a whole scope group, or clear it if it was already fully checked.
/// "Everything except these two" is the answer most often wanted, and walking
/// the list to get there is the part worth skipping.
fn toggle_all(items: &mut [ScopeItem]) {
    let all = items.iter().all(|item| item.selected);
    for item in items {
        item.selected = !all;
    }
}

/// The field being edited, or `None` when the cursor is parked on the runtime
/// row - that one is a choice, not text, and every editing key has to be a
/// no-op there rather than a panic.
fn active_text(form: &mut LaunchForm) -> Option<&mut String> {
    match form.field {
        LaunchField::Path => Some(&mut form.path),
        LaunchField::Label => Some(&mut form.label),
        LaunchField::Kind => None,
    }
}

fn single_line_paste(value: &str) -> String {
    value
        .trim_matches(['\r', '\n'])
        .chars()
        .filter_map(|character| match character {
            '\r' | '\n' | '\t' => Some(' '),
            character if character.is_control() => None,
            character => Some(character),
        })
        .collect()
}

/// Search the local backup for conversations matching `query`, de-duplicated to
/// one entry per session (best-ranked hit). Empty without the controller build.
#[cfg(feature = "controller")]
fn backup_search_hits(query: &str, limit: usize) -> Vec<CrossMachineHit> {
    let store = crate::backup::BackupStore::new(crate::backup::BackupStore::default_root());
    let hits = match crate::backup::search(&store, query, limit.saturating_mul(4)) {
        Ok(hits) => hits,
        Err(error) => {
            debug::log("backup", format!("history search failed: {error:#}"));
            return Vec::new();
        }
    };
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for hit in hits {
        if !seen.insert((hit.target_id.clone(), hit.session_id.clone())) {
            continue;
        }
        out.push(CrossMachineHit {
            target_id: hit.target_id,
            session_id: hit.session_id,
            kind: hit.kind,
            title: hit.title,
            snippet: hit.snippet,
            created_at: hit.created_at,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[cfg(not(feature = "controller"))]
fn backup_search_hits(_query: &str, _limit: usize) -> Vec<CrossMachineHit> {
    Vec::new()
}

/// Render a backed-up session's transcript as `role: text` lines, keeping at
/// most the last `max_chars` (most recent context). Empty without controller.
#[cfg(feature = "controller")]
fn backup_session_transcript(target_id: &str, session_id: &str, max_chars: usize) -> String {
    let store = crate::backup::BackupStore::new(crate::backup::BackupStore::default_root());
    let raw = store
        .read_blob(target_id, session_id, crate::backup::MESSAGES_BLOB)
        .unwrap_or_default();
    let mut out = String::new();
    for line in String::from_utf8_lossy(&raw).lines() {
        if let Ok(message) = serde_json::from_str::<crate::backup::ExtractedMessage>(line) {
            let text = message.text.trim();
            if text.is_empty() {
                continue;
            }
            out.push_str(&message.role);
            out.push_str(": ");
            out.push_str(text);
            out.push('\n');
        }
    }
    let chars: Vec<char> = out.chars().collect();
    if chars.len() > max_chars {
        let tail: String = chars[chars.len() - max_chars..].iter().collect();
        format!("…{tail}")
    } else {
        out
    }
}

#[cfg(not(feature = "controller"))]
fn backup_session_transcript(_target_id: &str, _session_id: &str, _max_chars: usize) -> String {
    String::new()
}

/// The conversations the local store holds for `alias` that the machine itself
/// no longer reports. `live` is the set of session ids it did report.
#[cfg(feature = "controller")]
fn recoverable_backup_records(
    root: &Path,
    alias: &str,
    live: &HashSet<String>,
) -> Vec<RecoverableSession> {
    let store = crate::backup::BackupStore::new(root.to_path_buf());
    crate::backup::recoverable_records(&store, alias, live)
        .into_iter()
        .map(|record| RecoverableSession {
            restorable: crate::backup::is_restorable(&record),
            machine_key: record.target_id,
            session_id: record.session_id,
            kind: record.kind,
            label: record.label,
            cwd: record.cwd,
            title: record.title,
            recap: record.recap,
            created_at: record.created_at,
        })
        .collect()
}

#[cfg(not(feature = "controller"))]
fn recoverable_backup_records(
    _root: &Path,
    _alias: &str,
    _live: &HashSet<String>,
) -> Vec<RecoverableSession> {
    Vec::new()
}

/// The raw terminal capture kept for a backed-up session, ready to be rendered
/// as read-only history. Empty when only messages were captured.
#[cfg(feature = "controller")]
fn backup_session_capture(
    root: &Path,
    target_id: &str,
    session_id: &str,
    max_bytes: usize,
) -> (String, bool) {
    let store = crate::backup::BackupStore::new(root.to_path_buf());
    match store.read_blob_tail(
        target_id,
        session_id,
        crate::backup::CAPTURE_BLOB,
        max_bytes,
    ) {
        Ok((bytes, clipped)) => (String::from_utf8_lossy(&bytes).into_owned(), clipped),
        Err(error) => {
            debug::log("backup", format!("capture unreadable: {error:#}"));
            (String::new(), false)
        }
    }
}

#[cfg(not(feature = "controller"))]
fn backup_session_capture(
    _root: &Path,
    _target_id: &str,
    _session_id: &str,
    _max_bytes: usize,
) -> (String, bool) {
    (String::new(), false)
}

/// Byte offset that leaves the last `max_lines` lines of `text`, or None when
/// it already fits. Cutting there keeps whole lines.
fn cut_to_last_lines(text: &str, max_lines: usize) -> Option<usize> {
    let mut seen = 0usize;
    for (offset, _) in text.rmatch_indices('\n') {
        seen += 1;
        if seen > max_lines {
            return Some(offset + 1);
        }
    }
    None
}

/// Canonicalise an ssh alias to its stable backup machine key, so "same
/// machine" comparisons survive alias churn. The daemon build has no registry,
/// so its stub just returns the alias unchanged.
#[cfg(feature = "controller")]
fn backup_machine_key_for_alias(alias: &str) -> String {
    let store = crate::backup::BackupStore::new(crate::backup::BackupStore::default_root());
    crate::backup::machine_key_for_alias(&store, alias)
}

#[cfg(not(feature = "controller"))]
fn backup_machine_key_for_alias(alias: &str) -> String {
    alias.to_string()
}

fn history_reference_prompt(launch: &LaunchForm, candidate: &ResumeCandidate) -> String {
    let mut prompt = format!(
        "Continue the work from a {} session in this directory. Its history cannot be resumed directly by {}, so use it as reference context.",
        candidate.kind, launch.kind
    );
    if candidate.source_path.is_empty() {
        prompt.push_str(&format!(" Source session ID: {}.", candidate.id));
    } else {
        prompt.push_str(&format!(
            " Read the complete source transcript at: {}",
            candidate.source_path
        ));
    }
    if let Some(recap) = candidate.recap.as_deref() {
        prompt.push_str(&format!(" Session recap: {recap}"));
    }
    if let Some(last) = candidate.last_message.as_deref() {
        prompt.push_str(&format!(" Most recent user request: {last}"));
    }
    prompt.push_str(
        " Preserve relevant decisions and completed work, verify the current workspace state, then continue the unfinished task.",
    );
    prompt
}

fn short_error(error: &str) -> String {
    error
        .lines()
        .next()
        .unwrap_or(error)
        .chars()
        .filter(|character| !character.is_control())
        .take(120)
        .collect()
}

fn sanitize_terminal_text(output: &str) -> String {
    let mut sanitized = String::with_capacity(output.len());
    let mut characters = output.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            match characters.peek().copied() {
                Some('[') => {
                    characters.next();
                    let mut parameters = String::new();
                    let mut final_byte = None;
                    for next in characters.by_ref() {
                        if ('@'..='~').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        parameters.push(next);
                    }
                    match final_byte {
                        Some('m') => {
                            sanitized.push('\x1b');
                            sanitized.push('[');
                            sanitized.push_str(&parameters);
                            sanitized.push('m');
                        }
                        // A rendered row crosses cells it never wrote by moving
                        // the cursor rather than by spacing over them, so a
                        // transcript laid out in columns -- every panel an
                        // agent draws -- arrives with its gaps as escapes.
                        // Nothing downstream moves a cursor, so spend them as
                        // spaces and the columns survive the trip.
                        Some('C') => {
                            let columns = parameters
                                .parse::<usize>()
                                .unwrap_or(1)
                                .clamp(1, MAXIMUM_CURSOR_ADVANCE);
                            sanitized.extend(std::iter::repeat_n(' ', columns));
                        }
                        _ => {}
                    }
                }
                Some(']') => {
                    characters.next();
                    while let Some(next) = characters.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    characters.next();
                }
                None => {}
            }
        } else if character == '\x08' {
            // A row that wraps a wide glyph is written as space-backspace-erase.
            // Taking the space back leaves the line the width it renders at.
            if !sanitized.ends_with('\n') {
                sanitized.pop();
            }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            sanitized.push(character);
        }
    }
    sanitized
}

/// How far one cursor-forward escape is allowed to space a line out. Rendered
/// rows never advance past their own width; this only stops a malformed count
/// in raw output from inflating a page.
const MAXIMUM_CURSOR_ADVANCE: usize = 1_000;

fn strip_terminal_styles(output: &str) -> String {
    let mut plain = String::with_capacity(output.len());
    let mut characters = output.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            match characters.peek().copied() {
                Some('[') => {
                    characters.next();
                    for next in characters.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    characters.next();
                    while let Some(next) = characters.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    characters.next();
                }
                None => {}
            }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            plain.push(character);
        }
    }
    plain
}

fn inside(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

/// A horizontal divider spans the whole window, so the row of slack a vertical
/// divider gets on each side would cover the terminal's last line of output
/// from edge to edge. Only the divider row and the border below it grab.
fn near_horizontal_divider(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row <= area.y.saturating_add(area.height)
}

fn near_divider(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x.saturating_sub(1)
        && column <= area.x.saturating_add(area.width)
        && row >= area.y.saturating_sub(1)
        && row <= area.y.saturating_add(area.height)
}

fn is_copy_shortcut(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c')
        && (key.modifiers.contains(KeyModifiers::SUPER)
            || key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn mark_search_edited(form: &mut SearchForm) {
    form.submitted_query.clear();
    form.results.clear();
    form.result_rows.clear();
    form.selected = 0;
    form.loading = false;
    form.error = None;
    form.edited_at = Instant::now();
}

fn display_column_slice(value: &str, start: u16, end: u16) -> String {
    if start >= end {
        return String::new();
    }
    let mut output = String::new();
    let mut column = 0_u16;
    for character in value.chars() {
        let width = u16::try_from(character.width().unwrap_or(0)).unwrap_or(u16::MAX);
        let next = column.saturating_add(width);
        if next > start && column < end {
            output.push(character);
        }
        if column >= end {
            break;
        }
        column = next;
    }
    output
}

/// Join the on-screen preview rows covered by a selection into copyable text.
/// Rows are the plain text of what is currently visible; columns are character
/// offsets. The end column is inclusive, matching the highlight.
fn selection_text(lines: &[String], selection: TerminalSelection) -> Option<String> {
    if selection.anchor == selection.cursor {
        return None;
    }
    let (start, end) = selection.normalized();
    let mut text = String::new();
    for row in start.row..=end.row {
        let characters: Vec<char> = lines
            .get(row as usize)
            .map(|line| line.chars().collect())
            .unwrap_or_default();
        let from = if row == start.row {
            usize::from(start.column)
        } else {
            0
        };
        let to = if row == end.row {
            (usize::from(end.column) + 1).min(characters.len())
        } else {
            characters.len()
        };
        if from < to {
            text.extend(&characters[from..to]);
        }
        if row != end.row {
            text.push('\n');
        }
    }
    let text = text.trim_end_matches([' ', '\n', '\r']).to_string();
    (!text.is_empty()).then_some(text)
}

fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".into(),
        Some(index) => trimmed[..index].to_string(),
        None => ".".into(),
    }
}

fn child_path(path: &str, child: &str) -> String {
    if path == "/" {
        format!("/{child}")
    } else {
        format!("{}/{child}", path.trim_end_matches('/'))
    }
}

fn default_download_directory() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn dropped_file_paths(value: &str) -> Vec<PathBuf> {
    let whole_path = PathBuf::from(value.trim());
    if whole_path.is_file() {
        return vec![whole_path];
    }
    let values = shell_words::split(value).unwrap_or_else(|_| {
        value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    });
    values
        .into_iter()
        .map(|value| {
            let value = value.strip_prefix("file://").unwrap_or(&value);
            PathBuf::from(percent_decode_path(value))
        })
        .filter(|path| path.is_file())
        .collect()
}

fn percent_decode_path(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn matched_directories(form: &PathPickerForm) -> Vec<String> {
    let mut matches: Vec<_> = form
        .directories
        .iter()
        .filter_map(|directory| {
            folder_match_rank(directory, &form.query).map(|rank| (rank, directory.clone()))
        })
        .collect();
    matches.sort_by(|(left_rank, left), (right_rank, right)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| left.to_lowercase().cmp(&right.to_lowercase()))
    });
    matches
        .into_iter()
        .map(|(_, directory)| directory)
        .collect()
}

/// Put a session on the list, then everything it started, indented under it.
///
/// A folded task is walked all the same and only its rows are left out: the
/// count and the state on the parent row are the whole point of folding one,
/// and both come from the walk. Returns what is under this session — how many
/// sessions, and whether any of them wants an answer or is working.
fn push_session_row<'a>(
    session: &'a AgentSession,
    depth: usize,
    draw: bool,
    children: &HashMap<&'a str, Vec<&'a AgentSession>>,
    folded: &BTreeSet<String>,
    rows: &mut Vec<(&'a AgentSession, RowShape)>,
    seen: &mut HashSet<&'a str>,
) -> (usize, bool, bool) {
    if !seen.insert(session.id.as_str()) {
        // A chain that loops back on itself would otherwise be walked for
        // ever. Whichever place the session reached first is the one it keeps.
        return (0, false, false);
    }
    let folded_here = folded.contains(&session.id);
    let slot = draw.then(|| {
        rows.push((
            session,
            RowShape {
                depth,
                ..RowShape::default()
            },
        ));
        rows.len() - 1
    });
    let (mut descendants, mut attention, mut working) = (0, false, false);
    for child in children.get(session.id.as_str()).into_iter().flatten() {
        let (under, waiting, busy) = push_session_row(
            child,
            (depth + 1).min(MAX_SUBAGENT_DEPTH),
            draw && !folded_here,
            children,
            folded,
            rows,
            seen,
        );
        descendants += under + 1;
        attention |= waiting || child.needs_attention;
        working |= busy || (child.working && !child.dead);
    }
    if let Some(slot) = slot {
        let shape = &mut rows[slot].1;
        shape.descendants = descendants;
        shape.folded = folded_here && descendants > 0;
        shape.attention = shape.folded && attention;
        shape.working = shape.folded && working;
    }
    (descendants, attention, working)
}

fn folder_match_rank(name: &str, query: &str) -> Option<(u8, usize, usize)> {
    let name = name.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some((0, 0, name.len()));
    }
    if name.starts_with(&query) {
        return Some((0, 0, name.len().saturating_sub(query.len())));
    }
    if let Some(position) = name.find(&query) {
        return Some((1, position, name.len().saturating_sub(query.len())));
    }

    let name_chars: Vec<_> = name.chars().collect();
    let mut cursor = 0;
    let mut first = None;
    let mut gaps = 0;
    for wanted in query.chars() {
        let relative = name_chars[cursor..]
            .iter()
            .position(|character| *character == wanted)?;
        let position = cursor + relative;
        first.get_or_insert(position);
        gaps += relative;
        cursor = position + 1;
    }
    Some((2, first.unwrap_or(0), gaps))
}

/// The shared tail of a transfer status line: how far along, and how fast.
fn transfer_progress(transferred: u64, total_size: u64, bytes_per_second: f64) -> String {
    let measured = if total_size > 0 {
        let percent = transferred
            .saturating_mul(100)
            .checked_div(total_size)
            .unwrap_or(0);
        format!(
            "{percent}%  {}/{}",
            format_transfer_bytes(transferred),
            format_transfer_bytes(total_size)
        )
    } else {
        format_transfer_bytes(transferred)
    };
    format!(
        "{measured}  {}/s",
        format_transfer_bytes(bytes_per_second as u64)
    )
}

fn format_transfer_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn parse_setting<T>(value: &str, label: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .trim()
        .parse()
        .map_err(|error| format!("Invalid {label}: {error}"))
}

fn parse_shell_list(value: &str, label: &str) -> Result<Vec<String>, String> {
    shell_words::split(value).map_err(|error| format!("Invalid {label}: {error}"))
}

fn format_shell_list(values: &[String]) -> String {
    shell_words::join(values.iter().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runtime::Runtime, worker::Worker};

    fn receive_request(receiver: &std::sync::mpsc::Receiver<Request>) -> Request {
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("expected worker request within two seconds")
    }

    #[test]
    fn preview_selection_copies_the_covered_rows() {
        let lines = vec!["hello world".to_string(), "second line".to_string()];
        // Multi-row selection: mid first row through mid second row (inclusive end).
        let span = TerminalSelection {
            anchor: TerminalPoint { row: 0, column: 6 },
            cursor: TerminalPoint { row: 1, column: 5 },
            dragging: false,
        };
        assert_eq!(
            selection_text(&lines, span).as_deref(),
            Some("world\nsecond")
        );
        // A zero-width selection (a plain click) copies nothing.
        let empty = TerminalSelection {
            anchor: TerminalPoint { row: 0, column: 2 },
            cursor: TerminalPoint { row: 0, column: 2 },
            dragging: false,
        };
        assert_eq!(selection_text(&lines, empty), None);
    }

    #[test]
    fn list_indices_clamp_at_the_first_and_last_item() {
        assert_eq!(clamped_index(0, 3, -1), 0);
        assert_eq!(clamped_index(2, 3, 1), 2);
        assert_eq!(clamped_index(1, 5, 7), 4);
        assert_eq!(clamped_index(1, 5, -7), 0);
        assert_eq!(parent_path("/work/project"), "/work");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(child_path("/work", "project"), "/work/project");
        let form = PathPickerForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Codex,
                path: ".".into(),
                label: String::new(),
                temporary: false,
                field: LaunchField::Path,
            },
            path: "/work".into(),
            directories: vec!["terminal".into(), "my-terminal".into(), "teamroom".into()],
            query: "term".into(),
            selected: 0,
            loading: false,
            error: None,
        };
        assert_eq!(
            matched_directories(&form),
            ["terminal", "my-terminal", "teamroom"]
        );
    }

    #[test]
    fn terminal_history_keeps_sgr_but_drops_other_control_sequences() {
        let styled = sanitize_terminal_text(
            "\x1b[31;1mred\x1b[0m\n\x1b]8;;https://example.com\x07link\x1b]8;;\x07",
        );
        assert_eq!(styled, "\x1b[31;1mred\x1b[0m\nlink");
        assert_eq!(strip_terminal_styles(&styled), "red\nlink");
    }

    #[test]
    fn rendered_history_rows_keep_the_columns_they_were_drawn_in() {
        // The daemon renders history through an emulator, and an emulator
        // crosses cells nobody wrote by moving the cursor. Dropping those moves
        // pulled every column of an agent's panels back against the left
        // margin, and took the copy-a-selection column maths with it.
        let log = b"\x1b[1;1Hname\x1b[1;12Hstatus\r\n\x1b[2;3Hclaude\x1b[2;12Hworking\r\n";
        let (page, _total, _offset) =
            crate::terminal_session::render_history_rows(&log[..], 24, 4, 0, 6).expect("render");
        let page = String::from_utf8(page).expect("utf-8 rows");

        let text = sanitize_terminal_text(&page);
        let plain = strip_terminal_styles(&text);
        let mut lines = plain.lines();

        assert_eq!(lines.next(), Some("name       status"));
        assert_eq!(lines.next(), Some("  claude   working"));
    }

    #[test]
    fn live_terminal_frames_update_working_state_without_waiting_for_a_scan() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "muxloomd-codex-live".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "live status".into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });

        app.sync_live_agent_activity(
            "muxloomd-codex-live",
            "• Working (1s • esc to interrupt)",
            None,
        );
        assert!(app.sessions[0].working);

        app.sync_live_agent_activity(
            "muxloomd-codex-live",
            "› Ask Codex anything\ngpt-5.6-sol xhigh · /work",
            None,
        );
        assert!(!app.sessions[0].working);

        app.sync_live_agent_activity(
            "muxloomd-codex-live",
            "partially erased status line",
            Some(true),
        );
        assert!(app.sessions[0].working);
        app.sync_live_agent_activity(
            "muxloomd-codex-live",
            "• Working (1s • esc to interrupt)",
            Some(false),
        );
        assert!(!app.sessions[0].working);
    }

    #[test]
    fn a_startup_update_finding_opens_the_prompt_once_the_screen_is_free() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.modal = Some(Modal::Help(HelpForm::default()));
        if let Ok(mut slot) = app.update_slot().lock() {
            *slot = Some(UpdateNote {
                message: None,
                staged_version: None,
                available_version: Some("9.9.9".into()),
                prompt: Some(UpdatePrompt {
                    latest: "9.9.9".into(),
                    current: "0.0.1".into(),
                    tag: "v9.9.9".into(),
                    version: "9.9.9".into(),
                    can_self_update: false,
                }),
            });
        }
        // An open form is not replaced by the prompt.
        app.on_tick();
        assert!(matches!(app.modal, Some(Modal::Help(_))));
        assert_eq!(app.available_update.as_deref(), Some("9.9.9"));

        app.modal = None;
        if let Ok(mut slot) = app.update_slot().lock() {
            *slot = Some(UpdateNote {
                message: None,
                staged_version: None,
                available_version: None,
                prompt: Some(UpdatePrompt {
                    latest: "9.9.9".into(),
                    current: "0.0.1".into(),
                    tag: "v9.9.9".into(),
                    version: "9.9.9".into(),
                    can_self_update: false,
                }),
            });
        }
        app.on_tick();
        assert!(matches!(app.modal, Some(Modal::UpdatePrompt(_))));

        // Declining leaves a pointer at the manual path instead of silence.
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(app.status_message.contains("muxloom update"));
    }

    #[test]
    fn a_forced_update_archives_cycles_and_resumes_in_order() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions = vec![
            AgentSession {
                id: "muxloomd-claude-old-1".into(),
                target_id: "local".into(),
                kind: AgentKind::Claude,
                path: "/work/project".into(),
                label: "big refactor".into(),
                created_at: 1,
                dead: false,
                pid: Some(10),
                working: false,
                needs_attention: false,
                attention_reason: None,
                recap: None,
                title: None,
                parent: None,
            },
            AgentSession {
                id: "muxloomd-terminal-old-2".into(),
                target_id: "local".into(),
                kind: AgentKind::Terminal,
                path: "/work/project".into(),
                label: "dev server".into(),
                created_at: 2,
                dead: false,
                pid: Some(11),
                working: false,
                needs_attention: false,
                attention_reason: None,
                recap: None,
                title: None,
                parent: None,
            },
        ];

        // The one-shot action always shows its plan first; the terminal in
        // the list is what makes the confirmation matter.
        app.propose_forced_update("local");
        assert!(matches!(app.modal, Some(Modal::ConfirmForcedUpdate { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        {
            let update = app.forced_updates.get("local").expect("orchestration");
            assert_eq!(update.phase, ForcedPhase::Archiving);
            assert_eq!(update.pending_acks, 2);
            assert_eq!(update.resumes.len(), 1);
            assert_eq!(update.terminals_archived, 1);
        }

        // Both archives acknowledge; the bridge cycle begins.
        for session_id in ["muxloomd-claude-old-1", "muxloomd-terminal-old-2"] {
            app.handle_worker_event(Event::Archived {
                target_id: "local".into(),
                session_id: session_id.into(),
                result: Ok(()),
            });
        }
        assert_eq!(
            app.forced_updates.get("local").unwrap().phase,
            ForcedPhase::Cycling
        );

        // The new generation answers; the resume scan goes out.
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Ok(Some(env!("CARGO_PKG_VERSION").into())),
        });
        assert_eq!(
            app.forced_updates.get("local").unwrap().phase,
            ForcedPhase::Resuming
        );

        // Candidates land: the agent resumes, the terminal stays archived,
        // and the orchestration reports and clears itself.
        app.handle_worker_event(Event::ResumesScanned {
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            result: Ok(vec![ResumeCandidate {
                id: "claude-native-resume-id".into(),
                kind: AgentKind::Claude,
                source_path: "/home/user/.claude/projects/x.jsonl".into(),
                recap: Some("big refactor".into()),
                first_message: None,
                last_message: None,
                updated_at: "2026-08-14T00:00:00Z".into(),
            }]),
            warning: None,
        });
        assert!(app.forced_updates.is_empty());
        assert!(
            app.status_message.contains("daemon updated"),
            "{}",
            app.status_message
        );
        assert!(app.status_message.contains("1 terminal(s) archived"));
    }

    #[test]
    fn a_deferred_forced_handover_escalates_to_a_restart_once_then_fails() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.propose_forced_update("local");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            app.forced_updates.get("local").unwrap().phase,
            ForcedPhase::Cycling
        );

        // The polite cycle came back with the old daemon still serving: the
        // orchestration escalates to an outright restart exactly once.
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Ok(Some("0.3.0".into())),
        });
        let update = app.forced_updates.get("local").expect("still in flight");
        assert!(update.escalated);
        assert_eq!(update.phase, ForcedPhase::Cycling);

        // A second failure gives up loudly.
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Ok(Some("0.3.0".into())),
        });
        assert!(app.forced_updates.is_empty());
        assert!(app.status_message.contains("forced update failed"));
    }

    #[test]
    fn the_settings_panel_carries_the_daemon_version_and_the_force_update_action() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.focus = Focus::Machines;
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        let Some(Modal::Settings(form)) = app.modal.clone() else {
            panic!(", did not open the machine settings");
        };
        assert!(matches!(form.scope, SettingsScope::Host(ref id) if id == "local"));
        // The version the bar no longer shows lives here instead.
        assert_eq!(form.notes.len(), 1);
        assert!(form.notes[0].contains("muxloomd") || form.notes[0] == "not connected");

        // Walking to the bottom of the panel lands on the action, and Enter
        // opens the confirmation the `u` shortcut used to.
        for _ in 0..form.focus_len() {
            app.handle_key(KeyEvent::from(KeyCode::Down));
        }
        let Some(Modal::Settings(form)) = app.modal.clone() else {
            panic!("navigation closed the settings");
        };
        assert_eq!(form.selected_action(), Some(FORCE_UPDATE_ACTION));
        assert_eq!(form.selected_value(), None);
        // Enter runs the action rather than saving the form. With no bridge
        // behind this test machine it lands on the "nothing to update" answer,
        // which is the one the `u` shortcut used to give.
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            app.status_message.contains("already current"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn a_forced_update_is_a_one_shot_action_never_an_ambient_state() {
        // A lagging cycle result on its own opens nothing and starts
        // nothing: the marker stays up, and the settings panel's action is
        // the only trigger.
        let mut app = ux_test_app(vec![Target::local()]);
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Ok(Some("0.3.0".into())),
        });
        assert!(app.forced_updates.is_empty() && app.modal.is_none());

        // With nothing to interrupt the confirmation still shows its plan,
        // and confirming goes straight to the bridge cycle.
        app.propose_forced_update("local");
        assert!(matches!(app.modal, Some(Modal::ConfirmForcedUpdate { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            app.forced_updates.get("local").unwrap().phase,
            ForcedPhase::Cycling
        );
    }

    #[test]
    fn an_offline_machine_keeps_its_mark_through_quiet_retries() {
        let mut app = ux_test_app(vec![Target::ssh("gpu")]);
        app.state.enabled_hosts.insert("gpu".into());
        app.targets[0].enabled = true;

        // First contact shows the scanning spinner and the connect progress.
        app.refresh_target("gpu");
        assert_eq!(app.targets[0].state, ConnectionState::Scanning);
        app.handle_worker_event(Event::Scanned {
            target_id: "gpu".into(),
            result: Err("no route to host".into()),
        });
        assert_eq!(app.targets[0].state, ConnectionState::Offline);

        // A background retry neither flips the row back to scanning nor
        // surfaces connect progress: the steady red mark is the message.
        app.refresh_target("gpu");
        assert_eq!(app.targets[0].state, ConnectionState::Offline);
        app.handle_worker_event(Event::TaskProgress {
            target_id: "gpu".into(),
            operation: TaskKind::Connect,
            progress: TaskProgress::pending("Connecting to gpu"),
        });
        assert!(app.visible_task_progress().is_none());
        app.handle_worker_event(Event::Scanned {
            target_id: "gpu".into(),
            result: Err("no route to host".into()),
        });
        assert_eq!(app.targets[0].state, ConnectionState::Offline);

        // A refresh the user asked for is loud again.
        app.refresh_enabled_manual();
        assert_eq!(app.targets[0].state, ConnectionState::Scanning);
        app.handle_worker_event(Event::TaskProgress {
            target_id: "gpu".into(),
            operation: TaskKind::Connect,
            progress: TaskProgress::pending("Connecting to gpu"),
        });
        assert!(app.visible_task_progress().is_some());
        app.handle_worker_event(Event::Scanned {
            target_id: "gpu".into(),
            result: Err("still unreachable".into()),
        });
        assert_eq!(app.targets[0].state, ConnectionState::Offline);
    }

    #[test]
    fn a_daemon_refresh_reports_success_and_stays_quiet_when_still_lagging() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Ok(Some(env!("CARGO_PKG_VERSION").into())),
        });
        assert!(
            app.status_message.contains("sessions kept running"),
            "{}",
            app.status_message
        );

        // A refresh that could not advance the daemon stays quiet: the old
        // generation is still serving and the backoff window will retry.
        let mut app = ux_test_app(vec![Target::local()]);
        let before = app.status_message.clone();
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Ok(Some("0.3.0".into())),
        });
        app.handle_worker_event(Event::DaemonRefreshed {
            target_id: "local".into(),
            result: Err("unreachable".into()),
        });
        assert_eq!(app.status_message, before);
    }

    #[test]
    fn background_activity_refresh_updates_every_agent_not_only_the_selected_one() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.selected_session_id = Some("muxloomd-codex-selected".into());
        app.sessions = vec![
            AgentSession {
                id: "muxloomd-codex-selected".into(),
                target_id: "local".into(),
                kind: AgentKind::Codex,
                path: "/work/a".into(),
                label: "selected".into(),
                created_at: 1,
                dead: false,
                pid: Some(10),
                working: false,
                needs_attention: false,
                attention_reason: None,
                recap: None,
                title: None,
                parent: None,
            },
            AgentSession {
                id: "muxloomd-claude-background".into(),
                target_id: "local".into(),
                kind: AgentKind::Claude,
                path: "/work/b".into(),
                label: "background".into(),
                created_at: 2,
                dead: false,
                pid: Some(20),
                working: true,
                needs_attention: false,
                attention_reason: None,
                recap: None,
                title: None,
                parent: None,
            },
        ];
        app.pending_activity_refreshes.insert("local".into());

        app.handle_worker_event(Event::ActivityRefreshed {
            target_id: "local".into(),
            result: Ok(vec![
                AgentSession {
                    working: true,
                    ..app.sessions[0].clone()
                },
                AgentSession {
                    working: false,
                    needs_attention: true,
                    attention_reason: Some("approval required".into()),
                    recap: Some("waiting for approval".into()),
                    ..app.sessions[1].clone()
                },
            ]),
        });

        assert!(app.pending_activity_refreshes.is_empty());
        assert!(app.sessions[0].working);
        assert!(!app.sessions[1].working);
        assert!(app.sessions[1].needs_attention);
        assert_eq!(
            app.sessions[1].attention_reason.as_deref(),
            Some("approval required")
        );
        assert_eq!(
            app.sessions[1].recap.as_deref(),
            Some("waiting for approval")
        );
        assert_eq!(app.notifications.len(), 1);
    }

    fn waiting_agent(reason: &str) -> AgentSession {
        AgentSession {
            id: "muxloomd-codex-waiting".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "waiting".into(),
            created_at: 1,
            dead: false,
            pid: Some(10),
            working: false,
            needs_attention: true,
            attention_reason: Some(reason.into()),
            recap: None,
            title: None,
            parent: None,
        }
    }

    /// A name typed by hand is what the user meant the session to be called.
    /// Failing that, the agent's own name for the conversation says what is
    /// going on in it, which the folder never does.
    #[test]
    fn a_session_goes_by_its_label_then_its_agents_name_then_its_folder() {
        let named = AgentSession {
            label: "deploy".into(),
            title: Some("rewriting the pty reader".into()),
            ..waiting_agent("")
        };
        assert_eq!(named.display_label(), "deploy");
        let titled = AgentSession {
            label: String::new(),
            ..named.clone()
        };
        assert_eq!(titled.display_label(), "rewriting the pty reader");
        let anonymous = AgentSession {
            title: None,
            ..titled.clone()
        };
        assert_eq!(anonymous.display_label(), "work");
        let unnamed = AgentSession {
            title: Some(String::new()),
            ..anonymous
        };
        assert_eq!(unnamed.display_label(), "work");
    }

    /// A session in a tree test: only the fields the ordering reads matter, so
    /// the rest come from one place and stay out of the way.
    fn tree_session(id: &str, parent: Option<&str>, created_at: u64) -> AgentSession {
        AgentSession {
            id: id.into(),
            target_id: "local".into(),
            kind: AgentKind::Claude,
            path: "/work".into(),
            label: id.into(),
            created_at,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: parent.map(Into::into),
        }
    }

    fn tree_shape(app: &App) -> Vec<(String, usize, usize)> {
        app.visible_session_rows()
            .into_iter()
            .map(|(session, shape)| (session.id.clone(), shape.depth, shape.descendants))
            .collect()
    }

    /// What an agent starts is part of that agent's work, so it is listed under
    /// it rather than alongside it in the folder.
    #[test]
    fn a_subagent_is_listed_indented_under_the_agent_that_started_it() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions = vec![
            tree_session("lead", None, 30),
            tree_session("helper", Some("lead"), 20),
            tree_session("deeper", Some("helper"), 15),
            tree_session("alone", None, 10),
        ];

        assert_eq!(
            tree_shape(&app),
            vec![
                ("lead".into(), 0, 2),
                ("helper".into(), 1, 1),
                ("deeper".into(), 2, 0),
                ("alone".into(), 0, 0),
            ]
        );

        // A subagent whose parent is not in this list has nothing to hang off,
        // and disappearing with it would be worse than standing on its own.
        app.sessions[1].parent = Some("gone-with-the-machine".into());
        assert_eq!(
            tree_shape(&app),
            vec![
                ("lead".into(), 0, 0),
                ("helper".into(), 0, 1),
                ("deeper".into(), 1, 0),
                ("alone".into(), 0, 0),
            ]
        );

        // Neither does a chain that loops back on itself lose anybody.
        app.sessions[0].parent = Some("deeper".into());
        app.sessions[1].parent = Some("lead".into());
        app.sessions[2].parent = Some("helper".into());
        let listed: Vec<_> = tree_shape(&app)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(listed, vec!["alone", "deeper", "helper", "lead"]);
    }

    /// An archived subagent belongs with the archive. Indenting it under a
    /// session that is still running would put it above the archive header and
    /// make the fold that hides the archive a lie.
    #[test]
    fn an_archived_subagent_is_not_indented_under_a_live_parent() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.state.show_archived = true;
        app.sessions = vec![
            tree_session("lead", None, 30),
            AgentSession {
                dead: true,
                ..tree_session("helper", Some("lead"), 20)
            },
        ];

        assert_eq!(
            tree_shape(&app),
            vec![("lead".into(), 0, 0), ("helper".into(), 0, 0)]
        );
    }

    /// A fold puts the subagents away and leaves their count on the row that
    /// hides them -- and says when one of them is waiting for an answer, which
    /// is the one thing a fold must never swallow.
    #[test]
    fn a_folded_task_reports_the_subagents_it_hides() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions = vec![
            tree_session("lead", None, 30),
            tree_session("helper", Some("lead"), 20),
            tree_session("alone", None, 10),
        ];
        app.selected_session_id = Some("helper".into());

        // Pressed on a subagent, the key folds the task the subagent is part
        // of: there is nothing under the subagent itself to put away.
        app.toggle_task_fold();
        assert_eq!(
            tree_shape(&app),
            vec![("lead".into(), 0, 1), ("alone".into(), 0, 0)]
        );
        assert_eq!(app.selected_session_id.as_deref(), Some("lead"));
        let folded = app.visible_session_rows()[0].1;
        assert!(folded.folded);
        assert!(!folded.attention);

        // Whatever the hidden subagent is doing shows on the row hiding it.
        app.sessions[1].needs_attention = true;
        app.sessions[1].working = true;
        let shape = app.visible_session_rows()[0].1;
        assert!(shape.attention && shape.working);

        app.toggle_task_fold();
        assert_eq!(
            tree_shape(&app),
            vec![
                ("lead".into(), 0, 1),
                ("helper".into(), 1, 0),
                ("alone".into(), 0, 0),
            ]
        );
        // Nothing is hidden now, so the row has nothing of its own to report:
        // the subagent is on screen saying it itself.
        let listed = app.visible_session_rows()[0].1;
        assert!(!listed.folded && !listed.attention && !listed.working);

        // An agent with no subagents and no task around it has nothing to fold.
        app.select_session("alone".into());
        app.toggle_task_fold();
        assert!(app.state.folded_tasks.is_empty());
        assert!(app.status_message.contains("No subagents"));
    }

    /// A fold outlives the session it hides things under only for as long as
    /// the machine still has that session. Otherwise the state file collects
    /// folds for tasks nobody can ever unfold.
    #[test]
    fn a_fold_is_forgotten_once_its_task_is_gone_from_the_machine() {
        let mut app = ux_test_app(vec![Target::local()]);
        let scan = |sessions: Vec<AgentSession>| Event::Scanned {
            target_id: "local".into(),
            result: Ok((crate::model::Probe::default(), sessions)),
        };
        app.handle_worker_event(scan(vec![
            tree_session("lead", None, 30),
            tree_session("helper", Some("lead"), 20),
        ]));
        app.select_session("lead".into());
        app.toggle_task_fold();
        assert!(app.state.folded_tasks.contains("lead"));

        // A machine that merely goes quiet says nothing about the task.
        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Err("offline".into()),
        });
        assert!(app.state.folded_tasks.contains("lead"));

        app.handle_worker_event(scan(vec![tree_session("later", None, 40)]));
        assert!(app.state.folded_tasks.is_empty());
    }

    /// The daemon reads the turn as the runtime recorded it. What the terminal
    /// happens to be painting is the same thing at best, and half a repaint at
    /// worst.
    #[test]
    fn the_recap_of_a_daemon_session_beats_what_is_on_its_screen() {
        let mut app = ux_test_app(vec![Target::local()]);
        let session = AgentSession {
            recap: Some("wrote the fix".into()),
            ..waiting_agent("")
        };
        app.selected_session_id = Some(session.id.clone());
        app.history.text = "※ recap: scraped off the screen\n".into();
        assert_eq!(app.recap_for(&session), "wrote the fix");
        // A session muxloom only reaches through tmux was scraped once, when
        // the machine was last scanned; the screen in front of us is newer.
        let pane = AgentSession {
            id: "pane-1".into(),
            ..session
        };
        app.selected_session_id = Some(pane.id.clone());
        assert_eq!(app.recap_for(&pane), "scraped off the screen");
    }

    #[test]
    fn opening_a_waiting_agent_clears_its_reminder_until_the_prompt_changes() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions = vec![waiting_agent("approval required")];

        assert_eq!(app.attention_sessions().len(), 1);

        app.acknowledge_attention("muxloomd-codex-waiting");
        assert!(app.attention_sessions().is_empty());
        // The session itself still reports what it is doing.
        assert!(app.sessions[0].needs_attention);

        app.sessions[0].attention_reason = Some("run tests?".into());
        assert_eq!(app.attention_sessions().len(), 1);
    }

    #[test]
    fn a_prompt_that_comes_back_after_being_answered_reminds_again() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions = vec![waiting_agent("approval required")];
        app.acknowledge_attention("muxloomd-codex-waiting");
        assert!(app.attention_sessions().is_empty());

        app.sessions[0].needs_attention = false;
        app.sessions[0].attention_reason = None;
        app.sync_attention_acks();

        app.sessions[0].needs_attention = true;
        app.sessions[0].attention_reason = Some("approval required".into());
        assert_eq!(app.attention_sessions().len(), 1);
    }

    #[test]
    fn a_prompt_in_the_session_you_are_typing_in_never_announces_itself() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions = vec![AgentSession {
            needs_attention: false,
            attention_reason: None,
            ..waiting_agent("")
        }];
        app.selected_session_id = Some("muxloomd-codex-waiting".into());
        app.terminal_session_id = Some("muxloomd-codex-waiting".into());
        app.interactive = true;
        app.pending_activity_refreshes.insert("local".into());

        app.handle_worker_event(Event::ActivityRefreshed {
            target_id: "local".into(),
            result: Ok(vec![waiting_agent("approval required")]),
        });

        assert!(app.sessions[0].needs_attention);
        assert!(app.notifications.is_empty());
        assert!(app.attention_sessions().is_empty());

        // Stepping back out keeps it quiet: that prompt has been seen.
        app.interactive = false;
        assert!(app.attention_sessions().is_empty());
    }

    #[test]
    fn online_daemon_targets_schedule_lightweight_activity_refreshes() {
        let config = Config::default();
        let runtime = Runtime::new(&config);
        let bridges = runtime.bridge_pool();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges,
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].state = ConnectionState::Online;
        app.sessions.push(AgentSession {
            id: "muxloomd-codex-refresh".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "refresh".into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });

        app.refresh_daemon_activity();

        assert!(app.pending_activity_refreshes.contains("local"));
        assert!(matches!(
            receive_request(&request_rx),
            Request::RefreshActivity { target } if target.id == "local"
        ));
    }

    #[test]
    fn grouped_launch_uses_selected_machine_not_old_session() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.extend(["local".into(), "gpu".into()]);
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local(), Target::ssh("gpu")],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "ad-codex-old".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/old".into(),
            label: "old".into(),
            created_at: 1,
            dead: false,
            pid: Some(10),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("ad-codex-old".into());
        app.selected_target = 1;
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Launch(LaunchForm { ref target, .. })) if target.id == "gpu"
        ));
    }

    #[test]
    fn modifier_shortcuts_change_visibility_and_focus() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let state_path =
            std::env::temp_dir().join(format!("muxloom-unit-state-{}.json", std::process::id()));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            state_path.clone(),
            vec![Target::local(), Target::ssh("gpu")],
            worker,
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(app.state.hide_disabled);
        assert_eq!(app.visible_target_indices(), vec![0]);
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.focus, Focus::Agents);
        app.focus = Focus::Recap;
        app.interactive = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.focus, Focus::Agents);
        assert!(
            !app.interactive,
            "sidebar focus must release terminal input"
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn modified_arrows_follow_the_rendered_pane_geometry() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.pane_layout = PaneLayout {
            recap: Some(Rect::new(0, 0, 100, 60)),
            machines: Some(Rect::new(0, 60, 45, 40)),
            agents: Some(Rect::new(45, 60, 55, 40)),
            ..PaneLayout::default()
        };
        app.focus = Focus::Agents;
        app.handle_key(KeyEvent::new(KeyCode::Up, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Recap);
        app.handle_key(KeyEvent::new(KeyCode::Down, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Agents);
        app.handle_key(KeyEvent::new(KeyCode::Left, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Machines);

        app.pane_layout = PaneLayout {
            machines: Some(Rect::new(0, 0, 25, 40)),
            agents: Some(Rect::new(25, 0, 35, 40)),
            recap: Some(Rect::new(60, 0, 60, 40)),
            ..PaneLayout::default()
        };
        app.focus = Focus::Machines;
        app.handle_key(KeyEvent::new(KeyCode::Down, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Machines);
        app.handle_key(KeyEvent::new(KeyCode::Right, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Agents);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn option_arrows_are_a_macos_focus_fallback() {
        assert_eq!(
            focus_navigation_direction(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            Some(FocusDirection::Left)
        );
        assert_eq!(
            focus_navigation_direction(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(FocusDirection::Left)
        );
        assert_eq!(
            focus_navigation_direction(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT)),
            Some(FocusDirection::Right)
        );
    }

    #[test]
    fn modified_arrows_do_not_latch_for_following_plain_arrows() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        assert_eq!(
            app.focus_direction_for_key(KeyEvent::new(KeyCode::Down, pane_focus_modifier())),
            Some(FocusDirection::Down)
        );
        assert_eq!(
            app.focus_direction_for_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn unmodified_terminal_arrows_are_not_used_for_focus() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.focus = Focus::Recap;
        app.interactive = true;
        app.pane_layout = PaneLayout {
            agents: Some(Rect::new(0, 0, 40, 30)),
            recap: Some(Rect::new(40, 0, 60, 30)),
            ..PaneLayout::default()
        };
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Recap);
        assert!(app.interactive);
        app.handle_key(KeyEvent::new(KeyCode::Left, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Agents);
        assert!(!app.interactive);
    }

    #[test]
    fn compact_portrait_focus_has_a_keyboard_exit_from_terminal() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.focus = Focus::Recap;
        app.pane_layout.recap = Some(Rect::new(0, 0, 40, 20));
        app.layout_debug_signature = Some((40, 20, 0, 0, true, true));
        app.handle_key(KeyEvent::new(KeyCode::Down, pane_focus_modifier()));
        assert_eq!(app.focus, Focus::Agents);
    }

    #[test]
    fn the_terminal_row_above_a_horizontal_divider_is_not_a_drag_handle() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.pane_layout = PaneLayout {
            recap: Some(Rect::new(0, 0, 80, 20)),
            agents: Some(Rect::new(0, 20, 80, 10)),
            portrait_terminal_divider: Some(Rect::new(0, 19, 80, 1)),
            ..PaneLayout::default()
        };
        // The last row of terminal output belongs to the terminal.
        assert!(!app.on_divider(40, 18));
        assert!(app.dragging.is_none());
        assert!(app.on_divider(40, 19));
        app.dragging = None;
        assert!(app.on_divider(40, 20));
    }

    #[test]
    fn mouse_drag_changes_sidebar_width() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.pane_layout = PaneLayout {
            machines: Some(Rect::new(0, 2, 32, 20)),
            agents: Some(Rect::new(32, 2, 40, 20)),
            recap: Some(Rect::new(72, 2, 50, 20)),
            machine_divider: Some(Rect::new(31, 2, 1, 20)),
            agents_divider: Some(Rect::new(71, 2, 1, 20)),
            ..PaneLayout::default()
        };
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 31,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 40,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        // The divider lands under the pointer, at column 40.
        assert_eq!(app.state.machine_width, 41);

        app.open_file_manager();
        app.focus = Focus::Agents;
        app.dragging = Some(DragDivider::Agents);
        app.drag_divider(80, 10);
        assert_eq!(app.state.file_width, 49);
        assert_eq!(app.state.agents_width, 40);
        // ...and stays there whichever pane happens to be focused.
        app.focus = Focus::Recap;
        app.drag_divider(80, 10);
        assert_eq!(app.state.file_width, 49);
    }

    #[test]
    fn mouse_wheel_moves_one_item_and_stops_at_list_boundaries() {
        let mut app = ux_test_app(vec![
            Target::local(),
            Target::ssh("gpu-a"),
            Target::ssh("gpu-b"),
        ]);
        app.pane_layout.machines = Some(Rect::new(0, 0, 30, 20));
        let wheel = |kind| MouseEvent {
            kind,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.selected_target, 1);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.selected_target, 2);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.selected_target, 2);
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.selected_target, 1);

        app.modal = Some(Modal::Help(HelpForm { offset: 0 }));
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert!(matches!(
            app.modal,
            Some(Modal::Help(HelpForm { offset: 1 }))
        ));
    }

    #[test]
    fn portrait_divider_drag_persists_independently() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_path = std::env::temp_dir().join(format!("muxloom-divider-{nonce}.json"));
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            state_path.clone(),
            vec![Target::local()],
            worker,
        );
        app.pane_layout = PaneLayout {
            recap: Some(Rect::new(0, 0, 80, 60)),
            machines: Some(Rect::new(0, 60, 36, 40)),
            agents: Some(Rect::new(36, 60, 44, 40)),
            portrait_terminal_divider: Some(Rect::new(0, 59, 80, 1)),
            portrait_machine_divider: Some(Rect::new(35, 60, 1, 40)),
            ..PaneLayout::default()
        };
        for event in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_mouse(MouseEvent {
                kind: event,
                column: 40,
                row: if matches!(event, MouseEventKind::Down(_)) {
                    59
                } else {
                    69
                },
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(app.state.portrait_terminal_percent, 70);
        assert_eq!(app.state.machine_width, 24);
        let reloaded = State::load(&state_path).unwrap();
        assert_eq!(reloaded.portrait_terminal_percent, 70);
        assert_eq!(reloaded.machine_width, 24);
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn direct_terminal_drag_copies_visible_history() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.pane_layout.recap = Some(Rect::new(0, 0, 10, 5));
        app.agent_viewport_width = 8;
        app.agent_viewport_height = 3;
        app.history_offset = 3;
        app.history.text = "one\ntwo\nthree".into();
        app.history_message.clear();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 3,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 3,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.take_clipboard_request().is_none(),
            "letting go holds the selection rather than taking it"
        );
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 3,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one"));
    }

    /// A pane with three lines of history under a small terminal pane, which
    /// is enough to select text in and to scroll back through.
    fn touch_test_app() -> App {
        let mut app = ux_test_app(vec![Target::local()]);
        app.pane_layout.recap = Some(Rect::new(0, 0, 10, 5));
        app.agent_viewport_width = 8;
        app.agent_viewport_height = 3;
        app.history_offset = 3;
        app.history = HistoryPage {
            text: "one\ntwo\nthree".into(),
            history_size: 40,
            pane_height: 3,
            pane_width: 8,
            offset_from_bottom: 3,
            rendered: true,
            more_history: false,
        };
        app.history_message.clear();
        // The terminal these tests run under is whatever the developer happens
        // to be sitting in, so pin the hint to "unknown" and let each test
        // exercise the pointer's own behavior.
        app.touch_hint = None;
        app
    }

    fn pointer(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_swipe_scrolls_a_list_instead_of_selecting_what_it_landed_on() {
        let mut app = ux_test_app(vec![
            Target::local(),
            Target::ssh("a"),
            Target::ssh("b"),
            Target::ssh("c"),
            Target::ssh("d"),
            Target::ssh("e"),
        ]);
        app.pane_layout.machines = Some(Rect::new(0, 0, 30, 12));
        app.machine_rows = std::iter::once((MachineRow::Moderators, 1))
            .chain((0..6).map(|index| (MachineRow::Machine(index), 1)))
            .collect();

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 4, 4));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 4, 1));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 4, 1));
        // Three rows of swipe move three rows of list. The row the finger
        // landed on was never selected on the way, and the lift selects
        // nothing either.
        assert_eq!(app.selected_target, 3);

        // A press that lifts where it landed is still a click.
        tap(
            &mut app,
            pointer(MouseEventKind::Down(MouseButton::Left), 4, 3),
        );
        assert_eq!(app.selected_target, 1);
    }

    #[test]
    fn a_flick_in_the_terminal_scrolls_the_scrollback_and_copies_nothing() {
        let mut app = touch_test_app();
        app.history_offset = 0;

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 3, 1));
        // Five rows between two reports is a finger, not a pointing device.
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 6));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 6));

        assert!(app.touch_detected, "the flick reveals a touch screen");
        assert_eq!(app.history_offset, 5, "the pane walked back five rows");
        assert!(
            app.take_clipboard_request().is_none(),
            "a swipe selects nothing"
        );
    }

    #[test]
    fn a_short_mouse_drag_in_the_terminal_still_selects_text() {
        let mut app = touch_test_app();

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 2));

        assert!(!app.touch_detected, "a mouse crosses cells one at a time");
        assert_eq!(app.history_offset, 3, "selecting never scrolls");
        assert!(app.terminal_selection.is_some(), "the selection is held");
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert_eq!(
            app.take_clipboard_request().as_deref(),
            Some("one\ntwo"),
            "the right-click copied the rows the drag covered"
        );
    }

    /// The bug that made a mouse unusable: one fast drag report latched the
    /// touch heuristic for the rest of the run, and from then on no drag ever
    /// selected anything again.
    #[test]
    fn a_hover_takes_back_the_touch_screen_the_motion_heuristic_guessed() {
        let mut app = touch_test_app();
        app.history_offset = 0;

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 3, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 6));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 6));
        assert!(app.touch_detected, "the flick looked like a finger");

        // Nothing hovers over a touch screen.
        app.handle_mouse(pointer(MouseEventKind::Moved, 4, 2));
        assert!(!app.touch_detected, "the hover proved a pointing device");

        app.history_offset = 3;
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one\ntwo"));

        // And a later flick cannot latch it again.
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 3, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 6));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 6));
        assert!(!app.touch_detected);
    }

    /// A desktop terminal names itself, and nothing a pointer does there is a
    /// finger however far it jumps between two reports.
    #[test]
    fn a_desktop_terminal_never_turns_a_fast_drag_into_a_touch_screen() {
        let mut app = touch_test_app();
        app.touch_hint = Some(false);
        app.history_offset = 3;

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 2));

        assert!(!app.touch_detected, "the terminal said it has a pointer");
        assert_eq!(
            app.history_offset, 3,
            "the drag selected, it did not scroll"
        );
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn the_terminal_a_run_starts_in_decides_what_the_pointer_is() {
        assert_eq!(touch_hint_from("xterm-256color", "", true), Some(true));
        assert_eq!(
            touch_hint_from("xterm-256color", "iTerm.app", false),
            Some(false)
        );
        assert_eq!(touch_hint_from("xterm-kitty", "", false), Some(false));
        // An unknown terminal, and an SSH hop that forwarded only TERM, are
        // both left to the pointer to settle.
        assert_eq!(touch_hint_from("xterm-256color", "", false), None);
        assert_eq!(touch_hint_from("screen", "tmux", false), None);
    }

    /// A finger that settles on the text it wants and only then starts to drag
    /// is selecting, not scrolling — even though it is now moving.
    #[test]
    fn a_press_that_rests_before_it_moves_still_selects() {
        let mut app = touch_test_app();
        app.config.touch = "on".into();

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        // A wobble inside the first moment is not the long press.
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 2, 1));
        let held = app.pointer.as_mut().expect("press tracked");
        held.pressed_at = Instant::now() - LONG_PRESS - Duration::from_millis(50);
        held.first_move_at = Some(Instant::now());
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 1));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 1));

        assert_eq!(app.history_offset, 3, "a rested press never scrolls");
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 1));
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one"));
    }

    #[test]
    fn a_right_click_copies_a_selection_and_pastes_without_one() {
        let mut app = touch_test_app();

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert!(app.take_clipboard_request().is_none(), "nothing to copy");
        assert!(app.take_clipboard_paste_request(), "so it pastes instead");
        assert!(
            !app.take_clipboard_paste_request(),
            "and the request is drained once"
        );

        // The click that copies also clears the highlight, so the click after
        // it pastes rather than taking the same text twice.
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one\ntwo"));
        assert!(!app.take_clipboard_paste_request());
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert!(app.take_clipboard_paste_request());
    }

    #[test]
    fn a_long_press_then_drag_selects_terminal_text_on_a_touch_screen() {
        let mut app = touch_test_app();
        app.config.touch = "on".into();

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        // The press sat still long enough to mean "select from here".
        let held = app.pointer.as_mut().expect("press tracked");
        held.pressed_at = Instant::now() - LONG_PRESS - Duration::from_millis(50);
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 1));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 1));

        assert_eq!(app.history_offset, 3, "a held press never scrolls");
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 1));
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one"));
    }

    #[test]
    fn touch_off_keeps_every_drag_a_selection() {
        let mut app = touch_test_app();
        app.config.touch = "off".into();

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 1, 1));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 3, 2));

        assert!(!app.touch_gestures_active());
        assert_eq!(app.history_offset, 3);
        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Right), 3, 2));
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn a_swipe_scrolls_the_open_modal() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.modal = Some(Modal::Help(HelpForm::default()));

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 10, 10));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 10, 6));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 10, 6));

        assert!(matches!(
            app.modal,
            Some(Modal::Help(HelpForm { offset: 4 }))
        ));
    }

    #[test]
    fn a_sideways_swipe_moves_panes_where_only_one_is_on_screen() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.focus = Focus::Machines;
        app.pane_layout = PaneLayout {
            machines: Some(Rect::new(0, 0, 40, 20)),
            ..PaneLayout::default()
        };
        app.layout_debug_signature = Some((40, 20, 0, 0, false, true));

        app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 30, 10));
        app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 8, 11));
        app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 8, 11));

        // The panes follow the finger: dragging left brings in the pane on the
        // right, and one swipe moves exactly one pane.
        assert_eq!(app.focus, Focus::Agents);
    }

    #[test]
    fn history_windows_materialize_small_scroll_steps() {
        let source = HistoryPage {
            text: (0..100)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            history_size: 1_000,
            pane_height: 20,
            pane_width: 80,
            offset_from_bottom: 0,
            rendered: true,
            more_history: false,
        };
        let page = materialize_history_page(&source, 3, 20).unwrap();
        assert!(page.text.ends_with("line-96"));
        assert_eq!(page.offset_from_bottom, 3);
        assert!(materialize_history_page(&source, 81, 20).is_none());
        assert!(materialize_history_page(&source, 1_001, 20).is_none());
        assert_eq!(history_capture_offset(481, 500, 20), 460);
    }

    #[test]
    fn history_scroll_stops_at_tmux_scrollback_boundary() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.history = HistoryPage {
            text: "oldest\nnewest".into(),
            history_size: 12,
            pane_height: 24,
            pane_width: 80,
            offset_from_bottom: 10,
            rendered: true,
            more_history: false,
        };
        app.history_offset = 10;
        app.scroll_history(true, 20);
        assert_eq!(app.history_offset, 12);
    }

    #[test]
    fn history_scroll_keeps_going_while_older_rows_are_still_expected() {
        // Rendering a page replays only as far back as the page was asked to
        // reach, so its history_size measures that reach and not the session.
        // Read as a boundary it ends the history after a single page, which is
        // what left Codex able to scroll a little and no further.
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.history = HistoryPage {
            text: "oldest\nnewest".into(),
            history_size: 40,
            pane_height: 40,
            pane_width: 80,
            offset_from_bottom: 0,
            rendered: true,
            more_history: true,
        };
        app.history_offset = 0;
        app.status_message.clear();

        app.scroll_history(true, 38);
        assert_eq!(app.history_offset, 38);
        app.scroll_history(true, 38);
        assert_eq!(
            app.history_offset, 76,
            "past the reach of the page it holds"
        );
        assert!(app.history.has_older());
        assert!(app.status_message.is_empty(), "{}", app.status_message);

        // Only once a page falls short of the offset it was asked for does the
        // history end, and then the view stops there.
        app.history.more_history = false;
        app.scroll_history(true, 38);
        assert_eq!(app.history_offset, 40);
        assert!(app.status_message.contains("oldest available history"));
    }

    #[test]
    fn attached_claude_pages_into_daemon_history_past_emulator_buffer() {
        let (mut app, request_rx, root, buffered_boundary) = attached_claude_app("history");

        app.scroll_history(true, 3);

        assert_eq!(app.history_offset, buffered_boundary + 3);
        assert!(!app.attached_history_is_buffered());
        let requested_offset = match receive_request(&request_rx) {
            Request::Capture {
                session_id,
                offset_from_bottom,
                ..
            } => {
                assert_eq!(session_id, "muxloomd-claude-long-history");
                assert!(offset_from_bottom <= app.history_offset);
                offset_from_bottom
            }
            request => panic!("expected history capture, got {request:?}"),
        };
        app.handle_worker_event(Event::Captured {
            target_id: "local".into(),
            session_id: "muxloomd-claude-long-history".into(),
            result: Ok(daemon_page(requested_offset, true)),
        });
        assert_eq!(app.history.offset_from_bottom, buffered_boundary + 3);
        assert!(app.history.text.contains("daemon-line"));
        assert!(!app.attached_history_is_buffered());

        app.scroll_history(false, 3);
        assert_eq!(app.history_offset, buffered_boundary);
        assert!(app.attached_history_is_buffered());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attached_paging_stops_at_the_buffer_when_the_daemon_answers_in_log_lines() {
        // A daemon too old to render history counts raw log lines, and an agent
        // writes tens of them per row it puts on screen. Handing that offset
        // over would drop the view somewhere near the live screen, showing a
        // fragment of a redraw, so the view stays on the oldest row it holds.
        let (mut app, request_rx, root, buffered_boundary) = attached_claude_app("log-lines");

        app.scroll_history(true, 3);
        let requested_offset = match receive_request(&request_rx) {
            Request::Capture {
                offset_from_bottom, ..
            } => offset_from_bottom,
            request => panic!("expected history capture, got {request:?}"),
        };
        app.handle_worker_event(Event::Captured {
            target_id: "local".into(),
            session_id: "muxloomd-claude-long-history".into(),
            result: Ok(daemon_page(requested_offset, false)),
        });

        assert_eq!(app.history_offset, buffered_boundary);
        assert!(app.attached_history_is_buffered(), "back on the emulator");
        assert!(
            app.status_message.contains("oldest buffered line"),
            "said so: {}",
            app.status_message
        );

        // And it stays there rather than asking for another page.
        app.scroll_history(true, 3);
        assert_eq!(app.history_offset, buffered_boundary);
        assert!(request_rx.try_recv().is_err(), "no further capture");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attached_paging_stops_at_the_oldest_row_the_daemon_holds() {
        // A page that read the log from its beginning measures the session, so
        // there is nothing above its oldest row to ask for. Scrolling past it
        // leaves the view on an offset no page can answer -- blank, and
        // spending a capture per keystroke on the same reply -- which is what
        // paging off the top of a session looked like.
        let (mut app, request_rx, root, boundary) = attached_claude_app("history-top");
        let top = boundary + 6;

        app.scroll_history(true, 3);
        let requested_offset = match receive_request(&request_rx) {
            Request::Capture {
                offset_from_bottom, ..
            } => offset_from_bottom,
            request => panic!("expected history capture, got {request:?}"),
        };
        let mut page = daemon_page(requested_offset, true);
        page.history_size = top;
        page.more_history = false;
        app.handle_worker_event(Event::Captured {
            target_id: "local".into(),
            session_id: "muxloomd-claude-long-history".into(),
            result: Ok(page),
        });
        assert_eq!(app.history_offset, boundary + 3, "still short of the top");

        app.scroll_history(true, 20);

        assert_eq!(app.history_offset, top, "stopped on the oldest row");
        assert!(
            app.status_message.contains("oldest available history"),
            "said so: {}",
            app.status_message
        );
        assert!(app.history.text.contains("daemon-line"), "still showing it");
        assert!(
            request_rx.try_recv().is_err(),
            "no capture for missing rows"
        );

        // And it stays there, however long the key is held.
        app.scroll_history(true, 20);
        assert_eq!(app.history_offset, top);
        assert!(
            request_rx.try_recv().is_err(),
            "no capture for missing rows"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A page of history as the daemon hands one back, either rendered into
    /// rows or read off as raw log lines.
    fn daemon_page(offset_from_bottom: usize, rendered: bool) -> HistoryPage {
        HistoryPage {
            text: (0..500)
                .map(|line| format!("daemon-line-{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            history_size: 1_000,
            pane_height: 5,
            pane_width: 20,
            offset_from_bottom,
            rendered,
            more_history: rendered,
        }
    }

    #[test]
    fn a_scrolled_back_view_stays_on_its_rows_while_the_session_prints() {
        // The emulator lifts its own offset for every line that arrives so the
        // page keeps the same rows. Handing it the app's older count back on
        // each frame slid the view down a line per line of output, which read
        // as the transcript crawling away under the reader.
        let (mut app, _request_rx, _root, _boundary) = attached_claude_app("scroll-drift");
        app.terminal
            .as_mut()
            .expect("terminal attached")
            .set_scrollback(0);
        app.history_offset = 0;
        app.terminal_scrollback_pin = 0;
        app.scroll_history(true, 2);
        assert_eq!(app.history_offset, 2, "two rows up");
        let before = app
            .terminal
            .as_ref()
            .expect("terminal attached")
            .screen()
            .contents();

        app.terminal
            .as_mut()
            .expect("terminal attached")
            .process_output_for_test(b"fresh-1\r\n");
        let offset = app.sync_terminal_scrollback();

        assert_eq!(offset, 3, "counted from the new bottom");
        assert_eq!(app.history_offset, offset, "and the app agrees");
        assert_eq!(
            app.terminal
                .as_ref()
                .expect("terminal attached")
                .screen()
                .contents(),
            before,
            "same rows on screen"
        );
    }

    #[test]
    fn scrolling_mid_drag_keeps_the_selection_being_made() {
        // Dragging a selection past the top of the pane scrolls the view; if
        // that scroll dropped the selection, the drag could never reach a row
        // that was off screen when it started.
        let (mut app, _request_rx, _root, _boundary) = attached_claude_app("drag-scroll");
        let dragging = TerminalSelection {
            anchor: TerminalPoint { row: 3, column: 0 },
            cursor: TerminalPoint { row: 0, column: 4 },
            dragging: true,
        };
        app.terminal_selection = Some(dragging);

        app.scroll_history(true, 1);
        assert_eq!(app.terminal_selection, Some(dragging), "drag survives");

        app.terminal_selection = Some(TerminalSelection {
            dragging: false,
            ..dragging
        });
        app.scroll_history(true, 1);

        assert_eq!(
            app.terminal_selection, None,
            "a finished selection still clears"
        );
    }

    #[test]
    fn an_upload_reports_how_far_along_it_is() {
        // A drop of a large file used to sit on "Uploading dropped files..."
        // until it finished, with no way to tell a slow link from a stuck one.
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );

        app.handle_worker_event(Event::FileUploadProgress {
            name: "notes.md".into(),
            transferred: 512,
            total_size: 2048,
            bytes_per_second: 1024.0,
        });

        assert!(
            app.status_message.contains("Uploading notes.md"),
            "named the file: {}",
            app.status_message
        );
        assert!(app.status_message.contains("25%"), "{}", app.status_message);
        assert!(
            app.status_message.contains("1.0 KiB/s"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn pasting_without_input_says_where_the_text_went() {
        let (mut app, _request_rx, _root, boundary) = attached_claude_app("paste");
        app.interactive = false;

        app.handle_paste("cargo test\n".into());

        assert!(
            app.status_message.contains("take input"),
            "explained itself: {}",
            app.status_message
        );
        assert_eq!(app.history_offset, boundary, "and left the view alone");
    }

    #[test]
    fn pasting_into_a_scrolled_back_terminal_returns_to_the_live_tail() {
        // The text lands at the prompt on the bottom row, so the view has to
        // follow it there the way a keystroke does.
        let (mut app, _request_rx, _root, boundary) = attached_claude_app("paste-scrolled");
        app.interactive = true;
        assert_eq!(app.history_offset, boundary, "starts scrolled back");

        app.handle_paste("cargo test\n".into());

        assert_eq!(app.history_offset, 0, "back on the live rows");
    }

    /// An app attached to a Claude session that has scrolled a screenful of
    /// rows into its emulator, parked at the oldest one it holds.
    fn attached_claude_app(
        name: &str,
    ) -> (App, std::sync::mpsc::Receiver<Request>, PathBuf, usize) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-claude-{name}-{nonce}"));
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            root.join("state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].state = ConnectionState::Online;
        app.sessions.push(AgentSession {
            id: "muxloomd-claude-long-history".into(),
            target_id: "local".into(),
            kind: AgentKind::Claude,
            path: "/work".into(),
            label: "long history".into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloomd-claude-long-history".into());
        app.terminal_session_id = Some("muxloomd-claude-long-history".into());
        app.agent_viewport_width = 20;
        app.agent_viewport_height = 5;

        let mut terminal = TerminalSession::detached(20, 5);
        let output = (1..=40)
            .map(|line| format!("line-{line}\r\n"))
            .collect::<String>();
        terminal.process_output_for_test(output.as_bytes());
        let buffered_boundary = terminal.max_scrollback();
        assert!(buffered_boundary > 3);
        terminal.set_scrollback(buffered_boundary);
        app.terminal = Some(terminal);
        app.history_offset = buffered_boundary;
        (app, request_rx, root, buffered_boundary)
    }

    #[test]
    fn terminal_title_back_button_returns_to_agents_with_the_mouse() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.focus = Focus::Recap;
        app.terminal_back = Some(Rect::new(1, 3, 8, 1));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.focus, Focus::Agents);
        assert_eq!(app.status_message, "Returned to agent list");
    }

    #[test]
    fn settings_save_and_reload_current_config() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-settings-{nonce}"));
        std::fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.toml");
        let ssh_path = root.join("ssh-config");
        std::fs::write(&ssh_path, "Host test-machine\n").unwrap();
        let config = Config {
            ssh_config: ssh_path.display().to_string(),
            ..Config::default()
        };
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            config_path.clone(),
            state,
            root.join("state.json"),
            vec![Target::local()],
            worker,
        );
        app.open_global_settings();
        let Some(Modal::Settings(mut form)) = app.modal.take() else {
            panic!("settings modal did not open");
        };
        form.values[0] = "1500".into();
        // The terminal's command sits behind the three general fields and the
        // command/args pair each agent runtime contributes.
        let terminal_command = 3 + AgentKind::agents().count() * 2;
        form.values[terminal_command] = "/bin/zsh".into();
        app.apply_settings(form);

        assert_eq!(app.config.refresh_interval_ms, 1500);
        assert_eq!(app.config.agents.terminal.command, "/bin/zsh");
        assert!(
            app.targets
                .iter()
                .any(|target| target.target.id == "test-machine")
        );
        let reloaded = Config::load(&config_path).unwrap();
        assert_eq!(reloaded.refresh_interval_ms, 1500);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_keeps_a_previously_online_machine_online_while_scanning() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].state = ConnectionState::Online;
        app.refresh_target("local");
        assert_eq!(app.targets[0].state, ConnectionState::Online);
    }

    #[test]
    fn dead_sessions_are_collapsed_into_archive_by_default() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "ad-codex-dead".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "finished".into(),
            created_at: 1,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.sessions.push(AgentSession {
            id: "muxloom-terminal-dead".into(),
            target_id: "local".into(),
            kind: AgentKind::Terminal,
            path: "/work".into(),
            label: "finished shell".into(),
            created_at: 2,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        assert!(app.visible_sessions().is_empty());
        assert_eq!(app.archived_count(), 1);
        app.state.show_archived = true;
        assert_eq!(app.visible_sessions().len(), 1);

        // Archiving something is how it gets out of the way. Whatever the
        // archive was doing, it goes on doing.
        for folded in [false, true] {
            app.state.show_archived = folded;
            app.handle_worker_event(Event::Archived {
                target_id: "local".into(),
                session_id: "ad-codex-dead".into(),
                result: Ok(()),
            });
            assert_eq!(app.state.show_archived, folded);
        }
    }

    #[test]
    fn opening_an_archived_agent_resumes_its_latest_history() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let state_path = std::env::temp_dir().join(format!(
            "muxloom-archived-resume-state-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            state_path.clone(),
            vec![Target::local()],
            worker,
        );
        app.targets[0].probe.set(AgentKind::Codex, true);
        app.sessions.push(AgentSession {
            id: "muxloom-codex-dead".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "fix renderer".into(),
            created_at: 1,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloom-codex-dead".into());
        app.focus = Focus::Agents;

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match receive_request(&request_rx) {
            Request::ScanResumes { target, kind, path } => {
                assert_eq!(target.id, "local");
                assert_eq!(kind, AgentKind::Codex);
                assert_eq!(path, "/work/project");
            }
            request => panic!("expected archived resume scan, got {request:?}"),
        }

        app.handle_worker_event(Event::ResumesScanned {
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            warning: None,
            result: Ok(vec![ResumeCandidate {
                id: "thread-id".into(),
                kind: AgentKind::Codex,
                source_path: "/home/test/.codex/sessions/thread.jsonl".into(),
                recap: Some("Fix the renderer".into()),
                first_message: None,
                last_message: None,
                updated_at: "2026-07-22T12:00:00Z".into(),
            }]),
        });
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmArchivedResume {
                ref source_session_id,
                ref resume_id,
                remove_archive: true,
                ..
            }) if source_session_id == "muxloom-codex-dead" && resume_id == "thread-id"
        ));
        assert!(request_rx.try_recv().is_err());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match receive_request(&request_rx) {
            Request::Launch {
                request,
                remove_archive_session_id,
                ..
            } => {
                assert_eq!(request.target.id, "local");
                assert_eq!(request.kind, AgentKind::Codex);
                assert_eq!(request.path, "/work/project");
                assert_eq!(request.label, "fix renderer");
                assert_eq!(request.resume_id.as_deref(), Some("thread-id"));
                assert_eq!(
                    remove_archive_session_id.as_deref(),
                    Some("muxloom-codex-dead")
                );
            }
            request => panic!("expected archived resume launch, got {request:?}"),
        }
        assert!(
            State::load(&state_path)
                .unwrap()
                .remove_archive_after_resume
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn archived_resume_can_keep_the_old_entry_and_remembers_the_choice() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let state_path = std::env::temp_dir().join(format!(
            "muxloom-keep-archive-state-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            State::default(),
            state_path.clone(),
            vec![Target::local()],
            worker,
        );
        app.targets[0].probe.set(AgentKind::Codex, true);
        app.modal = Some(Modal::ConfirmArchivedResume {
            source_session_id: "old-archive".into(),
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Codex,
                path: "/work/project".into(),
                label: "resume me".into(),
                temporary: false,
                field: LaunchField::Kind,
            },
            resume_id: "thread-id".into(),
            remove_archive: true,
        });

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmArchivedResume {
                remove_archive: false,
                ..
            })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            receive_request(&request_rx),
            Request::Launch {
                remove_archive_session_id: None,
                ..
            }
        ));
        assert!(!app.state.remove_archive_after_resume);
        assert!(
            !State::load(&state_path)
                .unwrap()
                .remove_archive_after_resume
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn old_archive_is_removed_only_after_a_successful_resumed_launch() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let state_path = std::env::temp_dir().join(format!(
            "muxloom-remove-archive-state-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            State::default(),
            state_path.clone(),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "old-archive".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "old".into(),
            created_at: 1,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });

        app.handle_worker_event(Event::Launched {
            target_id: "local".into(),
            notice: None,
            result: Ok("new-session".into()),
            remove_archive_session_id: Some("old-archive".into()),
        });
        assert!(matches!(
            receive_request(&request_rx),
            Request::RemoveResumedArchive {
                ref session_id,
                ..
            } if session_id == "old-archive"
        ));
        assert!(
            app.sessions
                .iter()
                .any(|session| session.id == "old-archive")
        );

        app.handle_worker_event(Event::ResumedArchiveRemoved {
            target_id: "local".into(),
            session_id: "old-archive".into(),
            result: Ok(()),
        });
        assert!(
            app.sessions
                .iter()
                .all(|session| session.id != "old-archive")
        );

        app.sessions.push(AgentSession {
            id: "failed-archive".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "failed".into(),
            created_at: 2,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.handle_worker_event(Event::Launched {
            target_id: "local".into(),
            notice: None,
            result: Err("resume failed".into()),
            remove_archive_session_id: Some("failed-archive".into()),
        });
        assert!(request_rx.try_recv().is_err());
        assert!(
            app.sessions
                .iter()
                .any(|session| session.id == "failed-archive")
        );
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn x_archives_live_agents_and_permanently_removes_dead_ones() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "muxloom-codex-live".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "live".into(),
            created_at: 1,
            dead: false,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloom-codex-live".into());
        app.focus = Focus::Agents;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmKill { archive: true, .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            receive_request(&request_rx),
            Request::Archive { .. }
        ));

        app.sessions[0].dead = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmKill { archive: false, .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(receive_request(&request_rx), Request::Kill { .. }));
    }

    #[test]
    fn legacy_tmux_fallback_requires_visible_acknowledgement() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.handle_worker_event(Event::Launched {
            target_id: "remote".into(),
            notice: Some("muxloomd bootstrap failed".into()),
            result: Ok("muxloom-codex-legacy".into()),
            remove_archive_session_id: None,
        });

        assert!(matches!(
            app.modal,
            Some(Modal::LegacyFallback {
                ref target_id,
                ref detail,
            }) if target_id == "remote" && detail.contains("bootstrap failed")
        ));
        assert!(app.status_message.contains("legacy tmux fallback"));
        assert!(
            app.notifications
                .iter()
                .any(|notification| notification.contains("legacy tmux fallback"))
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal.is_none());
    }

    #[test]
    fn file_manager_lists_previews_uploads_and_copies_remote_paths() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "muxloom-codex-files".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "files".into(),
            created_at: 1,
            dead: false,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloom-codex-files".into());
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.origin),
            Some(FileManagerOrigin::AgentPane)
        );
        assert!(matches!(
            receive_request(&request_rx),
            Request::ListFiles { ref path, .. } if path == "/work/project"
        ));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work/project".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work/project".into(),
                entries: vec![FileEntry {
                    name: "README.md".into(),
                    path: "/work/project/README.md".into(),
                    kind: FileEntryKind::File,
                    symlink: false,
                    size: 42,
                    mtime: 0,
                }],
            }),
        });
        // Plain letters filter the listing; downloads live on Ctrl-d so that
        // typing a name starting with "d" is not intercepted.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.as_str()),
            Some("d")
        );
        assert!(request_rx.try_recv().is_err());
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(
            receive_request(&request_rx),
            Request::DownloadFile { total_size: 42, .. }
        ));
        app.handle_worker_event(Event::FileDownloadProgress {
            remote_path: "/work/project/README.md".into(),
            transferred: 21,
            total_size: 42,
            bytes_per_second: 2048.0,
        });
        assert!(app.status_message.contains("50%"));
        assert!(app.status_message.contains("2.0 KiB/s"));
        app.handle_worker_event(Event::FileDownloaded {
            result: Ok(PathBuf::from("/tmp/README.md")),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.as_str()),
            Some("n")
        );
        assert!(app.modal.is_none());
        assert!(request_rx.try_recv().is_err());
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(app.modal.is_none());
        assert!(request_rx.try_recv().is_err());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            receive_request(&request_rx),
            Request::PreviewFile { ref path, .. } if path == "/work/project/README.md"
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(
            app.take_clipboard_request().as_deref(),
            Some("/work/project/README.md")
        );
        {
            let form = app.file_manager.as_mut().unwrap();
            form.preview_max_scroll = 20;
            form.preview_page_rows = 8;
        }
        for expected in [8, 16, 20, 20] {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(
                app.file_manager.as_ref().map(|form| form.preview_scroll),
                Some(expected)
            );
        }
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.preview_scroll),
            Some(12)
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            app.file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_none())
        );
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(
            receive_request(&request_rx),
            Request::ListFiles { ref path, .. } if path == "/work"
        ));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(
            receive_request(&request_rx),
            Request::ListFiles { ref path, .. } if path == "/work/project"
        ));
        assert!(app.file_manager.as_ref().is_some_and(|form| {
            form.loading && form.path == "/work/project" && !form.entries.is_empty()
        }));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: Vec::new(),
            }),
        });
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.path.as_str()),
            Some("/work/project")
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            receive_request(&request_rx),
            Request::PreviewFile { ref path, .. } if path == "/work/project/README.md"
        ));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work/project".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work/project".into(),
                entries: Vec::new(),
            }),
        });
        assert!(
            app.file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_none() && form.preview.is_none())
        );
        let dropped =
            std::env::temp_dir().join(format!("muxloom-file-drop-{}", std::process::id()));
        std::fs::write(&dropped, "upload").unwrap();
        app.handle_paste(dropped.display().to_string());
        assert!(matches!(
            receive_request(&request_rx),
            Request::UploadFiles { ref remote_directory, .. } if remote_directory == "/work/project"
        ));
        let _ = std::fs::remove_file(dropped);
    }

    #[test]
    fn file_browser_captures_input_only_while_its_pane_is_focused() {
        let config = Config::default();
        let (request_tx, _request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "muxloom-codex-modal".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "modal".into(),
            created_at: 1,
            dead: false,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloom-codex-modal".into());
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Agents);

        // Focused browser pane: keys edit its filter.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.clone()),
            Some("x".into())
        );

        // Pane shortcuts are handled before the browser's modal key capture.
        app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT));
        assert_eq!(app.focus, Focus::Recap);
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.clone()),
            Some("x".into())
        );

        // Focus another pane: the browser stays open but no longer swallows keys.
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.file_manager.is_some());
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.clone()),
            Some("x".into())
        );

        // Refocusing the browser pane routes input back into it.
        app.focus = Focus::Agents;
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.clone()),
            Some("xz".into())
        );
    }

    #[test]
    fn entering_or_leaving_a_folder_clears_the_search_filter() {
        let config = Config::default();
        let (request_tx, _request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "muxloom-codex-nav".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "nav".into(),
            created_at: 1,
            dead: false,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloom-codex-nav".into());
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work/project".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work/project".into(),
                entries: vec![FileEntry {
                    name: "src".into(),
                    path: "/work/project/src".into(),
                    kind: FileEntryKind::Directory,
                    symlink: false,
                    size: 0,
                    mtime: 0,
                }],
            }),
        });

        // Filter, then descend into the directory: the filter resets.
        app.file_manager.as_mut().unwrap().query = "src".into();
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.path.as_str()),
            Some("/work/project/src")
        );
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.as_str()),
            Some("")
        );

        // Filter again, then go back up to the parent: the filter resets again.
        app.file_manager.as_mut().unwrap().query = "proj".into();
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.path.as_str()),
            Some("/work/project")
        );
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.as_str()),
            Some("")
        );
    }

    #[test]
    fn spinner_frame_advances_on_wall_clock_not_per_iteration() {
        let config = Config::default();
        let (request_tx, _request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        // Many ticks in a tight loop (well under one frame window) must not
        // advance the spinner once per iteration; a burst of redraws from mouse
        // movement must not speed the animation up.
        let start = app.animation_frame;
        for _ in 0..64 {
            app.on_tick();
        }
        assert!(app.animation_frame - start < 64);
    }

    #[test]
    fn attached_terminal_ctrl_f_opens_the_terminal_pane_file_browser() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.focus = Focus::Recap;
        app.interactive = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));

        assert_eq!(
            app.file_manager.as_ref().map(|form| form.origin),
            Some(FileManagerOrigin::TerminalPane)
        );
        assert!(!app.interactive);
        assert!(matches!(
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Request::ListFiles { path, .. } if path == "."
        ));
    }

    #[test]
    fn file_manager_preloads_only_neighbor_file_previews() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.open_file_manager();
        assert!(matches!(
            receive_request(&request_rx),
            Request::ListFiles { .. }
        ));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: ".".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![
                    FileEntry {
                        name: "alpha.txt".into(),
                        path: "/work/alpha.txt".into(),
                        kind: FileEntryKind::File,
                        symlink: false,
                        size: 5,
                        mtime: 0,
                    },
                    FileEntry {
                        name: "beta.rs".into(),
                        path: "/work/beta.rs".into(),
                        kind: FileEntryKind::File,
                        symlink: false,
                        size: 12,
                        mtime: 0,
                    },
                    FileEntry {
                        name: "src".into(),
                        path: "/work/src".into(),
                        kind: FileEntryKind::Directory,
                        symlink: false,
                        size: 0,
                        mtime: 0,
                    },
                ],
            }),
        });
        let pending: Vec<_> = request_rx.try_iter().collect();
        assert!(pending.iter().any(|request| matches!(
            request,
            Request::PreloadPreview { path, .. } if path == "/work/beta.rs"
        )));
        assert!(
            !pending
                .iter()
                .any(|request| matches!(request, Request::PreloadDirectory { .. }))
        );
        app.handle_worker_event(Event::PreviewPreloaded {
            target_id: "local".into(),
            path: "/work/beta.rs".into(),
            result: Ok(FilePreview {
                path: "/work/beta.rs".into(),
                mime: "text/plain".into(),
                kind: crate::model::FilePreviewKind::Text,
                size: 12,
                content: "fn beta() {}".into(),
                truncated: false,
            }),
        });
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.file_manager.as_ref().is_some_and(|form| {
            form.preview_path.as_deref() == Some("/work/beta.rs")
                && form
                    .preview
                    .as_ref()
                    .is_some_and(|preview| preview.content == "fn beta() {}")
                && !form.preview_loading
        }));
        // The cached body shows at once, but it is only a head start: a refresh
        // goes out so an edit made since the preload still reaches the screen.
        assert!(request_rx.try_iter().any(
            |request| matches!(request, Request::PreviewFile { path, .. } if path == "/work/beta.rs")
        ));
    }

    /// Builds an app whose worker requests land in the returned channel, with a
    /// browser already showing `notes.log` previewed at scroll offset 4.
    fn preview_monitor_app(
        size: u64,
        mtime: u64,
    ) -> (App, std::sync::mpsc::Receiver<Request>, FileEntry) {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        let entry = FileEntry {
            name: "notes.log".into(),
            path: "/work/notes.log".into(),
            kind: FileEntryKind::File,
            symlink: false,
            size,
            mtime,
        };
        let mut form = blank_file_manager(Target::local(), None, "/work");
        form.entries = vec![entry.clone()];
        form.preview_path = Some(entry.path.clone());
        form.preview_stamp = Some((size, mtime));
        form.preview = Some(FilePreview {
            path: entry.path.clone(),
            mime: "text/plain".into(),
            kind: crate::model::FilePreviewKind::Text,
            size,
            content: "first\n".into(),
            truncated: false,
        });
        form.preview_scroll = 4;
        form.preview_max_scroll = 4;
        app.file_manager = Some(form);
        (app, request_rx, entry)
    }

    #[test]
    fn listing_refreshes_the_open_preview_only_when_the_file_changed() {
        let (mut app, request_rx, entry) = preview_monitor_app(6, 1_700_000_000);
        let listing = |entry: FileEntry| Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![entry],
            }),
        };

        // An unchanged stamp must not put a preview read on the wire; that poll
        // runs every second or two and the file can be a quarter of a megabyte.
        app.handle_worker_event(listing(entry.clone()));
        assert!(
            !request_rx
                .try_iter()
                .any(|request| matches!(request, Request::PreviewFile { .. }))
        );

        let grown = FileEntry {
            size: 12,
            mtime: 1_700_000_050,
            ..entry
        };
        app.handle_worker_event(listing(grown.clone()));
        assert!(request_rx.try_iter().any(
            |request| matches!(request, Request::PreviewFile { path, .. } if path == grown.path)
        ));
        // Nothing on screen changes yet: no spinner, and the body stays put
        // until the fresh copy actually arrives.
        let form = app.file_manager.as_ref().expect("browser stays open");
        assert!(!form.preview_loading);
        assert_eq!(
            form.preview
                .as_ref()
                .map(|preview| preview.content.as_str()),
            Some("first\n")
        );
    }

    /// A preview shows the whole file, so watching a big one would drag it
    /// across the link on every change. Those wait for the reader to ask.
    #[test]
    fn large_previews_refresh_on_demand_instead_of_on_every_change() {
        let (mut app, request_rx, entry) =
            preview_monitor_app(AUTO_REFRESH_LIMIT + 1, 1_700_000_000);
        app.focus = Focus::Agents;
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![FileEntry {
                    size: AUTO_REFRESH_LIMIT + 4_096,
                    mtime: 1_700_000_050,
                    ..entry.clone()
                }],
            }),
        });
        assert!(
            !request_rx
                .try_iter()
                .any(|request| matches!(request, Request::PreviewFile { .. })),
            "the monitor leaves large files alone"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(request_rx.try_iter().any(
            |request| matches!(request, Request::PreviewFile { path, .. } if path == entry.path)
        ));
        // Asking for a fresh copy must not close the file or reset the reader.
        let form = app.file_manager.as_ref().expect("browser stays open");
        assert_eq!(form.preview_path.as_deref(), Some(entry.path.as_str()));
        assert_eq!(form.preview_scroll, 4);
    }

    #[test]
    fn refreshed_preview_keeps_the_reader_where_they_were() {
        let (mut app, _request_rx, entry) = preview_monitor_app(6, 1_700_000_000);
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![FileEntry {
                    size: 12,
                    mtime: 1_700_000_050,
                    ..entry.clone()
                }],
            }),
        });
        app.handle_worker_event(Event::FilePreviewed {
            target_id: "local".into(),
            path: entry.path.clone(),
            result: Ok(FilePreview {
                path: entry.path.clone(),
                mime: "text/plain".into(),
                kind: crate::model::FilePreviewKind::Text,
                size: 12,
                content: "first\nsecond\n".into(),
                truncated: false,
            }),
        });
        let form = app.file_manager.as_ref().expect("browser stays open");
        assert_eq!(
            form.preview
                .as_ref()
                .map(|preview| preview.content.as_str()),
            Some("first\nsecond\n")
        );
        // A refresh is not a fresh open, so the view must not jump back to the
        // top of the file under the reader.
        assert_eq!(form.preview_scroll, 4);
        assert!(form.preview_requested_path.is_none());
    }

    #[test]
    fn preview_follows_the_tail_only_after_scrolling_to_the_bottom() {
        let (mut app, _request_rx, _entry) = preview_monitor_app(6, 1_700_000_000);
        app.focus = Focus::Recap;
        let scroll_to_top = |app: &mut App| {
            app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        };

        scroll_to_top(&mut app);
        assert_eq!(
            app.file_manager
                .as_ref()
                .map(|form| (form.preview_scroll, form.preview_follow_tail)),
            Some((0, false))
        );

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(
            app.file_manager
                .as_ref()
                .map(|form| (form.preview_scroll, form.preview_follow_tail)),
            Some((4, true))
        );

        // Paging back up releases the tail so a refresh no longer drags the
        // view forward.
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(
            app.file_manager
                .as_ref()
                .map(|form| form.preview_follow_tail),
            Some(false)
        );
    }

    #[test]
    fn file_preview_owns_terminal_pane_focus_without_activating_agent_input() {
        let mut app = ux_test_app(vec![Target::local()]);
        let mut form = blank_file_manager(Target::local(), None, "/work");
        form.preview_path = Some("/work/README.md".into());
        app.file_manager = Some(form);
        app.focus = Focus::Agents;
        app.interactive = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT));

        assert_eq!(app.focus, Focus::Recap);
        assert!(!app.interactive);
        assert!(
            app.file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_some())
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Agents);
        assert!(
            app.file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_none())
        );
    }

    #[test]
    fn file_manager_mouse_double_clicks_entries_and_right_clicks_parent() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.open_file_manager();
        let _ = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: ".".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![
                    FileEntry {
                        name: "README.md".into(),
                        path: "/work/README.md".into(),
                        kind: FileEntryKind::File,
                        symlink: false,
                        size: 300_000,
                        mtime: 0,
                    },
                    FileEntry {
                        name: "src".into(),
                        path: "/work/src".into(),
                        kind: FileEntryKind::Directory,
                        symlink: false,
                        size: 0,
                        mtime: 0,
                    },
                ],
            }),
        });
        {
            let form = app.file_manager.as_mut().unwrap();
            form.list_area = Some(Rect::new(0, 0, 20, 4));
            form.entry_rows = vec![(0, Rect::new(0, 1, 20, 1)), (1, Rect::new(0, 2, 20, 1))];
        }
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row,
            modifiers: KeyModifiers::NONE,
        };

        tap(&mut app, click(1));
        assert!(request_rx.try_recv().is_err(), "single click only selects");
        tap(&mut app, click(1));
        assert!(matches!(
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Request::PreviewFile { path, .. } if path == "/work/README.md"
        ));
        app.handle_worker_event(Event::FilePreviewed {
            target_id: "local".into(),
            path: "/work/README.md".into(),
            result: Ok(FilePreview {
                path: "/work/README.md".into(),
                mime: "text/markdown".into(),
                kind: FilePreviewKind::Markdown,
                size: 300_000,
                content: "# Muxloom".into(),
                truncated: true,
            }),
        });
        tap(&mut app, click(1));
        tap(&mut app, click(1));
        assert!(
            app.file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_none())
        );

        tap(&mut app, click(2));
        tap(&mut app, click(2));
        assert!(matches!(
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Request::ListFiles { path, .. } if path == "/work/src"
        ));
        {
            let form = app.file_manager.as_mut().unwrap();
            form.list_area = Some(Rect::new(0, 0, 20, 4));
        }
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Request::ListFiles { path, .. } if path == "/work"
        ));
    }

    #[test]
    fn changing_selection_while_parent_loads_does_not_reopen_the_previous_child() {
        let config = Config::default();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.open_file_manager();
        let _ = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let form = app.file_manager.as_mut().unwrap();
        form.path = "/work".into();
        form.entries = vec![
            FileEntry {
                name: "old".into(),
                path: "/work/old".into(),
                kind: FileEntryKind::Directory,
                symlink: false,
                size: 0,
                mtime: 0,
            },
            FileEntry {
                name: "new".into(),
                path: "/work/new".into(),
                kind: FileEntryKind::Directory,
                symlink: false,
                size: 0,
                mtime: 0,
            },
        ];
        form.selected = 0;
        form.loading = true;
        form.return_path = Some("/work/old".into());

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work".into(),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![
                    FileEntry {
                        name: "old".into(),
                        path: "/work/old".into(),
                        kind: FileEntryKind::Directory,
                        symlink: false,
                        size: 0,
                        mtime: 0,
                    },
                    FileEntry {
                        name: "new".into(),
                        path: "/work/new".into(),
                        kind: FileEntryKind::Directory,
                        symlink: false,
                        size: 0,
                        mtime: 0,
                    },
                ],
            }),
        });
        assert_eq!(app.file_manager.as_ref().map(|form| form.selected), Some(1));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Request::ListFiles { path, .. } if path == "/work/new"
        ));
    }

    #[test]
    fn transient_scan_failures_keep_last_successful_state_and_sessions() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].state = ConnectionState::Online;
        app.sessions.push(AgentSession {
            id: "ad-codex-stale".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "last success".into(),
            created_at: 1,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        for failure in 1..=2 {
            app.handle_worker_event(Event::Scanned {
                target_id: "local".into(),
                result: Err("temporary connection failure".into()),
            });
            assert_eq!(app.targets[0].state, ConnectionState::Online);
            assert_eq!(app.targets[0].consecutive_failures, failure);
            assert_eq!(app.sessions.len(), 1);
        }
        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Err("still offline".into()),
        });
        assert_eq!(app.targets[0].state, ConnectionState::Offline);
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn machine_settings_save_to_the_selected_host_override() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-host-settings-{nonce}"));
        std::fs::create_dir_all(&root).unwrap();
        let ssh_path = root.join("ssh-config");
        std::fs::write(&ssh_path, "Host gpu\n").unwrap();
        let mut config = Config {
            ssh_config: ssh_path.display().to_string(),
            ..Config::default()
        };
        // Overrides the trimmed-down form no longer shows must survive a
        // save untouched.
        config.hosts.insert(
            "gpu".into(),
            crate::config::HostConfig {
                reverse_tunnel: Some("18118:127.0.0.1:8080".into()),
                companion_binary: Some("~/Downloads/muxloomd-linux".into()),
                attention_patterns: Some(vec!["gpu approval".into()]),
                ..Default::default()
            },
        );
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let config_path = root.join("config.toml");
        let mut app = App::new(
            config,
            config_path.clone(),
            state,
            root.join("state.json"),
            vec![Target::local(), Target::ssh("gpu")],
            worker,
        );
        app.selected_target = 1;
        app.open_machine_settings();
        let Some(Modal::Settings(mut form)) = app.modal.take() else {
            panic!("machine settings modal did not open");
        };
        assert_eq!(form.scope, SettingsScope::Host("gpu".into()));
        form.values[0] = "HTTP_PROXY=http://proxy:8080".into();
        form.values[1] = "/opt/codex".into();
        form.values[2] = "--full-auto".into();
        app.apply_settings(form);

        let reloaded = Config::load(&config_path).unwrap();
        assert_eq!(
            reloaded.command_for("gpu", AgentKind::Codex).command,
            "/opt/codex"
        );
        assert_eq!(
            reloaded.command_for("gpu", AgentKind::Codex).args,
            ["--full-auto"]
        );
        assert_eq!(reloaded.attention_patterns_for("gpu"), ["gpu approval"]);
        assert_eq!(reloaded.reverse_tunnel_for("gpu"), "18118:127.0.0.1:8080");
        assert_eq!(
            reloaded.companion_binary_for("gpu"),
            "~/Downloads/muxloomd-linux"
        );
        assert_eq!(
            reloaded.environment_for("gpu").unwrap(),
            [("HTTP_PROXY".into(), "http://proxy:8080".into())]
        );
        assert_eq!(
            reloaded.command_for("local", AgentKind::Codex).command,
            "codex"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A shell has nothing to resume, so choosing its folder is the last
    /// decision there is to make - an agent still gets asked.
    #[test]
    fn confirming_a_folder_starts_a_terminal_and_asks_an_agent_about_resuming() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let state_path =
            std::env::temp_dir().join(format!("muxloom-launch-test-{}.json", std::process::id()));
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            state,
            state_path.clone(),
            vec![Target::local()],
            worker,
        );

        let picker = |kind| {
            Modal::PathPicker(PathPickerForm {
                launch: LaunchForm {
                    target: Target::local(),
                    kind,
                    path: ".".into(),
                    label: String::new(),
                    temporary: false,
                    field: LaunchField::Path,
                },
                path: "/tmp/project".into(),
                directories: vec!["src".into()],
                query: String::new(),
                selected: 0,
                loading: false,
                error: None,
            })
        };

        app.modal = Some(picker(AgentKind::Terminal));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Request::Launch { request, .. } = receive_request(&request_rx) else {
            panic!("the terminal did not start once its folder was chosen");
        };
        assert_eq!(request.kind, AgentKind::Terminal);
        assert_eq!(request.path, "/tmp/project");
        assert!(request.resume_id.is_none());
        assert!(app.modal.is_none(), "no modal is left over the terminal");

        app.modal = Some(picker(AgentKind::Codex));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Resume(ResumeForm {
                selected: 0,
                ref launch,
                ..
            })) if launch.path == "/tmp/project" && launch.kind == AgentKind::Codex
        ));
        assert!(matches!(
            receive_request(&request_rx),
            Request::ScanResumes { .. }
        ));
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn new_agent_prompts_before_installing_a_missing_runtime() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.modal = Some(Modal::Resume(ResumeForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Codex,
                path: "/tmp/project".into(),
                label: String::new(),
                temporary: false,
                field: LaunchField::Path,
            },
            candidates: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            query: String::new(),
            history_hits: Vec::new(),
            history_selected: 0,
            searched_query: String::new(),
            search_edited_at: None,
        }));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmInstall {
                ref launch,
                resume_id: None,
                ..
            }) if launch.kind == AgentKind::Codex && launch.target.id == "local"
        ));
    }

    #[test]
    fn paste_populates_new_agent_text_fields_without_trailing_newlines() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.modal = Some(Modal::Launch(LaunchForm {
            target: Target::local(),
            kind: AgentKind::Codex,
            path: String::new(),
            label: String::new(),
            temporary: false,
            field: LaunchField::Path,
        }));

        app.handle_paste("/tmp/project with spaces\r\n".into());

        let Some(Modal::Launch(form)) = app.modal else {
            panic!("launch modal was unexpectedly closed");
        };
        assert_eq!(form.path, "/tmp/project with spaces");
        assert_eq!(
            single_line_paste("first\nsecond\tthird"),
            "first second third"
        );
    }

    fn waiting_session(id: &str, reason: &str) -> AgentSession {
        AgentSession {
            id: id.into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: id.into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: true,
            attention_reason: Some(reason.into()),
            recap: None,
            title: None,
            parent: None,
        }
    }

    /// Prompts that were already waiting when muxloom started -- or when a
    /// machine came back -- are shown, not announced. Only prompts that appear
    /// while the user is watching ring the bell.
    #[test]
    fn a_prompt_that_was_already_waiting_does_not_raise_a_notification() {
        let mut app = ux_test_app(vec![Target::local()]);
        let scan = |sessions: Vec<AgentSession>| Event::Scanned {
            target_id: "local".into(),
            result: Ok((crate::model::Probe::default(), sessions)),
        };

        app.handle_worker_event(scan(vec![waiting_session("one", "approve?")]));
        assert!(app.take_notifications().is_empty());

        // A second prompt, found while the app is up, is news.
        app.handle_worker_event(scan(vec![
            waiting_session("one", "approve?"),
            waiting_session("two", "approve?"),
        ]));
        let raised = app.take_notifications();
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert!(raised[0].contains("two"), "{raised:?}");

        // Disabling a machine drops its sessions; getting them back is not news.
        app.sessions.retain(|session| session.target_id != "local");
        app.handle_worker_event(scan(vec![
            waiting_session("one", "approve?"),
            waiting_session("two", "approve?"),
        ]));
        assert!(app.take_notifications().is_empty());

        // A session that stops asking and asks again is news again.
        let mut answered = waiting_session("two", "approve?");
        answered.needs_attention = false;
        answered.attention_reason = None;
        app.handle_worker_event(scan(vec![waiting_session("one", "approve?"), answered]));
        assert!(app.take_notifications().is_empty());
        app.handle_worker_event(scan(vec![
            waiting_session("one", "approve?"),
            waiting_session("two", "approve?"),
        ]));
        assert_eq!(app.take_notifications().len(), 1);
    }

    /// Press and release at one point. A click is two reports, and the pane
    /// acts on the second one, so tests must send both.
    fn tap(app: &mut App, mouse: MouseEvent) {
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..mouse
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..mouse
        });
    }

    fn ux_test_app(targets: Vec<Target>) -> App {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        for target in &targets {
            state.enabled_hosts.insert(target.id.clone());
        }
        App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            std::env::temp_dir().join(format!("muxloom-ux-test-{}.json", std::process::id())),
            targets,
            worker,
        )
    }

    fn blank_file_manager(
        target: Target,
        session_id: Option<String>,
        path: &str,
    ) -> FileManagerForm {
        FileManagerForm {
            origin: FileManagerOrigin::AgentPane,
            target,
            session_id,
            path: path.into(),
            entries: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            directory_cache: HashMap::new(),
            return_path: None,
            preview_path: None,
            preview: None,
            preview_requested_path: None,
            preview_loading: false,
            preview_error: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_page_rows: 1,
            preview_follow_tail: false,
            preview_stamp: None,
            preview_rendered: None,
            query: String::new(),
            search_request_id: None,
            searching: false,
            search_truncated: false,
            search_edited_at: None,
            preview_cache: HashMap::new(),
            preload_pending: HashSet::new(),
            entry_rows: Vec::new(),
            list_area: None,
            preview_area: None,
            preview_text_area: None,
            preview_visible: Vec::new(),
            preview_selection: None,
            media_playback: None,
            media_frame: None,
            media_loading: false,
            media_error: None,
        }
    }

    #[test]
    fn launching_lands_in_the_new_agents_terminal() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            state,
            std::env::temp_dir().join(format!(
                "muxloom-launch-focus-test-{}.json",
                std::process::id()
            )),
            vec![Target::local()],
            worker,
        );
        app.focus = Focus::Machines;
        // Simulate an older discovery request that was already in flight when
        // the launch completed.
        app.pending_scans.insert("local".into());
        app.handle_worker_event(Event::Launched {
            target_id: "local".into(),
            notice: None,
            result: Ok("muxloomd-codex-1-2-0".into()),
            remove_archive_session_id: None,
        });
        assert_eq!(app.focus, Focus::Agents);
        assert_ne!(
            app.selected_session_id.as_deref(),
            Some("muxloomd-codex-1-2-0")
        );

        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Ok((crate::model::Probe::default(), Vec::new())),
        });
        assert!(matches!(receive_request(&request_rx), Request::Scan(_)));

        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Ok((
                crate::model::Probe::default(),
                vec![AgentSession {
                    id: "muxloomd-codex-1-2-0".into(),
                    target_id: "local".into(),
                    kind: AgentKind::Codex,
                    path: "/work/new".into(),
                    label: "new agent".into(),
                    created_at: 1,
                    dead: false,
                    pid: Some(1),
                    working: false,
                    needs_attention: false,
                    attention_reason: None,
                    recap: None,
                    title: None,
                    parent: None,
                }],
            )),
        });
        assert_eq!(
            app.selected_session_id.as_deref(),
            Some("muxloomd-codex-1-2-0")
        );
        assert!(app.pending_launch_selection.is_none());
        assert_eq!(
            app.focus,
            Focus::Recap,
            "a launched agent is opened, not merely highlighted"
        );
        let attach = app
            .pending_attach
            .as_ref()
            .expect("the new agent's terminal must be connecting");
        assert_eq!(attach.session_id, "muxloomd-codex-1-2-0");
        assert!(
            attach.take_input,
            "the first keystroke belongs to the agent that was just launched"
        );
    }

    #[test]
    fn temporary_chat_chooses_runtime_never_inherits_a_project_and_is_destroyed_without_history() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        state
            .last_launch_dirs
            .insert("local".into(), "/work/last".into());
        let state_path =
            std::env::temp_dir().join(format!("muxloom-temporal-test-{}.json", std::process::id()));
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            state,
            state_path.clone(),
            vec![Target::local()],
            worker,
        );
        app.targets[0].probe.set(AgentKind::Codex, true);
        app.targets[0].probe.set(AgentKind::Claude, true);
        app.sessions.push(AgentSession {
            id: "muxloomd-codex-current".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/current".into(),
            label: "current".into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloomd-codex-current".into());
        app.focus = Focus::Agents;

        // Neither the selected project nor the folder this machine launched
        // into last: a temporary chat is a scratch pad, and the daemon gives it
        // a scratch folder of its own. The path in the form is only what a
        // daemon too old to do that falls back to.
        let home = app.user_folder(&Target::local());
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Temporal(TemporalForm {
                kind: AgentKind::Codex,
                ref path,
                ..
            })) if *path == home
        ));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Temporal(TemporalForm {
                kind: AgentKind::Claude,
                ..
            }))
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Request::Launch { request, .. } = receive_request(&request_rx) else {
            panic!("Temporal Chat did not launch after runtime selection");
        };
        assert_eq!(request.kind, AgentKind::Claude);
        assert_eq!(request.path, home);
        assert_eq!(request.label, "Temporal Chat");
        assert!(request.temporary);
        assert!(request.resume_id.is_none() && request.initial_prompt.is_none());
        assert_eq!(
            app.state.last_launch_dirs.get("local").map(String::as_str),
            Some("/work/last"),
            "a scratch chat must not aim the machine's next ordinary launch"
        );

        // Typing names the chat rather than picking a runtime, which stays on
        // the one the machine launched last.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        for character in "clip notes".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Temporal(TemporalForm {
                kind: AgentKind::Claude,
                ref label,
                ..
            })) if label == "clip note"
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Request::Launch { request, .. } = receive_request(&request_rx) else {
            panic!("a named Temporal Chat did not launch");
        };
        assert_eq!(request.label, "clip note");
        assert!(request.temporary);

        app.sessions.push(AgentSession {
            id: "muxloomd-temporal-codex-test".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/current".into(),
            label: "Temporal Chat".into(),
            created_at: 2,
            dead: false,
            pid: Some(2),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloomd-temporal-codex-test".into());
        app.request_history();
        assert_eq!(app.status_message, "Temporal Chat does not retain history");
        assert!(matches!(
            request_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmKill { archive: false, .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(receive_request(&request_rx), Request::Kill { .. }));
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn port_detection_extracts_loopback_urls_and_ignores_privileged_ports() {
        assert_eq!(
            detected_ports_in_text(
                "ready at http://localhost:3000 and HTTPS://127.0.0.1:8443/x; ssh 127.0.0.1:22"
            ),
            [3000, 8443]
        );
        assert_eq!(
            detected_ports_in_text("Vite: http://0.0.0.0:5173 and http://[::1]:9000"),
            [5173, 9000]
        );
    }

    #[test]
    fn agents_p_opens_port_settings_and_merges_daemon_listener_detection() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.sessions.push(AgentSession {
            id: "muxloomd-claude-forward".into(),
            target_id: "local".into(),
            kind: AgentKind::Claude,
            path: "/work/web".into(),
            label: "web".into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloomd-claude-forward".into());
        app.focus = Focus::Agents;

        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(matches!(
            receive_request(&request_rx),
            Request::DetectPorts { target } if target.id == "local"
        ));
        app.handle_worker_event(Event::PortsDetected {
            target_id: "local".into(),
            result: Ok(vec![5173, 3000, 5173]),
        });
        let Some(Modal::PortForward(form)) = app.modal else {
            panic!("port-forward settings did not stay open");
        };
        assert_eq!(form.folder, "/work/web");
        assert_eq!(form.detected_ports, [3000, 5173]);
        assert_eq!(form.remote_port, "3000");
        assert_eq!(form.local_port, "3000");
    }

    #[test]
    fn task_progress_survives_other_targets_and_operations_finishing() {
        let mut app = ux_test_app(vec![Target::ssh("gpu-a"), Target::ssh("gpu-b")]);
        app.handle_worker_event(Event::TaskProgress {
            target_id: "gpu-a".into(),
            operation: TaskKind::Connect,
            progress: TaskProgress::pending("Connecting to gpu-a"),
        });
        app.handle_worker_event(Event::TaskProgress {
            target_id: "gpu-a".into(),
            operation: TaskKind::Install,
            progress: TaskProgress::bytes("Downloading Claude", 40, Some(100)),
        });
        app.handle_worker_event(Event::TaskProgress {
            target_id: "gpu-b".into(),
            operation: TaskKind::Connect,
            progress: TaskProgress::pending("Connecting to gpu-b"),
        });

        app.handle_worker_event(Event::Scanned {
            target_id: "gpu-b".into(),
            result: Err("offline".into()),
        });
        assert_eq!(
            app.visible_task_progress()
                .map(|(target, progress)| (target, progress.label.as_str())),
            Some(("gpu-a", "Downloading Claude"))
        );

        app.handle_worker_event(Event::Installed {
            target_id: "gpu-a".into(),
            kind: AgentKind::Claude,
            result: Err("failed".into()),
        });
        assert_eq!(
            app.visible_task_progress()
                .map(|(target, progress)| (target, progress.label.as_str())),
            Some(("gpu-a", "Connecting to gpu-a"))
        );

        app.handle_worker_event(Event::Scanned {
            target_id: "gpu-a".into(),
            result: Err("offline".into()),
        });
        assert!(app.visible_task_progress().is_none());
    }

    /// A scratch state directory for a test that writes moderator folders, so
    /// two of them never meet in the shared temporary directory.
    fn moderator_scratch(app: &mut App, name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "muxloom-moderator-app-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        app.moderator_state_dir = path.clone();
        path
    }

    /// The moderators row is pinned above the machines and is not one of them:
    /// it has nothing to enable, and selecting it must not move the machine the
    /// rest of the window is pointed at.
    #[test]
    fn the_moderators_row_sits_above_the_machines_without_being_one() {
        let mut app = ux_test_app(vec![Target::local(), Target::ssh("gpu")]);
        app.selected_target = 1;
        assert_eq!(
            app.machine_column(),
            vec![
                MachineRow::Moderators,
                MachineRow::Machine(0),
                MachineRow::Machine(1)
            ]
        );

        app.select_machine_row(MachineRow::Moderators);
        assert!(app.showing_moderators());
        assert_eq!(
            app.selected_target, 0,
            "the moderators row runs on this machine, and the panes that need a machine keep working"
        );

        // Space enables a machine, and there is no machine here to enable.
        app.focus = Focus::Machines;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.targets[0].enabled, "the local machine was not toggled");
        assert!(app.status_message.contains("not a machine"));
    }

    /// A moderator lives in a folder muxloom owns on this machine, which is the
    /// only thing that marks it. It must not also show up as one of the local
    /// machine's own agents.
    #[test]
    fn a_moderator_shows_under_its_own_row_and_not_under_this_machine() {
        let mut app = ux_test_app(vec![Target::local()]);
        let state = moderator_scratch(&mut app, "listing");
        let session = |id: &str, path: &str| AgentSession {
            id: id.into(),
            target_id: "local".into(),
            kind: AgentKind::Claude,
            path: path.into(),
            label: id.into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        };
        app.sessions = vec![
            session("worker", "/work/Terminal"),
            session(
                "lead",
                &state.join("projects/fleet-lead").display().to_string(),
            ),
        ];

        app.select_machine_row(MachineRow::Machine(0));
        let ids: Vec<_> = app
            .visible_sessions()
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(ids, vec!["worker"]);

        app.select_machine_row(MachineRow::Moderators);
        let ids: Vec<_> = app
            .visible_sessions()
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(ids, vec!["lead"]);
    }

    /// Nothing enforces a moderator's scope, so the one thing the form must get
    /// right is what it hands the briefing: everything checked means "the whole
    /// fleet, including what appears later", not a snapshot of today's list.
    #[test]
    fn an_untouched_scope_means_everything_rather_than_todays_list() {
        let mut app = ux_test_app(vec![Target::local(), Target::ssh("gpu")]);
        app.select_machine_row(MachineRow::Moderators);
        app.open_launch();
        let Some(Modal::Moderator(mut form)) = app.modal.clone() else {
            panic!("the moderators row starts a moderator, not an agent");
        };
        assert_eq!(form.machines.len(), 2);
        assert!(form.chosen_machines().is_empty(), "all checked is 'every'");

        // What the briefing carries is what the dashboard calls a machine, not
        // its internal id: the moderator has to recognise it, and so does the
        // person who reads the brief back.
        form.machines[1].selected = false;
        assert_eq!(form.chosen_machines(), vec!["This machine".to_string()]);
    }

    /// A moderator hands work to the fleet, so the agents it can be pointed at
    /// are the fleet's — and unchecking a machine takes that machine's agents
    /// out of the question rather than leaving them on the list.
    #[test]
    fn the_agents_on_offer_are_every_machines_and_follow_the_machines_chosen() {
        let mut app = ux_test_app(vec![Target::local(), Target::ssh("gpu")]);
        let session = |id: &str, machine: &str| AgentSession {
            id: id.into(),
            target_id: machine.into(),
            kind: AgentKind::Claude,
            path: "/work".into(),
            label: id.into(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        };
        app.sessions = vec![session("far", "gpu"), session("near", "local")];
        app.select_machine_row(MachineRow::Moderators);
        app.open_launch();
        let Some(Modal::Moderator(mut form)) = app.modal.clone() else {
            panic!("the moderators row starts a moderator");
        };

        // Both machines' agents, checked, and grouped the way the machines are
        // listed rather than in whatever order the scans came back.
        assert_eq!(form.agents.len(), 2);
        assert!(form.agents[0].label.contains("near"), "{:?}", form.agents);
        assert!(form.agents[1].label.contains("far"), "{:?}", form.agents);
        assert!(form.chosen_agents().is_empty(), "all checked is 'every'");

        // Dropping the remote drops its agent from the column and from the
        // brief, and what is left still reads as "all of them".
        form.machines[1].selected = false;
        assert_eq!(form.visible_agents(), vec![0]);
        assert!(
            !form
                .rows()
                .contains(&ModeratorRow::Agent(form.agents.len() - 1))
        );
        assert!(form.chosen_agents().is_empty(), "every agent still there");

        // And with the remote back, clearing one box names the rest.
        form.machines[1].selected = true;
        form.agents[0].selected = false;
        assert_eq!(form.chosen_agents().len(), 1);
        assert!(form.chosen_agents()[0].contains("far"));
    }

    #[test]
    fn a_moderator_needs_a_name_and_starts_with_its_briefing_written() {
        let mut app = ux_test_app(vec![Target::local()]);
        let state = moderator_scratch(&mut app, "launch");
        app.targets[0].state = ConnectionState::Online;
        app.targets[0].probe.set(AgentKind::Claude, true);
        app.select_machine_row(MachineRow::Moderators);
        app.open_launch();

        // The form opens on the name, and Enter on an unnamed one says why
        // rather than starting something nobody can address.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Modal::Moderator(form)) = &app.modal else {
            panic!("the form stays up until it can be submitted");
        };
        assert!(form.error.as_deref().is_some_and(|e| e.contains("name")));

        for character in "Fleet Lead".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal.is_none(), "a named moderator starts");

        let folder = state.join("projects/fleet-lead");
        let brief = std::fs::read_to_string(folder.join("CLAUDE.md")).expect("briefing written");
        assert!(brief.contains("You are **Fleet Lead**"), "{brief}");
        assert!(brief.contains("This is a brief, not a fence."), "{brief}");

        // The folder is muxloom's, so it must not become where the next
        // ordinary agent on this machine is launched from.
        assert!(
            !app.state.last_launch_dirs.contains_key("local"),
            "a moderator's folder is not the machine's default launch directory"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn new_agent_defaults_to_the_machines_last_launch_dir() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.state
            .last_launch_dirs
            .insert("local".into(), "/work/remembered".into());
        app.open_launch();
        let Some(Modal::Launch(form)) = &app.modal else {
            panic!("expected launch modal");
        };
        assert_eq!(form.path, "/work/remembered");
    }

    /// The runtime you launched last on a machine is the one you usually want
    /// next, so the form opens on it rather than always on the first one.
    #[test]
    fn a_launch_form_opens_on_the_runtime_that_machine_launched_last() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.targets[0].state = ConnectionState::Online;
        for kind in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
            app.targets[0].probe.set(kind, true);
        }
        app.open_launch();
        let Some(Modal::Launch(form)) = &app.modal else {
            panic!("expected launch modal");
        };
        assert_eq!(form.kind, AgentKind::Codex, "nothing is remembered yet");

        let form = LaunchForm {
            kind: AgentKind::Pi,
            path: "/work".into(),
            ..form.clone()
        };
        app.submit_launch(form, None, None, None);
        assert_eq!(
            app.state.last_launch_kinds.get("local"),
            Some(&AgentKind::Pi)
        );

        app.open_launch();
        let Some(Modal::Launch(form)) = &app.modal else {
            panic!("expected launch modal");
        };
        assert_eq!(form.kind, AgentKind::Pi);
        // A Temporal Chat picks from the same memory.
        app.open_temporary_agent();
        let Some(Modal::Temporal(form)) = &app.modal else {
            panic!("expected temporal modal");
        };
        assert_eq!(form.kind, AgentKind::Pi);

        // A remembered runtime the machine no longer has cannot be offered, so
        // the form falls back to the first one that is.
        app.targets[0].probe.set(AgentKind::Pi, false);
        app.open_launch();
        let Some(Modal::Launch(form)) = &app.modal else {
            panic!("expected launch modal");
        };
        assert_eq!(form.kind, AgentKind::Codex);

        // A terminal is a fine thing to remember, but never what a Temporal
        // Chat starts on.
        app.state
            .last_launch_kinds
            .insert("local".into(), AgentKind::Terminal);
        app.open_launch();
        let Some(Modal::Launch(form)) = &app.modal else {
            panic!("expected launch modal");
        };
        assert_eq!(form.kind, AgentKind::Terminal);
        app.open_temporary_agent();
        let Some(Modal::Temporal(form)) = &app.modal else {
            panic!("expected temporal modal");
        };
        assert_eq!(form.kind, AgentKind::Codex);
    }

    /// A runtime a machine does not have is not a choice worth showing, so the
    /// picker lists what the probe found and nothing else.
    #[test]
    fn the_launch_picker_offers_only_the_runtimes_a_machine_has() {
        let mut app = ux_test_app(vec![Target::local()]);
        // A machine muxloom has not reached yet says nothing about what it
        // holds; everything stays on offer so the install prompt is reachable.
        assert_eq!(app.offered_kinds("local"), AgentKind::ALL);

        app.targets[0].state = ConnectionState::Online;
        app.targets[0].probe.set(AgentKind::Claude, true);
        app.targets[0].probe.set(AgentKind::Pi, true);
        assert_eq!(
            app.offered_kinds("local"),
            [AgentKind::Claude, AgentKind::Pi, AgentKind::Terminal]
        );
        assert_eq!(
            app.offered_agent_kinds("local"),
            [AgentKind::Claude, AgentKind::Pi]
        );

        app.open_launch();
        let Some(Modal::Launch(form)) = &app.modal else {
            panic!("expected launch modal");
        };
        assert_eq!(
            form.kind,
            AgentKind::Claude,
            "the form must not start on a runtime the machine lacks"
        );
        // Stepping walks the offered runtimes only, and wraps within them.
        for expected in [AgentKind::Pi, AgentKind::Terminal, AgentKind::Claude] {
            app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
            let Some(Modal::Launch(form)) = &app.modal else {
                panic!("expected launch modal");
            };
            assert_eq!(form.kind, expected);
        }

        // A machine with no agent at all would otherwise offer a bare terminal
        // and no way to install anything.
        app.targets[0].probe = crate::model::Probe {
            tmux: true,
            ..Default::default()
        };
        assert_eq!(app.offered_kinds("local"), AgentKind::ALL);
    }

    /// The machine panel carries a one-click install for exactly the runtimes
    /// that machine is missing, and Enter on one sends the install off.
    #[test]
    fn machine_settings_offer_an_install_for_a_missing_runtime() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].state = ConnectionState::Online;
        for kind in [AgentKind::Codex, AgentKind::Claude, AgentKind::Pi] {
            app.targets[0].probe.set(kind, true);
        }

        app.open_machine_settings();
        let Some(Modal::Settings(form)) = &app.modal else {
            panic!("machine settings modal did not open");
        };
        assert_eq!(form.missing, [AgentKind::OpenCode]);
        let rows = form.rows();
        assert!(
            rows.contains(&SettingsRow::Action(
                install_action(AgentKind::OpenCode),
                "Enter: install it on this machine"
            )),
            "the missing runtime has no install action: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| matches!(
                row,
                SettingsRow::Action(label, _) if install_action_kind(label) == Some(AgentKind::Pi)
            )),
            "an installed runtime must not offer to install itself: {rows:?}"
        );
        let selected = form
            .focusable()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    SettingsRow::Action(label, _)
                        if install_action_kind(label) == Some(AgentKind::OpenCode)
                )
            })
            .expect("the install action must be selectable");

        let Some(Modal::Settings(form)) = app.modal.as_mut() else {
            unreachable!("the panel is open");
        };
        form.selected = selected;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Request::Install { target, kind, .. } = receive_request(&request_rx) else {
            panic!("Enter on the install action did not request an install");
        };
        assert_eq!(kind, AgentKind::OpenCode);
        assert_eq!(target.id, "local");
        assert!(
            app.modal.is_none(),
            "the panel must close so the install gauge is visible"
        );
    }

    /// The global panel is not about one machine, so it never offers installs
    /// however the form was built.
    #[test]
    fn global_settings_never_offer_an_install() {
        let form = SettingsForm {
            scope: SettingsScope::Global,
            values: Vec::new(),
            notes: Vec::new(),
            missing: AgentKind::agents().collect(),
            selected: 0,
            error: None,
        };
        assert!(
            !form
                .rows()
                .iter()
                .any(|row| matches!(row, SettingsRow::Action(label, _)
                    if install_action_kind(label).is_some())),
            "the global panel must not install onto a machine it does not name"
        );
    }

    #[test]
    fn renaming_an_agent_sets_applies_and_clears_its_display_name() {
        let mut app = ux_test_app(vec![Target::local()]);
        app.sessions.push(AgentSession {
            id: "s1".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: String::new(),
            created_at: 1,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        });
        app.submit_rename_agent("s1".into(), "  My Bot  ".into());
        assert_eq!(
            app.state.session_labels.get("s1").map(String::as_str),
            Some("My Bot")
        );
        assert_eq!(app.sessions[0].label, "My Bot");

        // A blank name removes the override and reverts to the folder name.
        app.submit_rename_agent("s1".into(), "   ".into());
        assert!(!app.state.session_labels.contains_key("s1"));
        assert_eq!(app.sessions[0].display_label(), "project");

        // Overrides re-apply when a refresh rebuilds the session list.
        app.state
            .session_labels
            .insert("s1".into(), "Persisted".into());
        app.sessions[0].label = String::new();
        app.apply_session_labels();
        assert_eq!(app.sessions[0].label, "Persisted");
    }

    #[test]
    fn file_browser_parks_and_restores_per_machine() {
        let mut app = ux_test_app(vec![Target::local(), Target::ssh("remote")]);
        app.file_manager = Some(blank_file_manager(
            Target::local(),
            Some("s1".into()),
            "/work/local",
        ));
        // Switching to another machine parks the local browser.
        app.set_selected_target(1);
        assert!(app.file_manager.is_none());
        // The agent's last browsed directory was remembered while parked.
        assert_eq!(
            app.file_dirs.get("s1").map(String::as_str),
            Some("/work/local")
        );
        // Switching back restores the browser where it was.
        app.set_selected_target(0);
        let restored = app.file_manager.as_ref().expect("browser restored");
        assert_eq!(restored.path, "/work/local");
        assert_eq!(restored.target.id, "local");
    }

    #[test]
    fn machine_click_selects_and_double_click_toggles() {
        let mut app = ux_test_app(vec![Target::local(), Target::ssh("remote")]);
        app.pane_layout.machines = Some(Rect::new(0, 0, 30, 8));
        // As the pane renders it: the moderators row first, then the machines.
        app.machine_rows = vec![
            (MachineRow::Moderators, 2),
            (MachineRow::Machine(0), 2),
            (MachineRow::Machine(1), 2),
        ];
        let click_remote = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        tap(&mut app, click_remote);
        assert_eq!(app.selected_target, 1);
        assert!(app.targets[1].enabled, "single click only selects");

        tap(&mut app, click_remote);
        assert!(
            app.targets[1].enabled,
            "double-clicking outside [x] must only select"
        );
        tap(&mut app, click_remote);
        assert!(app.targets[1].enabled);

        let click_checkbox = MouseEvent {
            column: 6,
            ..click_remote
        };
        tap(&mut app, click_checkbox);
        assert!(app.targets[1].enabled, "first checkbox click only selects");
        tap(&mut app, click_checkbox);
        assert!(
            !app.targets[1].enabled,
            "double-click inside [x] toggles the machine"
        );
    }

    #[test]
    fn switching_machines_restores_each_last_selected_session() {
        let mut app = ux_test_app(vec![Target::local(), Target::ssh("remote")]);
        app.state.show_archived = true;
        let session = |id: &str, target_id: &str, created_at| AgentSession {
            id: id.into(),
            target_id: target_id.into(),
            kind: AgentKind::Codex,
            path: format!("/work/{id}"),
            label: id.into(),
            created_at,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            parent: None,
        };
        app.sessions = vec![
            session("local-a", "local", 1),
            session("local-b", "local", 2),
            session("remote-a", "remote", 1),
        ];
        app.selected_session_id = Some("local-b".into());

        app.set_selected_target(1);
        app.ensure_session_selection();
        assert_eq!(app.selected_session_id.as_deref(), Some("remote-a"));

        app.set_selected_target(0);
        app.ensure_session_selection();
        assert_eq!(app.selected_session_id.as_deref(), Some("local-b"));
    }

    #[test]
    fn cross_agent_history_requires_confirmation_then_launches_as_reference() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].probe.set(AgentKind::Codex, true);
        let launch = LaunchForm {
            target: Target::local(),
            kind: AgentKind::Codex,
            path: "/work/project".into(),
            label: "handoff".into(),
            temporary: false,
            field: LaunchField::Path,
        };
        let claude = ResumeCandidate {
            id: "claude-thread".into(),
            kind: AgentKind::Claude,
            source_path: "/home/test/.claude/projects/claude-thread.jsonl".into(),
            recap: Some("Finish the renderer".into()),
            first_message: Some("Start the renderer".into()),
            last_message: Some("Wire the final state".into()),
            updated_at: "2026-07-24T08:00:00Z".into(),
        };
        app.modal = Some(Modal::Resume(ResumeForm {
            launch,
            candidates: vec![claude.clone()],
            selected: 1,
            loading: false,
            error: None,
            query: String::new(),
            history_hits: Vec::new(),
            history_selected: 0,
            searched_query: String::new(),
            search_edited_at: None,
        }));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmHistoryReference { ref candidate, .. })
                if candidate.id == "claude-thread"
        ));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Resume(ResumeForm { ref candidates, selected: 1, .. }))
                if candidates == std::slice::from_ref(&claude)
        ));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match receive_request(&request_rx) {
            Request::Launch { request, .. } => {
                assert_eq!(request.kind, AgentKind::Codex);
                assert!(request.resume_id.is_none());
                let prompt = request.initial_prompt.expect("reference prompt");
                assert!(prompt.contains("claude"));
                assert!(prompt.contains("claude-thread.jsonl"));
                assert!(prompt.contains("Wire the final state"));
            }
            request => panic!("expected reference launch, got {request:?}"),
        }
    }

    #[test]
    fn recursive_file_search_accepts_wildcards_and_ignores_stale_results() {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        let original = FileEntry {
            name: "README.md".into(),
            path: "/work/README.md".into(),
            kind: FileEntryKind::File,
            symlink: false,
            size: 10,
            mtime: 0,
        };
        let mut form = blank_file_manager(Target::local(), None, "/work");
        form.entries = vec![original.clone()];
        form.directory_cache
            .insert("/work".into(), vec![original.clone()]);
        app.file_manager = Some(form);
        app.focus = Focus::Agents;

        for character in "/j**.rs".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.file_manager.as_mut().unwrap().search_edited_at =
            Some(Instant::now() - FILE_SEARCH_DEBOUNCE);
        app.maybe_submit_file_search();
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.query.as_str()),
            Some("/j**.rs")
        );

        let mut latest = None;
        while let Ok(request) = request_rx.try_recv() {
            if let Request::SearchFiles {
                pattern,
                request_id,
                ..
            } = request
            {
                latest = Some((pattern, request_id));
            }
        }
        let (pattern, request_id) = latest.expect("recursive search request");
        assert_eq!(pattern, "j**.rs");
        app.handle_worker_event(Event::FilesSearched {
            target_id: "local".into(),
            root: "/work".into(),
            pattern: pattern.clone(),
            request_id: request_id.saturating_sub(1),
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![FileEntry {
                    name: "stale.rs".into(),
                    path: "/work/stale.rs".into(),
                    kind: FileEntryKind::File,
                    symlink: false,
                    size: 1,
                    mtime: 0,
                }],
            }),
        });
        assert_eq!(
            app.file_manager.as_ref().unwrap().entries,
            std::slice::from_ref(&original)
        );

        let found = FileEntry {
            name: "src/job.rs".into(),
            path: "/work/src/job.rs".into(),
            kind: FileEntryKind::File,
            symlink: false,
            size: 20,
            mtime: 0,
        };
        let second = FileEntry {
            name: "tests/job.rs".into(),
            path: "/work/tests/job.rs".into(),
            kind: FileEntryKind::File,
            symlink: false,
            size: 30,
            mtime: 0,
        };
        app.handle_worker_event(Event::FilesSearched {
            target_id: "local".into(),
            root: "/work".into(),
            pattern,
            request_id,
            result: Ok(FileListing {
                truncated: false,
                path: "/work".into(),
                entries: vec![found.clone(), second.clone()],
            }),
        });
        assert_eq!(app.file_manager.as_ref().unwrap().entries, [found, second]);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.file_manager.as_ref().unwrap().selected, 1);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.file_manager.as_ref().unwrap().entries, [original]);
    }

    /// A machine that came back empty still shows what the local backup holds
    /// for it, reads that history out of the local store, and pushes it back on
    /// demand without giving up the local copy.
    #[test]
    #[cfg(feature = "controller")]
    fn a_machine_that_lost_its_history_lists_it_from_the_backup_and_can_take_it_back() {
        use crate::{
            backup::{BackupIndex, BackupRecord, BackupStore, CAPTURE_BLOB},
            model::{Probe, RestoredTranscript},
        };

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-app-restore-{nonce}"));
        let store = BackupStore::new(root.clone());
        store
            .append_frame(
                "local",
                "muxloomd-codex-lost",
                CAPTURE_BLOB,
                b"$ cargo test\nall green\n",
            )
            .unwrap();
        let mut index = BackupIndex::default();
        index.upsert(BackupRecord {
            target_id: "local".into(),
            session_id: "muxloomd-codex-lost".into(),
            kind: "codex".into(),
            cwd: "/work/project".into(),
            created_at: 42,
            label: "lost work".into(),
            recap: "was fixing the pager".into(),
            dead: true,
            native_id: "native-lost".into(),
            native_path: "/home/me/.codex/sessions/2026/08/09/rollout-native-lost.jsonl".into(),
            jsonl_bytes_synced: 128,
            message_count: 4,
            ..Default::default()
        });
        store.save_index(&index).unwrap();

        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut app = App::new(
            Config::default(),
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.backup_root = root.clone();
        app.targets[0].probe.set(AgentKind::Codex, true);

        // The machine answers the scan with nothing at all.
        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Ok((Probe::default(), Vec::new())),
        });
        assert_eq!(app.sessions.len(), 1, "backed-up history must be listed");
        let listed = app.sessions[0].clone();
        assert_eq!(listed.id, "muxloomd-codex-lost");
        assert_eq!(listed.path, "/work/project");
        assert!(listed.dead, "nothing is running, so it reads as archived");
        assert_eq!(listed.recap.as_deref(), Some("was fixing the pager"));
        assert!(app.is_recoverable("local", "muxloomd-codex-lost"));
        assert!(app.is_restorable("local", "muxloomd-codex-lost"));

        // Selecting it renders the capture from the local store instead of
        // asking the machine, which has nothing to give.
        app.select_session("muxloomd-codex-lost".into());
        assert!(app.history.text.contains("all green"));
        assert!(
            app.pending_capture.is_none(),
            "no capture should be requested from a machine that lost the session"
        );

        // Deleting must not be how history leaves the local store.
        app.delete_session("muxloomd-codex-lost");
        assert!(app.status_message.contains("Kept"));
        assert!(app.sessions.iter().any(|s| s.id == "muxloomd-codex-lost"));

        // Enter transfers it back in the background.
        app.activate_terminal();
        assert!(app.is_restoring("local", "muxloomd-codex-lost"));
        match receive_request(&request_rx) {
            Request::BackupRestore {
                target,
                machine_key,
                session_id,
            } => {
                assert_eq!(target.id, "local");
                assert_eq!(machine_key, "local");
                assert_eq!(session_id, "muxloomd-codex-lost");
            }
            request => panic!("expected a restore, got {request:?}"),
        }

        // Once it lands, the agent resumes from the id that came back.
        app.handle_worker_event(Event::BackupRestored {
            target_id: "local".into(),
            session_id: "muxloomd-codex-lost".into(),
            result: Ok(RestoredTranscript {
                resume_id: "native-lost".into(),
                path: "/home/me/.codex/sessions/2026/08/09/rollout-native-lost.jsonl".into(),
                bytes: 128,
            }),
        });
        assert!(!app.is_restoring("local", "muxloomd-codex-lost"));
        assert!(
            !app.is_recoverable("local", "muxloomd-codex-lost"),
            "the machine has the transcript now"
        );
        match receive_request(&request_rx) {
            Request::Launch { request, .. } => {
                assert_eq!(request.kind, AgentKind::Codex);
                assert_eq!(request.path, "/work/project");
                assert_eq!(request.resume_id.as_deref(), Some("native-lost"));
            }
            request => panic!("expected a resume launch, got {request:?}"),
        }
        // A later scan does not resurrect the entry now that it is on the box.
        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Ok((Probe::default(), Vec::new())),
        });
        assert!(app.sessions.is_empty());
        // And the local copy is still there: this copies out, it never moves.
        assert!(store.blob_len("local", "muxloomd-codex-lost", CAPTURE_BLOB) > 0);
        assert!(
            store
                .load_index()
                .unwrap()
                .position("local", "muxloomd-codex-lost")
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A capture blob holds every row a session ever printed, and the terminal
    /// panel re-parses whatever is on the page on every redraw, so a page built
    /// from the backup has to be bounded exactly like a live one.
    #[test]
    #[cfg(feature = "controller")]
    fn a_page_built_from_the_backup_is_no_bigger_than_a_live_one() {
        use crate::{
            backup::{BackupIndex, BackupRecord, BackupStore, CAPTURE_BLOB},
            model::Probe,
        };

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-app-bigpage-{nonce}"));
        let store = BackupStore::new(root.clone());
        // Far more output than any pane shows, in the frames a real sync leaves.
        let mut written = 0usize;
        for frame in 0..40u32 {
            let chunk: String = (0..2_000)
                .map(|line| format!("frame {frame} line {line} of terminal output\n"))
                .collect();
            written += chunk.len();
            store
                .append_frame(
                    "local",
                    "muxloomd-claude-huge",
                    CAPTURE_BLOB,
                    chunk.as_bytes(),
                )
                .unwrap();
        }
        assert!(written > 2_000_000, "wrote only {written} bytes");
        let mut index = BackupIndex::default();
        index.upsert(BackupRecord {
            target_id: "local".into(),
            session_id: "muxloomd-claude-huge".into(),
            kind: "claude".into(),
            cwd: "/work/project".into(),
            created_at: 7,
            dead: true,
            message_count: 1,
            ..Default::default()
        });
        store.save_index(&index).unwrap();

        let (request_tx, _request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let worker = Worker {
            requests: request_tx,
            events: event_rx,
            bridges: crate::bridge::BridgePool::default(),
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let config = Config::default();
        let chunk_lines = config.history_chunk_lines;
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.backup_root = root.clone();
        app.targets[0].probe.set(AgentKind::Claude, true);
        app.handle_worker_event(Event::Scanned {
            target_id: "local".into(),
            result: Ok((Probe::default(), Vec::new())),
        });

        let start = std::time::Instant::now();
        app.select_session("muxloomd-claude-huge".into());
        let elapsed = start.elapsed();
        let lines = app.history.text.lines().count();
        assert!(
            lines <= chunk_lines + 2,
            "page carries {lines} lines for a {chunk_lines}-line chunk"
        );
        assert!(
            app.history.text.len() < 1_000_000,
            "page carries {} bytes",
            app.history.text.len()
        );
        assert!(
            app.history
                .text
                .contains("older output stays in the local backup"),
            "a clipped page has to say so"
        );
        assert!(
            app.history.text.contains("frame 39 line 1999"),
            "the newest output has to be the output shown"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "building the page took {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One message on the board, as it arrives from the local daemon.
    fn said(seq: u64, ts: u64, scope: TalkScope, kind: TalkKind, text: &str) -> TalkMessage {
        TalkMessage {
            id: format!("mars:{seq}"),
            origin: "mars".into(),
            seq,
            ts,
            scope,
            author: TalkAuthor {
                machine: "mars".into(),
                machine_label: "mars".into(),
                voice: TalkVoice {
                    session_id: Some("ad-claude-1".into()),
                    label: Some("review-bot".into()),
                    kind: Some("claude".into()),
                    human: false,
                },
            },
            kind,
            to: None,
            reply_to: None,
            text: text.into(),
        }
    }

    /// A dashboard whose worker is a plain channel, so a test can read what the
    /// board asked for instead of watching it happen.
    fn board_app() -> (App, std::sync::mpsc::Receiver<Request>) {
        let config = Config::default();
        let bridges = Runtime::new(&config).bridge_pool();
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel::<Event>();
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            Worker {
                requests: request_tx,
                events: event_rx,
                bridges,
            },
        );
        (app, request_rx)
    }

    /// The Task tab is the one view keyed to the agent list rather than to the
    /// board: whichever session is selected, it gathers what that whole piece
    /// of work has said, across every scope its members used.
    #[test]
    fn the_task_tab_gathers_what_one_piece_of_work_said_wherever_it_said_it() {
        let (mut app, _requests) = board_app();
        let under = |id: &str, parent: Option<&str>| AgentSession {
            parent: parent.map(Into::into),
            ..waiting_session(id, "waiting")
        };
        app.sessions = vec![
            under("lead", None),
            under("scout", Some("lead")),
            under("digger", Some("scout")),
            // Somebody else's work, on the same machine and in the same
            // directory, which is exactly what the other tabs cannot tell
            // apart from this one.
            under("stranger", None),
        ];
        let from = |seq: u64, session: &str, scope: TalkScope, text: &str| TalkMessage {
            author: TalkAuthor {
                voice: TalkVoice {
                    session_id: Some(session.into()),
                    ..TalkVoice::default()
                },
                ..said(seq, seq * 10, TalkScope::Global, TalkKind::Message, text).author
            },
            scope,
            ..said(seq, seq * 10, TalkScope::Global, TalkKind::Message, text)
        };
        let here = || TalkScope::Path {
            machine: "mars".into(),
            path: "/work".into(),
        };
        let task = |root: &str| TalkScope::Task {
            machine: "mars".into(),
            root_session: root.into(),
        };
        app.board.merge(vec![
            from(1, "lead", here(), "starting on the parser"),
            from(2, "scout", task("lead"), "the retry lives in client.rs"),
            from(3, "digger", TalkScope::Global, "and the pool is fine"),
            from(4, "stranger", here(), "unrelated work in the same folder"),
            // A member that has since gone still answers for its task,
            // because the scope says which one it was.
            from(5, "forgotten", task("lead"), "left before anyone looked"),
            // And so does a message aimed at a member from outside.
            TalkMessage {
                to: Some(crate::talk::TalkAddress {
                    machine: "mars".into(),
                    session_id: "scout".into(),
                }),
                ..from(6, "stranger", here(), "asking the scout something")
            },
        ]);
        let texts = |app: &App, tab| {
            app.board_view(tab, "")
                .into_iter()
                .map(|message| message.text.clone())
                .collect::<Vec<_>>()
        };
        // Nothing selected is no task, and an empty tab says so rather than
        // guessing at one.
        assert!(texts(&app, BoardTab::Task).is_empty());

        // Selecting anywhere in the task shows the whole of it: the tab is
        // keyed to the work, not to the row the cursor happens to be on.
        for standing in ["lead", "scout", "digger"] {
            app.selected_session_id = Some(standing.into());
            assert_eq!(
                texts(&app, BoardTab::Task),
                [
                    "starting on the parser",
                    "the retry lives in client.rs",
                    "and the pool is fine",
                    "left before anyone looked",
                    "asking the scout something",
                ],
                "standing on {standing}"
            );
        }
        // Somebody else's work is somebody else's.
        app.selected_session_id = Some("stranger".into());
        assert_eq!(
            texts(&app, BoardTab::Task),
            [
                "unrelated work in the same folder",
                "asking the scout something",
            ]
        );

        // How deep each member sits, which is what the tab indents by.
        app.selected_session_id = Some("digger".into());
        let shape = app.selected_task();
        assert_eq!(shape.get("lead"), Some(&0));
        assert_eq!(shape.get("scout"), Some(&1));
        assert_eq!(shape.get("digger"), Some(&2));
        assert_eq!(shape.get("stranger"), None);

        // A task is a view of what was said, not a channel a person can speak
        // into, and saying so beats a message that lands nowhere.
        assert!(app.board_scope(BoardTab::Task).is_err());
    }

    #[test]
    fn each_board_tab_holds_the_scope_it_names() {
        let (mut app, _requests) = board_app();
        app.board.merge(vec![
            said(1, 10, TalkScope::Global, TalkKind::Message, "fleet-wide"),
            said(
                2,
                20,
                TalkScope::Machine {
                    machine: "mars".into(),
                },
                TalkKind::Message,
                "this machine",
            ),
            said(
                3,
                30,
                TalkScope::Path {
                    machine: "mars".into(),
                    path: "/work/terminal".into(),
                },
                TalkKind::Note,
                "left for later",
            ),
            // A direct message is filed under a scope like anything else, but
            // it was said to one session, so it answers on its own tab.
            said(
                4,
                40,
                TalkScope::Path {
                    machine: "mars".into(),
                    path: "/work/terminal".into(),
                },
                TalkKind::Direct,
                "just for you",
            ),
        ]);
        let texts = |app: &App, tab| {
            app.board_view(tab, "")
                .into_iter()
                .map(|message| message.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&app, BoardTab::Global), ["fleet-wide"]);
        assert_eq!(texts(&app, BoardTab::Machine), ["this machine"]);
        assert_eq!(texts(&app, BoardTab::Path), ["left for later"]);
        assert_eq!(texts(&app, BoardTab::Direct), ["just for you"]);
        assert_eq!(texts(&app, BoardTab::All).len(), 4);
        // The filter looks at what was said and who said it, not at the scope.
        assert_eq!(
            app.board_view(BoardTab::All, "review-bot").len(),
            4,
            "every message here has the same author"
        );
        assert_eq!(
            app.board_view(BoardTab::All, "LATER")
                .into_iter()
                .map(|message| message.text.clone())
                .collect::<Vec<_>>(),
            ["left for later"]
        );
    }

    #[test]
    fn a_board_round_asks_only_for_what_it_has_not_seen() {
        let (mut app, requests) = board_app();
        app.maybe_talk_sync();
        assert!(
            matches!(
                receive_request(&requests),
                Request::TalkSync { board_since, .. } if board_since.is_empty()
            ),
            "the first round has no board to catch up from"
        );
        app.talk_in_flight = false;
        app.last_talk_sync = None;
        app.absorb_board(TalkPage {
            messages: vec![said(1, 10, TalkScope::Global, TalkKind::Message, "hello")],
            cursor: "mars:1".into(),
            truncated: false,
        });
        assert_eq!(app.board.unread, 1);
        // The same page again is the same message: replication replays, and a
        // board that counted it twice would be lying about what is new.
        app.absorb_board(TalkPage {
            messages: vec![said(1, 10, TalkScope::Global, TalkKind::Message, "hello")],
            cursor: "mars:1".into(),
            truncated: false,
        });
        assert_eq!(app.board.messages.len(), 1);
        assert_eq!(app.board.unread, 1);
        app.maybe_talk_sync();
        assert!(matches!(
            receive_request(&requests),
            Request::TalkSync { board_since, .. } if board_since == "mars:1"
        ));
        app.open_board();
        assert_eq!(app.board.unread, 0, "reading the board clears the mark");
    }

    #[test]
    fn a_message_posted_here_is_the_same_thing_an_agent_would_post() {
        let (mut app, requests) = board_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::Board(_))));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        for character in "ship it".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Request::TalkPost { draft } = receive_request(&requests) else {
            panic!("the post never reached the worker");
        };
        assert_eq!(draft.text, "ship it");
        // Same store, same replication, same shape — the only difference is
        // that a person has no session to speak from.
        assert_eq!(draft.kind, TalkKind::Message);
        assert_eq!(draft.scope, TalkScope::Global);
        assert!(draft.author.voice.human);
        assert_eq!(draft.author.voice.session_id, None);
        assert!(matches!(app.modal, Some(Modal::Board(_))));
    }

    #[test]
    fn the_board_refuses_to_answer_a_direct_message_in_public() {
        let (mut app, _requests) = board_app();
        app.board.merge(vec![said(
            1,
            10,
            TalkScope::Global,
            TalkKind::Direct,
            "just for you",
        )]);
        app.open_board();
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let Some(Modal::Board(form)) = app.modal.as_ref() else {
            panic!("the board closed");
        };
        assert!(form.compose.is_none(), "nothing should be being written");
        assert!(
            form.error
                .as_deref()
                .is_some_and(|error| error.contains("open it and answer there")),
            "the board has to say why: {:?}",
            form.error
        );
    }
}
