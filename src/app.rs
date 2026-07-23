use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, text::Text, widgets::ListState};
use unicode_width::UnicodeWidthChar;

use crate::{
    config::{CommandConfig, Config, HostConfig, State},
    debug,
    media::{MediaFrame, MediaPlayback, MediaUpdate},
    model::{
        AgentKind, AgentSession, ConnectionState, DirectoryListing, FileEntry, FileEntryKind,
        FileListing, FilePreview, FilePreviewKind, HistoryPage, LOCAL_TARGET_ID, LaunchRequest,
        ResumeCandidate, SearchResult, Target, TargetStatus,
    },
    recap::extract_recap,
    runtime::{Runtime, agent_is_working, attention_reason},
    ssh_config,
    terminal_session::TerminalSession,
    worker::{Event, Request, ScanRequest, Worker},
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

#[derive(Debug, Clone)]
pub struct LaunchForm {
    pub target: Target,
    pub kind: AgentKind,
    pub path: String,
    pub label: String,
    pub field: LaunchField,
}

#[derive(Debug, Clone)]
struct ArchivedResume {
    source_session_id: String,
    launch: LaunchForm,
}

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
}

#[derive(Debug)]
pub struct FileManagerForm {
    pub origin: FileManagerOrigin,
    pub target: Target,
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
    pub preview_scroll: u16,
    pub preview_max_scroll: u16,
    pub preview_page_rows: u16,
    pub preview_rendered: Option<Text<'static>>,
    pub query: String,
    pub preview_cache: HashMap<String, FilePreview>,
    pub preload_pending: HashSet<String>,
    pub entry_rows: Vec<(usize, Rect)>,
    pub list_area: Option<Rect>,
    pub preview_area: Option<Rect>,
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
    ConfirmKill {
        session_id: String,
        label: String,
        archive: bool,
    },
    ConfirmInstall {
        launch: LaunchForm,
        resume_id: Option<String>,
    },
    LegacyFallback {
        target_id: String,
        detail: String,
    },
    Help(HelpForm),
    Settings(SettingsForm),
    Search(SearchForm),
    PathPicker(PathPickerForm),
    Resume(ResumeForm),
}

#[derive(Debug, Clone, Default)]
pub struct HelpForm {
    pub offset: usize,
}

pub const HELP_CONTENT_ROWS: usize = 50;

/// Wall-clock milliseconds each agent-spinner frame is shown. Deriving the
/// frame index from elapsed time divided by this keeps the animation speed
/// constant regardless of how frequently the UI redraws.
const ANIMATION_FRAME_MS: u128 = 90;

#[derive(Debug, Clone)]
pub struct SettingsForm {
    pub scope: SettingsScope,
    pub values: Vec<String>,
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

pub const SETTING_LABELS: [&str; 20] = [
    "Refresh interval (ms)",
    "SSH timeout (sec)",
    "History limit",
    "History chunk lines",
    "SSH config path",
    "Environment (A=x B=y)",
    "Tunnel RPORT:LHOST:LPORT",
    "Companion command",
    "Companion binary (local)",
    "Codex command",
    "Codex args",
    "Codex install command",
    "Codex sync files",
    "Claude command",
    "Claude args",
    "Claude install command",
    "Claude sync files",
    "Terminal command",
    "Terminal args",
    "Attention patterns",
];

pub const HOST_SETTING_LABELS: [&str; 15] = [
    "Environment (A=x B=y)",
    "Tunnel RPORT:LHOST:LPORT",
    "Companion command",
    "Companion binary (local)",
    "Codex command",
    "Codex args",
    "Codex install command",
    "Codex sync files",
    "Claude command",
    "Claude args",
    "Claude install command",
    "Claude sync files",
    "Terminal command",
    "Terminal args",
    "Attention patterns",
];

impl SettingsForm {
    pub fn labels(&self) -> &[&str] {
        match &self.scope {
            SettingsScope::Global => &SETTING_LABELS,
            SettingsScope::Host(_) => &HOST_SETTING_LABELS,
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

pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub state: State,
    pub state_path: PathBuf,
    pub targets: Vec<TargetStatus>,
    pub sessions: Vec<AgentSession>,
    pub focus: Focus,
    pub selected_target: usize,
    pub selected_session_id: Option<String>,
    pub history: HistoryPage,
    pub history_message: String,
    pub history_loading: bool,
    pub history_offset: usize,
    pub interactive: bool,
    pub modal: Option<Modal>,
    pub file_manager: Option<FileManagerForm>,
    pub status_message: String,
    pub busy_operations: usize,
    pub pane_layout: PaneLayout,
    pub attention_banner: Option<Rect>,
    pub terminal_back: Option<Rect>,
    pub layout_debug_signature: Option<(u16, u16, u16, u16, bool, bool)>,
    pub attention_ids: Vec<String>,
    pub machine_list_state: ListState,
    pub agent_list_state: ListState,
    pub machine_rows: Vec<(usize, u16)>,
    pub agent_rows: Vec<(Option<String>, u16)>,
    pub archive_row: Option<usize>,
    pub agent_viewport_width: u16,
    pub agent_viewport_height: u16,
    pub terminal: Option<TerminalSession>,
    pub terminal_session_id: Option<String>,
    pub pending_terminal: Option<TerminalSession>,
    pub pending_terminal_session_id: Option<String>,
    pub terminal_selection: Option<TerminalSelection>,
    pub animation_frame: u64,
    animation_epoch: Instant,
    worker: Worker,
    pending_scans: HashSet<String>,
    pending_capture: Option<(String, String, usize)>,
    history_cache: HashMap<String, Vec<HistoryPage>>,
    history_cache_dir: PathBuf,
    dragging: Option<DragDivider>,
    last_refresh: Instant,
    top_up_count: u8,
    last_top_up: Option<Instant>,
    notifications: Vec<String>,
    terminal_retry_at: Option<Instant>,
    terminal_failures: u8,
    pending_terminal_started_at: Option<Instant>,
    pending_terminal_has_output: bool,
    pending_terminal_take_input: bool,
    clipboard_request: Option<String>,
    pending_install_launch: Option<(LaunchForm, Option<String>)>,
    pending_archived_resume: Option<ArchivedResume>,
    last_file_click: Option<FileClick>,
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
        let history_cache_dir = state_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("history");
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
            sessions: Vec::new(),
            focus: Focus::Machines,
            selected_target: 0,
            selected_session_id: None,
            history: HistoryPage::default(),
            history_message: "Select an agent to load its terminal history.".into(),
            history_loading: false,
            history_offset: 0,
            interactive: false,
            modal: None,
            file_manager: None,
            status_message: "Space enables a machine; n starts an agent".into(),
            busy_operations: 0,
            pane_layout: PaneLayout::default(),
            attention_banner: None,
            terminal_back: None,
            layout_debug_signature: None,
            attention_ids: Vec::new(),
            machine_list_state: ListState::default(),
            agent_list_state: ListState::default(),
            machine_rows: Vec::new(),
            agent_rows: Vec::new(),
            archive_row: None,
            agent_viewport_width: 80,
            agent_viewport_height: 20,
            terminal: None,
            terminal_session_id: None,
            pending_terminal: None,
            pending_terminal_session_id: None,
            terminal_selection: None,
            animation_frame: 0,
            animation_epoch: Instant::now(),
            worker,
            pending_scans: HashSet::new(),
            pending_capture: None,
            history_cache: HashMap::new(),
            history_cache_dir,
            dragging: None,
            last_refresh: Instant::now(),
            top_up_count: 0,
            last_top_up: None,
            notifications: Vec::new(),
            terminal_retry_at: None,
            terminal_failures: 0,
            pending_terminal_started_at: None,
            pending_terminal_has_output: false,
            pending_terminal_take_input: false,
            clipboard_request: None,
            pending_install_launch: None,
            pending_archived_resume: None,
            last_file_click: None,
        }
    }

