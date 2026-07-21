use std::{
    collections::HashSet,
    env,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, widgets::ListState};

use crate::{
    config::{CommandConfig, Config, HostConfig, State},
    debug,
    model::{
        AgentKind, AgentSession, ConnectionState, DirectoryListing, HistoryPage, LOCAL_TARGET_ID,
        LaunchRequest, ResumeCandidate, SearchResult, Target, TargetStatus,
    },
    runtime::Runtime,
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

#[derive(Debug, Clone)]
pub enum Modal {
    Launch(LaunchForm),
    ConfirmKill { session_id: String, label: String },
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

pub const HELP_CONTENT_ROWS: usize = 41;

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
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

pub const SETTING_LABELS: [&str; 12] = [
    "Refresh interval (ms)",
    "SSH timeout (sec)",
    "History limit",
    "History chunk lines",
    "SSH config path",
    "Codex command",
    "Codex args (JSON)",
    "Claude command",
    "Claude args (JSON)",
    "Terminal command",
    "Terminal args (JSON)",
    "Attention patterns (JSON)",
];

pub const HOST_SETTING_LABELS: [&str; 7] = [
    "Codex command",
    "Codex args (JSON)",
    "Claude command",
    "Claude args (JSON)",
    "Terminal command",
    "Terminal args (JSON)",
    "Attention patterns (JSON)",
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
}

#[derive(Debug, Clone, Default)]
pub struct PaneLayout {
    pub machines: Option<Rect>,
    pub agents: Option<Rect>,
    pub recap: Option<Rect>,
    pub machine_divider_x: Option<u16>,
    pub agents_divider_x: Option<u16>,
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
    worker: Worker,
    pending_scans: HashSet<String>,
    pending_capture: Option<String>,
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
            worker,
            pending_scans: HashSet::new(),
            pending_capture: None,
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
        }
    }

    pub fn start(&mut self) {
        self.ensure_target_visible();
        self.refresh_enabled();
    }

    pub fn on_tick(&mut self) {
        self.drain_worker();
        self.poll_terminal();
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
        if let Some(modal) = self.modal.take() {
            return self.handle_modal(key, modal);
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
                KeyCode::Char('f') => {
                    self.toggle_flatten();
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
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                self.focus_left();
                Action::Continue
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                self.focus_right();
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

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        if let Some(Modal::Help(form)) = self.modal.as_mut() {
            match mouse.kind {
                MouseEventKind::ScrollUp => form.offset = form.offset.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    form.offset = form.offset.saturating_add(3).min(HELP_CONTENT_ROWS - 1)
                }
                _ => {}
            }
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
                if button == MouseButton::Left && self.on_divider(mouse.column, mouse.row) {
                    return Action::Continue;
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
                    self.drag_divider(mouse.column);
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

    pub fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
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
                    form.submitted_query.clear();
                    form.results.clear();
                    form.selected = 0;
                    form.error = None;
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
                    && (self.state.flatten || selected_target == Some(session.target_id.as_str()))
            })
            .count()
    }

    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
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
            .and_then(first_meaningful_line)
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
        if key.code == KeyCode::Left && key.modifiers.is_empty() {
            self.release_terminal_input("Agent terminal remains attached");
            self.focus = Focus::Agents;
            return Action::Continue;
        }
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
        self.connect_terminal(true);
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
        match TerminalSession::attach(
            &target,
            &session.id,
            self.agent_viewport_width,
            self.agent_viewport_height,
        ) {
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
            Event::Captured { session_id, result } => {
                if self.pending_capture.as_deref() == Some(&session_id) {
                    self.pending_capture = None;
                    if self.selected_session_id.as_deref() == Some(&session_id) {
                        match result {
                            Ok(mut page) => {
                                page.text = sanitize_terminal_text(&page.text);
                                if page.offset_from_bottom == self.history_offset {
                                    self.history = page;
                                    self.history_message = if self.history.text.is_empty() {
                                        "No terminal output yet.".into()
                                    } else {
                                        String::new()
                                    };
                                    self.history_loading = false;
                                } else {
                                    self.request_history();
                                }
                            }
                            Err(error) => {
                                self.history_loading = false;
                                self.history_message =
                                    format!("History unavailable: {}", short_error(&error));
                            }
                        }
                    } else {
                        self.request_history();
                    }
                }
            }
            Event::Launched { target_id, result } => {
                self.busy_operations = self.busy_operations.saturating_sub(1);
                match result {
                    Ok(session_id) => {
                        self.selected_session_id = Some(session_id);
                        self.status_message = format!("Agent launched on {target_id}");
                        self.refresh_target(&target_id);
                    }
                    Err(error) => {
                        self.status_message = format!("Launch failed: {}", short_error(&error))
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
            Event::Searched { query, results } => {
                if let Some(Modal::Search(form)) = self.modal.as_mut()
                    && form.submitted_query == query
                {
                    form.loading = false;
                    form.results = results;
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
                    match result {
                        Ok(candidates) => {
                            form.candidates = candidates;
                            form.selected = 0;
                            form.error = None;
                        }
                        Err(error) => {
                            form.error = Some(short_error(&error));
                        }
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

    fn focus_left(&mut self) {
        self.focus = match self.focus {
            Focus::Machines => Focus::Machines,
            Focus::Agents if self.state.flatten => Focus::Agents,
            Focus::Agents => Focus::Machines,
            Focus::Recap => Focus::Agents,
        };
        if self.focus != Focus::Recap {
            self.interactive = false;
        }
    }

    fn focus_right(&mut self) {
        self.focus = match self.focus {
            Focus::Machines => Focus::Agents,
            Focus::Agents => Focus::Recap,
            Focus::Recap => Focus::Recap,
        };
        if self.focus == Focus::Recap {
            self.activate_terminal();
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
        self.interactive = false;
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
        if self.pending_capture.is_some() {
            self.history_loading = true;
            return;
        }
        let Some(target) = self.target(&session.target_id).cloned() else {
            return;
        };
        if self
            .worker
            .requests
            .send(Request::Capture {
                target,
                session_id: session.id.clone(),
                offset_from_bottom: self.history_offset,
                lines: self.config.history_chunk_lines.max(50),
                width: self.agent_viewport_width,
                height: self.agent_viewport_height,
            })
            .is_ok()
        {
            self.pending_capture = Some(session.id);
            self.history_loading = true;
        }
    }

    fn page_history(&mut self, older: bool) {
        if self.selected_session().is_none() {
            return;
        }
        let page = self.agent_viewport_height.saturating_sub(2).max(1) as usize;
        if older {
            let max_offset = self.history.total_lines().saturating_sub(1);
            self.history_offset = self.history_offset.saturating_add(page).min(max_offset);
        } else {
            self.history_offset = self.history_offset.saturating_sub(page);
        }
        self.request_history();
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
                self.config.agents.codex.command.clone(),
                serde_json::to_string(&self.config.agents.codex.args).unwrap_or_default(),
                self.config.agents.claude.command.clone(),
                serde_json::to_string(&self.config.agents.claude.args).unwrap_or_default(),
                self.config.agents.terminal.command.clone(),
                serde_json::to_string(&self.config.agents.terminal.args).unwrap_or_default(),
                serde_json::to_string(&self.config.attention_patterns).unwrap_or_default(),
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
                codex.command,
                serde_json::to_string(&codex.args).unwrap_or_default(),
                claude.command,
                serde_json::to_string(&claude.args).unwrap_or_default(),
                terminal.command,
                serde_json::to_string(&terminal.args).unwrap_or_default(),
                serde_json::to_string(self.config.attention_patterns_for(&target_id))
                    .unwrap_or_default(),
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
            selected: 0,
            loading: false,
            error: None,
        }));
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
                    form.submitted_query.clear();
                    form.results.clear();
                    form.selected = 0;
                    form.error = None;
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.query.clear();
                    form.submitted_query.clear();
                    form.results.clear();
                    form.selected = 0;
                    form.error = None;
                    self.modal = Some(Modal::Search(form));
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.query.push(character);
                    form.submitted_query.clear();
                    form.results.clear();
                    form.selected = 0;
                    form.error = None;
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
                KeyCode::Enter if form.selected == 0 => self.submit_launch(form.launch, None),
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
                    self.submit_launch(form.launch, resume_id);
                }
                _ => self.modal = Some(Modal::Resume(form)),
            },
            Modal::ConfirmKill { session_id, label } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.delete_session(&session_id),
                KeyCode::Esc | KeyCode::Char('n') => {}
                _ => self.modal = Some(Modal::ConfirmKill { session_id, label }),
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
                    config.agents.codex.command = form.values[5].clone();
                    config.agents.codex.args =
                        parse_json_array(&form.values[6], SETTING_LABELS[6])?;
                    config.agents.claude.command = form.values[7].clone();
                    config.agents.claude.args =
                        parse_json_array(&form.values[8], SETTING_LABELS[8])?;
                    config.agents.terminal.command = form.values[9].clone();
                    config.agents.terminal.args =
                        parse_json_array(&form.values[10], SETTING_LABELS[10])?;
                    config.attention_patterns =
                        parse_json_array(&form.values[11], SETTING_LABELS[11])?;
                }
                SettingsScope::Host(target_id) => {
                    let codex = CommandConfig {
                        command: form.values[0].clone(),
                        args: parse_json_array(&form.values[1], HOST_SETTING_LABELS[1])?,
                    };
                    let claude = CommandConfig {
                        command: form.values[2].clone(),
                        args: parse_json_array(&form.values[3], HOST_SETTING_LABELS[3])?,
                    };
                    let terminal = CommandConfig {
                        command: form.values[4].clone(),
                        args: parse_json_array(&form.values[5], HOST_SETTING_LABELS[5])?,
                    };
                    let attention_patterns =
                        parse_json_array(&form.values[6], HOST_SETTING_LABELS[6])?;
                    config.hosts.insert(
                        target_id.clone(),
                        HostConfig {
                            codex: Some(codex),
                            claude: Some(claude),
                            terminal: Some(terminal),
                            attention_patterns: Some(attention_patterns),
                        },
                    );
                }
            }
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
            .send(Request::Launch { request, command })
            .is_ok()
        {
            self.busy_operations += 1;
            self.status_message = "Launching agent...".into();
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
        if !self.pane_layout.recap.is_some_and(|area| row_in(area, row)) {
            return false;
        }
        if self
            .pane_layout
            .machine_divider_x
            .is_some_and(|x| column.abs_diff(x) <= 1)
        {
            self.dragging = Some(DragDivider::Machines);
            return true;
        }
        if self
            .pane_layout
            .agents_divider_x
            .is_some_and(|x| column.abs_diff(x) <= 1)
        {
            self.dragging = Some(DragDivider::Agents);
            return true;
        }
        false
    }

    fn drag_divider(&mut self, column: u16) {
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
                self.state.agents_width = column
                    .saturating_sub(area.x)
                    .saturating_add(1)
                    .saturating_sub(focused_bonus)
                    .clamp(24, 72);
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

    fn scroll_at(&mut self, column: u16, row: u16, up: bool) {
        if self
            .pane_layout
            .recap
            .is_some_and(|area| inside(area, column, row))
        {
            self.focus = Focus::Recap;
            self.activate_terminal();
            self.page_history(up);
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
    output
        .chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect()
}

fn first_meaningful_line(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("Pane is dead")
            || line.chars().all(|character| {
                character.is_whitespace()
                    || matches!(character, '─' | '│' | '┌' | '┐' | '└' | '┘' | '-' | '=')
            })
        {
            return None;
        }
        Some(line.split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

fn inside(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn row_in(area: Rect, row: u16) -> bool {
    row >= area.y && row < area.y.saturating_add(area.height)
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

fn parse_json_array(value: &str, label: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(value).map_err(|error| format!("Invalid {label}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runtime::Runtime, worker::Worker};

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
            needs_attention: false,
            attention_reason: None,
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
            machine_divider_x: Some(31),
            agents_divider_x: Some(71),
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
        form.values[9] = "/bin/zsh".into();
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
            needs_attention: false,
            attention_reason: None,
        });
        assert!(app.visible_sessions().is_empty());
        assert_eq!(app.archived_count(), 1);
        app.state.show_archived = true;
        assert_eq!(app.visible_sessions().len(), 1);
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
            needs_attention: false,
            attention_reason: None,
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
        form.values[0] = "/opt/codex".into();
        form.values[1] = "[\"--full-auto\"]".into();
        form.values[6] = "[\"gpu approval\"]".into();
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