    pub fn start(&mut self) {
        self.ensure_target_visible();
        self.refresh_enabled();
    }

    pub fn on_tick(&mut self) {
        // Advance the spinner from wall-clock time, not per-iteration, so its
        // speed stays constant no matter how often the loop redraws (e.g. a
        // stream of mouse-move events must not make the animation race).
        self.animation_frame =
            (self.animation_epoch.elapsed().as_millis() / ANIMATION_FRAME_MS) as u64;
        self.drain_worker();
        self.poll_media();
        self.poll_terminal();
        self.maybe_auto_submit_search();
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
        if is_copy_shortcut(key) && self.copy_terminal_selection() {
            return Action::Continue;
        }
        if let Some(modal) = self.modal.take() {
            return self.handle_modal(key, modal);
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
            if self.focus == Focus::Agents {
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
                    self.refresh_enabled();
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
                KeyCode::Char('1') => {
                    if !self.state.flatten {
                        self.focus = Focus::Machines;
                    }
                    return Action::Continue;
                }
                KeyCode::Char('2') => {
                    self.focus = Focus::Agents;
                    return Action::Continue;
                }
                KeyCode::Char('3') => {
                    self.focus = Focus::Recap;
                    self.activate_terminal();
                    return Action::Continue;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('1') if !self.state.flatten => {
                    self.focus = Focus::Machines;
                    return Action::Continue;
                }
                KeyCode::Char('2') => {
                    self.focus = Focus::Agents;
                    return Action::Continue;
                }
                KeyCode::Char('3') => {
                    self.focus = Focus::Recap;
                    self.activate_terminal();
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
                self.refresh_enabled();
                Action::Continue
            }
            KeyCode::Char('a') if self.focus == Focus::Agents => {
                self.toggle_archived();
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
            KeyCode::Enter if matches!(self.focus, Focus::Agents | Focus::Recap) => {
                self.focus = Focus::Recap;
                self.activate_terminal();
                Action::Continue
            }
            KeyCode::Char(' ') if self.focus == Focus::Machines => {
                self.toggle_target(self.selected_target);
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

    fn handle_file_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut form) = self.file_manager.take() else {
            return false;
        };
        self.last_file_click = None;
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            self.file_manager = Some(form);
            return true;
        }

        if form.preview_path.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    Self::clear_file_preview(&mut form);
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
                KeyCode::Home | KeyCode::Char('g') => form.preview_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => form.preview_scroll = form.preview_max_scroll,
                KeyCode::Char('c') => {
                    if let Some(entry) = form.entries.get(form.selected) {
                        self.clipboard_request = Some(entry.path.clone());
                        self.status_message = format!("Copied path: {}", entry.path);
                    }
                }
                KeyCode::Char('d') => self.download_selected_file(&form),
                _ => {}
            }
            self.file_manager = Some(form);
            return true;
        }

        match key.code {
            KeyCode::Esc => {
                if form.query.is_empty() {
                    self.status_message = "File browser closed".into();
                } else {
                    form.query.clear();
                    self.file_manager = Some(form);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                Self::move_file_selection(&mut form, -1);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::Down | KeyCode::Char('j') => {
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
            KeyCode::Right | KeyCode::Enter => self.open_file_entry(form),
            KeyCode::PageUp => {
                let page = form.preview_page_rows.max(1) as isize;
                Self::move_file_selection(&mut form, -page);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::PageDown => {
                let page = form.preview_page_rows.max(1) as isize;
                Self::move_file_selection(&mut form, page);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::Char('d') => {
                self.download_selected_file(&form);
                self.file_manager = Some(form);
            }
            KeyCode::Char('c') => {
                if let Some(entry) = form.entries.get(form.selected) {
                    self.clipboard_request = Some(entry.path.clone());
                    self.status_message = format!("Copied path: {}", entry.path);
                }
                self.file_manager = Some(form);
            }
            KeyCode::Char('r') | KeyCode::F(5) => self.request_file_listing(form),
            KeyCode::Backspace => {
                form.query.pop();
                Self::select_file_query_match(&mut form);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            KeyCode::Char(character) => {
                form.query.push(character);
                Self::select_file_query_match(&mut form);
                self.queue_file_preloads(&mut form);
                self.file_manager = Some(form);
            }
            _ => {
                self.file_manager = Some(form);
            }
        }
        true
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        if let Some(modal) = self.modal.as_mut() {
            match modal {
                Modal::Help(form) => match mouse.kind {
                    MouseEventKind::ScrollUp => form.offset = form.offset.saturating_sub(3),
                    MouseEventKind::ScrollDown => {
                        form.offset = form.offset.saturating_add(3).min(HELP_CONTENT_ROWS - 1)
                    }
                    _ => {}
                },
                Modal::Search(form) => match mouse.kind {
                    MouseEventKind::ScrollUp if !form.results.is_empty() => {
                        form.selected = form.selected.saturating_sub(1);
                    }
                    MouseEventKind::ScrollDown if !form.results.is_empty() => {
                        form.selected = (form.selected + 1).min(form.results.len() - 1);
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some((index, _)) = form
                            .result_rows
                            .iter()
                            .find(|(_, area)| inside(*area, mouse.column, mouse.row))
                        {
                            form.selected = *index;
                        }
                    }
                    _ => {}
                },
                _ => {
                    // Modal clicks must never activate panes behind the overlay.
                }
            }
            return Action::Continue;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.on_divider(mouse.column, mouse.row)
        {
            return Action::Continue;
        }
        if self.dragging.is_none() && self.handle_file_mouse(mouse) {
            return Action::Continue;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && !mouse.modifiers.contains(KeyModifiers::ALT)
            && self.begin_terminal_selection(mouse.column, mouse.row)
        {
            return Action::Continue;
        }
        if mouse.kind == MouseEventKind::Drag(MouseButton::Left)
            && self
                .terminal_selection
                .is_some_and(|selection| selection.dragging)
        {
            self.update_terminal_selection(mouse.column, mouse.row);
            return Action::Continue;
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left)
            && self
                .terminal_selection
                .is_some_and(|selection| selection.dragging)
        {
            self.update_terminal_selection(mouse.column, mouse.row);
            self.finish_terminal_selection(mouse);
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
            MouseEventKind::ScrollUp => self.scroll_at(mouse.column, mouse.row, true),
            MouseEventKind::ScrollDown => self.scroll_at(mouse.column, mouse.row, false),
            MouseEventKind::Moved => {
                self.forward_terminal_mouse(mouse);
            }
            _ => {}
        }
        Action::Continue
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
        if !in_list && !in_preview {
            return false;
        }
        let mut form = self.file_manager.take().expect("file form disappeared");
        self.focus = Focus::Agents;
        if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
            self.last_file_click = None;
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
                form.preview_scroll = form.preview_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if in_preview => {
                form.preview_scroll = form
                    .preview_scroll
                    .saturating_add(3)
                    .min(form.preview_max_scroll);
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
                if let Some(path) = form.preview_path.clone() {
                    let key = format!("preview:{path}");
                    if self.is_file_double_click(&key) {
                        self.last_file_click = None;
                        Self::clear_file_preview(&mut form);
                        self.status_message = "File preview closed; terminal restored".into();
                    }
                }
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
                Modal::Launch(form) if form.field != LaunchField::Kind => {
                    active_text(form).push_str(&text);
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
                _ => {
                    self.status_message = "Select a text field before pasting".into();
                }
            }
            return;
        }
        if self.interactive
            && let Some(terminal) = self.terminal.as_mut()
            && let Err(error) = terminal.write_paste(&text)
        {
            self.status_message = format!("Paste failed: {}", short_error(&error.to_string()));
        }
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
                self.status_message = format!("Terminal resize failed: {}", short_error(&error));
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

    pub fn visible_sessions(&self) -> Vec<&AgentSession> {
        let selected_target = self
            .targets
            .get(self.selected_target)
            .map(|target| target.target.id.as_str());
        let mut sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|session| {
                (self.state.flatten || selected_target == Some(session.target_id.as_str()))
                    && !(session.dead && session.kind == AgentKind::Terminal)
                    && (!session.dead || self.state.show_archived)
            })
            .collect();
        sessions.sort_by(|left, right| {
            left.dead
                .cmp(&right.dead)
                .then_with(|| left.target_id.cmp(&right.target_id))
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| right.created_at.cmp(&left.created_at))
        });
        sessions
    }

    pub fn archived_count(&self) -> usize {
        let selected_target = self
            .targets
            .get(self.selected_target)
            .map(|target| target.target.id.as_str());
        self.sessions
            .iter()
            .filter(|session| {
                session.dead
                    && session.kind != AgentKind::Terminal
                    && (self.state.flatten || selected_target == Some(session.target_id.as_str()))
            })
            .count()
    }

    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
    }

    pub fn take_clipboard_request(&mut self) -> Option<String> {
        self.clipboard_request.take()
    }

    pub fn selected_session(&self) -> Option<&AgentSession> {
        let id = self.selected_session_id.as_deref()?;
        self.sessions.iter().find(|session| session.id == id)
    }

    pub fn recap_for(&self, session: &AgentSession) -> String {
        let source = if self.terminal_session_id.as_deref() == Some(&session.id) {
            self.terminal
                .as_ref()
                .map(|terminal| terminal.screen().contents())
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

    pub fn attention_sessions(&self) -> Vec<&AgentSession> {
        let mut sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|session| session.needs_attention)
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
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
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

    fn activate_terminal(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.dead {
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
            self.status_message = "Archived session machine is no longer available".into();
            return;
        };
        self.close_terminal();
        let launch = LaunchForm {
            target: target.clone(),
            kind: session.kind,
            path: session.path.clone(),
            label: session.label.clone(),
            field: LaunchField::Kind,
        };
        let request = Request::ScanResumes {
            target,
            kind: session.kind,
            path: session.path,
        };
        if self.worker.requests.send(request).is_err() {
            self.status_message = "Resume scanner is unavailable".into();
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
        let terminal = if crate::runtime::is_daemon_session_id(&session.id) {
            TerminalSession::attach_daemon(
                self.worker.bridges.clone(),
                &target,
                &session.id,
                self.agent_viewport_width,
                self.agent_viewport_height,
            )
        } else {
            TerminalSession::attach(
                &target,
                &session.id,
                self.agent_viewport_width,
                self.agent_viewport_height,
            )
        };
        match terminal {
            Ok(terminal) => {
                self.pending_terminal = Some(terminal);
                self.pending_terminal_session_id = Some(session.id);
                self.pending_terminal_started_at = Some(Instant::now());
                self.pending_terminal_has_output = false;
                self.pending_terminal_take_input = take_input;
                self.history_offset = 0;
                if take_input {
                    self.focus = Focus::Recap;
                    self.status_message =
                        "Terminal is connecting; input activates with its first frame".into();
                } else {
                    self.status_message = "Switching terminal in background".into();
                }
            }
            Err(error) => {
                debug::log(
                    "app",
                    format!(
                        "attach failed target={} session={}: {error:#}",
                        target.id, session.id
                    ),
                );
                self.status_message = format!("Attach failed: {}", short_error(&error.to_string()));
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
        self.clear_pending_terminal();
    }

    fn clear_pending_terminal(&mut self) {
        self.pending_terminal = None;
        self.pending_terminal_session_id = None;
        self.pending_terminal_started_at = None;
        self.pending_terminal_has_output = false;
        self.pending_terminal_take_input = false;
    }

    fn has_terminal_for_selected(&self) -> bool {
        let selected = self.selected_session_id.as_deref();
        selected.is_some()
            && (self.terminal_session_id.as_deref() == selected
                || self.pending_terminal_session_id.as_deref() == selected)
    }

    /// True when a live emulator is attached for the currently selected session,
    /// so scrolling and copying should read its rendered scrollback rather than
    /// the linearized raw output log.
    pub(crate) fn attached_terminal_for_selected(&self) -> bool {
        self.terminal.is_some()
            && self.selected_session_id.is_some()
            && self.terminal_session_id.as_deref() == self.selected_session_id.as_deref()
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
            self.status_message = format!("Terminal resize failed: {}", short_error(&error));
        }
    }

    fn poll_terminal(&mut self) {
        let (changed, closed) = self
            .terminal
            .as_mut()
            .map(|terminal| (terminal.drain(), terminal.is_closed()))
            .unwrap_or((false, false));
        if changed
            && !closed
            && let (Some(session_id), Some(screen)) = (
                self.terminal_session_id.clone(),
                self.terminal
                    .as_ref()
                    .map(|terminal| terminal.screen().contents()),
            )
        {
            self.sync_live_agent_activity(&session_id, &screen);
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
            self.interactive = false;
            if closed_selected && self.pending_terminal.is_none() {
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
            self.status_message = if take_input {
                "Agent terminal input active; Left returns to agent list".into()
            } else {
                "Live terminal connected in background".into()
            };
        }
    }

    fn sync_live_agent_activity(&mut self, session_id: &str, screen: &str) {
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
        let working = attention.is_none() && agent_is_working(kind, screen);
        let session = &mut self.sessions[index];
        let changed = session.working != working
            || session.needs_attention != attention.is_some()
            || session.attention_reason != attention;
        session.working = working;
        session.needs_attention = attention.is_some();
        session.attention_reason = attention;
        if changed {
            debug::log(
                "activity",
                format!(
                    "source=live-terminal target={} session={} kind={} working={} attention={}",
                    target_id, session_id, kind, session.working, session.needs_attention
                ),
            );
        }
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

    fn handle_worker_event(&mut self, event: Event) {
        match event {
            Event::Scanned { target_id, result } => {
                self.pending_scans.remove(&target_id);
                let previous_attention: HashSet<_> = self
                    .sessions
                    .iter()
                    .filter(|session| session.target_id == target_id && session.needs_attention)
                    .map(|session| session.id.clone())
                    .collect();
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
                            for session in sessions.iter().filter(|session| {
                                session.needs_attention && !previous_attention.contains(&session.id)
                            }) {
                                let reason = session
                                    .attention_reason
                                    .as_deref()
                                    .unwrap_or("input required");
                                self.notifications.push(format!(
                                    "{} / {} needs input ({reason})",
                                    session.target_id,
                                    session.display_label()
                                ));
                                debug::log(
                                    "attention",
                                    format!(
                                        "new prompt target={} session={} reason={reason}",
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
                            self.sessions
                                .retain(|session| session.target_id != target_id);
                            self.sessions.extend(sessions);
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
                self.ensure_session_selection();
                if self
                    .selected_session()
                    .is_some_and(|session| session.target_id == target_id)
                    && (!self.has_terminal_for_selected() || self.history_offset > 0)
                {
                    self.request_history();
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
                                && self.history_offset > page.history_size
                            {
                                self.history_offset = page.history_size;
                                self.status_message = if page.history_size == 0 {
                                    "This terminal has no older scrollback".into()
                                } else {
                                    format!(
                                        "Reached the oldest available history ({} lines)",
                                        page.history_size
                                    )
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
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(session_id) => {
                        let legacy_tmux = session_id.starts_with("muxloom-");
                        self.selected_session_id = Some(session_id);
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
                        self.refresh_target(&target_id);
                    }
                    Err(error) => {
                        self.status_message = format!("Launch failed: {}", short_error(&error))
                    }
                }
            }
            Event::Installed {
                target_id,
                kind,
                result,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(message) => {
                        self.status_message = message;
                        self.refresh_target(&target_id);
                        if let Some((launch, resume_id)) = self.pending_install_launch.take()
                            && launch.target.id == target_id
                            && launch.kind == kind
                        {
                            self.submit_launch(launch, resume_id);
                        }
                    }
                    Err(error) => {
                        self.pending_install_launch = None;
                        self.status_message = format!("Install failed: {}", short_error(&error));
                    }
                }
            }
            Event::Killed { target_id, result } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
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
                    Err(error) => {
                        self.status_message = format!("Close failed: {}", short_error(&error))
                    }
                }
            }
            Event::Archived {
                target_id,
                session_id,
                result,
            } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(()) => {
                        if self.selected_session_id.as_deref() == Some(&session_id) {
                            self.close_terminal();
                            self.history = HistoryPage::default();
                        }
                        self.status_message =
                            "Agent stopped and moved to Archived; x there removes it permanently"
                                .into();
                        self.state.show_archived = true;
                        self.persist_state();
                        self.refresh_target(&target_id);
                    }
                    Err(error) => {
                        self.status_message = format!("Archive failed: {}", short_error(&error));
                    }
                }
            }
            Event::Searched { query, results } => {
                if let Some(Modal::Search(form)) = self.modal.as_mut()
                    && form.submitted_query == query
                {
                    form.loading = false;
                    form.results = results;
                    form.result_rows.clear();
                    form.selected = 0;
                    form.error = if form.results.is_empty() {
                        Some("No matching agent name, recap, or history".into())
                    } else {
                        None
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
            } => {
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
                            form.error = None;
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
                            if let Some(candidate) = candidates.first() {
                                debug::log(
                                    "resume",
                                    format!(
                                        "archived match source={} resume_id={} candidates={}",
                                        pending.source_session_id,
                                        candidate.id,
                                        candidates.len()
                                    ),
                                );
                                self.confirm_or_submit_launch(
                                    pending.launch,
                                    Some(candidate.id.clone()),
                                );
                            } else {
                                self.request_history();
                                self.status_message = format!(
                                    "No resumable {kind} history found; archived output is read-only"
                                );
                            }
                        }
                        Err(error) => {
                            self.request_history();
                            self.status_message = format!(
                                "Could not find resumable {kind} history: {}",
                                short_error(&error)
                            );
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
                    match result {
                        Ok(FileListing { path, entries }) => {
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
                            }
                            form.directory_cache.insert(path.clone(), entries.clone());
                            form.path = path;
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
                    match result {
                        Ok(preview) => {
                            if matches!(
                                preview.kind,
                                FilePreviewKind::Image | FilePreviewKind::Video
                            ) {
                                media_request =
                                    Some((form.target.clone(), path.clone(), preview.kind));
                            }
                            form.preview_cache.insert(path.clone(), preview.clone());
                            form.preview = Some(preview);
                            form.preview_rendered = None;
                            form.preview_error = None;
                            form.preview_scroll = 0;
                        }
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
                        form.preview_cache.insert(path, preview);
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
                let percent = transferred
                    .saturating_mul(100)
                    .checked_div(total_size)
                    .unwrap_or(0);
                let progress = if total_size > 0 {
                    format!(
                        "{}%  {}/{}",
                        percent,
                        format_transfer_bytes(transferred),
                        format_transfer_bytes(total_size)
                    )
                } else {
                    format_transfer_bytes(transferred)
                };
                self.status_message = format!(
                    "Downloading {name}  {progress}  {}/s",
                    format_transfer_bytes(bytes_per_second as u64)
                );
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
                    Ok(count) => {
                        self.status_message = format!("Uploaded {count} file(s)");
                        let refresh = matches!(self.file_manager.as_ref(), Some(form)
                            if form.target.id == target_id && form.path == remote_directory);
                        if refresh && let Some(form) = self.file_manager.take() {
                            self.request_file_listing(form);
                        }
                    }
                    Err(error) => {
                        self.status_message = format!("Upload failed: {}", short_error(&error));
                    }
                }
            }
        }
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
        if status.state != ConnectionState::Online {
            status.state = ConnectionState::Scanning;
        }
        let request = ScanRequest {
            target: status.target.clone(),
            codex_command: self
                .config
                .command_for(id, AgentKind::Codex)
                .command
                .clone(),
            claude_command: self
                .config
                .command_for(id, AgentKind::Claude)
                .command
                .clone(),
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
            self.selected_target = visible.first().copied().unwrap_or(0);
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
            self.activate_terminal();
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
                let visible = self.visible_target_indices();
                if visible.is_empty() {
                    return;
                }
                let current = visible
                    .iter()
                    .position(|index| *index == self.selected_target)
                    .unwrap_or(0);
                self.selected_target = visible[shifted(current, visible.len(), delta)];
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
                let next = shifted(current, ids.len(), delta);
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
            self.selected_target = index;
        }
        self.select_session(session_id);
        self.focus = Focus::Recap;
        self.activate_terminal();
        self.status_message = "Opened agent waiting for input".into();
    }

    fn select_session(&mut self, id: String) {
        if self.selected_session_id.as_deref() == Some(&id) {
            return;
        }
        self.pending_archived_resume = None;
        self.interactive = false;
        self.terminal_selection = None;
        self.clear_pending_terminal();
        self.terminal_retry_at = None;
        self.terminal_failures = 0;
        self.selected_session_id = Some(id);
        self.history_offset = 0;
        self.history = HistoryPage::default();
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
            self.history_message = "No agents on this machine.".into();
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
                && next_capture <= self.history.history_size
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
                self.history.total_lines() == 0 || page.total_lines() == self.history.total_lines()
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
        let path = self.history_cache_path(target_id, session_id, page.offset_from_bottom);
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
        let path = self.history_cache_path(target_id, session_id, offset);
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

    fn history_cache_path(&self, target_id: &str, session_id: &str, offset: usize) -> PathBuf {
        self.history_cache_dir
            .join(cache_path_component(target_id))
            .join(session_id)
            .join(format!("{offset}.json"))
    }

    fn page_history(&mut self, older: bool) {
        if self.selected_session().is_none() {
            return;
        }
        let page = self.agent_viewport_height.saturating_sub(2).max(1) as usize;
        self.scroll_history(older, page);
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
            self.history_offset = if self.history.total_lines() > 0 {
                let maximum = self.history.history_size;
                if next > maximum {
                    self.status_message = if maximum == 0 {
                        "This terminal has no older scrollback".into()
                    } else {
                        format!("Reached the oldest available history ({maximum} lines)")
                    };
                }
                next.min(maximum)
            } else {
                next
            };
        } else {
            self.history_offset = self.history_offset.saturating_sub(lines.max(1));
        }
        self.terminal_selection = None;
        if self.history_offset == 0 && self.selected_session().is_some_and(|session| !session.dead)
        {
            self.history_loading = false;
            self.history_message.clear();
        } else {
            self.request_history();
        }
    }

    /// Scroll a live attached terminal through the emulator's own rendered
    /// scrollback. `history_offset` mirrors the emulator offset (rows up from the
    /// live bottom); the emulator clamps to what its buffer holds.
    fn scroll_attached_terminal(&mut self, older: bool, lines: usize) {
        let step = lines.max(1);
        let desired = if older {
            self.history_offset.saturating_add(step)
        } else {
            self.history_offset.saturating_sub(step)
        };
        if let Some(terminal) = self.terminal.as_mut() {
            terminal.set_scrollback(desired);
            self.history_offset = terminal.scrollback();
        }
        if older && self.history_offset < desired {
            self.status_message = if self.history_offset == 0 {
                "This terminal has no scrollback yet".into()
            } else {
                format!(
                    "Reached the oldest buffered line ({} up)",
                    self.history_offset
                )
            };
        }
        self.terminal_selection = None;
        self.history_loading = false;
        self.history_message.clear();
    }

    fn open_launch(&mut self) {
        // In grouped mode the machine sidebar is authoritative, even if an old
        // session selection still points at a different host.
        let target = if !self.state.flatten {
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
        }
        .or_else(|| {
            self.targets
                .iter()
                .find(|status| status.enabled)
                .map(|status| status.target.clone())
        });
        let Some(target) = target else {
            self.status_message = "Enable a machine before launching an agent".into();
            return;
        };
        let selected_path = self
            .selected_session()
            .filter(|session| session.target_id == target.id)
            .map(|session| session.path.clone());
        let path = if let Some(path) = selected_path {
            path
        } else if target.id == LOCAL_TARGET_ID {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .display()
                .to_string()
        } else {
            ".".into()
        };
        self.modal = Some(Modal::Launch(LaunchForm {
            target,
            kind: AgentKind::Codex,
            path,
            label: String::new(),
            field: LaunchField::Kind,
        }));
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
        let mut form = ResumeForm {
            launch,
            candidates: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
        };
        if form.launch.kind != AgentKind::Terminal {
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
        }
        self.modal = Some(Modal::Resume(form));
    }

    fn open_global_settings(&mut self) {
        self.modal = Some(Modal::Settings(SettingsForm {
            scope: SettingsScope::Global,
            values: vec![
                self.config.refresh_interval_ms.to_string(),
                self.config.ssh_connect_timeout_secs.to_string(),
                self.config.history_limit.to_string(),
                self.config.history_chunk_lines.to_string(),
                self.config.ssh_config.clone(),
                self.config.environment.clone(),
                self.config.reverse_tunnel.clone(),
                self.config.companion_command.clone(),
                self.config.companion_binary.clone(),
                self.config.agents.codex.command.clone(),
                format_shell_list(&self.config.agents.codex.args),
                self.config.agents.codex.install.clone(),
                format_shell_list(&self.config.agents.codex.sync_files),
                self.config.agents.claude.command.clone(),
                format_shell_list(&self.config.agents.claude.args),
                self.config.agents.claude.install.clone(),
                format_shell_list(&self.config.agents.claude.sync_files),
                self.config.agents.terminal.command.clone(),
                format_shell_list(&self.config.agents.terminal.args),
                format_shell_list(&self.config.attention_patterns),
            ],
            selected: 0,
            error: None,
        }));
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
        let codex = self
            .config
            .command_for(&target_id, AgentKind::Codex)
            .clone();
        let claude = self
            .config
            .command_for(&target_id, AgentKind::Claude)
            .clone();
        let terminal = self
            .config
            .command_for(&target_id, AgentKind::Terminal)
            .clone();
        self.modal = Some(Modal::Settings(SettingsForm {
            scope: SettingsScope::Host(target_id.clone()),
            values: vec![
                self.config
                    .hosts
                    .get(&target_id)
                    .and_then(|host| host.environment.clone())
                    .unwrap_or_else(|| self.config.environment.clone()),
                self.config
                    .hosts
                    .get(&target_id)
                    .and_then(|host| host.reverse_tunnel.clone())
                    .unwrap_or_else(|| self.config.reverse_tunnel.clone()),
                self.config
                    .hosts
                    .get(&target_id)
                    .and_then(|host| host.companion_command.clone())
                    .unwrap_or_else(|| self.config.companion_command.clone()),
                self.config
                    .hosts
                    .get(&target_id)
                    .and_then(|host| host.companion_binary.clone())
                    .unwrap_or_else(|| self.config.companion_binary.clone()),
                codex.command,
                format_shell_list(&codex.args),
                codex.install,
                format_shell_list(&codex.sync_files),
                claude.command,
                format_shell_list(&claude.args),
                claude.install,
                format_shell_list(&claude.sync_files),
                terminal.command,
                format_shell_list(&terminal.args),
                format_shell_list(self.config.attention_patterns_for(&target_id)),
            ],
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
        if self.file_manager.take().is_some() {
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
        let path = selected
            .filter(|session| session.target_id == target.id)
            .map(|session| session.path)
            .unwrap_or_else(|| ".".into());
        self.release_terminal_input("File manager opened");
        self.focus = Focus::Agents;
        self.request_file_listing(FileManagerForm {
            origin,
            target,
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
            preview_rendered: None,
            query: String::new(),
            preview_cache: HashMap::new(),
            preload_pending: HashSet::new(),
            entry_rows: Vec::new(),
            list_area: None,
            preview_area: None,
            media_playback: None,
            media_frame: None,
            media_loading: false,
            media_error: None,
        });
    }

    fn request_file_listing(&mut self, mut form: FileManagerForm) {
        form.loading = true;
        form.error = None;
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

    fn clear_file_preview(form: &mut FileManagerForm) {
        form.preview_path = None;
        form.preview = None;
        form.preview_requested_path = None;
        form.preview_loading = false;
        form.preview_error = None;
        form.preview_scroll = 0;
        form.preview_max_scroll = 0;
        form.preview_page_rows = 1;
        form.preview_rendered = None;
        form.preview_area = None;
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
        form.query.clear();
        if !form.entries.is_empty() {
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
        form.selected = shifted(form.selected, form.entries.len(), delta);
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
        let step = form.preview_page_rows.max(1);
        if form.preview_max_scroll == 0 {
            form.preview_scroll = 0;
        } else if forward {
            form.preview_scroll = if form.preview_scroll >= form.preview_max_scroll {
                0
            } else {
                form.preview_scroll
                    .saturating_add(step)
                    .min(form.preview_max_scroll)
            };
        } else {
            form.preview_scroll = if form.preview_scroll == 0 {
                form.preview_max_scroll
            } else {
                form.preview_scroll.saturating_sub(step)
            };
        }
    }

    fn queue_file_preloads(&self, form: &mut FileManagerForm) {
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
            self.status_message = "File preview closed; terminal restored".into();
            self.file_manager = Some(form);
        } else {
            Self::clear_file_preview(&mut form);
            form.preview_path = Some(entry.path.clone());
            let mut media_kind = None;
            if let Some(preview) = form.preview_cache.get(&entry.path).cloned() {
                if matches!(
                    preview.kind,
                    FilePreviewKind::Image | FilePreviewKind::Video
                ) {
                    media_kind = Some(preview.kind);
                }
                form.preview = Some(preview);
                form.preview_loading = false;
                self.status_message = "Opened preloaded preview".into();
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

    fn open_search_result(&mut self, result: SearchResult) {
        let Some(target_index) = self
            .targets
            .iter()
            .position(|target| target.target.id == result.target_id)
        else {
            self.status_message = "Search result machine is no longer available".into();
            return;
        };
        if !self
            .sessions
            .iter()
            .any(|session| session.id == result.session_id)
        {
            self.status_message = "Search result session is no longer available".into();
            return;
        }
        self.selected_target = target_index;
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
            archive: !session.dead && session.kind != AgentKind::Terminal,
        });
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
                    form.selected = (form.selected + 1) % form.values.len();
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.selected = form
                        .selected
                        .checked_sub(1)
                        .unwrap_or(form.values.len() - 1);
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::Enter | KeyCode::Char('s')
                    if key.code == KeyCode::Enter
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.apply_settings(form);
                }
                KeyCode::Backspace => {
                    form.values[form.selected].pop();
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.values[form.selected].clear();
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.values[form.selected].push(character);
                    form.error = None;
                    self.modal = Some(Modal::Settings(form));
                }
                _ => self.modal = Some(Modal::Settings(form)),
            },
            Modal::Search(mut form) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::BackTab if !form.results.is_empty() => {
                    form.selected = shifted(form.selected, form.results.len(), -1);
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Down | KeyCode::Tab if !form.results.is_empty() => {
                    form.selected = shifted(form.selected, form.results.len(), 1);
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
                    form.selected = shifted(form.selected, matched_directories(&form).len(), -1);
                    self.modal = Some(Modal::PathPicker(form));
                }
                KeyCode::Down if !matched_directories(&form).is_empty() => {
                    form.selected = shifted(form.selected, matched_directories(&form).len(), 1);
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
            Modal::Resume(mut form) => match key.code {
                KeyCode::Esc | KeyCode::Left => self.modal = Some(Modal::Launch(form.launch)),
                KeyCode::Enter if form.selected == 0 => {
                    self.confirm_or_submit_launch(form.launch, None)
                }
                KeyCode::Up | KeyCode::Char('k') if !form.loading => {
                    form.selected = shifted(form.selected, form.candidates.len() + 1, -1);
                    self.modal = Some(Modal::Resume(form));
                }
                KeyCode::Down | KeyCode::Char('j') if !form.loading => {
                    form.selected = shifted(form.selected, form.candidates.len() + 1, 1);
                    self.modal = Some(Modal::Resume(form));
                }
                KeyCode::Enter if !form.loading => {
                    let resume_id = form
                        .selected
                        .checked_sub(1)
                        .and_then(|index| form.candidates.get(index))
                        .map(|candidate| candidate.id.clone());
                    self.confirm_or_submit_launch(form.launch, resume_id);
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
            Modal::ConfirmInstall { launch, resume_id } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.install_and_launch(launch, resume_id),
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => self.modal = Some(Modal::ConfirmInstall { launch, resume_id }),
            },
            Modal::LegacyFallback { target_id, detail } => match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.modal = Some(Modal::LegacyFallback { target_id, detail }),
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
                    form.kind = form.kind.previous();
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Right | KeyCode::Char(' ') if form.field == LaunchField::Kind => {
                    form.kind = form.kind.next();
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
                    active_text(&mut form).pop();
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Char('u')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && form.field != LaunchField::Kind =>
                {
                    active_text(&mut form).clear();
                    self.modal = Some(Modal::Launch(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if form.field != LaunchField::Kind {
                        active_text(&mut form).push(character);
                    }
                    self.modal = Some(Modal::Launch(form));
                }
                _ => self.modal = Some(Modal::Launch(form)),
            },
        }
        Action::Continue
    }

    fn apply_settings(&mut self, mut form: SettingsForm) {
        let parsed = (|| -> Result<Config, String> {
            let mut config = self.config.clone();
            match &form.scope {
                SettingsScope::Global => {
                    config.refresh_interval_ms = parse_setting(&form.values[0], SETTING_LABELS[0])?;
                    config.ssh_connect_timeout_secs =
                        parse_setting(&form.values[1], SETTING_LABELS[1])?;
                    config.history_limit = parse_setting(&form.values[2], SETTING_LABELS[2])?;
                    config.history_chunk_lines = parse_setting(&form.values[3], SETTING_LABELS[3])?;
                    if config.refresh_interval_ms < 500 {
                        return Err("Refresh interval must be at least 500 ms".into());
                    }
                    if config.ssh_connect_timeout_secs == 0 {
                        return Err("SSH timeout must be greater than zero".into());
                    }
                    if config.history_limit < 2_000 || config.history_chunk_lines == 0 {
                        return Err(
                            "History limit must be >= 2000 and chunk lines must be > 0".into()
                        );
                    }
                    config.ssh_config = form.values[4].clone();
                    config.environment = form.values[5].clone();
                    config.reverse_tunnel = form.values[6].clone();
                    config.companion_command = form.values[7].clone();
                    config.companion_binary = form.values[8].clone();
                    config.agents.codex.command = form.values[9].clone();
                    config.agents.codex.args =
                        parse_shell_list(&form.values[10], SETTING_LABELS[10])?;
                    config.agents.codex.install = form.values[11].clone();
                    config.agents.codex.sync_files =
                        parse_shell_list(&form.values[12], SETTING_LABELS[12])?;
                    config.agents.claude.command = form.values[13].clone();
                    config.agents.claude.args =
                        parse_shell_list(&form.values[14], SETTING_LABELS[14])?;
                    config.agents.claude.install = form.values[15].clone();
                    config.agents.claude.sync_files =
                        parse_shell_list(&form.values[16], SETTING_LABELS[16])?;
                    config.agents.terminal.command = form.values[17].clone();
                    config.agents.terminal.args =
                        parse_shell_list(&form.values[18], SETTING_LABELS[18])?;
                    config.attention_patterns =
                        parse_shell_list(&form.values[19], SETTING_LABELS[19])?;
                }
                SettingsScope::Host(target_id) => {
                    let codex = CommandConfig {
                        command: form.values[4].clone(),
                        args: parse_shell_list(&form.values[5], HOST_SETTING_LABELS[5])?,
                        install: form.values[6].clone(),
                        sync_files: parse_shell_list(&form.values[7], HOST_SETTING_LABELS[7])?,
                    };
                    let claude = CommandConfig {
                        command: form.values[8].clone(),
                        args: parse_shell_list(&form.values[9], HOST_SETTING_LABELS[9])?,
                        install: form.values[10].clone(),
                        sync_files: parse_shell_list(&form.values[11], HOST_SETTING_LABELS[11])?,
                    };
                    let terminal = CommandConfig {
                        command: form.values[12].clone(),
                        args: parse_shell_list(&form.values[13], HOST_SETTING_LABELS[13])?,
                        ..CommandConfig::default()
                    };
                    let attention_patterns =
                        parse_shell_list(&form.values[14], HOST_SETTING_LABELS[14])?;
                    config.hosts.insert(
                        target_id.clone(),
                        HostConfig {
                            codex: Some(codex),
                            claude: Some(claude),
                            terminal: Some(terminal),
                            environment: Some(form.values[0].clone()),
                            reverse_tunnel: Some(form.values[1].clone()),
                            companion_command: Some(form.values[2].clone()),
                            companion_binary: Some(form.values[3].clone()),
                            attention_patterns: Some(attention_patterns),
                        },
                    );
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
            let codex_empty = effective_host
                .map(|host| config.command_for(host, AgentKind::Codex))
                .unwrap_or(&config.agents.codex)
                .command
                .trim()
                .is_empty();
            let claude_empty = effective_host
                .map(|host| config.command_for(host, AgentKind::Claude))
                .unwrap_or(&config.agents.claude)
                .command
                .trim()
                .is_empty();
            if codex_empty || claude_empty {
                return Err("Codex and Claude commands cannot be empty".into());
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
        self.refresh_enabled();
    }

    fn submit_launch(&mut self, form: LaunchForm, resume_id: Option<String>) {
        if form.path.trim().is_empty() {
            self.status_message = "Launch cancelled: working directory is required".into();
            return;
        }
        let command = self.config.command_for(&form.target.id, form.kind).clone();
        let environment = self
            .config
            .environment_for(&form.target.id)
            .unwrap_or_default();
        let request = LaunchRequest {
            target: form.target,
            kind: form.kind,
            path: form.path,
            label: form.label,
            resume_id,
        };
        if self
            .worker
            .requests
            .send(Request::Launch {
                request,
                command,
                environment,
            })
            .is_ok()
        {
            self.busy_operations += 1;
            self.status_message = "Launching agent...".into();
        }
    }

    fn confirm_or_submit_launch(&mut self, form: LaunchForm, resume_id: Option<String>) {
        let available = self
            .targets
            .iter()
            .find(|target| target.target.id == form.target.id)
            .is_some_and(|target| match form.kind {
                AgentKind::Codex => target.probe.codex,
                AgentKind::Claude => target.probe.claude,
                AgentKind::Terminal => true,
            });
        if available || form.kind == AgentKind::Terminal {
            self.submit_launch(form, resume_id);
        } else {
            self.modal = Some(Modal::ConfirmInstall {
                launch: form,
                resume_id,
            });
        }
    }

    fn install_and_launch(&mut self, launch: LaunchForm, resume_id: Option<String>) {
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
            self.pending_install_launch = Some((launch.clone(), resume_id));
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

    fn on_divider(&mut self, column: u16, row: u16) -> bool {
        if self
            .pane_layout
            .portrait_terminal_divider
            .is_some_and(|area| near_divider(area, column, row))
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

    fn drag_divider(&mut self, column: u16, row: u16) {
        match self.dragging {
            Some(DragDivider::Machines) => {
                let Some(area) = self.pane_layout.machines else {
                    return;
                };
                let focused_bonus = u16::from(self.focus == Focus::Machines) * 8;
                self.state.machine_width = column
                    .saturating_sub(area.x)
                    .saturating_add(1)
                    .saturating_sub(focused_bonus)
                    .clamp(16, 52);
            }
            Some(DragDivider::Agents) => {
                let Some(area) = self.pane_layout.agents else {
                    return;
                };
                let focused_bonus = u16::from(self.focus == Focus::Agents) * 10;
                let width = column
                    .saturating_sub(area.x)
                    .saturating_add(1)
                    .saturating_sub(focused_bonus)
                    .clamp(24, 72);
                if self.file_manager.is_some() {
                    self.state.file_width = width;
                } else {
                    self.state.agents_width = width;
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
                let focus_adjustment: i16 = match self.focus {
                    Focus::Machines => 10,
                    Focus::Agents => -10,
                    Focus::Recap => 0,
                };
                self.state.portrait_machine_percent =
                    (i16::try_from(display_percent).unwrap_or(i16::MAX) - focus_adjustment)
                        .clamp(25, 75) as u16;
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
            for (target_index, height) in self
                .machine_rows
                .iter()
                .skip(self.machine_list_state.offset())
            {
                if line < *height {
                    hit = Some((*target_index, line));
                    break;
                }
                line = line.saturating_sub(*height);
            }
            if let Some((target_index, item_line)) = hit {
                self.selected_target = target_index;
                self.ensure_session_selection();
                if item_line == 0
                    && column >= area.x.saturating_add(5)
                    && column <= area.x.saturating_add(7)
                {
                    self.toggle_target(target_index);
                }
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
        self.terminal_selection = Some(TerminalSelection {
            anchor: point,
            cursor: point,
            dragging: true,
        });
        self.status_message = "Selecting terminal text...".into();
        true
    }

    fn update_terminal_selection(&mut self, column: u16, row: u16) {
        let Some(point) = self.terminal_cell_at(column, row) else {
            return;
        };
        if let Some(selection) = self.terminal_selection.as_mut() {
            selection.cursor = point;
        }
    }

    fn finish_terminal_selection(&mut self, mouse: MouseEvent) {
        if let Some(selection) = self.terminal_selection.as_mut() {
            selection.dragging = false;
        }
        if self.copy_terminal_selection() {
            return;
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

    fn copy_terminal_selection(&mut self) -> bool {
        // Make sure the emulator is at the same scrollback position that is on
        // screen, so a selection made while scrolled back copies what is shown.
        if self.attached_terminal_for_selected() {
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

    fn selected_terminal_text(&self) -> Option<String> {
        let selection = self.terminal_selection?;
        if selection.anchor == selection.cursor {
            return None;
        }
        let (start, end) = selection.normalized();
        let text = if self.attached_terminal_for_selected() {
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
            self.focus = Focus::Recap;
            if self.history_offset == 0 {
                self.activate_terminal();
            }
            self.scroll_history(up, 1);
            return;
        }
        if let Some(area) = self
            .pane_layout
            .machines
            .filter(|area| inside(*area, column, row))
        {
            self.focus = Focus::Machines;
            let page = (area.height.saturating_sub(2) / 2).max(1) as isize;
            self.move_selection(if up { -page } else { page });
            return;
        }
        if let Some(area) = self
            .pane_layout
            .agents
            .filter(|area| inside(*area, column, row))
        {
            self.focus = Focus::Agents;
            let page = area.height.saturating_sub(2).max(1) as isize;
            self.move_selection(if up { -page } else { page });
        }
    }

    fn persist_state(&mut self) {
        if let Err(error) = self.state.save(&self.state_path) {
            self.status_message =
                format!("Could not save state: {}", short_error(&error.to_string()));
        }
    }
}

fn shifted(current: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(length as isize) as usize
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
    if source.offset_from_bottom > source.history_size || desired_offset > source.history_size {
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
    })
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

fn active_text(form: &mut LaunchForm) -> &mut String {
    match form.field {
        LaunchField::Path => &mut form.path,
        LaunchField::Label => &mut form.label,
        LaunchField::Kind => unreachable!("agent kind is not a text field"),
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
                    let mut sequence = String::from("\x1b[");
                    let mut final_byte = None;
                    for next in characters.by_ref() {
                        sequence.push(next);
                        if ('@'..='~').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                    }
                    if final_byte == Some('m') {
                        sanitized.push_str(&sequence);
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
            sanitized.push(character);
        }
    }
    sanitized
}

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
    fn shifted_wraps_and_pages_in_both_directions() {
        assert_eq!(shifted(0, 3, -1), 2);
        assert_eq!(shifted(2, 3, 1), 0);
        assert_eq!(shifted(1, 5, 7), 3);
        assert_eq!(shifted(1, 5, -7), 4);
        assert_eq!(parent_path("/work/project"), "/work");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(child_path("/work", "project"), "/work/project");
        let form = PathPickerForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Codex,
                path: ".".into(),
                label: String::new(),
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
        });

        app.sync_live_agent_activity("muxloomd-codex-live", "• Working (1s • esc to interrupt)");
        assert!(app.sessions[0].working);

        app.sync_live_agent_activity(
            "muxloomd-codex-live",
            "› Ask Codex anything\ngpt-5.6-sol xhigh · /work",
        );
        assert!(!app.sessions[0].working);
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
        assert_eq!(app.state.machine_width, 33);

        app.open_file_manager();
        app.focus = Focus::Agents;
        app.dragging = Some(DragDivider::Agents);
        app.drag_divider(80, 10);
        assert_eq!(app.state.file_width, 39);
        assert_eq!(app.state.agents_width, 40);
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
        assert_eq!(app.take_clipboard_request().as_deref(), Some("one"));
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
        };
        app.history_offset = 10;
        app.scroll_history(true, 20);
        assert_eq!(app.history_offset, 12);
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
        form.values[17] = "/bin/zsh".into();
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
        });
        assert!(app.visible_sessions().is_empty());
        assert_eq!(app.archived_count(), 1);
        app.state.show_archived = true;
        assert_eq!(app.visible_sessions().len(), 1);
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
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        app.targets[0].probe.codex = true;
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
            result: Ok(vec![ResumeCandidate {
                id: "thread-id".into(),
                recap: Some("Fix the renderer".into()),
                first_message: None,
                last_message: None,
                updated_at: "2026-07-22T12:00:00Z".into(),
            }]),
        });
        match receive_request(&request_rx) {
            Request::Launch { request, .. } => {
                assert_eq!(request.target.id, "local");
                assert_eq!(request.kind, AgentKind::Codex);
                assert_eq!(request.path, "/work/project");
                assert_eq!(request.label, "fix renderer");
                assert_eq!(request.resume_id.as_deref(), Some("thread-id"));
            }
            request => panic!("expected archived resume launch, got {request:?}"),
        }
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
                path: "/work/project".into(),
                entries: vec![FileEntry {
                    name: "README.md".into(),
                    path: "/work/project/README.md".into(),
                    kind: FileEntryKind::File,
                    size: 42,
                }],
            }),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
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
        for expected in [8, 16, 20, 0] {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(
                app.file_manager.as_ref().map(|form| form.preview_scroll),
                Some(expected)
            );
        }
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.file_manager.as_ref().map(|form| form.preview_scroll),
            Some(20)
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

        // Focus another pane: the browser stays open but no longer swallows keys.
        app.focus = Focus::Recap;
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
        });
        app.selected_session_id = Some("muxloom-codex-nav".into());
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        app.handle_worker_event(Event::FilesListed {
            target_id: "local".into(),
            requested_path: "/work/project".into(),
            result: Ok(FileListing {
                path: "/work/project".into(),
                entries: vec![FileEntry {
                    name: "src".into(),
                    path: "/work/project/src".into(),
                    kind: FileEntryKind::Directory,
                    size: 0,
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
                path: "/work".into(),
                entries: vec![
                    FileEntry {
                        name: "alpha.txt".into(),
                        path: "/work/alpha.txt".into(),
                        kind: FileEntryKind::File,
                        size: 5,
                    },
                    FileEntry {
                        name: "beta.rs".into(),
                        path: "/work/beta.rs".into(),
                        kind: FileEntryKind::File,
                        size: 12,
                    },
                    FileEntry {
                        name: "src".into(),
                        path: "/work/src".into(),
                        kind: FileEntryKind::Directory,
                        size: 0,
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
        assert!(
            !request_rx
                .try_iter()
                .any(|request| matches!(request, Request::PreviewFile { .. }))
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
                path: "/work".into(),
                entries: vec![
                    FileEntry {
                        name: "README.md".into(),
                        path: "/work/README.md".into(),
                        kind: FileEntryKind::File,
                        size: 300_000,
                    },
                    FileEntry {
                        name: "src".into(),
                        path: "/work/src".into(),
                        kind: FileEntryKind::Directory,
                        size: 0,
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

        app.handle_mouse(click(1));
        assert!(request_rx.try_recv().is_err(), "single click only selects");
        app.handle_mouse(click(1));
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
        app.handle_mouse(click(1));
        app.handle_mouse(click(1));
        assert!(
            app.file_manager
                .as_ref()
                .is_some_and(|form| form.preview_path.is_none())
        );

        app.handle_mouse(click(2));
        app.handle_mouse(click(2));
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
                size: 0,
            },
            FileEntry {
                name: "new".into(),
                path: "/work/new".into(),
                kind: FileEntryKind::Directory,
                size: 0,
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
                path: "/work".into(),
                entries: vec![
                    FileEntry {
                        name: "old".into(),
                        path: "/work/old".into(),
                        kind: FileEntryKind::Directory,
                        size: 0,
                    },
                    FileEntry {
                        name: "new".into(),
                        path: "/work/new".into(),
                        kind: FileEntryKind::Directory,
                        size: 0,
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
        let config = Config {
            ssh_config: ssh_path.display().to_string(),
            ..Config::default()
        };
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
        form.values[1] = "18118:127.0.0.1:8080".into();
        form.values[2] = "~/.local/bin/muxloomd".into();
        form.values[3] = "~/Downloads/muxloomd-linux".into();
        form.values[4] = "/opt/codex".into();
        form.values[5] = "--full-auto".into();
        form.values[14] = "'gpu approval'".into();
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

    #[test]
    fn confirming_a_terminal_folder_advances_to_new_session_choice() {
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
        app.modal = Some(Modal::PathPicker(PathPickerForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Terminal,
                path: ".".into(),
                label: String::new(),
                field: LaunchField::Path,
            },
            path: "/tmp/project".into(),
            directories: vec!["src".into()],
            query: String::new(),
            selected: 0,
            loading: false,
            error: None,
        }));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::Resume(ResumeForm {
                selected: 0,
                loading: false,
                ref launch,
                ..
            })) if launch.path == "/tmp/project" && launch.kind == AgentKind::Terminal
        ));
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
                field: LaunchField::Path,
            },
            candidates: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
        }));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Some(Modal::ConfirmInstall {
                ref launch,
                resume_id: None,
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
}
