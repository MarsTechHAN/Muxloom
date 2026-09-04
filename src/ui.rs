use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::LazyLock,
};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntaxStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        App, BoardForm, BoardTab, ChannelChats, ChannelKeys, ChannelScan, ChannelStep,
        ChannelsForm, FileManagerForm, Focus, HELP_CONTENT_ROWS, HelpForm, KeysField, LaunchField,
        LaunchForm, MachineRow, Modal, ModeratorForm, ModeratorRow, PaneLayout, PathPickerForm,
        PortForwardForm, ResumeForm, ScanState, SearchForm, SettingsForm, SettingsRow,
        SettingsScope,
    },
    channel::ChannelKind,
    debug,
    model::{AgentKind, ConnectionState, FileEntryKind, FilePreviewKind, SearchMatchKind},
    port_forward::PortForwardState,
    runtime::is_temporary_session_id,
    talk::{TalkKind, TalkMessage, TalkScope, civil_utc, clock_utc, folded},
};

const ACCENT: Color = Color::Rgb(112, 184, 255);
const CODEX: Color = Color::Cyan;
const CLAUDE: Color = Color::Rgb(215, 119, 87);
const OPENCODE: Color = Color::Rgb(168, 148, 255);
const PI: Color = Color::Rgb(240, 196, 96);
const TERMINAL: Color = Color::Green;
const MUTED: Color = Color::DarkGray;
/// The stripe behind a folder row, so the folders read as separate blocks.
const GROUP_BAND: Color = Color::Rgb(42, 48, 58);
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static SYNTAX_THEME: LazyLock<Theme> = LazyLock::new(|| {
    let themes = ThemeSet::load_defaults();
    themes
        .themes
        .get("base16-eighties.dark")
        .or_else(|| themes.themes.get("base16-ocean.dark"))
        .or_else(|| themes.themes.values().next())
        .expect("syntect ships at least one default theme")
        .clone()
});

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let reported_size = crossterm::terminal::window_size().ok();
    let pixels = reported_size
        .as_ref()
        .filter(|size| size.columns == area.width && size.rows == area.height)
        .filter(|size| size.width > 0 && size.height > 0)
        .map(|size| (size.width, size.height));
    let portrait = portrait_layout(area, pixels);
    let was_compact = app
        .layout_debug_signature
        .is_some_and(|(_, _, _, _, _, compact)| compact);
    let compact = compact_layout(was_compact, area, portrait);
    let (pixel_width, pixel_height) = pixels.unwrap_or_default();
    let signature = (
        area.width,
        area.height,
        pixel_width,
        pixel_height,
        portrait,
        compact,
    );
    if app.layout_debug_signature != Some(signature) {
        debug::log(
            "layout",
            format!(
                "cells={}x{} pixels={}x{} portrait={} compact={}",
                area.width, area.height, pixel_width, pixel_height, portrait, compact
            ),
        );
        app.layout_debug_signature = Some(signature);
    }
    // The solver honours `Min` before a trailing `Length`, so a content minimum
    // of 5 used to eat the whole footer below nine rows -- and the footer is the
    // only place the keys are written down. Give the content one row and shrink
    // the header instead: the counts matter more than the tagline at this size.
    let header_height = match area.height {
        height if height < 9 => 1,
        height if height < 12 => 2,
        _ => 3,
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, app, vertical[0]);
    draw_content(frame, app, vertical[1], portrait, compact);
    draw_footer(frame, app, vertical[2]);

    // Which runtimes the machine offers is the app's knowledge, not the
    // form's, and the modal is drawn from a mutable borrow of it.
    let kinds = match app.modal.as_ref() {
        Some(Modal::Launch(form)) => app.offered_kinds(&form.target.id),
        Some(Modal::Temporal(form)) => app.offered_agent_kinds(&form.target.id),
        Some(Modal::Moderator(_)) => app.moderator_kinds(),
        _ => Vec::new(),
    };
    // The board is drawn from the app's own copy of what every machine has been
    // saying, so it steps out of the modal slot for the length of the frame —
    // the renderer cannot borrow the app and the form at once — and back in
    // after. Every other modal carries everything it needs.
    match app.modal.take() {
        Some(Modal::Board(mut form)) => {
            draw_board_modal(frame, app, &mut form, area);
            app.modal = Some(Modal::Board(form));
        }
        Some(mut modal) => {
            draw_modal(frame, &mut modal, area, &kinds);
            app.modal = Some(modal);
        }
        None => {}
    }
}

/// Whether the window has to fall back to showing one pane at a time.
fn compact_layout(was_compact: bool, area: Rect, portrait: bool) -> bool {
    // Leaving the compact layout takes a few more cells than falling into it,
    // so dragging a window across the threshold does not flip the whole screen
    // back and forth a cell at a time.
    let slack = if was_compact { 4 } else { 0 };
    if portrait {
        area.width < 48 + slack || area.height < 28 + slack
    } else {
        // Height on its own never forces it. A 200x15 window has room for all
        // three columns; stacking them would throw that width away and leave
        // the machine list reachable only by hiding the terminal.
        area.width < 72 + slack
    }
}

fn portrait_layout(area: Rect, pixels: Option<(u16, u16)>) -> bool {
    pixels
        .map(|(width, height)| width < height)
        .unwrap_or_else(|| area.height.saturating_mul(2) > area.width)
}

fn draw_header(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let online = app
        .targets
        .iter()
        .filter(|target| target.state == ConnectionState::Online)
        .count();
    let enabled = app.targets.iter().filter(|target| target.enabled).count();
    let running = app.sessions.iter().filter(|session| !session.dead).count();
    let waiting = app
        .sessions
        .iter()
        .filter(|session| !session.dead && session.needs_attention)
        .count();
    let archived = app
        .sessions
        .iter()
        .filter(|session| {
            session.dead
                && session.kind != AgentKind::Terminal
                && !is_temporary_session_id(&session.id)
        })
        .count();
    let launch_target = app
        .targets
        .get(app.selected_target)
        .map(|target| target.target.label.as_str())
        .unwrap_or("none");
    let mut first_spans = vec![
        Span::styled(
            " MUXLOOM ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(MUTED),
        ),
    ];
    // The tagline is the first thing a narrow terminal can do without; the
    // version and the staged-update notice next to it are not.
    if area.width >= 72 {
        first_spans.push(Span::raw("  "));
        first_spans.push(Span::styled(
            "persistent multi-machine agent sessions",
            Style::default().fg(Color::Gray),
        ));
    }
    if let Some(version) = &app.staged_update {
        first_spans.push(Span::raw("  "));
        first_spans.push(Span::styled(
            format!("↑ v{version} ready — restart"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(version) = &app.available_update {
        first_spans.push(Span::raw("  "));
        first_spans.push(Span::styled(
            format!("↑ v{version} available — muxloom update"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let first = Line::from(first_spans);
    let second = Line::from(vec![
        Span::styled(
            format!(" {online}/{enabled} machines online"),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{running} running"),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{waiting} waiting"),
            Style::default()
                .fg(if waiting > 0 { Color::Yellow } else { MUTED })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(format!("{archived} archived"), Style::default().fg(MUTED)),
        Span::raw("  "),
        Span::styled(
            format!("launch: {launch_target}"),
            Style::default().fg(ACCENT),
        ),
    ]);
    let attention: Vec<_> = app
        .attention_sessions()
        .into_iter()
        .map(|session| {
            (
                session.id.clone(),
                session.target_id.clone(),
                session.display_label().to_string(),
            )
        })
        .collect();
    app.attention_ids = attention.iter().map(|(id, _, _)| id.clone()).collect();
    let third = if let Some((_, target, label)) = attention.first() {
        Line::from(vec![
            Span::styled(
                format!(" INPUT REQUIRED {} ", attention.len()),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {target} / {label}  click or press Up twice at top"),
                Style::default().fg(Color::Yellow),
            ),
        ])
    } else {
        Line::raw("")
    };
    // A squeezed header drops the tagline first and keeps the alert last, and
    // the banner is only clickable on the row it was actually drawn on.
    let (lines, banner_row) = match (area.height, attention.is_empty()) {
        (0, _) => (Vec::new(), None),
        (1, false) => (vec![third], Some(0)),
        (1, true) => (vec![second], None),
        (2, false) => (vec![second, third], Some(1)),
        (2, true) => (vec![first, second], None),
        (_, empty) => (vec![first, second, third], (!empty).then_some(2)),
    };
    app.attention_banner =
        banner_row.map(|offset| Rect::new(area.x, area.y + offset, area.width, 1));
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_content(frame: &mut Frame<'_>, app: &mut App, area: Rect, portrait: bool, compact: bool) {
    let panes = compute_layout(app, area, portrait, compact);
    app.pane_layout = panes.clone();
    app.terminal_back = None;
    if let Some(machine_area) = panes.machines {
        draw_machines(frame, app, machine_area);
    } else {
        app.machine_rows.clear();
    }
    if let Some(agent_area) = panes.agents {
        draw_agents(frame, app, agent_area);
    } else {
        app.agent_rows.clear();
    }
    if let Some(recap_area) = panes.recap {
        draw_terminal_panel(frame, app, recap_area);
    }
    draw_divider_handles(frame, &panes);
}

fn compute_layout(app: &App, area: Rect, portrait: bool, compact: bool) -> PaneLayout {
    if app.file_manager_fills_the_terminal_pane() {
        let maximum = area.width.saturating_sub(24).max(12);
        let file_width = app.state.file_width.clamp(12, maximum);
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(file_width), Constraint::Min(24)])
            .split(area);
        return PaneLayout {
            agents: Some(split[0]),
            recap: Some(split[1]),
            agents_divider: Some(vertical_divider(area, split[0])),
            ..PaneLayout::default()
        };
    }
    if compact {
        return match app.focus {
            Focus::Machines if !app.state.flatten => PaneLayout {
                machines: Some(area),
                ..PaneLayout::default()
            },
            Focus::Recap => PaneLayout {
                recap: Some(area),
                ..PaneLayout::default()
            },
            _ => {
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Percentage(app.state.portrait_terminal_percent.clamp(45, 82)),
                        Constraint::Percentage(
                            100 - app.state.portrait_terminal_percent.clamp(45, 82),
                        ),
                    ])
                    .split(area);
                PaneLayout {
                    recap: Some(split[0]),
                    agents: Some(split[1]),
                    portrait_terminal_divider: Some(horizontal_divider(area, split[0])),
                    ..PaneLayout::default()
                }
            }
        };
    }

    if portrait {
        let terminal_percent = app.state.portrait_terminal_percent.clamp(45, 82);
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(terminal_percent),
                Constraint::Percentage(100 - terminal_percent),
            ])
            .split(area);
        if app.state.flatten {
            return PaneLayout {
                recap: Some(vertical[0]),
                agents: Some(vertical[1]),
                portrait_terminal_divider: Some(horizontal_divider(area, vertical[0])),
                ..PaneLayout::default()
            };
        }
        let machine_percent = app.state.portrait_machine_percent.clamp(25, 75);
        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(machine_percent),
                Constraint::Percentage(100 - machine_percent),
            ])
            .split(vertical[1]);
        return PaneLayout {
            recap: Some(vertical[0]),
            machines: Some(bottom[0]),
            agents: Some(bottom[1]),
            portrait_machine_divider: Some(vertical_divider(vertical[1], bottom[0])),
            portrait_terminal_divider: Some(horizontal_divider(area, vertical[0])),
            ..PaneLayout::default()
        };
    }

    if app.state.flatten {
        let base_width = if app.file_manager.is_some() {
            app.state.file_width.clamp(22, 72)
        } else {
            app.state.agents_width.clamp(24, 72)
        };
        let agents_width = base_width.min(area.width.saturating_sub(28));
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(agents_width), Constraint::Min(28)])
            .split(area);
        return PaneLayout {
            agents: Some(split[0]),
            recap: Some(split[1]),
            agents_divider: Some(vertical_divider(area, split[0])),
            ..PaneLayout::default()
        };
    }

    // No focus bump on any of these widths: a focused pane is already marked by
    // its border and highlight, and widening it moved both dividers and sent a
    // SIGWINCH to the attached PTY every time the user switched panes.
    let mut machine_width = app.state.machine_width.clamp(16, 52);
    let mut agents_width = if app.file_manager.is_some() {
        app.state.file_width.clamp(22, 72)
    } else {
        app.state.agents_width.clamp(24, 72)
    };
    let available = area.width.saturating_sub(28);
    while machine_width + agents_width > available && agents_width > 24 {
        agents_width -= 1;
    }
    while machine_width + agents_width > available && machine_width > 16 {
        machine_width -= 1;
    }
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(machine_width),
            Constraint::Length(agents_width),
            Constraint::Min(28),
        ])
        .split(area);
    PaneLayout {
        machines: Some(split[0]),
        agents: Some(split[1]),
        recap: Some(split[2]),
        machine_divider: Some(vertical_divider(area, split[0])),
        agents_divider: Some(vertical_divider(area, split[1])),
        ..PaneLayout::default()
    }
}

fn vertical_divider(area: Rect, left: Rect) -> Rect {
    Rect::new(
        left.x.saturating_add(left.width.saturating_sub(1)),
        area.y,
        1,
        area.height,
    )
}

fn horizontal_divider(area: Rect, top: Rect) -> Rect {
    Rect::new(
        area.x,
        top.y.saturating_add(top.height.saturating_sub(1)),
        area.width,
        1,
    )
}

/// How long a drag handle is: a share of the divider it sits on, kept between
/// a visible minimum and a length that still reads as a handle rather than a
/// second border. Never longer than the divider itself.
fn grip(span: u16, share: u16, shortest: u16, longest: u16) -> u16 {
    (span / share).clamp(shortest, longest).min(span)
}

/// The grab handles in the middle of each divider. One cell was easy to miss
/// on a line that already looks like a border — a horizontal divider spans the
/// window, so a lone dash in the middle of it disappeared entirely — so each
/// handle is a short heavy run in the accent colour.
fn draw_divider_handles(frame: &mut Frame<'_>, panes: &PaneLayout) {
    let style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    for divider in [
        panes.machine_divider,
        panes.agents_divider,
        panes.portrait_machine_divider,
    ]
    .into_iter()
    .flatten()
    {
        let height = grip(divider.height, 6, 3, 7);
        let y = divider
            .y
            .saturating_add(divider.height.saturating_sub(height) / 2);
        frame.render_widget(
            Paragraph::new(vec!["┃"; height as usize].join("\n")).style(style),
            Rect::new(divider.x, y, 1, height),
        );
    }
    if let Some(divider) = panes.portrait_terminal_divider {
        let width = grip(divider.width, 6, 9, 21);
        let x = divider
            .x
            .saturating_add(divider.width.saturating_sub(width) / 2);
        frame.render_widget(
            Paragraph::new("━".repeat(width as usize)).style(style),
            Rect::new(x, divider.y, width, 1),
        );
    }
}

fn draw_machines(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.visible_target_indices();
    let name_width = area.width.saturating_sub(10).max(1) as usize;
    let mut items = Vec::new();
    let mut rows = Vec::new();
    {
        // The moderators row sits above the machines because the agents behind
        // it are not on any one machine: they coordinate across all of them.
        let live = app
            .sessions
            .iter()
            .filter(|session| !session.dead && app.is_moderator_session(session))
            .count();
        let busy = app
            .sessions
            .iter()
            .filter(|session| !session.dead && session.working && app.is_moderator_session(session))
            .count();
        let mut lines = vec![Line::from(vec![
            Span::styled(
                "* ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            // Four spaces where a machine draws its `[x]`: this row has
            // nothing to enable, and the names still line up.
            Span::raw("    "),
            Span::styled("Moderators", Style::default().add_modifier(Modifier::BOLD)),
        ])];
        lines.push(match live {
            0 => Line::styled("    none yet - n to start", Style::default().fg(MUTED)),
            _ if busy > 0 => Line::styled(
                format!("    {live} running, {busy} working"),
                Style::default().fg(Color::Cyan),
            ),
            _ => Line::styled(format!("    {live} running"), Style::default().fg(MUTED)),
        });
        rows.push((MachineRow::Moderators, lines.len() as u16));
        items.push(ListItem::new(lines));
    }
    for target_index in &visible {
        let status = &app.targets[*target_index];
        // One capability glyph per agent runtime the machine has, plus any
        // runtime that is busy right now even though the probe missed it.
        let working = |kind: AgentKind| {
            app.sessions.iter().any(|session| {
                session.target_id == status.target.id
                    && session.kind == kind
                    && session.working
                    && !session.dead
            })
        };
        let mut runtimes: Vec<_> = AgentKind::agents()
            .map(|kind| (kind, status.probe.has(kind), working(kind)))
            .filter(|(_, installed, busy)| *installed || *busy)
            .collect();
        // A machine with nothing installed still says so, with the two
        // runtimes muxloom can hand it itself greyed out.
        if runtimes.is_empty() {
            runtimes = vec![
                (AgentKind::Codex, false, false),
                (AgentKind::Claude, false, false),
            ];
        }
        let (marker, marker_color) = match status.state {
            ConnectionState::Disabled => (" ", MUTED),
            ConnectionState::Scanning => ("~", Color::Yellow),
            ConnectionState::Online => ("+", Color::Green),
            ConnectionState::Offline => ("!", Color::Red),
        };
        let enabled = if status.enabled { "x" } else { " " };
        let name_lines = wrap_display(&status.target.label, name_width);
        let mut lines = Vec::with_capacity(name_lines.len() + 1);
        let first_name = name_lines.first().map(String::as_str).unwrap_or("");
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} "),
                Style::default()
                    .fg(marker_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("[{enabled}] ")),
            Span::styled(
                first_name.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        for continuation in name_lines.iter().skip(1) {
            lines.push(Line::styled(
                format!("      {continuation}"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        let detail = if let Some(error) = &status.error {
            Line::styled(
                format!(
                    "    {}",
                    truncate(error, area.width.saturating_sub(8) as usize)
                ),
                Style::default().fg(Color::Red),
            )
        } else if status.enabled {
            let mut spans = vec![Span::raw("    ")];
            for (index, (kind, installed, busy)) in runtimes.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(runtime_capability(
                    *kind,
                    *installed,
                    *busy,
                    app.animation_frame,
                ));
            }
            // A lagging daemon gets a marker, not a version: the exact
            // versions live in the machine's settings panel, next to the
            // action that updates them.
            if app.daemon_lag_version(&status.target.id).is_some() {
                spans.push(Span::styled("  ⟳", Style::default().fg(Color::Yellow)));
            }
            Line::from(spans)
        } else {
            Line::styled("    disabled", Style::default().fg(MUTED))
        };
        lines.push(detail);
        rows.push((MachineRow::Machine(*target_index), lines.len() as u16));
        items.push(ListItem::new(lines));
    }
    // Machines this dashboard only knows about: some other controller told a
    // daemon here that it can reach them. Selecting one lists its agents and
    // shows their screens, fetched by leaving errands on the daemon that named
    // it — looking, which is all a relay carries. There is still nothing to
    // enable and no session to start: the route belongs to whoever is named on
    // the row, and starting or typing into something over there is that
    // machine's own agents' to do.
    for (index, forwarded) in app.forwarded.iter().enumerate() {
        let peer = &forwarded.peer;
        let name_lines = wrap_display(peer.display(), name_width);
        let first_name = name_lines.first().map(String::as_str).unwrap_or("");
        let mut lines = vec![Line::from(vec![
            Span::styled("» ", Style::default().fg(Color::Cyan)),
            // Four spaces where a machine draws its `[x]`, so the names line
            // up with the machines this dashboard does reach.
            Span::raw("    "),
            Span::styled(first_name.to_string(), Style::default().fg(MUTED)),
        ])];
        for continuation in name_lines.iter().skip(1) {
            lines.push(Line::styled(
                format!("      {continuation}"),
                Style::default().fg(MUTED),
            ));
        }
        let looking = app.forwarded_pending.as_deref() == Some(peer.id.as_str());
        lines.push(Line::styled(
            if looking {
                "    looking...".to_string()
            } else if peer.via.is_empty() {
                "    via another muxloom".to_string()
            } else {
                format!("    via {}", truncate(&peer.via, name_width))
            },
            Style::default().fg(MUTED),
        ));
        rows.push((MachineRow::Forwarded(index), lines.len() as u16));
        items.push(ListItem::new(lines));
    }
    // Trails the rows it explains, and carries no entry in `rows`, so a click
    // on it lands on nothing rather than on the last machine.
    if visible.is_empty() {
        items.push(ListItem::new(Line::styled(
            "No enabled machines. Press v to show all.",
            Style::default().fg(MUTED),
        )));
    }
    let selected = rows
        .iter()
        .position(|(row, _)| *row == app.selected_machine_row());
    app.machine_rows = rows;
    app.machine_list_state.select(selected);
    let title = if app.state.hide_disabled {
        " Machines - enabled "
    } else {
        " Machines "
    };
    let list = List::new(items)
        .block(panel(title, app.focus == Focus::Machines))
        .highlight_style(list_highlight_style(app.focus == Focus::Machines, false))
        .highlight_symbol(if app.focus == Focus::Machines {
            "> "
        } else {
            "  "
        });
    frame.render_stateful_widget(list, area, &mut app.machine_list_state);
}

fn draw_agents(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if let Some(form) = app.file_manager.as_mut() {
        app.agent_rows.clear();
        app.archive_row = None;
        draw_file_browser(frame, form, area, app.focus == Focus::Agents);
        return;
    }
    let sessions: Vec<_> = app
        .visible_session_rows()
        .into_iter()
        .map(|(session, shape)| (session.clone(), shape))
        .collect();
    // Which rows are the last subagent of the task they sit in, so the tree
    // draws an elbow there and a tee everywhere else. A row is not the last one
    // while another at the same depth follows it before the list climbs back
    // out to the parent.
    let last_child: Vec<bool> = (0..sessions.len())
        .map(|index| {
            let depth = sessions[index].1.depth;
            !sessions[index + 1..]
                .iter()
                .take_while(|(_, shape)| shape.depth >= depth)
                .any(|(_, shape)| shape.depth == depth)
        })
        .collect();
    // Group rows carry their children's state as a steady row colour:
    // attention outranks working. Animation stays on the agent rows alone.
    let mut state_by_group = HashMap::<String, (bool, bool)>::new();
    for (session, _) in &sessions {
        if session.dead || !(session.working || session.needs_attention) {
            continue;
        }
        let folder = if is_temporary_session_id(&session.id) {
            "Temporal Chat"
        } else {
            &session.path
        };
        let group = if app.state.flatten {
            format!("{}  {folder}", session.target_id)
        } else {
            folder.to_string()
        };
        let state = state_by_group.entry(group).or_default();
        if session.needs_attention {
            state.0 = true;
        }
        if session.working {
            state.1 = true;
        }
    }
    let archived_count = app.archived_count();
    let mut items = Vec::new();
    let mut row_ids = Vec::new();
    // Which folder band each row sits under, so a row left at the top of the
    // pane can still be told which one it fell out of. Empty for the rows that
    // belong to no folder — the bands themselves, the watches, the archive.
    let mut row_groups: Vec<String> = Vec::new();
    let mut selected_row = None;
    let mut previous_group = String::new();
    let mut archive_header_added = false;
    app.archive_row = None;

    // File watches are pseudo-sessions: pinned above every real agent so a
    // watched log is always one step away, in its own steady colour.
    for watch in &app.file_watches {
        let row = items.len();
        let selected = app.selected_session_id.as_deref() == Some(watch.id.as_str());
        if selected {
            selected_row = Some(row);
        }
        let status = if watch.loading { "..." } else { "live" };
        items.push(
            ListItem::new(Line::from(vec![
                Span::styled("📄 ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    truncate(&watch.label, area.width.saturating_sub(12) as usize),
                    Style::default().fg(Color::Cyan).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(format!("  {status}"), Style::default().fg(MUTED)),
            ]))
            .style(Style::default().fg(Color::Cyan)),
        );
        row_ids.push((Some(watch.id.clone()), 1));
        row_groups.push(String::new());
    }

    for (index, (session, shape)) in sessions.iter().enumerate() {
        let session = session.clone();
        let shape = *shape;
        if session.dead && !archive_header_added {
            app.archive_row = Some(items.len());
            items.push(archive_item(archived_count, true));
            row_ids.push((None, 1));
            row_groups.push(String::new());
            previous_group.clear();
            archive_header_added = true;
        }
        let folder = if is_temporary_session_id(&session.id) {
            "Temporal Chat"
        } else {
            &session.path
        };
        let group = if app.state.flatten {
            format!("{}  {folder}", session.target_id)
        } else {
            folder.to_string()
        };
        // A subagent belongs to the task it was started from, not to its own
        // folder, so it stays inside the block its parent opened even when it
        // runs somewhere else. Its folder is on its own row when it is selected.
        if shape.depth == 0 && group != previous_group {
            let (attention, working) = state_by_group.get(&group).copied().unwrap_or_default();
            let colour = if attention {
                Color::Yellow
            } else if working {
                Color::Green
            } else {
                Color::Gray
            };
            let spans = vec![Span::styled(
                truncate(&group, area.width.saturating_sub(4) as usize),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            )];
            // A band across the whole row is what separates one folder's agents
            // from the next; the rows between them carry no background.
            items.push(ListItem::new(Line::from(spans)).style(Style::default().bg(GROUP_BAND)));
            row_ids.push((None, 1));
            row_groups.push(String::new());
            previous_group = group;
        }

        let row = items.len();
        if app.selected_session_id.as_deref() == Some(&session.id) {
            selected_row = Some(row);
        }
        let (icon, runtime_name, _) = agent_visual(session.kind);
        let recoverable = session.dead && app.is_recoverable(&session.target_id, &session.id);
        let state = if app.is_restoring(&session.target_id, &session.id) {
            "restoring to machine..."
        } else if recoverable && app.is_restorable(&session.target_id, &session.id) {
            "local backup only - Enter to restore"
        } else if recoverable {
            "local backup only - read-only"
        } else if session.dead {
            "archived - Enter to resume"
        } else if session.needs_attention {
            "waiting for input"
        } else if session.working {
            "working"
        } else {
            "idle"
        };
        let state_color = if session.dead {
            MUTED
        } else if session.needs_attention {
            Color::Yellow
        } else if session.working {
            Color::Green
        } else {
            Color::Gray
        };
        let selected = app.selected_session_id.as_deref() == Some(&session.id);
        let attention_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let agent_style = |normal| {
            if session.needs_attention {
                attention_style
            } else {
                normal
            }
        };
        let activity = if session.needs_attention {
            "!"
        } else if session.working && !session.dead {
            running_agent_effect(session.kind, app.animation_frame)
        } else {
            icon
        };
        // The elbow says where the session came from: an agent started it, and
        // it is that agent's work rather than another entry in the folder.
        let branch = if shape.depth == 0 {
            String::new()
        } else {
            format!(
                "{}{} ",
                "  ".repeat(shape.depth - 1),
                if last_child[index] { '└' } else { '├' }
            )
        };
        // What a fold is hiding, on the row that hides it: how many sessions,
        // and whether any of them is waiting for an answer. A fold that could
        // swallow a prompt without a word would not be worth having.
        let count = (shape.descendants > 0).then(|| {
            let mark = if shape.folded { '+' } else { '-' };
            let mut text = format!("  [{mark}] {}", shape.descendants);
            if shape.attention {
                text.push_str(" !");
            }
            text
        });
        let count_width = count.as_deref().map(str::len).unwrap_or_default();
        let mut label = vec![
            Span::styled(branch.clone(), Style::default().fg(MUTED)),
            Span::styled(
                activity,
                agent_style(
                    Style::default()
                        .fg(if session.needs_attention {
                            Color::Yellow
                        } else {
                            agent_visual(session.kind).2
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(
                    session.display_label(),
                    (area.width as usize).saturating_sub(10 + branch.chars().count() + count_width),
                ),
                agent_style(Style::default().fg(Color::White)),
            ),
        ];
        if let Some(count) = count {
            label.push(Span::styled(
                count,
                if shape.attention {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if shape.working {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(MUTED)
                },
            ));
        }
        let mut lines = vec![Line::from(label)];
        if selected {
            let value_width = area.width.saturating_sub(14) as usize;
            lines.push(Line::from(vec![
                Span::styled("    folder  ", agent_style(Style::default().fg(MUTED))),
                Span::styled(
                    truncate(folder, value_width),
                    agent_style(Style::default().fg(Color::Gray)),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    status  ", agent_style(Style::default().fg(MUTED))),
                Span::styled(
                    format!("{icon} {runtime_name}  {state}"),
                    agent_style(Style::default().fg(state_color)),
                ),
            ]));
            if shape.descendants > 0 {
                lines.push(Line::from(vec![
                    Span::styled("    task    ", agent_style(Style::default().fg(MUTED))),
                    Span::styled(
                        format!(
                            "{} subagent{}  space {}",
                            shape.descendants,
                            if shape.descendants == 1 { "" } else { "s" },
                            if shape.folded { "lists" } else { "folds" }
                        ),
                        agent_style(Style::default().fg(Color::Gray)),
                    ),
                ]));
            }
        }
        let height = lines.len() as u16;
        items.push(ListItem::new(lines).style(if session.needs_attention {
            attention_style
        } else {
            Style::default()
        }));
        row_ids.push((Some(session.id), height));
        // A subagent keeps the band its parent opened even when it runs
        // somewhere else, so the band in effect is the answer, not the path.
        row_groups.push(previous_group.clone());
    }
    if archived_count > 0 && !app.state.show_archived {
        app.archive_row = Some(items.len());
        items.push(archive_item(archived_count, false));
        row_ids.push((None, 1));
        row_groups.push(String::new());
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "No sessions. Press n to launch or t for a Temporal Chat.",
            Style::default().fg(MUTED),
        )));
        row_ids.push((None, 1));
        row_groups.push(String::new());
    }

    app.agent_rows = row_ids;
    app.agent_list_state.select(selected_row);
    let title = if app.state.flatten {
        " Agents - all machines "
    } else if app.showing_moderators() {
        " Moderators "
    } else {
        " Agents by folder "
    };
    let list = List::new(items)
        .block(panel(title, app.focus == Focus::Agents))
        .highlight_style(list_highlight_style(app.focus == Focus::Agents, true))
        .highlight_symbol(if app.focus == Focus::Agents {
            "> "
        } else {
            "  "
        });
    frame.render_stateful_widget(list, area, &mut app.agent_list_state);

    // A folder band scrolls away with the rows above it, and the agent left at
    // the top of the pane is then in no folder at all — the one row on screen
    // that cannot say where it is. Repeat the band it fell out of along the
    // pane's own edge, which costs no row and reads in the same colour.
    let offset = app.agent_list_state.offset();
    let scrolled_out_of = row_groups
        .get(offset)
        .filter(|group| !group.is_empty())
        .filter(|_| {
            offset > 0
                && app
                    .agent_rows
                    .get(offset)
                    .is_some_and(|(id, _)| id.is_some())
        });
    if let Some(group) = scrolled_out_of {
        let room = (area.width as usize).saturating_sub(UnicodeWidthStr::width(title) + 5);
        if room >= 4 {
            let (attention, working) = state_by_group.get(group).copied().unwrap_or_default();
            let text = format!(" {} ", truncate(group, room));
            let width = UnicodeWidthStr::width(text.as_str()) as u16;
            frame.buffer_mut().set_string(
                area.x + area.width.saturating_sub(width + 1),
                area.y,
                text,
                Style::default()
                    .fg(if attention {
                        Color::Yellow
                    } else if working {
                        Color::Green
                    } else {
                        Color::Gray
                    })
                    .add_modifier(Modifier::BOLD),
            );
        }
    }
}

fn archive_item(count: usize, expanded: bool) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(
            if expanded { "[-]" } else { "[+]" },
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format!(" Archived ({count})"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if expanded {
                "  a collapse"
            } else {
                "  a expand"
            },
            Style::default().fg(MUTED),
        ),
    ]))
}

fn draw_terminal_panel(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if app
        .file_manager
        .as_ref()
        .is_some_and(|form| form.preview_path.is_some())
    {
        app.terminal_back = None;
        draw_file_preview_panel(frame, app, area);
        return;
    }
    // A file watch is selected: the recap pane shows the watched file, live.
    if app.selected_file_watch().is_some() {
        app.terminal_back = None;
        draw_file_watch_panel(frame, app, area);
        return;
    }
    // A machine only another controller reaches has no terminal to attach to:
    // the relay carries one question and one answer, never a stream. What it
    // shows is the last screen that came back, refetched while it is watched.
    if app.forwarded_in_view().is_some() {
        app.terminal_back = None;
        draw_forwarded_screen(frame, app, area);
        return;
    }
    // Take the emulator's own scrollback position before anything reads
    // `history_offset`, so the title and the rows agree on where the view sits.
    app.sync_terminal_scrollback();
    let selected = app.selected_session().cloned();
    let current_matches = app.terminal_session_id.as_deref() == app.selected_session_id.as_deref();
    let pending_matches = app.pending_terminal_session_id.as_deref()
        == app.selected_session_id.as_deref()
        || app.attach_in_flight_for_selected();
    let loading = if app.history_loading {
        " [loading]"
    } else {
        ""
    };
    let title = if pending_matches && !current_matches && app.history_offset == 0 {
        " Switching terminal - keeping previous frame ".into()
    } else if current_matches && app.terminal.is_some() && app.history_offset == 0 {
        format!(
            " Attached terminal [{}{}] ",
            if app.interactive {
                "INPUT"
            } else {
                "CONNECTED"
            },
            if app.terminal_scroll_lock {
                " wheel:history"
            } else {
                ""
            }
        )
    } else if app.history_offset > 0 {
        format!(
            " Terminal history - {} lines from bottom{loading} ",
            app.history_offset,
        )
    } else if let Some(session) = &selected {
        if app.is_restoring(&session.target_id, &session.id) {
            format!(
                " {} / {} / restoring history to the machine... ",
                session.kind, session.target_id
            )
        } else if app.is_recoverable(&session.target_id, &session.id) {
            format!(
                " {} / {} / local backup only - {} ",
                session.kind,
                session.target_id,
                if app.is_restorable(&session.target_id, &session.id) {
                    "Enter restores it to the machine"
                } else {
                    "terminal output only"
                }
            )
        } else if session.dead {
            format!(
                " {} / {} / archived - Enter to resume{loading} ",
                session.kind, session.target_id
            )
        } else {
            format!(
                " {} / {} / running{loading} ",
                session.kind, session.target_id
            )
        }
    } else {
        " Agent terminal ".into()
    };
    let show_back = app.focus == Focus::Recap;
    app.terminal_back = show_back.then(|| Rect::new(area.x + 1, area.y, 8.min(area.width), 1));
    let mut title_spans = Vec::new();
    if show_back {
        title_spans.push(Span::styled(
            " ← Back ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
        title_spans.push(Span::raw(" "));
    }
    title_spans.push(Span::raw(title.trim().to_string()));
    title_spans.push(Span::raw(" "));
    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if show_back { ACCENT } else { Color::DarkGray }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.resize_agent_viewport(inner.width, inner.height);

    if app.attached_history_is_buffered() {
        // Recent history comes from the emulator so terminal rows stay exact.
        // Older rows fall through to the daemon's append-only history pages.
        let offset = app.history_offset;
        let show_cursor = app.interactive && offset == 0;
        let selection = app.terminal_selection;
        if let Some(terminal) = app.terminal.as_mut() {
            render_vt_screen(frame, terminal.screen(), inner, show_cursor);
        }
        highlight_terminal_selection(frame, inner, selection);
        return;
    }
    if (app.history_offset == 0 || (app.history_loading && app.history.text.is_empty()))
        && let Some(terminal) = app.terminal.as_ref()
    {
        render_vt_screen(frame, terminal.screen(), inner, app.interactive);
        highlight_terminal_selection(frame, inner, app.terminal_selection);
        return;
    }

    let body = if !app.history_message.is_empty() {
        app.history_message.clone()
    } else {
        app.history.text.clone()
    };
    let history = ansi_history_text(&body);
    let line_count = history.height().min(u16::MAX as usize) as u16;
    let scroll = line_count.saturating_sub(inner.height);
    let paragraph = Paragraph::new(history).scroll((scroll, 0));
    frame.render_widget(paragraph, inner);
    highlight_terminal_selection(frame, inner, app.terminal_selection);
}

/// The pane for a machine this muxloom has no route to, borrowed from the
/// controller that has one.
///
/// It says so in the title, because the difference matters to whoever is
/// reading: this is a photograph taken a moment ago and taken again every few
/// seconds, not a terminal. Nothing typed here goes anywhere. Work on that
/// machine is the job of the agents living on it — message one.
fn draw_forwarded_screen(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(peer) = app.forwarded_in_view() else {
        return;
    };
    let machine = peer.peer.display().to_string();
    let via = peer.peer.via.clone();
    let selected = app.selected_session().cloned();
    let title = match &selected {
        Some(session) => format!(" {machine} / {} / snapshot via {via} ", session.kind),
        None => format!(" {machine} - reached through {via} "),
    };
    let block = Block::default()
        .title(Line::from(vec![
            Span::raw(title.trim().to_string()),
            Span::raw(" "),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.focus == Focus::Recap {
            ACCENT
        } else {
            Color::DarkGray
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.resize_agent_viewport(inner.width, inner.height);

    let waiting = |what: &str| {
        Text::from(vec![Line::from(Span::styled(
            what.to_string(),
            Style::default().fg(MUTED),
        ))])
    };
    let body = match &selected {
        Some(session) => match &app.forwarded_screen {
            Some((id, screen)) if id == &session.id => ansi_history_text(screen),
            _ => waiting("Asking another muxloom for this screen..."),
        },
        None => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("{machine} is not a machine this muxloom can reach."),
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    format!("Another controller, on {via}, is looking at it for you."),
                    Style::default().fg(MUTED),
                )),
                Line::default(),
                Line::from(Span::styled(
                    "Pick one of its agents to see what is on its screen. There is no",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    "attaching and no typing over a relay: to get something done there,",
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    "message one of its agents.",
                    Style::default().fg(MUTED),
                )),
            ];
            if let Some(error) = &app.forwarded_error {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    error.clone(),
                    Style::default().fg(Color::Red),
                )));
            }
            Text::from(lines)
        }
    };
    let height = body.height().min(u16::MAX as usize) as u16;
    let scroll = if selected.is_some() {
        height.saturating_sub(inner.height)
    } else {
        0
    };
    frame.render_widget(Paragraph::new(body).scroll((scroll, 0)), inner);
}

fn highlight_terminal_selection(
    frame: &mut Frame<'_>,
    area: Rect,
    selection: Option<crate::app::TerminalSelection>,
) {
    let Some(selection) = selection else {
        return;
    };
    let buffer = frame.buffer_mut();
    for row in 0..area.height {
        for column in 0..area.width {
            if selection.contains(row, column) {
                buffer[(area.x + column, area.y + row)]
                    .set_bg(Color::Rgb(62, 82, 112))
                    .set_fg(Color::White);
            }
        }
    }
}

fn render_vt_screen(frame: &mut Frame<'_>, screen: &vt100::Screen, area: Rect, show_cursor: bool) {
    let (rows, cols) = screen.size();
    let cursor = if show_cursor && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        (row < area.height && col < area.width).then_some((area.x + col, area.y + row))
    } else {
        None
    };
    {
        let buffer = frame.buffer_mut();
        for row in 0..area.height.min(rows) {
            for col in 0..area.width.min(cols) {
                let Some(source) = screen.cell(row, col) else {
                    continue;
                };
                let destination = &mut buffer[(area.x + col, area.y + row)];
                let contents = source.contents();
                destination.set_symbol(if contents.is_empty() { " " } else { &contents });
                destination.set_style(vt_style(source));
            }
        }
    }
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

/// Dim and strikethrough are missing here on purpose: vt100 0.15 tracks them
/// internally but `Cell` exposes no accessor for either, so a live screen
/// cannot render what `ansi_history_text` shows for the same bytes in deep
/// history. Add them here once the crate grows the accessors.
fn vt_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default()
        .fg(vt_color(cell.fgcolor()))
        .bg(vt_color(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

/// Deep history laid out as text: colour kept, everything else the terminal
/// would have acted on rather than shown left out.
///
/// This is not an emulator and cannot be one - it is reading a byte log at
/// whatever width the pane happens to be now, not the width the session was
/// when it wrote them, so cursor addressing has nothing true to address. What
/// it can do is not put on screen the things that were never meant to be seen.
/// An escape it does not act on is an escape it swallows whole: a window title
/// is not session output, and one agent's capture here carries four hundred and
/// sixty thousand of them, a spinner written into the title bar twenty times a
/// second. Printing their payloads is what made deep history unreadable.
fn ansi_history_text(value: &str) -> Text<'static> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut style = Style::default();
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' && characters.peek() == Some(&'[') {
            characters.next();
            if !buffer.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buffer), style));
            }
            let mut parameters = String::new();
            let mut final_byte = None;
            for next in characters.by_ref() {
                if ('@'..='~').contains(&next) {
                    final_byte = Some(next);
                    break;
                }
                parameters.push(next);
            }
            if final_byte == Some('m') {
                apply_sgr(&mut style, &parameters);
            }
        } else if character == '\x1b'
            && matches!(characters.peek(), Some(&next) if is_string_escape(next))
        {
            // A window title, a colour query, a shell-integration mark, an
            // inline image: a run of bytes addressed to the terminal itself,
            // ended by a bell or by a string terminator. None of it was ever on
            // the screen, so none of it belongs in a picture of the screen.
            characters.next();
            while let Some(next) = characters.next() {
                if next == '\x07' {
                    break;
                }
                if next == '\x1b' {
                    // ESC \ ends the string; any other ESC is a sequence that
                    // was never terminated, and swallowing the rest of the
                    // capture over one stray byte would be worse than stopping.
                    if characters.peek() == Some(&'\\') {
                        characters.next();
                    }
                    break;
                }
            }
        } else if character == '\x1b' {
            // Two- and three-byte escapes - charset designations, keypad modes,
            // save and restore cursor. Nothing to show either way; the point is
            // to eat the byte that follows so it is not mistaken for text.
            if let Some(&next) = characters.peek() {
                characters.next();
                if matches!(next, '(' | ')' | '*' | '+' | '%' | '#') {
                    characters.next();
                }
            }
        } else if character == '\r' {
            // The carriage return that ends a line is the line ending; on its
            // own it means the line is about to be written again. Keeping what
            // it overwrote is how one spinner became a thousand lines of it.
            if characters.peek() != Some(&'\n') {
                buffer.clear();
                spans.clear();
            }
        } else if character == '\n' {
            if !buffer.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buffer), style));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
        } else if character == '\t' {
            buffer.push_str("    ");
        } else if !character.is_control() {
            buffer.push(character);
        }
    }
    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, style));
    }
    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

/// Whether this byte after an ESC opens a run that ends at a string
/// terminator rather than at a final byte: OSC, DCS, SOS, PM, APC.
fn is_string_escape(intro: char) -> bool {
    matches!(intro, ']' | 'P' | 'X' | '^' | '_')
}

fn apply_sgr(style: &mut Style, parameters: &str) {
    let values: Vec<u16> = if parameters.is_empty() {
        vec![0]
    } else {
        parameters
            .split(';')
            .map(|value| value.parse().unwrap_or(0))
            .collect()
    };
    let mut index = 0;
    while index < values.len() {
        let value = values[index];
        match value {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => *style = style.fg(ansi_basic_color(value - 30, false)),
            90..=97 => *style = style.fg(ansi_basic_color(value - 90, true)),
            40..=47 => *style = style.bg(ansi_basic_color(value - 40, false)),
            100..=107 => *style = style.bg(ansi_basic_color(value - 100, true)),
            38 | 48 => {
                let foreground = value == 38;
                if values.get(index + 1) == Some(&5)
                    && let Some(color) = values.get(index + 2)
                {
                    let color = Color::Indexed((*color).min(255) as u8);
                    *style = if foreground {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                    index += 2;
                } else if values.get(index + 1) == Some(&2)
                    && let (Some(red), Some(green), Some(blue)) = (
                        values.get(index + 2),
                        values.get(index + 3),
                        values.get(index + 4),
                    )
                {
                    let color = Color::Rgb(
                        (*red).min(255) as u8,
                        (*green).min(255) as u8,
                        (*blue).min(255) as u8,
                    );
                    *style = if foreground {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                    index += 4;
                }
            }
            39 => *style = style.fg(Color::Reset),
            49 => *style = style.bg(Color::Reset),
            _ => {}
        }
        index += 1;
    }
}

fn ansi_basic_color(index: u16, bright: bool) -> Color {
    match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    }
}

fn draw_footer(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.board_chip = None;
    let busy = if app.busy_operations > 0 {
        "  [working]"
    } else {
        ""
    };
    // A failure reads differently from the progress lines it sits among.
    let status_style = if app.status_is_error() {
        Style::default().fg(Color::LightRed)
    } else {
        Style::default().fg(Color::Gray)
    };
    if let Some((target_id, progress)) = app.visible_task_progress() {
        let gauge_width = area.width.min(52);
        let areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(gauge_width)])
            .split(area);
        let status_width = areas[0].width.saturating_sub(1) as usize;
        frame.render_widget(
            Paragraph::new(format!(
                " {}{busy}",
                truncate(&app.status_message, status_width)
            ))
            .style(status_style),
            areas[0],
        );
        let ratio = progress
            .total
            .filter(|total| *total > 0)
            .map(|total| progress.completed as f64 / total as f64)
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let percent = progress
            .total
            .filter(|total| *total > 0)
            .map(|_| format!(" {:.0}%", ratio * 100.0))
            .unwrap_or_default();
        let label = truncate(
            &format!("{target_id}: {}{percent}", progress.label),
            gauge_width.saturating_sub(2) as usize,
        );
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
                .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
                .use_unicode(true),
            areas[1],
        );
        return;
    }
    let help = if let Some(form) = app.file_manager.as_ref() {
        // The browser swallows every key while it is open, so the footer has to
        // advertise its own bindings rather than the pane ones underneath.
        if form.preview_path.is_some() {
            if area.width < 88 {
                "  Esc close  j/k scroll  d download  r reload"
            } else {
                "  Esc/Enter close  j/k scroll  g/G top/end  d download  c copy path  r reload"
            }
        } else if !form.query.is_empty() {
            if area.width < 88 {
                "  Esc clear filter  Enter open  Ctrl-f close"
            } else {
                "  type to filter  Esc clear filter  Enter open  ↑↓ move  Ctrl-f close"
            }
        } else if area.width < 88 {
            "  Enter open  ← up  Ctrl-d download  Ctrl-f close"
        } else {
            "  Enter open  ← up  type to filter  / find  Ctrl-d download  Ctrl-y copy path  Ctrl-f close"
        }
    } else if app.interactive {
        "  Cmd/Opt+Arrow panes  Shift/Opt+Enter newline  PgUp history"
    } else if area.width < 88 {
        match app.focus {
            Focus::Machines => "  Space toggle  n new  c channels  / search  q quit",
            Focus::Agents => "  Enter open  n new  t temporal  p ports  / search  q quit",
            Focus::Recap => "  Cmd/Opt+Arrow panes  PgUp history  / search  q quit",
        }
    } else {
        match app.focus {
            Focus::Machines => {
                "  Space toggle  n new  c channels  , settings  / search  b board  q quit  ? more"
            }
            // The fold key is only worth a place in the line while something
            // in the list has subagents to fold away.
            Focus::Agents => match (
                app.archived_count() > 0,
                app.state.show_archived,
                app.has_subagents(),
            ) {
                (true, true, true) => {
                    "  Enter open  a collapse  space fold  / search  n new  q quit"
                }
                (true, true, false) => {
                    "  Enter open  a collapse  p ports  / search  n new  t temporal  q quit"
                }
                (true, false, true) => {
                    "  Enter open  a expand  space fold  / search  n new  q quit"
                }
                (true, false, false) => {
                    "  Enter open  a expand  p ports  / search  n new  t temporal  q quit"
                }
                (false, _, true) => {
                    "  Enter open  space fold  p ports  / search  n new  t temporal  q quit"
                }
                (false, _, false) => {
                    "  Enter open  p ports  / search  n new  t temporal  b board  q quit  ? more"
                }
            },
            Focus::Recap => {
                "  Cmd/Opt+Arrow panes  PgUp history  / search  b board  q quit  ? more"
            }
        }
    };
    // Machines whose running daemon lags this build get a bottom-right chip
    // pointing at where the update lives; narrow footers keep the
    // keybindings instead.
    let lagging = app.outdated_daemons();
    let chip = if lagging.is_empty() || area.width < 72 {
        String::new()
    } else if let [(target_id, _)] = lagging.as_slice() {
        format!(" ⟳ {target_id} daemon outdated — , to update ")
    } else {
        format!(" ⟳ {} daemons outdated — , to update ", lagging.len())
    };
    // Something said on the board while nobody was reading gets a chip of its
    // own, after that one: it is news rather than a warning, and it is the one
    // piece of footer chrome that opens what it points at.
    let board = if app.board.unread == 0 || area.width < 60 {
        String::new()
    } else {
        format!(" ● {} board ", app.board.unread)
    };
    let help_width = UnicodeWidthStr::width(help);
    let chip_width = UnicodeWidthStr::width(chip.as_str());
    let board_width = UnicodeWidthStr::width(board.as_str());
    let status_width = (area.width as usize)
        .saturating_sub(help_width + chip_width + board_width + busy.len() + 2);
    let status = format!(" {}{busy}", truncate(&app.status_message, status_width));
    // The spans are laid out left to right rather than right-aligned, so the
    // chip's rectangle is whatever the three widths before it add up to.
    let board_x = UnicodeWidthStr::width(status.as_str()) + help_width + chip_width;
    if board_width > 0 && board_x + board_width <= area.width as usize {
        app.board_chip = Some(Rect::new(
            area.x + board_x as u16,
            area.y,
            board_width as u16,
            1,
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(status, status_style),
            Span::styled(help, Style::default().fg(MUTED)),
            Span::styled(chip, Style::default().fg(Color::Yellow)),
            Span::styled(
                board,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ])),
        area,
    );
}

fn draw_modal(frame: &mut Frame<'_>, modal: &mut Modal, outer: Rect, kinds: &[AgentKind]) {
    match modal {
        Modal::Launch(form) => draw_launch_modal(frame, form, outer, kinds),
        Modal::Temporal(form) => draw_temporal_modal(frame, form, outer, kinds),
        Modal::Moderator(form) => draw_moderator_modal(frame, form, outer, kinds),
        Modal::PortForward(form) => draw_port_forward_modal(frame, form, outer),
        Modal::Channels(form) => draw_channels_modal(frame, form, outer),
        Modal::ConfirmKill { label, archive, .. } => {
            let area = centered_rect(54, 7, outer);
            frame.render_widget(Clear, area);
            let (title, prompt, action) = if *archive {
                (
                    " Archive agent ",
                    format!("Stop '{label}' and keep it in Archived?"),
                    "Enter/y archive    Esc/n cancel",
                )
            } else {
                (
                    " Permanently remove session ",
                    format!("Permanently remove '{label}'?"),
                    "Enter/y remove    Esc/n cancel",
                )
            };
            let text = vec![
                Line::raw(""),
                Line::raw(prompt),
                Line::raw(""),
                Line::styled(action, Style::default().fg(MUTED)),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .block(panel(title, true)),
                area,
            );
        }
        Modal::ConfirmArchivedResume {
            source_session_id,
            launch,
            summary,
            remove_archive,
            ..
        } => {
            let area = centered_rect(72, 14, outer);
            frame.render_widget(Clear, area);
            // A daemon record comes back as itself - same entry, history and
            // subagents - so there is no old archive to offer to remove. Only
            // a legacy tmux session is relaunched beside its archive.
            let in_place = crate::runtime::is_daemon_session_id(source_session_id);
            let checkbox = if *remove_archive { "[x]" } else { "[ ]" };
            let mut text = vec![
                Line::raw(""),
                Line::raw(if in_place {
                    "Reopen this conversation and bring the agent back?"
                } else {
                    "Reopen this conversation as a new running agent?"
                }),
                Line::raw(""),
                // The name of what is about to be reopened, so a wrong match is
                // caught here rather than after the agent has started.
                Line::styled(
                    truncate(summary, area.width.saturating_sub(6) as usize),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Line::styled(format!("in {}", launch.path), Style::default().fg(MUTED)),
                Line::raw(""),
            ];
            if in_place {
                text.extend([
                    Line::styled(
                        "It comes back on its own entry: history, label and subagents included.",
                        Style::default().fg(MUTED),
                    ),
                    Line::raw(""),
                    Line::styled("Enter/y resume    Esc/n cancel", Style::default().fg(MUTED)),
                ]);
            } else {
                text.extend([
                    Line::from(vec![
                        Span::styled(
                            checkbox,
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" Remove the previous Archived entry after resume"),
                    ]),
                    Line::styled(
                        "The old archive is removed only after the new agent starts successfully.",
                        Style::default().fg(MUTED),
                    ),
                    Line::styled(
                        "This choice is remembered for future resumes.",
                        Style::default().fg(MUTED),
                    ),
                    Line::raw(""),
                    Line::styled(
                        "Space toggle    Enter/y resume    Esc/n cancel",
                        Style::default().fg(MUTED),
                    ),
                ]);
            }
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Resume archived agent ", true)),
                area,
            );
        }
        Modal::ConfirmInstall {
            target,
            kind,
            launch,
            sync_config,
        } => {
            let remote = target.is_remote();
            let area = centered_rect(68, if remote { 15 } else { 11 }, outer);
            frame.render_widget(Clear, area);
            let mut text = vec![
                Line::raw(""),
                Line::raw(format!("{kind} was not detected on {}.", target.label)),
                Line::raw(""),
                Line::raw(if launch.is_some() {
                    "Install it now, then continue launching this agent?"
                } else {
                    "Install it now?"
                }),
                Line::styled(
                    "Uses a compatible local binary or downloads the checked target package locally.",
                    Style::default().fg(MUTED),
                ),
                Line::styled(
                    "The target needs no internet; its configured installer is only the final fallback.",
                    Style::default().fg(MUTED),
                ),
            ];
            // Sending the credentials is what makes the remote agent the same
            // agent - signed in to the same account - rather than a fresh
            // install asking whoever finds it to log in. It is also this
            // machine's account leaving this machine, so it is said plainly and
            // the person decides, every install.
            if remote {
                text.push(Line::raw(""));
                text.push(Line::from(vec![
                    Span::styled(
                        if *sync_config { "[x]" } else { "[ ]" },
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " Also send this machine's {kind} settings and sign-in"
                    )),
                ]));
                text.push(Line::styled(
                    "Copied over SSH so the remote agent runs in the same environment as here.",
                    Style::default().fg(MUTED),
                ));
                text.push(Line::styled(
                    "Any file it replaces is backed up on the target first.",
                    Style::default().fg(MUTED),
                ));
            }
            text.push(Line::raw(""));
            text.push(Line::styled(
                if remote {
                    "Space toggle    Enter/y install    Esc/n cancel"
                } else {
                    "Enter/y install    Esc/n cancel"
                },
                Style::default().fg(MUTED),
            ));
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Install agent runtime ", true)),
                area,
            );
        }
        Modal::ConfirmHistoryReference { form, candidate } => {
            let area = centered_rect(76, 12, outer);
            frame.render_widget(Clear, area);
            let (_, source_name, source_color) = agent_visual(candidate.kind);
            let (_, target_name, target_color) = agent_visual(form.launch.kind);
            let text = vec![
                Line::raw(""),
                Line::from(vec![
                    Span::styled(source_name, Style::default().fg(source_color).bold()),
                    Span::raw(" history does not match "),
                    Span::styled(target_name, Style::default().fg(target_color).bold()),
                    Span::raw(" resume format."),
                ]),
                Line::raw(""),
                Line::raw("Direct resume is unavailable, but this history can be referenced."),
                Line::raw(
                    "Muxloom will start a new agent with the source history file in its prompt.",
                ),
                Line::styled(
                    truncate(
                        &candidate.source_path,
                        area.width.saturating_sub(6) as usize,
                    ),
                    Style::default().fg(MUTED),
                ),
                Line::raw(""),
                Line::styled(
                    "Enter/r reference history    Esc/n back",
                    Style::default().fg(MUTED),
                ),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel(" History type not match ", true)),
                area,
            );
        }
        Modal::LegacyFallback { target_id, detail } => {
            let area = centered_rect(72, 11, outer);
            frame.render_widget(Clear, area);
            let text = vec![
                Line::raw(""),
                Line::styled(
                    format!("{target_id} is running this session through legacy tmux."),
                    Style::default().fg(Color::Yellow).bold(),
                ),
                Line::raw(""),
                Line::raw(
                    "The agent is running, but native history, files, and reconnect behavior may differ.",
                ),
                Line::styled(detail.clone(), Style::default().fg(MUTED)),
                Line::raw(""),
                Line::styled("Enter/Esc acknowledge", Style::default().fg(MUTED)),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Legacy tmux compatibility fallback ", true)),
                area,
            );
        }
        Modal::UpdatePrompt(prompt) => {
            let area = centered_rect(64, 11, outer);
            frame.render_widget(Clear, area);
            let action = if prompt.can_self_update {
                "Update the installed bundle now? The change applies on the next launch."
            } else {
                "Fetch the release's muxloomd companions into the local cache? This install \
                 is updated elsewhere — with cargo, or by the package manager that owns it \
                 — but the cache is what updates machines."
            };
            let text = vec![
                Line::raw(""),
                Line::styled(
                    format!("muxloom {} is available", prompt.latest),
                    Style::default().fg(Color::Yellow).bold(),
                ),
                // Two nightlies differ only by a commit count, so the build
                // being left behind has to be on screen to make sense of it.
                Line::styled(
                    format!("this one is {}", prompt.current),
                    Style::default().fg(MUTED),
                ),
                Line::raw(""),
                Line::raw(action),
                Line::raw(""),
                Line::styled(
                    "Enter/y update    Esc/n not now",
                    Style::default().fg(MUTED),
                ),
                Line::styled(
                    "(config: update_prompt, update_channel)",
                    Style::default().fg(MUTED),
                ),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Update available ", true)),
                area,
            );
        }
        Modal::ConfirmForcedUpdate {
            target,
            working,
            terminals,
            resumable,
        } => {
            let area = centered_rect(72, 14, outer);
            frame.render_widget(Clear, area);
            let mut text = vec![
                Line::raw(""),
                Line::styled(
                    format!("Force the daemon update on {}?", target.id),
                    Style::default().fg(Color::Yellow).bold(),
                ),
                Line::raw(""),
                Line::raw(format!(
                    "{resumable} agent(s) will be archived and resumed from their transcripts."
                )),
            ];
            if !working.is_empty() {
                text.push(Line::styled(
                    format!("Interrupts work in progress: {}", working.join(", ")),
                    Style::default().fg(Color::Yellow),
                ));
            }
            if !terminals.is_empty() {
                text.push(Line::styled(
                    format!(
                        "Ends without resume (terminals/temporal): {}",
                        terminals.join(", ")
                    ),
                    Style::default().fg(Color::Red),
                ));
            }
            text.push(Line::raw(""));
            text.push(Line::styled(
                "Enter/y force update    Esc/n not now",
                Style::default().fg(MUTED),
            ));
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel(" Forced daemon update ", true)),
                area,
            );
        }
        Modal::Help(form) => draw_help_modal(frame, form, outer),
        Modal::Settings(form) => draw_settings_modal(frame, form, outer),
        Modal::Search(form) => draw_search_modal(frame, form, outer),
        // Drawn by `draw` instead, which still holds the board it reads from.
        Modal::Board(_) => {}
        Modal::PathPicker(form) => draw_path_picker(frame, form, outer),
        Modal::Resume(form) => draw_resume_modal(frame, form, outer),
        Modal::RenameAgent { value, .. } => {
            let area = centered_rect(54, 8, outer);
            frame.render_widget(Clear, area);
            let text = vec![
                Line::raw(""),
                Line::styled(
                    "Display name (blank restores the folder name)",
                    Style::default().fg(MUTED),
                ),
                Line::from(vec![
                    Span::raw(value.clone()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::raw(""),
                Line::styled(
                    "Enter save    Ctrl-u clear    Esc cancel",
                    Style::default().fg(MUTED),
                ),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .block(panel(" Rename agent ", true)),
                area,
            );
        }
    }
}

fn draw_file_browser(
    frame: &mut Frame<'_>,
    form: &mut FileManagerForm,
    outer: Rect,
    focused: bool,
) {
    let title = format!(
        " Files{}  {}:{}{} ",
        if form.loading || form.searching {
            " [loading]"
        } else if form.error.is_some() {
            " [error]"
        } else {
            ""
        },
        form.target.label,
        truncate(&form.path, outer.width.saturating_sub(20) as usize),
        if form.query.is_empty() {
            String::new()
        } else {
            if form.query.starts_with('/') {
                format!("  search: {}", &form.query[1..])
            } else {
                format!("  match: {}", form.query)
            }
        }
    );
    let block = panel(&title, focused);
    let inner = block.inner(outer);
    frame.render_widget(block, outer);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1)])
        .split(inner);
    let list_area = rows[0];
    form.list_area = Some(list_area);
    if form.preview_path.is_none() {
        form.preview_area = None;
    }

    let list_inner = list_area;
    let items: Vec<_> = if (form.loading || form.searching) && form.entries.is_empty() {
        vec![ListItem::new(Line::styled(
            "Loading...",
            Style::default().fg(MUTED),
        ))]
    } else if let Some(error) = &form.error
        && form.entries.is_empty()
    {
        vec![ListItem::new(Line::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        ))]
    } else if form.entries.is_empty() {
        vec![ListItem::new(Line::styled(
            if form.query.starts_with('/') {
                "No matching files"
            } else {
                "Empty directory"
            },
            Style::default().fg(MUTED),
        ))]
    } else {
        form.entries
            .iter()
            .map(|entry| {
                // A link shows where it points (a directory arrow still opens
                // like a directory) but keeps its own colour so it reads as one.
                let (icon, color) = match (entry.kind, entry.symlink) {
                    (_, true) => ("↗", Color::Cyan),
                    (FileEntryKind::Directory, false) => ("▸", ACCENT),
                    (FileEntryKind::File, false) => ("·", Color::White),
                    (FileEntryKind::Symlink, false) => ("↗", Color::Cyan),
                    (FileEntryKind::Other, false) => ("?", MUTED),
                };
                let size = if entry.kind == FileEntryKind::File {
                    format_bytes(entry.size)
                } else {
                    String::new()
                };
                let name_width = list_inner.width.saturating_sub(12) as usize;
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {icon} "), Style::default().fg(color)),
                    Span::styled(
                        truncate(&entry.name, name_width),
                        Style::default().fg(color),
                    ),
                    Span::styled(format!(" {size:>8}"), Style::default().fg(MUTED)),
                ]))
            })
            .collect()
    };
    let mut state = ratatui::widgets::ListState::default();
    if !form.entries.is_empty() {
        state.select(Some(form.selected.min(form.entries.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(list_highlight_style(focused, false))
        .highlight_symbol(if focused { "> " } else { "  " });
    frame.render_stateful_widget(list, list_area, &mut state);
    form.entry_rows = if form.entries.is_empty() {
        Vec::new()
    } else {
        form.entries
            .iter()
            .enumerate()
            .skip(state.offset())
            .take(list_inner.height as usize)
            .enumerate()
            .map(|(visible, (index, _))| {
                (
                    index,
                    Rect::new(
                        list_inner.x,
                        list_inner.y + visible as u16,
                        list_inner.width,
                        1,
                    ),
                )
            })
            .collect()
    };

    // A listing that failed while cached rows are still on screen has nowhere
    // else to say so: the rows look current and only the title carries a tag.
    let footer = if let Some(error) = form.error.as_deref().filter(|_| !form.entries.is_empty()) {
        error
    } else if form.preview_path.is_some() {
        "Double-click/Enter close  Scroll or arrows page  Right-click parent"
    } else if form.loading || form.searching {
        "Click select  Double-click open  Right-click parent  loading"
    } else if form.query.starts_with('/') {
        if form.search_truncated {
            "Too many to list — these are some of the matches; narrow the pattern"
        } else {
            "Recursive filename search  * and ** wildcards  Esc clears"
        }
    } else {
        "Type / to search subfolders  Double-click open  Ctrl-y copy  Ctrl-d download"
    };
    let footer_color = if form.error.is_some() && !form.entries.is_empty() {
        Color::Red
    } else {
        MUTED
    };
    frame.render_widget(
        Paragraph::new(truncate(footer, rows[1].width as usize))
            .style(Style::default().fg(footer_color)),
        rows[1],
    );
}

fn draw_file_preview_panel(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let path = app
        .file_manager
        .as_ref()
        .and_then(|form| form.preview_path.as_deref())
        .unwrap_or_default()
        .to_string();
    let title = format!(
        " Preview  {} ",
        truncate(&path, area.width.saturating_sub(14) as usize)
    );
    let block = panel(&title, app.focus == Focus::Recap);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let form = app
        .file_manager
        .as_mut()
        .expect("preview requires file manager");
    form.preview_area = Some(area);
    let visual_media = form.preview.as_ref().is_some_and(|preview| {
        matches!(
            preview.kind,
            FilePreviewKind::Image | FilePreviewKind::Video
        )
    });
    if visual_media {
        form.preview_scroll = 0;
        form.preview_max_scroll = 0;
        form.preview_page_rows = inner.height.max(1);
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
        if let Some(preview) = &form.preview {
            frame.render_widget(Paragraph::new(file_preview_metadata(preview)), rows[0]);
        }
        if let Some(media) = &form.media_frame {
            draw_media_frame(frame, media, rows[1]);
        } else if let Some(error) = &form.media_error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                rows[1],
            );
        } else {
            let message = if form.media_loading {
                "Streaming encoded media and decoding on this machine..."
            } else {
                "Waiting for the media decoder..."
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(MUTED))
                    .alignment(Alignment::Center),
                rows[1],
            );
        }
        return;
    }
    // Transient states borrow nothing from the cache; the rendered preview is
    // moved out and put back so no frame ever clones a multi-megabyte body.
    let mut transient = true;
    let mut render = if let Some(error) = &form.preview_error {
        PreviewRender::notice(error.clone(), Color::Red)
    } else if form.preview_loading {
        PreviewRender::notice("Loading preview...".to_string(), MUTED)
    } else if let Some(render) = form.preview_rendered.take() {
        transient = false;
        render
    } else if let Some(preview) = &form.preview {
        transient = false;
        file_preview_render(preview)
    } else {
        PreviewRender::notice("No preview output".to_string(), MUTED)
    };
    // Measuring is O(lines) but only runs when the body or the pane width
    // changed, so scrolling a large file stays O(visible rows).
    render.measure(inner.width);
    let pinned_rows = render
        .pinned_height()
        .min(inner.height.saturating_sub(1) as usize) as u16;
    let body_area = Rect::new(
        inner.x,
        inner.y + pinned_rows,
        inner.width,
        inner.height.saturating_sub(pinned_rows),
    );
    if pinned_rows > 0 {
        let header = render.pinned_window(pinned_rows as usize);
        frame.render_widget(
            Paragraph::new(Text::from(header)),
            Rect::new(inner.x, inner.y, inner.width, pinned_rows),
        );
    }
    form.preview_max_scroll = render.height().saturating_sub(body_area.height as usize);
    form.preview_page_rows = body_area.height.max(1);
    // A reader parked at the end of the file stays there as it grows: the
    // monitor swaps in longer content and the view follows to the new tail.
    form.preview_scroll = if form.preview_follow_tail {
        form.preview_max_scroll
    } else {
        form.preview_scroll.min(form.preview_max_scroll)
    };
    // Wrap and window the preview ourselves instead of leaning on ratatui's
    // Paragraph scroll+wrap. That path miscounts wrapped rows for the very long
    // lines in JSON/JSONL session dumps and, once scrolled, leaves stray glyphs
    // on otherwise-empty rows. Emitting exactly the visible rows keeps scroll
    // bounds accurate and drops control characters that would shift columns.
    let window = render.window(form.preview_scroll, body_area.height as usize);
    // Remember the plain text of the visible rows so a mouse selection over the
    // preview can be copied, mirroring terminal selection.
    form.preview_text_area = Some(body_area);
    form.preview_visible = window
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let selection = form.preview_selection;
    frame.render_widget(Paragraph::new(Text::from(window)), body_area);
    highlight_terminal_selection(frame, body_area, selection);
    if !transient {
        form.preview_rendered = Some(render);
    }
}

/// Rows a single logical line occupies when hard-wrapped to `width` display
/// columns. Must mirror `split_line` exactly so scroll bounds line up with what
/// is actually rendered. Zero-width and control characters are ignored.
fn wrapped_row_count(line: &Line<'_>, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut column = 0usize;
    for span in &line.spans {
        for character in span.content.chars() {
            let advance = UnicodeWidthChar::width(character).unwrap_or(0);
            if advance == 0 {
                continue;
            }
            if column > 0 && column + advance > width {
                rows += 1;
                column = 0;
            }
            column += advance;
        }
    }
    rows
}

/// Hard-wrap one logical line into visual rows no wider than `width`, splitting
/// styled spans at the wrap points and folding the line-level style into each
/// span so highlighting survives. Kept consistent with `wrapped_row_count`.
fn split_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let mut column = 0usize;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        for character in span.content.chars() {
            let advance = UnicodeWidthChar::width(character).unwrap_or(0);
            if advance == 0 {
                continue;
            }
            if column > 0 && column + advance > width {
                if !buffer.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buffer), style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                column = 0;
            }
            buffer.push(character);
            column += advance;
        }
        if !buffer.is_empty() {
            current.push(Span::styled(std::mem::take(&mut buffer), style));
        }
    }
    rows.push(Line::from(current));
    rows
}

/// The visual rows visible in a `height`-row viewport that starts at wrapped
/// row `start`. Only lines overlapping the window are materialised, so large
/// files stay off the allocation-heavy part of the render path.
fn window_lines(lines: &[Line<'_>], width: u16, start: usize, height: usize) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let width = width.max(1) as usize;
    let end = start.saturating_add(height);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut row = 0usize;
    for line in lines {
        if out.len() >= height {
            break;
        }
        let count = wrapped_row_count(line, width);
        if row + count <= start {
            row += count;
            continue;
        }
        for (index, visual) in split_line(line, width).into_iter().enumerate() {
            let global = row + index;
            if global >= start && global < end {
                out.push(visual);
            }
        }
        row += count;
    }
    out
}

/// Cut a line at `width` display columns, dropping zero-width and control
/// characters the way `split_line` does. Used where wrapping would break column
/// alignment, so the row always occupies exactly one visual row.
fn clip_line(line: &Line<'_>, width: usize) -> Line<'static> {
    let spans = line
        .spans
        .iter()
        .map(|span| Span::styled(span.content.to_string(), line.style.patch(span.style)))
        .collect();
    Line::from(truncate_spans(spans, width.max(1)).0)
}

/// Rows a plain string occupies when hard-wrapped to `width` columns. Mirrors
/// `wrapped_row_count` without allocating a `Line` for the measurement.
fn wrapped_str_rows(value: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut column = 0usize;
    for character in value.chars() {
        let advance = UnicodeWidthChar::width(character).unwrap_or(0);
        if advance == 0 {
            continue;
        }
        if column > 0 && column + advance > width {
            rows += 1;
            column = 0;
        }
        column += advance;
    }
    rows
}

/// Bodies larger than this are rendered as plain rows instead of being
/// highlighted or reformatted. Running syntect (or a JSON round-trip) over
/// megabytes costs far more than the colour is worth, and the plain body is
/// materialised lazily so the whole file still stays readable.
const RICH_RENDER_LIMIT: usize = 512 * 1024;

/// A preview split into rows pinned above the viewport — the file summary and,
/// for delimited data, the column ruler and header — and a scrolling body.
#[derive(Debug, Default)]
pub struct PreviewRender {
    pinned: Vec<Line<'static>>,
    body: PreviewBody,
    layout: PreviewLayout,
}

/// The scrolling part of a preview. Small bodies are turned into styled lines
/// once, where highlighting pays for itself; large text and delimited data keep
/// their source and become `Line`s only for the rows actually on screen, so a
/// multi-megabyte file costs the same per frame as a small one.
#[derive(Debug)]
enum PreviewBody {
    Lines(Vec<Line<'static>>),
    Plain(PlainBody),
    Table(TableBody),
}

impl Default for PreviewBody {
    fn default() -> Self {
        Self::Lines(Vec::new())
    }
}

#[derive(Debug)]
struct PlainBody {
    content: String,
    /// Byte range of every logical line in `content`, newline excluded.
    lines: Vec<(usize, usize)>,
    style: Style,
}

#[derive(Debug)]
struct TableBody {
    rows: Vec<Vec<String>>,
    widths: Vec<usize>,
    /// Display width of the row-number gutter.
    gutter: usize,
    /// First row of `rows` that carries data: 1 when the file has a header,
    /// because that header is pinned rather than scrolled.
    first_data_row: usize,
}

/// Cumulative wrapped-row offsets for one body at one pane width. Rebuilt only
/// when either changes, so scroll bounds and windowing stay O(visible rows).
#[derive(Debug, Default)]
struct PreviewLayout {
    width: u16,
    /// `starts[i]` is the first visual row of body line `i`; the final entry is
    /// the total row count. Empty while the body is rendered one row per line.
    starts: Vec<u32>,
    total: usize,
}

impl PreviewRender {
    fn notice(message: String, color: Color) -> Self {
        Self {
            pinned: Vec::new(),
            body: PreviewBody::Lines(vec![Line::styled(message, Style::default().fg(color))]),
            layout: PreviewLayout::default(),
        }
    }

    fn lines(pinned: Vec<Line<'static>>, lines: Vec<Line<'static>>) -> Self {
        Self {
            pinned,
            body: PreviewBody::Lines(lines),
            layout: PreviewLayout::default(),
        }
    }

    /// Table rows are clipped to the pane instead of wrapped: a wrapped table
    /// loses the column alignment that makes it worth drawing, and one row per
    /// record keeps the scroll math exact without materialising any line.
    fn wraps(&self) -> bool {
        !matches!(self.body, PreviewBody::Table(_))
    }

    fn measure(&mut self, width: u16) {
        let width = width.max(1);
        if !self.wraps() {
            self.layout.width = width;
            self.layout.starts = Vec::new();
            self.layout.total = self.body.len();
            return;
        }
        if self.layout.width == width && self.layout.starts.len() == self.body.len() + 1 {
            return;
        }
        self.layout.width = width;
        self.layout.starts.clear();
        self.layout.starts.reserve(self.body.len() + 1);
        let mut row = 0u32;
        for index in 0..self.body.len() {
            self.layout.starts.push(row);
            row = row.saturating_add(self.body.rows_at(index, width as usize) as u32);
        }
        self.layout.starts.push(row);
        self.layout.total = row as usize;
    }

    fn height(&self) -> usize {
        self.layout.total
    }

    fn pinned_height(&self) -> usize {
        if !self.wraps() {
            return self.pinned.len();
        }
        let width = self.layout.width.max(1) as usize;
        self.pinned
            .iter()
            .map(|line| wrapped_row_count(line, width))
            .sum()
    }

    fn pinned_window(&self, height: usize) -> Vec<Line<'static>> {
        let width = self.layout.width.max(1);
        if !self.wraps() {
            return self
                .pinned
                .iter()
                .take(height)
                .map(|line| clip_line(line, width as usize))
                .collect();
        }
        window_lines(&self.pinned, width, 0, height)
    }

    /// The visual rows visible in a `height`-row viewport that starts at row
    /// `start`. Only the lines overlapping the window are built, so scrolling a
    /// large file never touches the rest of it. Reads the width recorded by
    /// `measure`, so the rows can never be split differently than they were
    /// counted.
    fn window(&self, start: usize, height: usize) -> Vec<Line<'static>> {
        if height == 0 {
            return Vec::new();
        }
        let width = self.layout.width.max(1) as usize;
        if !self.wraps() {
            let end = start.saturating_add(height).min(self.body.len());
            return (start.min(end)..end)
                .map(|index| clip_line(&self.body.line(index), width))
                .collect();
        }
        let end = start.saturating_add(height);
        let mut out: Vec<Line<'static>> = Vec::with_capacity(height);
        // Binary search the cumulative offsets instead of walking the file.
        let mut index = match self
            .layout
            .starts
            .binary_search(&(start.min(u32::MAX as usize) as u32))
        {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        };
        while index < self.body.len() && out.len() < height {
            let row = self.layout.starts.get(index).copied().unwrap_or(0) as usize;
            for (offset, visual) in split_line(&self.body.line(index), width)
                .into_iter()
                .enumerate()
            {
                let global = row + offset;
                if global >= start && global < end {
                    out.push(visual);
                }
            }
            index += 1;
        }
        out
    }
}

impl PreviewBody {
    fn len(&self) -> usize {
        match self {
            Self::Lines(lines) => lines.len(),
            Self::Plain(plain) => plain.lines.len(),
            Self::Table(table) => table.rows.len().saturating_sub(table.first_data_row),
        }
    }

    fn line(&self, index: usize) -> Line<'static> {
        match self {
            Self::Lines(lines) => lines.get(index).cloned().unwrap_or_default(),
            Self::Plain(plain) => plain
                .lines
                .get(index)
                .map(|&(start, end)| {
                    Line::styled(plain.content[start..end].to_string(), plain.style)
                })
                .unwrap_or_default(),
            Self::Table(table) => table.data_line(index),
        }
    }

    /// Wrapped rows the line occupies, measured without building it.
    fn rows_at(&self, index: usize, width: usize) -> usize {
        match self {
            Self::Lines(lines) => lines
                .get(index)
                .map(|line| wrapped_row_count(line, width))
                .unwrap_or(1),
            Self::Plain(plain) => plain
                .lines
                .get(index)
                .map(|&(start, end)| wrapped_str_rows(&plain.content[start..end], width))
                .unwrap_or(1),
            Self::Table(_) => 1,
        }
    }
}

fn file_preview_render(preview: &crate::model::FilePreview) -> PreviewRender {
    let mut pinned = vec![file_preview_metadata(preview)];
    if preview.truncated {
        pinned.push(Line::styled(
            format!(
                "Showing the first {} of {} — download it to read the rest",
                format_bytes(preview.content.len() as u64),
                format_bytes(preview.size)
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    let notice = |message: &str| {
        vec![Line::styled(
            message.to_string(),
            Style::default().fg(MUTED),
        )]
    };
    match preview.kind {
        FilePreviewKind::Markdown if preview.content.len() <= RICH_RENDER_LIMIT => {
            PreviewRender::lines(pinned, markdown_lines(&preview.content))
        }
        FilePreviewKind::Markdown => plain_render(pinned, &preview.content),
        FilePreviewKind::Text => text_preview_render(pinned, preview),
        FilePreviewKind::Image => {
            PreviewRender::lines(pinned, notice("Image decoding has not started."))
        }
        FilePreviewKind::Audio => {
            PreviewRender::lines(pinned, notice("Audio playback is not implemented yet."))
        }
        FilePreviewKind::Video => {
            PreviewRender::lines(pinned, notice("Video decoding has not started."))
        }
        FilePreviewKind::Binary => {
            PreviewRender::lines(pinned, notice("Binary content is not rendered."))
        }
    }
}

/// Keep the source as one string and remember where each line sits, so nothing
/// but the visible rows is ever turned into styled spans.
fn plain_render(pinned: Vec<Line<'static>>, content: &str) -> PreviewRender {
    PreviewRender {
        pinned,
        body: PreviewBody::Plain(PlainBody {
            lines: line_ranges(content),
            content: content.to_string(),
            style: Style::default(),
        }),
        layout: PreviewLayout::default(),
    }
}

fn line_ranges(content: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            let mut end = index;
            if end > start && content.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
            ranges.push((start, end));
            start = index + 1;
        }
    }
    if start < content.len() {
        ranges.push((start, content.len()));
    }
    ranges
}

fn file_preview_metadata(preview: &crate::model::FilePreview) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{}  ", preview.kind),
            Style::default().fg(ACCENT).bold(),
        ),
        Span::styled(preview.mime.clone(), Style::default().fg(MUTED)),
        Span::styled(
            format!("  {}", format_bytes(preview.size)),
            Style::default().fg(MUTED),
        ),
    ])
}

/// The recap pane for a selected file watch: the watched file's current bytes,
/// re-rendered only when the content changes, with the same paging and
/// tail-follow behaviour as the file preview.
fn draw_file_watch_panel(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(index) = app
        .selected_session_id
        .as_deref()
        .and_then(|id| app.file_watches.iter().position(|watch| watch.id == id))
    else {
        return;
    };
    let title = format!(
        " Watching  {}  (x to stop) ",
        truncate(
            &app.file_watches[index].path,
            area.width.saturating_sub(26) as usize
        )
    );
    let block = panel(&title, app.focus == Focus::Recap);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let watch = &mut app.file_watches[index];
    // Reuse the preview pipeline: a watch is a file preview that keeps
    // refreshing. The styled body is cached per content, the way the browser's
    // preview does, so paging never re-parses the file.
    let mut transient = true;
    let mut render = if watch.loading && watch.content.is_empty() {
        PreviewRender::notice("Loading file...".to_string(), MUTED)
    } else if let Some(render) = watch.rendered.take() {
        transient = false;
        render
    } else {
        transient = false;
        file_preview_render(&crate::model::FilePreview {
            path: watch.path.clone(),
            mime: watch.mime.clone(),
            kind: watch.kind,
            size: watch.last_stamp.0,
            content: watch.content.clone(),
            truncated: watch.truncated,
        })
    };
    render.measure(inner.width);
    let pinned_rows = render
        .pinned_height()
        .min(inner.height.saturating_sub(1) as usize) as u16;
    let body_area = Rect::new(
        inner.x,
        inner.y + pinned_rows,
        inner.width,
        inner.height.saturating_sub(pinned_rows),
    );
    if pinned_rows > 0 {
        let header = render.pinned_window(pinned_rows as usize);
        frame.render_widget(
            Paragraph::new(Text::from(header)),
            Rect::new(inner.x, inner.y, inner.width, pinned_rows),
        );
    }
    watch.max_scroll = render.height().saturating_sub(body_area.height as usize);
    watch.page_rows = body_area.height.max(1);
    // A reader parked at the end stays there as the file grows.
    watch.scroll = if watch.follow_tail {
        watch.max_scroll
    } else {
        watch.scroll.min(watch.max_scroll)
    };
    let window = render.window(watch.scroll, body_area.height as usize);
    frame.render_widget(Paragraph::new(Text::from(window)), body_area);
    if !transient {
        watch.rendered = Some(render);
    }
}

fn draw_media_frame(frame: &mut Frame<'_>, media: &crate::media::MediaFrame, area: Rect) {
    let text = media_frame_text(media);
    let height = text.height().min(area.height as usize) as u16;
    let top = area.y + area.height.saturating_sub(height) / 2;
    frame.render_widget(
        Paragraph::new(text).alignment(Alignment::Center),
        Rect::new(area.x, top, area.width, height),
    );
}

fn media_frame_text(media: &crate::media::MediaFrame) -> Text<'static> {
    const BACKGROUND: [u8; 3] = [18, 20, 24];
    if media.width == 0
        || media.height == 0
        || media.rgba.len() != media.width as usize * media.height as usize * 4
    {
        return Text::default();
    }
    let mut lines = Vec::with_capacity(media.height.div_ceil(2) as usize);
    for top_y in (0..media.height).step_by(2) {
        let mut spans = Vec::with_capacity(media.width as usize);
        for x in 0..media.width {
            let top = composited_media_color(media, x, top_y, BACKGROUND);
            let bottom = if top_y + 1 < media.height {
                composited_media_color(media, x, top_y + 1, BACKGROUND)
            } else {
                Color::Rgb(BACKGROUND[0], BACKGROUND[1], BACKGROUND[2])
            };
            spans.push(Span::styled("▄", Style::default().fg(bottom).bg(top)));
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn composited_media_color(
    media: &crate::media::MediaFrame,
    x: u16,
    y: u16,
    background: [u8; 3],
) -> Color {
    let offset = (y as usize * media.width as usize + x as usize) * 4;
    let alpha = media.rgba[offset + 3] as u16;
    let blend = |foreground: u8, background: u8| {
        ((foreground as u16 * alpha + background as u16 * (255 - alpha) + 127) / 255) as u8
    };
    Color::Rgb(
        blend(media.rgba[offset], background[0]),
        blend(media.rgba[offset + 1], background[1]),
        blend(media.rgba[offset + 2], background[2]),
    )
}

fn text_preview_render(
    pinned: Vec<Line<'static>>,
    preview: &crate::model::FilePreview,
) -> PreviewRender {
    let extension = Path::new(&preview.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let rich = preview.content.len() <= RICH_RENDER_LIMIT;
    match extension.as_str() {
        // Building the table parses every record and measures every cell, all
        // on the render thread, so a big file gets the plain reader instead.
        "csv" if rich => delimited_render(pinned, &preview.content, b','),
        "tsv" if rich => delimited_render(pinned, &preview.content, b'\t'),
        "json" if rich => PreviewRender::lines(
            pinned,
            parsed_json_lines(&preview.content)
                .unwrap_or_else(|| syntax_lines(&preview.content, &preview.path, Some("json"))),
        ),
        "jsonl" | "ndjson" if rich => PreviewRender::lines(pinned, json_lines(&preview.content)),
        _ if rich => {
            PreviewRender::lines(pinned, syntax_lines(&preview.content, &preview.path, None))
        }
        _ => plain_render(pinned, &preview.content),
    }
}

fn parsed_json_lines(content: &str) -> Option<Vec<Line<'static>>> {
    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let pretty = serde_json::to_string_pretty(&value).ok()?;
    Some(syntax_lines(&pretty, "preview.json", Some("json")))
}

fn json_lines(content: &str) -> Vec<Line<'static>> {
    let mut rendered = Vec::new();
    for (index, source) in content.lines().enumerate() {
        if source.trim().is_empty() {
            continue;
        }
        rendered.push(Line::styled(
            format!("record {}", index + 1),
            Style::default().fg(ACCENT).bold(),
        ));
        match serde_json::from_str::<serde_json::Value>(source) {
            Ok(value) => {
                let pretty =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| source.to_string());
                rendered.extend(syntax_lines(&pretty, "record.json", Some("json")));
            }
            Err(error) => {
                rendered.push(Line::styled(
                    format!("invalid JSON: {error}"),
                    Style::default().fg(Color::Red),
                ));
                rendered.push(Line::raw(source.to_string()));
            }
        }
        rendered.push(Line::raw(""));
    }
    rendered
}

/// Render delimited data as a numbered table. The column ruler and, when the
/// first record looks like a header, that header row are pinned above the
/// viewport so they stay in place while the reader pages through the file.
fn delimited_render(mut pinned: Vec<Line<'static>>, content: &str, delimiter: u8) -> PreviewRender {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(content.as_bytes());
    let mut rows: Vec<Vec<String>> = Vec::new();
    for record in reader.records() {
        match record {
            Ok(record) => rows.push(
                record
                    .iter()
                    .map(|cell| cell.replace(['\r', '\n'], " "))
                    .collect(),
            ),
            Err(error) => {
                return PreviewRender::lines(
                    pinned,
                    vec![Line::styled(
                        format!("Could not parse delimited data: {error}"),
                        Style::default().fg(Color::Red),
                    )],
                );
            }
        }
    }
    let first_data_row = usize::from(looks_like_header(&rows));
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![1; columns];
    for (index, width) in widths.iter_mut().enumerate() {
        *width = (index + 1).to_string().len();
    }
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index]
                .max(UnicodeWidthStr::width(cell.as_str()))
                .min(32);
        }
    }
    let gutter = rows
        .len()
        .saturating_sub(first_data_row)
        .to_string()
        .len()
        .max(1);
    pinned.push(table_line(
        &"#".repeat(gutter),
        &(1..=columns)
            .map(|index| index.to_string())
            .collect::<Vec<_>>(),
        &widths,
        gutter,
        Style::default().fg(MUTED),
    ));
    if first_data_row == 1 {
        pinned.push(table_line(
            "",
            &rows[0],
            &widths,
            gutter,
            Style::default().fg(ACCENT).bold(),
        ));
    }
    pinned.push(table_rule(&widths, gutter));
    PreviewRender {
        pinned,
        body: PreviewBody::Table(TableBody {
            rows,
            widths,
            gutter,
            first_data_row,
        }),
        layout: PreviewLayout::default(),
    }
}

impl TableBody {
    fn data_line(&self, index: usize) -> Line<'static> {
        let Some(row) = self.rows.get(self.first_data_row + index) else {
            return Line::default();
        };
        table_line(
            &(index + 1).to_string(),
            row,
            &self.widths,
            self.gutter,
            Style::default(),
        )
    }
}

/// A first record counts as a header when every field is a non-empty label that
/// is not a number, which is what a spreadsheet export looks like.
fn looks_like_header(rows: &[Vec<String>]) -> bool {
    let Some(first) = rows.first() else {
        return false;
    };
    rows.len() > 1
        && !first.is_empty()
        && first.iter().all(|cell| {
            let cell = cell.trim();
            !cell.is_empty() && cell.parse::<f64>().is_err()
        })
}

/// One table row: a right-aligned number gutter followed by the cells, each
/// padded to its column width so every row lines up.
fn table_line(
    number: &str,
    cells: &[String],
    widths: &[usize],
    gutter: usize,
    style: Style,
) -> Line<'static> {
    let muted = Style::default().fg(MUTED);
    let mut spans = vec![
        Span::styled("│", muted),
        Span::styled(
            format!(" {number:>gutter$} ", gutter = gutter),
            Style::default().fg(MUTED),
        ),
        Span::styled("│", muted),
    ];
    for (column, width) in widths.iter().enumerate() {
        let cell = cells.get(column).map(String::as_str).unwrap_or("");
        spans.push(Span::styled(" ", style));
        let (cell_spans, used) =
            truncate_spans(vec![Span::styled(cell.to_string(), style)], *width);
        spans.extend(cell_spans);
        spans.push(Span::styled(
            format!("{} ", " ".repeat(width.saturating_sub(used))),
            style,
        ));
        spans.push(Span::styled("│", muted));
    }
    Line::from(spans)
}

fn table_rule(widths: &[usize], gutter: usize) -> Line<'static> {
    let mut segments = vec!["─".repeat(gutter + 2)];
    segments.extend(widths.iter().map(|width| "─".repeat(width + 2)));
    Line::styled(
        format!("├{}┤", segments.join("┼")),
        Style::default().fg(MUTED),
    )
}

fn syntax_lines(content: &str, path: &str, token: Option<&str>) -> Vec<Line<'static>> {
    let syntaxes = &*SYNTAX_SET;
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str());
    let file_name = Path::new(path).file_name().and_then(|name| name.to_str());
    let first_line = content.lines().next().unwrap_or_default();
    let syntax = token
        .and_then(|token| syntaxes.find_syntax_by_token(token))
        .or_else(|| extension.and_then(|extension| syntaxes.find_syntax_by_extension(extension)))
        .or_else(|| file_name.and_then(|name| syntaxes.find_syntax_by_token(name)))
        .or_else(|| syntaxes.find_syntax_by_first_line(first_line))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &SYNTAX_THEME);
    let mut lines = Vec::new();
    for source in LinesWithEndings::from(content) {
        let Ok(regions) = highlighter.highlight_line(source, syntaxes) else {
            lines.push(Line::raw(source.trim_end_matches(['\r', '\n']).to_string()));
            continue;
        };
        let spans = regions
            .into_iter()
            .filter_map(|(style, value)| {
                let value = value.trim_end_matches(['\r', '\n']);
                (!value.is_empty()).then(|| Span::styled(value.to_string(), syntax_style(style)))
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

fn syntax_style(style: SyntaxStyle) -> Style {
    let mut rendered = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        rendered = rendered.bold();
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        rendered = rendered.italic();
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        rendered = rendered.underlined();
    }
    rendered
}

fn markdown_lines(content: &str) -> Vec<Line<'static>> {
    let source: Vec<_> = content.lines().collect();
    let mut lines = Vec::new();
    let mut code = false;
    let mut index = 0;
    while index < source.len() {
        let raw = source[index];
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            code = !code;
            index += 1;
        } else if code {
            lines.push(Line::styled(
                format!("  {raw}"),
                Style::default()
                    .fg(Color::Rgb(180, 210, 190))
                    .bg(Color::Rgb(28, 34, 32)),
            ));
            index += 1;
        } else if index + 1 < source.len()
            && markdown_table_cells(raw).is_some()
            && is_markdown_table_separator(source[index + 1])
        {
            let mut rows = vec![markdown_table_cells(raw).unwrap_or_default()];
            index += 2;
            while index < source.len() {
                let Some(row) = markdown_table_cells(source[index]) else {
                    break;
                };
                rows.push(row);
                index += 1;
            }
            lines.extend(data_table_lines(&rows, true));
        } else if let Some((level, heading)) = markdown_heading(trimmed) {
            let style = match level {
                1 => Style::default().fg(ACCENT).bold().underlined(),
                2 => Style::default().fg(Color::Yellow).bold(),
                3 => Style::default().fg(Color::Cyan).bold(),
                _ => Style::default().fg(Color::Gray).bold(),
            };
            lines.push(markdown_inline(heading, style));
            index += 1;
        } else if is_markdown_rule(trimmed) {
            lines.push(Line::styled(
                "─".repeat(48),
                Style::default().fg(Color::DarkGray),
            ));
            index += 1;
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled(" • ", Style::default().fg(ACCENT))];
            spans.extend(markdown_inline_spans(item, Style::default()));
            lines.push(Line::from(spans));
            index += 1;
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            let style = Style::default().fg(Color::Gray).italic();
            let mut spans = vec![Span::styled("│ ", style)];
            spans.extend(markdown_inline_spans(quote, style));
            lines.push(Line::from(spans));
            index += 1;
        } else {
            lines.push(markdown_inline(raw, Style::default()));
            index += 1;
        }
    }
    lines
}

fn markdown_heading(value: &str) -> Option<(usize, &str)> {
    for level in (1..=4).rev() {
        let prefix = format!("{} ", "#".repeat(level));
        if let Some(heading) = value.strip_prefix(&prefix) {
            return Some((level, heading));
        }
    }
    None
}

fn markdown_inline(value: &str, style: Style) -> Line<'static> {
    Line::from(markdown_inline_spans(value, style))
}

fn markdown_inline_spans(value: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("**") {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), style));
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find("**") else {
            spans.push(Span::styled(rest[start..].to_string(), style));
            rest = "";
            break;
        };
        spans.push(Span::styled(
            after[..end].to_string(),
            style.add_modifier(Modifier::BOLD),
        ));
        rest = &after[end + 2..];
    }
    if !rest.is_empty() || spans.is_empty() {
        spans.push(Span::styled(rest.to_string(), style));
    }
    spans
}

fn markdown_table_cells(value: &str) -> Option<Vec<String>> {
    if !value.contains('|') {
        return None;
    }
    let value = value.trim().trim_matches('|');
    let cells: Vec<_> = value
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect();
    (cells.len() >= 2).then_some(cells)
}

fn is_markdown_table_separator(value: &str) -> bool {
    markdown_table_cells(value).is_some_and(|cells| {
        cells.iter().all(|cell| {
            let cell = cell.trim_matches(':').trim();
            cell.len() >= 3 && cell.chars().all(|character| character == '-')
        })
    })
}

fn data_table_lines(rows: &[Vec<String>], markdown: bool) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![1; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let width = if markdown {
                markdown_inline_spans(cell, Style::default())
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum()
            } else {
                UnicodeWidthStr::width(cell.as_str())
            };
            widths[index] = widths[index].max(width).min(32);
        }
    }
    let mut lines = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let mut spans = vec![Span::styled("│", Style::default().fg(MUTED))];
        for (column, width) in widths.iter().enumerate() {
            let cell = row.get(column).map(String::as_str).unwrap_or("");
            let style = if row_index == 0 {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default()
            };
            spans.push(Span::styled(" ", style));
            let cell_spans = if markdown {
                markdown_inline_spans(cell, style)
            } else {
                vec![Span::styled(cell.to_string(), style)]
            };
            let (cell_spans, used) = truncate_spans(cell_spans, *width);
            spans.extend(cell_spans);
            spans.push(Span::styled(
                format!("{} ", " ".repeat(width.saturating_sub(used))),
                style,
            ));
            spans.push(Span::styled("│", Style::default().fg(MUTED)));
        }
        lines.push(Line::from(spans));
        if row_index == 0 {
            let separator = widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join("┼");
            lines.push(Line::styled(
                format!("├{separator}┤"),
                Style::default().fg(MUTED),
            ));
        }
    }
    lines
}

fn truncate_spans(spans: Vec<Span<'static>>, maximum: usize) -> (Vec<Span<'static>>, usize) {
    let mut rendered = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut value = String::new();
        for character in span.content.chars() {
            let width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used + width > maximum {
                break;
            }
            value.push(character);
            used += width;
        }
        if !value.is_empty() {
            rendered.push(Span::styled(value, span.style));
        }
        if used >= maximum {
            break;
        }
    }
    (rendered, used)
}

fn is_markdown_rule(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact.len() >= 3
        && compact
            .chars()
            .next()
            .is_some_and(|marker| matches!(marker, '-' | '*' | '_'))
        && compact
            .chars()
            .all(|character| character == compact.chars().next().unwrap())
}

pub(crate) fn format_bytes(size: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", size, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn draw_help_modal(frame: &mut Frame<'_>, form: &mut HelpForm, outer: Rect) {
    let lines = vec![
        help_header("Navigation"),
        help_row(
            "Cmd / Option / Alt + Left/Right",
            "macOS Cmd or Option; Windows/Linux Alt; horizontal neighbor",
        ),
        help_row(
            "Cmd/Option/Alt + Up/Down",
            "Move to a visible vertical neighbor",
        ),
        help_row("Arrows in terminal", "Forward directly to the agent editor"),
        help_row("Up/Down / j/k", "Move the current selection"),
        help_row("Alt-1 / 2 / 3", "Jump directly to a pane"),
        help_row("Mouse click", "Focus and select an item"),
        help_row("Drag divider", "Resize and save the current layout split"),
        Line::raw(""),
        help_header("Launch"),
        help_row("n / Ctrl-n", "Start the runtime and path flow"),
        help_row(
            "t in Agents",
            "Choose a runtime for a no-history Temporal Chat",
        ),
        help_row(
            "Left / Right",
            "Choose one of the runtimes this machine has",
        ),
        help_row("Tab", "Move between launch fields"),
        help_row("Enter on path", "Open the local or remote folder picker"),
        help_row("Enter in picker", "Confirm folder; choose New or Resume"),
        Line::raw(""),
        help_header("Sessions"),
        help_row(
            "Enter / click",
            "Focus a running terminal or resume an archived agent",
        ),
        help_row(
            "Cmd/Option/Alt + arrow",
            "Leave terminal by the visible layout",
        ),
        help_row(
            "Shift/Option + Enter",
            "Insert a newline without submitting",
        ),
        help_row("Ctrl-c / Ctrl-d", "Forward directly to the focused session"),
        help_row("Mouse drag", "Select terminal text; Alt-drag forwards"),
        help_row(
            "Right-click terminal",
            "Copy the selection, or paste when there is none",
        ),
        help_row(
            "Cmd-c / Ctrl-Shift-c",
            "Copy the selection; plain Ctrl-c goes to the agent",
        ),
        help_row("x", "Archive live agents; directly destroy a Temporal Chat"),
        help_row("a", "Expand or collapse Archived sessions"),
        help_row(
            "Space",
            "Fold away the subagents an agent started, or list them",
        ),
        help_row("e", "Rename the selected agent's display name"),
        help_row("p", "Configure local forwarding to the selected machine"),
        help_row("d in Ports", "Stop the highlighted active forward"),
        help_row("Up twice at top", "Open the first agent waiting for input"),
        Line::raw(""),
        help_header("Machines"),
        help_row(
            "Space / double-click [x]",
            "Enable or disable; clicks elsewhere only select",
        ),
        help_row("v / Ctrl-h", "Hide disabled machines or show all"),
        help_row("r / Ctrl-r", "Refresh enabled machines now"),
        help_row(
            "» at the bottom",
            "A machine another muxloom reaches; agents here can look at it",
        ),
        Line::raw(""),
        help_header("Channels"),
        help_row(
            "c in Machines",
            "Bind WeChat or Lark so any agent in the fleet can reach you",
        ),
        help_row(
            "n there",
            "Bind a chat: WeChat is a scan, Lark asks for two keys",
        ),
        help_row(
            "Lark's chat list",
            "The app's own chats, so no id has to be dug out of a URL",
        ),
        help_row(
            "r on the code",
            "A fresh code, once the old one has timed out",
        ),
        help_row("e / x there", "Rename a binding, or remove it"),
        help_row("Enter there", "Where a message that names no channel goes"),
        help_row("t there", "Send yourself a test message through it"),
        help_row(
            "Say anything in WeChat",
            "A new binding cannot answer until you have spoken to it once",
        ),
        help_row(
            "Reply in the chat",
            "Answers the agent whose message you replied to; /who lists them",
        ),
        Line::raw(""),
        help_header("Moderators"),
        help_row(
            "Top row of Machines",
            "Agents muxloom runs to coordinate the others",
        ),
        help_row("n there", "Name one and choose what it looks after"),
        help_row(
            "Space in its form",
            "Check a machine or agent; on a header, the group",
        ),
        Line::raw(""),
        help_header("History And Search"),
        help_row("Wheel / PageUp", "Scroll one line / move one history page"),
        help_row(
            "Alt+Wheel / F2",
            "Take the wheel from an app that claimed it (F2 holds it)",
        ),
        help_row("PageDown", "Move back toward the live terminal"),
        help_row("/ / Ctrl-p", "Search every discovered agent history"),
        help_row("Enter in search", "Open the selected match"),
        Line::raw(""),
        help_header("Talk Board"),
        help_row(
            "b / footer ● chip",
            "Open what every machine has been saying",
        ),
        help_row(
            "Tab / Left / Right",
            "All, Global, Machine, Path, Task, Direct",
        ),
        help_row(
            "Task tab",
            "What the selected agent's whole task said, indented by who started whom",
        ),
        help_row("Enter in board", "Expand the selected message in full"),
        help_row(
            "p / r in board",
            "Post to the open scope; reply to the selection",
        ),
        help_row("/ in board", "Narrow to messages matching what you type"),
        Line::raw(""),
        help_header("File Manager"),
        help_row(
            "Ctrl-f",
            "Open or close files on the selected agent machine",
        ),
        help_row("Arrows / Enter", "Select, move to parent, or open an entry"),
        help_row("/pattern", "Search subfolder filenames with * or **"),
        help_row("Type text", "Filter the entries in the browsed directory"),
        help_row("Ctrl-d", "Download the selected file to Downloads"),
        help_row("Ctrl-y", "Copy the selected target path to the clipboard"),
        help_row(
            "Right-click preview",
            "Copy selected preview text, else go to the parent",
        ),
        help_row("Drop local files", "Upload them into the visible directory"),
        Line::raw(""),
        help_header("Touch Screens"),
        help_row("Swipe a list", "Scroll it; a tap selects what it lands on"),
        help_row("Swipe the terminal", "Walk through the scrollback"),
        help_row(
            "Long-press then drag",
            "Select terminal or file preview text",
        ),
        help_row("Swipe sideways", "Move panes where only one is on screen"),
        Line::raw(""),
        help_header("View And Configuration"),
        help_row("f", "Toggle grouped and flat agent views"),
        help_row(
            ",",
            "Edit configuration for the selected machine; force its ⟳ daemon update",
        ),
        help_row("Ctrl-,", "Edit global configuration defaults"),
        help_row("?", "Open or close this help"),
        help_row("q", "Quit the dashboard; leave agents running"),
    ];
    debug_assert_eq!(lines.len(), HELP_CONTENT_ROWS);

    let area = centered_rect(92, 30, outer);
    frame.render_widget(Clear, area);
    let visible_height = area.height.saturating_sub(3).max(1) as usize;
    let max_offset = lines.len().saturating_sub(visible_height);
    form.offset = form.offset.min(max_offset);
    let first = form.offset + 1;
    let last = (form.offset + visible_height).min(lines.len());
    let title = format!(" Help  {first}-{last}/{} ", lines.len());
    let block = panel(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let content = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    frame.render_widget(
        Paragraph::new(lines).scroll((form.offset as u16, 0)),
        content,
    );
    frame.render_widget(
        Paragraph::new("Up/Down or wheel scroll   PgUp/PgDn page   Home/End jump   Esc close")
            .style(Style::default().fg(MUTED)),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
}

fn help_header(title: &'static str) -> Line<'static> {
    Line::styled(
        title,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )
}

fn help_row(shortcut: &'static str, action: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {shortcut:<20}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(action, Style::default().fg(Color::Gray)),
    ])
}

fn draw_path_picker(frame: &mut Frame<'_>, form: &PathPickerForm, outer: Rect) {
    let area = centered_rect(92, 27, outer);
    frame.render_widget(Clear, area);
    let title = format!(
        " Folders on {} ",
        truncate(
            &form.launch.target.label,
            area.width.saturating_sub(16) as usize
        )
    );
    let block = panel(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new(truncate(&form.path, inner.width as usize))
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            row,
        );
    }
    let query_prefix = "Match: ";
    if let Some(row) = modal_row(inner, 1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(query_prefix, Style::default().fg(ACCENT)),
                Span::styled(form.query.as_str(), Style::default().fg(Color::White)),
            ])),
            row,
        );
    }
    let matches = form.matches();
    let status = if form.loading {
        "Loading folders..."
    } else if let Some(error) = &form.error {
        error
    } else if form.directories.is_empty() {
        "No child folders"
    } else if matches.is_empty() {
        "No folders match the current text"
    } else {
        ""
    };
    if let Some(row) = modal_row(inner, 2) {
        frame.render_widget(
            Paragraph::new(truncate(status, inner.width as usize)).style(Style::default().fg(
                if form.error.is_some() {
                    Color::Red
                } else {
                    MUTED
                },
            )),
            row,
        );
    }
    let available = inner.height.saturating_sub(5) as usize;
    let start = if form.selected >= available && available > 0 {
        form.selected + 1 - available
    } else {
        0
    };
    for (visible, (index, directory)) in matches
        .iter()
        .enumerate()
        .skip(start)
        .take(available)
        .enumerate()
    {
        let Some(row) = modal_row(inner, 3 + visible as u16) else {
            break;
        };
        let selected = index == form.selected;
        let row_text = format!("{} {directory}/", if selected { ">" } else { " " });
        frame.render_widget(
            Paragraph::new(truncate(&row_text, inner.width as usize)).style(if selected {
                Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default().fg(Color::Gray)
            }),
            row,
        );
    }
    if let Some(row) = modal_row(inner, inner.height.saturating_sub(1)) {
        frame.render_widget(
            Paragraph::new(truncate(
                "Type to match  Backspace/Ctrl-u edit  Arrows navigate  Enter use  Esc back",
                inner.width as usize,
            ))
            .style(Style::default().fg(MUTED)),
            row,
        );
    }
    if !form.loading && inner.height > 1 {
        let cursor_x = inner
            .x
            .saturating_add(query_prefix.len() as u16)
            .saturating_add(UnicodeWidthStr::width(form.query.as_str()) as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, inner.y + 1));
    }
}

fn draw_resume_modal(frame: &mut Frame<'_>, form: &ResumeForm, outer: Rect) {
    let area = centered_rect(96, 27, outer);
    frame.render_widget(Clear, area);
    let title = format!(
        " Start {} in {} ",
        form.launch.kind,
        truncate(&form.launch.path, area.width.saturating_sub(24) as usize)
    );
    let block = panel(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    // Second-to-last row hosts the cross-machine search box; the list above it
    // shows candidates (collapsed) or backed-up conversations (expanded).
    let search_y = footer_y.saturating_sub(1);
    draw_resume_history_panel(frame, form, inner, search_y);
    if form.history_active() {
        frame.render_widget(
            Paragraph::new(
                "Up/Down select   Enter reference (cross-machine)   Backspace edit   Esc back",
            )
            .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
        return;
    }

    let new_selected = form.selected == 0;
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new(if new_selected {
                "> New session"
            } else {
                "  New session"
            })
            .style(if new_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            }),
            row,
        );
    }
    let status = if form.loading {
        "Scanning agent history... Enter starts New immediately"
    } else if let Some(error) = &form.error {
        error
    } else if form.candidates.is_empty() {
        "No matching history; press Enter for a new session"
    } else {
        "Resume matching history or reference the other agent's history"
    };
    if let Some(row) = modal_row(inner, 1) {
        frame.render_widget(
            Paragraph::new(truncate(status, inner.width as usize)).style(Style::default().fg(
                if form.error.is_some() {
                    Color::Yellow
                } else {
                    MUTED
                },
            )),
            row,
        );
    }

    let available = inner.height.saturating_sub(4) as usize;
    let selected_candidate = form.selected.saturating_sub(1);
    let start = selected_candidate.saturating_sub(available.saturating_sub(4));
    let mut y = inner.y + 2;
    let last_y = search_y;
    for (index, candidate) in form.candidates.iter().enumerate().skip(start) {
        let selected = form.selected == index + 1;
        let details: Vec<(&str, &str)> = if selected {
            if let Some(recap) = candidate.recap.as_deref() {
                vec![("recap", recap)]
            } else {
                let mut messages = Vec::new();
                if let Some(first) = candidate.first_message.as_deref() {
                    messages.push(("first", first));
                }
                if let Some(last) = candidate.last_message.as_deref()
                    && candidate.first_message.as_deref() != Some(last)
                {
                    messages.push(("last", last));
                }
                messages
            }
        } else {
            Vec::new()
        };
        let height = 1 + usize::from(selected) + details.len();
        if y.saturating_add(height as u16) > last_y {
            break;
        }
        let background = if selected {
            Color::Rgb(42, 48, 58)
        } else {
            Color::Reset
        };
        frame.render_widget(
            Paragraph::new(truncate(
                &format!(
                    "{} {} {}  {}",
                    if selected { ">" } else { " " },
                    agent_visual(candidate.kind).0,
                    if candidate.kind == form.launch.kind {
                        "Resume"
                    } else {
                        "Reference"
                    },
                    candidate.summary()
                ),
                inner.width as usize,
            ))
            .style(Style::default().fg(Color::White).bg(background)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
        if selected {
            frame.render_widget(
                Paragraph::new(truncate(
                    &format!("    {}  {}", candidate.updated_at, candidate.id),
                    inner.width as usize,
                ))
                .style(Style::default().fg(MUTED).bg(background)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
            for (label, value) in details {
                frame.render_widget(
                    Paragraph::new(truncate(
                        &format!("    {label:<5}  {value}"),
                        inner.width as usize,
                    ))
                    .style(Style::default().fg(Color::Gray).bg(background)),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }
    }
    frame.render_widget(
        Paragraph::new(
            "Up/Down select   Enter launch   type to search other machines   Left/Esc back",
        )
        .style(Style::default().fg(MUTED)),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

/// The cross-machine reference panel shown at the bottom of the resume modal.
/// Collapsed to a one-line prompt until the user types a query, then it expands
/// to a searchable list of backed-up conversations from every machine.
fn draw_resume_history_panel(frame: &mut Frame<'_>, form: &ResumeForm, inner: Rect, search_y: u16) {
    let query_line = if form.query.is_empty() {
        " Type to search & reference history on any machine".to_string()
    } else {
        format!(" Search all machines: {}", form.query)
    };
    frame.render_widget(
        Paragraph::new(truncate(&query_line, inner.width as usize))
            .style(Style::default().fg(if form.query.is_empty() { MUTED } else { ACCENT })),
        Rect::new(inner.x, search_y, inner.width, 1),
    );
    if !form.history_active() {
        return;
    }
    // Draw the expanded hit list over the candidate area (from the top).
    let body_top = inner.y + 2;
    let mut y = body_top;
    // The hits below belong to `searched_query`, which is only the query on
    // screen once the reading is over. Until then they are the last question's
    // answers, and saying so is the difference between a list that lags and a
    // list that lies.
    let status = if form.history_loading || form.searched_query != form.query.trim() {
        "Searching backed-up history..."
    } else if form.history_hits.is_empty() {
        "No matching history on any machine"
    } else {
        "Enter references the selected conversation (transcript is injected as context)"
    };
    frame.render_widget(
        Paragraph::new(truncate(status, inner.width as usize)).style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    let rows = search_y.saturating_sub(body_top) as usize;
    let start = form
        .history_selected
        .saturating_sub(rows.saturating_sub(2).max(1));
    for (index, hit) in form.history_hits.iter().enumerate().skip(start) {
        if y >= search_y {
            break;
        }
        let selected = index == form.history_selected;
        let glyph = hit
            .kind
            .parse::<AgentKind>()
            .map(|k| agent_visual(k).0)
            .unwrap_or("•");
        let title = if hit.title.trim().is_empty() {
            hit.snippet.as_str()
        } else {
            hit.title.as_str()
        };
        let background = if selected {
            Color::Rgb(42, 48, 58)
        } else {
            Color::Reset
        };
        frame.render_widget(
            Paragraph::new(truncate(
                &format!(
                    "{} {glyph} [{}]  {title}",
                    if selected { ">" } else { " " },
                    hit.target_id
                ),
                inner.width as usize,
            ))
            .style(Style::default().fg(Color::White).bg(background)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
        if selected && y < search_y {
            frame.render_widget(
                Paragraph::new(truncate(
                    &format!("    {}", hit.snippet),
                    inner.width as usize,
                ))
                .style(Style::default().fg(Color::Gray).bg(background)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }
}

/// A bar of `width` cells, `done` of `total` filled. Nothing done still shows
/// the track, so the bar appears at its full length the moment work starts
/// rather than growing out of nothing.
fn progress_bar(done: usize, total: usize, width: usize) -> String {
    let filled = if total == 0 {
        width
    } else {
        (done * width).div_ceil(total).min(width)
    };
    format!("[{}{}]", "━".repeat(filled), "·".repeat(width - filled))
}

fn draw_search_modal(frame: &mut Frame<'_>, form: &mut SearchForm, outer: Rect) {
    let area = centered_rect(104, 31, outer);
    frame.render_widget(Clear, area);
    let block = panel(" Search all agent history ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let query_prefix = "Search: ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(query_prefix, Style::default().fg(ACCENT)),
            Span::styled(form.query.as_str(), Style::default().fg(Color::White)),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let status = if let Some((read, total)) = form.reading {
        // Names and recaps are already listed by now; what is left is reading
        // scrollback, which is the part worth a bar.
        format!(
            "{} {read}/{total} histories read{}",
            progress_bar(read, total, 24),
            if form.results.is_empty() {
                String::new()
            } else {
                format!("; {} matches so far", form.results.len())
            }
        )
    } else if form.loading {
        "Searching full tmux scrollback on all discovered machines...".to_string()
    } else if let Some(error) = &form.error {
        error.clone()
    } else if !form.results.is_empty() {
        format!(
            "{} matches; exact optional name/path, recap, then newest history",
            form.results.len()
        )
    } else if form.query.trim().chars().count() >= 2 {
        "Search starts after a short typing pause; Enter runs it now".into()
    } else {
        "Type at least two characters for live search, or press Enter".into()
    };
    frame.render_widget(
        Paragraph::new(truncate(&status, inner.width as usize)).style(Style::default().fg(
            if form.error.is_some() {
                Color::Yellow
            } else {
                MUTED
            },
        )),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    let visible_results = inner.height.saturating_sub(3) as usize / 3;
    let start = if form.selected >= visible_results && visible_results > 0 {
        form.selected + 1 - visible_results
    } else {
        0
    };
    let mut result_rows = Vec::new();
    for (visible_index, (index, result)) in form
        .results
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_results)
        .enumerate()
    {
        let y = inner.y + 2 + (visible_index * 3) as u16;
        let selected = index == form.selected;
        let match_color = match result.match_kind {
            SearchMatchKind::Name => ACCENT,
            SearchMatchKind::Recap => Color::Yellow,
            SearchMatchKind::History => Color::Gray,
        };
        let state = if result.dead { " archived" } else { "" };
        let first = format!(
            "{} [{}] {} / {}{}",
            if selected { ">" } else { " " },
            result.kind,
            result.target_id,
            result.label,
            state
        );
        let line = result
            .line_number
            .map(|line| format!(" line {line}"))
            .unwrap_or_default();
        let second = format!("  {}", result.path);
        let third = format!("  [{}{}] {}", result.match_kind, line, result.snippet);
        let background = if selected {
            Color::Rgb(42, 48, 58)
        } else {
            Color::Reset
        };
        frame.render_widget(
            Paragraph::new(truncate(&first, inner.width as usize))
                .style(Style::default().fg(Color::White).bg(background)),
            Rect::new(inner.x, y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate(&second, inner.width as usize))
                .style(Style::default().fg(MUTED).bg(background)),
            Rect::new(inner.x, y + 1, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate(&third, inner.width as usize))
                .style(Style::default().fg(match_color).bg(background)),
            Rect::new(inner.x, y + 2, inner.width, 1),
        );
        result_rows.push((index, Rect::new(inner.x, y, inner.width, 3)));
    }
    form.result_rows = result_rows;
    frame.render_widget(
        Paragraph::new("Type to search   Up/Down or wheel select   Enter open   Esc close")
            .style(Style::default().fg(MUTED)),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
    if !form.loading {
        let cursor_x = inner
            .x
            .saturating_add(query_prefix.len() as u16)
            .saturating_add(UnicodeWidthStr::width(form.query.as_str()) as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor_x, inner.y));
    }
}

/// The colour a scope reads in. Global is the accent everyone shares, a machine
/// borrows the header's cyan, a directory takes the green the terminals use, a
/// task takes magenta because it cuts across all three, and a direct message is
/// yellow because it was aimed at someone.
fn board_scope_color(message: &TalkMessage) -> Color {
    if message.kind == TalkKind::Direct {
        return Color::Yellow;
    }
    match message.scope {
        TalkScope::Global => ACCENT,
        TalkScope::Machine { .. } => CODEX,
        TalkScope::Path { .. } => TERMINAL,
        TalkScope::Task { .. } => Color::Magenta,
    }
}

/// How a message names where it was said: the tab it would sit under.
fn board_scope_tag(message: &TalkMessage) -> &'static str {
    if message.kind == TalkKind::Direct {
        "direct"
    } else {
        message.scope.name()
    }
}

/// How deep in the task a message sits, for the indentation on the Task tab.
///
/// Whoever said it decides it. A message from outside the task sits on the
/// line of the session it was said to instead — a person at a dashboard
/// writing to one subagent belongs beside that subagent, not at the top of a
/// tree they are not in.
fn board_task_depth(message: &TalkMessage, task: &BTreeMap<String, usize>) -> usize {
    message
        .author
        .voice
        .session_id
        .as_deref()
        .and_then(|id| task.get(id))
        .or_else(|| {
            message
                .to
                .as_ref()
                .and_then(|to| task.get(to.session_id.as_str()))
        })
        .copied()
        .unwrap_or(0)
}

/// Who said it, short enough for a line: `name@machine` plus the last part of
/// the directory when the message belongs to one, since the whole path is
/// rarely what tells two channels apart.
fn board_author(message: &TalkMessage) -> String {
    let machine = if message.author.machine_label.is_empty() {
        message.author.machine.as_str()
    } else {
        message.author.machine_label.as_str()
    };
    let mut who = format!("{}@{}", message.author.voice.name(), machine);
    if let Some(path) = message.scope.path() {
        let leaf = Path::new(path)
            .file_name()
            .map(|leaf| leaf.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        if !leaf.is_empty() {
            who.push(':');
            who.push_str(&leaf);
        }
    }
    who
}

/// The board: everything every machine has said, in the order it was said.
///
/// Drawn from [`App::board_view`] rather than from anything held on the form,
/// so switching tabs costs a filter over what is already in hand instead of a
/// round trip to the daemons.
fn draw_board_modal(frame: &mut Frame<'_>, app: &App, form: &mut BoardForm, outer: Rect) {
    let area = centered_rect(110, 32, outer);
    frame.render_widget(Clear, area);
    let block = panel(" Talk board ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    form.rows.clear();
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let view = app.board_view(form.tab, &form.query);
    // Who is working with whom, for the Task tab's indentation. The other tabs
    // have no hierarchy to draw and do not pay for one.
    let task = if form.tab == BoardTab::Task {
        app.selected_task()
    } else {
        BTreeMap::new()
    };
    let selected_at = form
        .selected
        .as_ref()
        .and_then(|id| view.iter().position(|message| message.id == *id));
    // Nothing selected follows the newest, which is also what gets expanded.
    let opened = selected_at
        .map(|at| view[at])
        .or_else(|| view.last().copied());

    // The tab strip, then the hint line at the bottom, then the status or
    // compose line above it — whatever height is left over is the board.
    let mut rest = inner.height - 1;
    let tabs = Rect::new(inner.x, inner.y, inner.width, 1);
    let hints = (rest > 0).then(|| {
        rest -= 1;
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1)
    });
    let status = (rest > 0).then(|| {
        rest -= 1;
        Rect::new(inner.x, inner.y + inner.height - 2, inner.width, 1)
    });
    // An expanded message takes the room its own text needs and no more, up to
    // half the board, and only while there is still a list to read above it.
    let wrapped = opened
        .filter(|_| form.expanded && rest >= 6)
        .map(|message| wrap_display(&message.text, inner.width as usize))
        .unwrap_or_default();
    let detail_height = if wrapped.is_empty() {
        0
    } else {
        // One row for the stamp line, one for the rule the block draws.
        (wrapped.len() as u16 + 2).min(rest / 2).min(10)
    };
    let list_height = (rest - detail_height) as usize;
    form.page = list_height.max(1);

    let mut tab_spans = Vec::new();
    for tab in BoardTab::ORDER {
        let here = tab == form.tab;
        tab_spans.push(Span::styled(
            if here {
                format!("[{}] ", tab.title())
            } else {
                format!(" {}  ", tab.title())
            },
            if here {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            },
        ));
    }
    // The Task tab is the one view that answers a question about the agent
    // list rather than about the board, so it says which task it is showing.
    let heading = task
        .iter()
        .find(|(_, depth)| **depth == 0)
        .map(|(root, _)| {
            app.sessions
                .iter()
                .find(|session| session.id == *root)
                .map_or_else(|| root.clone(), |session| session.display_label().into())
        })
        .map(|label| format!(" · {}", truncate(&label, 18)))
        .unwrap_or_default();
    let counted = format!(
        "{} of {}{heading} · times UTC ",
        view.len(),
        app.board.messages.len()
    );
    let used: usize = tab_spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let gap = (inner.width as usize)
        .saturating_sub(used + UnicodeWidthStr::width(counted.as_str()))
        .max(1);
    tab_spans.push(Span::raw(" ".repeat(gap)));
    tab_spans.push(Span::styled(counted, Style::default().fg(MUTED)));
    frame.render_widget(
        Paragraph::new(Line::from(
            truncate_spans(tab_spans, inner.width as usize).0,
        )),
        tabs,
    );

    // Nothing selected means following the newest, which is the bottom of the
    // board; a selection keeps itself in the middle of what is on screen.
    let max_start = view.len().saturating_sub(list_height);
    let start = match selected_at {
        None => max_start,
        Some(at) => at.saturating_sub(list_height / 2).min(max_start),
    };
    // A board is read from the bottom: a half-full one leaves the gap above the
    // messages, not below them, so the newest line is always in the same place.
    let shown = view.len().saturating_sub(start).min(list_height);
    let head = inner.y + 1 + (list_height - shown) as u16;
    if view.is_empty() && list_height > 0 {
        frame.render_widget(
            Paragraph::new(if !form.query.trim().is_empty() {
                "Nothing here matches."
            } else if form.tab == BoardTab::Task && task.is_empty() {
                "Select an agent to see the task it is part of."
            } else {
                "Nothing said here yet."
            })
            .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, head - 1, inner.width, 1),
        );
    }
    for (offset, message) in view.iter().skip(start).take(list_height).enumerate() {
        let row = Rect::new(inner.x, head + offset as u16, inner.width, 1);
        let here = selected_at == Some(start + offset);
        let background = if here { GROUP_BAND } else { Color::Reset };
        let voice = if message.author.voice.human {
            Color::White
        } else {
            message
                .author
                .voice
                .kind
                .as_deref()
                .and_then(|kind| kind.parse::<AgentKind>().ok())
                .map(|kind| agent_visual(kind).2)
                .unwrap_or(Color::Gray)
        };
        let marker = match message.kind {
            TalkKind::Note => "✎ ",
            _ if message.reply_to.is_some() => "↳ ",
            _ => "",
        };
        let spans = vec![
            Span::styled(
                format!("{} ", clock_utc(message.ts)),
                Style::default().fg(MUTED).bg(background),
            ),
            Span::styled(
                format!("{:<7} ", board_scope_tag(message)),
                Style::default()
                    .fg(board_scope_color(message))
                    .bg(background),
            ),
            Span::styled(
                format!(
                    "{:<26} ",
                    truncate(
                        &format!(
                            "{}{}",
                            "  ".repeat(board_task_depth(message, &task)),
                            board_author(message)
                        ),
                        26
                    )
                ),
                Style::default().fg(voice).bg(background),
            ),
            Span::styled(
                format!("{marker}{}", folded(&message.text)),
                Style::default().fg(Color::White).bg(background),
            ),
        ];
        let (spans, width) = truncate_spans(spans, inner.width as usize);
        let mut spans = spans;
        // The highlight has to run to the edge or the selected row reads as a
        // ragged block rather than a line.
        if here && width < inner.width as usize {
            spans.push(Span::styled(
                " ".repeat(inner.width as usize - width),
                Style::default().bg(background),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
        form.rows.push((message.id.clone(), row));
    }

    if let Some(message) = opened.filter(|_| detail_height > 0) {
        let detail = Rect::new(
            inner.x,
            inner.y + 1 + list_height as u16,
            inner.width,
            detail_height,
        );
        let answered = message
            .reply_to
            .as_ref()
            .map(|id| format!(" · reply to {id}"));
        let mut lines = vec![Line::styled(
            truncate(
                &format!(
                    "{} · {} · {}{}",
                    civil_utc(message.ts),
                    board_scope_tag(message),
                    board_author(message),
                    answered.unwrap_or_default()
                ),
                inner.width as usize,
            ),
            Style::default().fg(MUTED),
        )];
        // The rule the block draws costs a row, and the stamp above costs
        // another; what is left is how much of the message fits.
        lines.extend(
            wrapped
                .into_iter()
                .take(detail_height as usize - 2)
                .map(Line::raw),
        );
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(MUTED)),
            ),
            detail,
        );
    }

    if let Some(status) = status {
        let (line, style) = if let Some(text) = form.compose.as_ref() {
            let into = form
                .reply_to
                .as_ref()
                .map(|id| format!("Reply to {id}"))
                .unwrap_or_else(|| format!("Post to {}", form.tab.title().to_lowercase()));
            (
                format!("{into}: {text}█"),
                Style::default().fg(Color::White),
            )
        } else if form.searching {
            (
                format!("Find: {}█", form.query),
                Style::default().fg(Color::White),
            )
        } else if let Some(error) = form.error.as_ref() {
            (error.clone(), Style::default().fg(Color::Yellow))
        } else if form.query.trim().is_empty() {
            (
                "Everyone posts here as themselves — agents and you alike.".into(),
                Style::default().fg(MUTED),
            )
        } else {
            (
                format!("Filtered by \"{}\" — Esc in / clears it", form.query),
                Style::default().fg(MUTED),
            )
        };
        frame.render_widget(
            Paragraph::new(truncate(&line, status.width as usize)).style(style),
            status,
        );
    }
    if let Some(hints) = hints {
        let help = if form.compose.is_some() {
            "Enter send    Ctrl-u clear    Esc cancel"
        } else if form.searching {
            "Enter keep filter    Esc clear"
        } else if hints.width < 84 {
            "Tab scope  j/k move  p post  Esc close"
        } else {
            "Tab scope   j/k move   Enter expand   / find   p post   r reply   Esc close"
        };
        frame.render_widget(
            Paragraph::new(truncate(help, hints.width as usize)).style(Style::default().fg(MUTED)),
            hints,
        );
    }
}

fn draw_settings_modal(frame: &mut Frame<'_>, form: &SettingsForm, outer: Rect) {
    let area = centered_rect(92, 23, outer);
    frame.render_widget(Clear, area);
    let title = match &form.scope {
        SettingsScope::Global => " Global settings ".to_string(),
        SettingsScope::Host(target) => format!(
            " Settings for {} ",
            truncate(target, area.width.saturating_sub(18) as usize)
        ),
    };
    let block = panel(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let label_width = 27u16.min(inner.width / 2);
    let value_width = inner.width.saturating_sub(label_width + 1) as usize;
    let visible_rows = inner.height.saturating_sub(3) as usize;
    // Lay every row out first — section headings interleaved with fields —
    // then scroll so the selected field stays visible.
    let mut lines: Vec<(Option<usize>, Line)> = Vec::new();
    let mut field_index = 0usize;
    let mut focus_index = 0usize;
    let mut note_index = 0usize;
    let mut selected_row = 0usize;
    for row in form.rows() {
        match row {
            SettingsRow::Note(label) => {
                let value = form.notes.get(note_index).cloned().unwrap_or_default();
                lines.push((
                    None,
                    Line::from(vec![
                        Span::styled(
                            format!("  {label:<width$}", width = label_width as usize - 2),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            truncate(&value, value_width),
                            Style::default().fg(Color::Gray),
                        ),
                    ]),
                ));
                note_index += 1;
            }
            SettingsRow::Action(label, hint) => {
                let active = focus_index == form.selected;
                if active {
                    selected_row = lines.len();
                }
                lines.push((
                    None,
                    Line::from(vec![
                        Span::styled(
                            format!("  {label:<width$}", width = label_width as usize - 2),
                            if active {
                                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::Gray)
                            },
                        ),
                        Span::raw(" "),
                        Span::styled(
                            truncate(hint, value_width),
                            Style::default().fg(if active { Color::White } else { MUTED }),
                        ),
                    ]),
                ));
                focus_index += 1;
            }
            SettingsRow::Section(title) => {
                if !lines.is_empty() {
                    lines.push((None, Line::raw("")));
                }
                lines.push((
                    None,
                    Line::styled(
                        format!("— {title} —"),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                ));
            }
            SettingsRow::Field(label) => {
                let value = form.values.get(field_index).cloned().unwrap_or_default();
                let active = focus_index == form.selected;
                if active {
                    selected_row = lines.len();
                }
                let shown = tail_display(&value, value_width);
                lines.push((
                    Some(field_index),
                    Line::from(vec![
                        Span::styled(
                            format!("  {label:<width$}", width = label_width as usize - 2),
                            Style::default().fg(if active { ACCENT } else { Color::Gray }),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            shown,
                            if active {
                                Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ),
                    ]),
                ));
                field_index += 1;
                focus_index += 1;
            }
        }
    }
    let start = selected_row.saturating_add(1).saturating_sub(visible_rows);
    let mut cursor_position = None;
    for (visible_index, (field, line)) in
        lines.into_iter().skip(start).take(visible_rows).enumerate()
    {
        let row = Rect::new(inner.x, inner.y + visible_index as u16, inner.width, 1);
        if let Some(selected_value) = form.selected_value()
            && field == Some(selected_value)
            && let Some(value) = form.values.get(selected_value)
        {
            let shown = tail_display(value, value_width);
            cursor_position = Some((
                inner
                    .x
                    .saturating_add(label_width + 1)
                    .saturating_add(UnicodeWidthStr::width(shown.as_str()) as u16)
                    .min(inner.x + inner.width.saturating_sub(1)),
                row.y,
            ));
        }
        frame.render_widget(Paragraph::new(line), row);
    }
    let error_y = inner.y + inner.height.saturating_sub(2);
    if let Some(error) = &form.error {
        frame.render_widget(
            Paragraph::new(truncate(error, inner.width as usize))
                .style(Style::default().fg(Color::Red)),
            Rect::new(inner.x, error_y, inner.width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(
            "Shell syntax: --flag 'value' / A=value   Tab field   Enter save   Esc cancel",
        )
        .style(Style::default().fg(MUTED)),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
    if let Some(position) = cursor_position {
        frame.set_cursor_position(position);
    }
}

/// The runtime row, with the hint that Left/Right change it. Only the
/// runtimes the machine offers are on it, so the row is as short as the
/// machine is bare.
fn kind_line(kinds: &[AgentKind], current: AgentKind) -> Line<'static> {
    let mut spans = Vec::with_capacity(kinds.len() * 2 + 1);
    for kind in kinds {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        let (_, _, color) = agent_visual(*kind);
        spans.push(segment(
            format!(" {} ", kind.as_str().to_uppercase()),
            *kind == current,
            color,
        ));
    }
    spans.push(Span::styled("  Left/Right", Style::default().fg(MUTED)));
    Line::from(spans)
}

/// The launch form as one row per field, for a terminal too short to hold the
/// captioned layout. Without this the captions win the row budget and the path
/// and label the user is typing into simply stop being drawn.
fn draw_launch_modal_compact(
    frame: &mut Frame<'_>,
    form: &LaunchForm,
    content: Rect,
    kinds: &[AgentKind],
) {
    const FIELDS: usize = 3;
    let focused = match form.field {
        LaunchField::Kind => 0usize,
        LaunchField::Path => 1,
        LaunchField::Label => 2,
    };
    let visible = usize::from(content.height).min(FIELDS);
    // Scroll just enough to keep the field being edited on screen.
    let start = focused
        .saturating_sub(visible.saturating_sub(1))
        .min(FIELDS - visible);
    let mut cursor = None;
    for (offset, index) in (start..start + visible).enumerate() {
        let row = Rect::new(content.x, content.y + offset as u16, content.width, 1);
        let (prefix, value, focused_field) = match index {
            0 => {
                frame.render_widget(
                    Paragraph::new(kind_line(kinds, form.kind))
                        .style(field_style(form.field == LaunchField::Kind)),
                    row,
                );
                continue;
            }
            1 => ("Dir ", form.path.as_str(), LaunchField::Path),
            _ => ("Label ", form.label.as_str(), LaunchField::Label),
        };
        let is_focused = form.field == focused_field;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(MUTED)),
                Span::styled(
                    truncate(
                        value,
                        usize::from(content.width).saturating_sub(prefix.len()),
                    ),
                    field_style(is_focused),
                ),
            ])),
            row,
        );
        if is_focused {
            let used = prefix.len() + UnicodeWidthStr::width(value);
            cursor = Some((
                row.x
                    .saturating_add(used.min(row.width.saturating_sub(1) as usize) as u16),
                row.y,
            ));
        }
    }
    if let Some(position) = cursor {
        frame.set_cursor_position(position);
    }
}

fn draw_launch_modal(frame: &mut Frame<'_>, form: &LaunchForm, outer: Rect, kinds: &[AgentKind]) {
    let area = centered_rect(70, 13, outer);
    frame.render_widget(Clear, area);
    let inner = Block::default()
        .title(format!(" New agent on {} ", form.target.label))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT));
    let content = inner.inner(area);
    frame.render_widget(inner, area);
    if content.width == 0 || content.height == 0 {
        return;
    }
    // The captioned layout below needs its ten rows; anything less and the
    // later constraints resolve to nothing and the fields vanish silently.
    if content.height < 10 {
        draw_launch_modal_compact(frame, form, content, kinds);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .split(content);
    frame.render_widget(Paragraph::new("Agent runtime"), rows[0]);
    frame.render_widget(
        Paragraph::new(kind_line(kinds, form.kind))
            .style(field_style(form.field == LaunchField::Kind)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new("Working directory - Enter to browse"),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(form.path.as_str())
            .style(field_style(form.field == LaunchField::Path))
            .block(Block::default().borders(Borders::BOTTOM)),
        rows[3],
    );
    frame.render_widget(Paragraph::new("Label (optional)"), rows[4]);
    frame.render_widget(
        Paragraph::new(form.label.as_str())
            .style(field_style(form.field == LaunchField::Label))
            .block(Block::default().borders(Borders::BOTTOM)),
        rows[5],
    );
    // A shell is never asked about resuming, so promising that step would be a
    // keypress the folder row does not take.
    let hint = if form.kind == AgentKind::Terminal {
        "Enter advances runtime -> folder -> start    Tab edits label"
    } else {
        "Enter advances runtime -> folder -> New/Resume    Tab edits label"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(MUTED)),
        rows[6],
    );

    let (text, row) = match form.field {
        LaunchField::Path => (&form.path, rows[3]),
        LaunchField::Label => (&form.label, rows[5]),
        LaunchField::Kind => return,
    };
    let x = row.x.saturating_add(
        UnicodeWidthStr::width(text.as_str()).min(row.width.saturating_sub(1) as usize) as u16,
    );
    frame.set_cursor_position((x, row.y));
}

fn draw_temporal_modal(
    frame: &mut Frame<'_>,
    form: &crate::app::TemporalForm,
    outer: Rect,
    kinds: &[AgentKind],
) {
    let area = centered_rect(62, 11, outer);
    frame.render_widget(Clear, area);
    let block = panel(" Temporal Chat ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
    frame.render_widget(Paragraph::new("Choose agent runtime"), rows[0]);
    frame.render_widget(Paragraph::new(kind_line(kinds, form.kind)), rows[1]);
    let mut name = vec![
        Span::styled("Name: ", Style::default().fg(MUTED)),
        Span::styled(
            truncate(&form.label, inner.width.saturating_sub(8) as usize),
            Style::default().fg(Color::White),
        ),
        Span::styled("█", Style::default().fg(ACCENT)),
    ];
    if form.label.trim().is_empty() {
        name.push(Span::styled(
            format!("  {}", crate::app::TemporalForm::DEFAULT_LABEL),
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(name)), rows[2]);
    // No folder to choose: a temporal chat runs in a scratch folder muxloom
    // makes for it and removes with it, so the form says where it goes rather
    // than offering a project for it to move into.
    frame.render_widget(
        Paragraph::new(truncate(
            "Folder: a scratch folder muxloom makes, removed with the chat",
            inner.width as usize,
        ))
        .style(Style::default().fg(Color::Gray)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new("Enter start    Ctrl-u clear    Esc cancel")
            .style(Style::default().fg(MUTED)),
        rows[5],
    );
}

/// The new-moderator form: a runtime, a name, and two lists of checkboxes that
/// scroll as one column. The scope is written into the moderator's briefing and
/// is not enforced anywhere, so the panel says so where the user chooses it —
/// a checkbox that reads like a permission and is not one is worse than none.
fn draw_moderator_modal(
    frame: &mut Frame<'_>,
    form: &ModeratorForm,
    outer: Rect,
    kinds: &[AgentKind],
) {
    let area = centered_rect(72, 24, outer);
    frame.render_widget(Clear, area);
    let block = panel(" New moderator ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height < 6 {
        return;
    }
    let width = inner.width as usize;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let rows = form.rows();
    let selected = form.row();
    frame.render_widget(Paragraph::new(kind_line(kinds, form.kind)), layout[0]);
    let name_style = if selected == ModeratorRow::Name {
        Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
    } else {
        Style::default().fg(Color::White)
    };
    let mut name = vec![
        Span::styled("Name  ", Style::default().fg(MUTED)),
        Span::styled(truncate(&form.name, width.saturating_sub(8)), name_style),
    ];
    if selected == ModeratorRow::Name {
        name.push(Span::styled("█", Style::default().fg(ACCENT)));
    }
    if form.name.trim().is_empty() {
        name.push(Span::styled(
            "  required",
            Style::default().fg(if selected == ModeratorRow::Name {
                Color::Yellow
            } else {
                MUTED
            }),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(name)), layout[1]);
    frame.render_widget(
        Paragraph::new(truncate(
            "Scope goes in its briefing. muxloom does not enforce it.",
            width,
        ))
        .style(Style::default().fg(MUTED)),
        layout[2],
    );

    // One scrolling column over both lists, so a long fleet does not push the
    // agents off the panel and out of reach.
    let list_rows = layout[3].height as usize;
    let first_list_row = 2;
    let cursor = form.selected.max(first_list_row) - first_list_row;
    let offset = cursor.saturating_sub(list_rows.saturating_sub(1));
    let mut lines = Vec::with_capacity(list_rows);
    for (index, row) in rows.iter().enumerate().skip(first_list_row + offset) {
        if lines.len() >= list_rows {
            break;
        }
        let active = index == form.selected;
        let (text, style) = match *row {
            ModeratorRow::MachinesHeader => (
                moderator_group_line("Machines", &form.machines.iter().collect::<Vec<_>>()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            // Counted over what is on show: agents on unchecked machines are
            // not "unchosen", they are not on offer.
            ModeratorRow::AgentsHeader => (
                moderator_group_line(
                    "Agents",
                    &form
                        .visible_agents()
                        .into_iter()
                        .map(|item| &form.agents[item])
                        .collect::<Vec<_>>(),
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            ModeratorRow::Machine(item) => (
                moderator_item_line(&form.machines[item]),
                Style::default().fg(Color::White),
            ),
            ModeratorRow::Agent(item) => (
                moderator_item_line(&form.agents[item]),
                Style::default().fg(Color::White),
            ),
            ModeratorRow::Kind | ModeratorRow::Name => continue,
        };
        let style = if active {
            style.bg(Color::Rgb(42, 48, 58))
        } else {
            style
        };
        lines.push(Line::styled(
            format!("{:<width$}", truncate(&text, width), width = width),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(lines), layout[3]);

    if let Some(error) = &form.error {
        frame.render_widget(
            Paragraph::new(truncate(error, width)).style(Style::default().fg(Color::Red)),
            layout[4],
        );
    }
    frame.render_widget(
        Paragraph::new("Enter start    Space toggle    Left/Right runtime    Esc cancel")
            .style(Style::default().fg(MUTED)),
        layout[5],
    );
}

/// A group header, carrying what its checkboxes currently add up to. "All"
/// matters because it is the one answer the briefing writes as "every machine",
/// including the ones that appear after the moderator starts.
fn moderator_group_line(title: &str, items: &[&crate::app::ScopeItem]) -> String {
    let chosen = items.iter().filter(|item| item.selected).count();
    let summary = if items.is_empty() {
        "none to choose from".into()
    } else if chosen == items.len() {
        format!("all {chosen}, and any that appear later")
    } else {
        format!("{chosen} of {}", items.len())
    };
    format!("{title} - {summary}")
}

fn moderator_item_line(item: &crate::app::ScopeItem) -> String {
    format!(
        "  [{}] {}",
        if item.selected { "x" } else { " " },
        item.label
    )
}

/// The communication panel. Its list is the whole fleet's, not the selected
/// machine's: a binding written here is pushed to every enabled machine, so
/// that an agent finishing something at three in the morning on a machine
/// nobody is watching can still say so.
fn draw_channels_modal(frame: &mut Frame<'_>, form: &ChannelsForm, outer: Rect) {
    // Every step with a code on it takes the window it needs. A code has to
    // come out square and big enough for a phone to read across a desk, and on
    // the credentials step the two fields above it are three rows of nothing
    // much — there is no reason to make the code pay for them.
    let area = match &form.step {
        Some(ChannelStep::Scan(_) | ChannelStep::Keys(_)) => centered_rect(76, 40, outer),
        // A chooser with nothing to choose from turns into a code as well, and
        // a code has to come out square and big enough to read across a desk.
        Some(ChannelStep::Chats(chats))
            if chats.found.as_deref().is_some_and(|found| found.is_empty()) =>
        {
            centered_rect(76, 40, outer)
        }
        _ => centered_rect(76, 18, outer),
    };
    frame.render_widget(Clear, area);
    let title = match &form.step {
        None => " Channels · how an agent reaches you ".to_string(),
        Some(ChannelStep::Pick { .. }) => " Bind a chat ".to_string(),
        Some(ChannelStep::Scan(_)) => " Bind a chat · WeChat ".to_string(),
        Some(ChannelStep::Keys(_) | ChannelStep::Chats(_)) => " Bind a chat · Lark ".to_string(),
        Some(ChannelStep::Rename { index, .. }) => match form.set.bindings.get(*index) {
            Some(binding) => format!(" Rename {} ", binding.id),
            None => " Rename ".to_string(),
        },
    };
    let block = panel(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let hints = match &form.step {
        None => "n new   e rename   x remove   Enter default   t test   Esc save & close",
        Some(ChannelStep::Pick { .. }) => "Up/Down choose   Enter start   Esc back",
        Some(ChannelStep::Scan(_)) => "r new code   Esc back",
        Some(ChannelStep::Keys(_)) => "Tab field   Enter find my chats   Esc back",
        Some(ChannelStep::Chats(chats)) => match chats.found.as_deref() {
            // Nothing to move through and nothing to bind, and nothing anybody
            // has to press either: the panel is asking Lark by itself. r is
            // only here for whoever would rather not wait out the interval.
            Some([]) => "Nothing to press — r check now   Esc back",
            _ => "Up/Down choose   Enter bind   r ask again   Esc back",
        },
        Some(ChannelStep::Rename { .. }) => "Enter rename   Esc back",
    };
    let message = match (&form.error, &form.note) {
        (Some(error), _) => (truncate(error, inner.width as usize), Color::Red),
        (None, Some(note)) => (truncate(note, inner.width as usize), Color::Green),
        (None, None) => (String::new(), MUTED),
    };
    if let Some(row) = modal_row(inner, inner.height.saturating_sub(2)) {
        frame.render_widget(
            Paragraph::new(message.0).style(Style::default().fg(message.1)),
            row,
        );
    }
    if let Some(row) = modal_row(inner, inner.height.saturating_sub(1)) {
        frame.render_widget(
            Paragraph::new(truncate(hints, inner.width as usize)).style(Style::default().fg(MUTED)),
            row,
        );
    }
    // Two rows at the bottom are the message and the hints, whatever the step.
    let body = Rect {
        height: inner.height.saturating_sub(3),
        ..inner
    };
    match &form.step {
        None => draw_channel_list(frame, form, inner),
        Some(ChannelStep::Pick { selected }) => draw_channel_pick(frame, *selected, body),
        Some(ChannelStep::Scan(scan)) => draw_channel_scan(frame, scan, body),
        Some(ChannelStep::Keys(keys)) => draw_channel_keys(frame, form, keys, body),
        Some(ChannelStep::Chats(chats)) => draw_channel_chats(frame, chats, body),
        Some(ChannelStep::Rename { label, .. }) => draw_channel_rename(frame, label, body),
    }
}

fn draw_channel_list(frame: &mut Frame<'_>, form: &ChannelsForm, inner: Rect) {
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new("Bound chats").style(Style::default().fg(Color::Gray).bold()),
            row,
        );
    }
    if form.set.bindings.is_empty()
        && let Some(row) = modal_row(inner, 1)
    {
        frame.render_widget(
            Paragraph::new("None yet. Press n — WeChat takes a scan and about ten seconds.")
                .style(Style::default().fg(MUTED)),
            row,
        );
    }
    // Two trailing rows carry the message and the hints, and four more the
    // notes below the list.
    let rows = usize::from(inner.height.saturating_sub(8)).max(1);
    let first = form.selected.saturating_add(1).saturating_sub(rows);
    for (visible, (index, binding)) in form
        .set
        .bindings
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .enumerate()
    {
        let Some(row) = modal_row(inner, 1 + visible as u16) else {
            break;
        };
        let selected = index == form.selected;
        let name = if binding.label.is_empty() {
            binding.kind.title().to_string()
        } else {
            binding.label.clone()
        };
        // What is wrong with it, when something is, in place of the flags that
        // would otherwise be there: a row that cannot be used should say so
        // where a person is already looking.
        let state = match binding.ready() {
            Err(_) if binding.kind == ChannelKind::WeChat => "  ·  say hello to it in WeChat",
            Err(_) => "  ·  unfinished",
            Ok(()) if binding.preferred => "  ·  default",
            Ok(()) => "",
        };
        let line = format!(
            "{:<10}{name}  ·  {}{state}",
            binding.id,
            binding.destination()
        );
        frame.render_widget(
            Paragraph::new(truncate(&line, inner.width as usize)).style(
                Style::default()
                    .fg(if binding.ready().is_err() {
                        Color::Yellow
                    } else if binding.preferred {
                        ACCENT
                    } else {
                        Color::White
                    })
                    .bg(if selected {
                        Color::Rgb(42, 48, 58)
                    } else {
                        Color::Reset
                    }),
            ),
            row,
        );
    }
    // Everything below is said plainly rather than discovered: what a person
    // most wants to know here is whether a message will actually arrive, and
    // how long an answer takes to come back.
    let notes = [
        form.reach.clone(),
        "Both listen: a reply goes back to the agent that spoke, read every ~5s.".into(),
        "In WeChat, whatever you say goes to whoever spoke last. /who lists them.".into(),
    ];
    for (offset, note) in notes.iter().enumerate() {
        let Some(row) = modal_row(inner, inner.height.saturating_sub(6) + offset as u16) else {
            break;
        };
        frame.render_widget(
            Paragraph::new(truncate(note, inner.width as usize)).style(Style::default().fg(MUTED)),
            row,
        );
    }
}

/// The chooser. Two rows, each saying what it will cost to go that way — a name
/// on its own is not a decision anybody can make.
fn draw_channel_pick(frame: &mut Frame<'_>, selected: usize, inner: Rect) {
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new("Which chat should agents reach you in?")
                .style(Style::default().fg(Color::Gray).bold()),
            row,
        );
    }
    for (index, kind) in ChannelKind::ALL.iter().enumerate() {
        let Some(row) = modal_row(inner, 2 + index as u16 * 2) else {
            break;
        };
        let active = index == selected.min(ChannelKind::ALL.len() - 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if active { "  ▸ " } else { "    " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    kind.title(),
                    Style::default()
                        .fg(if active { Color::White } else { Color::Gray })
                        .bold(),
                ),
            ])),
            row,
        );
        if let Some(row) = modal_row(inner, 3 + index as u16 * 2) {
            frame.render_widget(
                Paragraph::new(truncate(
                    &format!("      {}", kind.pitch()),
                    inner.width as usize,
                ))
                .style(Style::default().fg(MUTED)),
                row,
            );
        }
    }
}

/// The code, and one line saying what to do with it.
fn draw_channel_scan(frame: &mut Frame<'_>, scan: &ChannelScan, inner: Rect) {
    let asking = if scan.lark {
        "Asking Feishu to create a bot…"
    } else {
        "Asking WeChat for a code…"
    };
    let showing = if scan.lark {
        "Open Lark › 扫一扫 and point it here to create your bot"
    } else {
        "Open WeChat › 扫一扫 and point it here"
    };
    let (headline, colour) = match &scan.state {
        ScanState::Asking => (asking, MUTED),
        ScanState::Showing => (showing, Color::White),
        ScanState::Scanned => ("Scanned — now tap confirm on your phone", Color::Green),
        ScanState::Expired => ("That code timed out. Press r for another.", Color::Yellow),
        ScanState::Failed(error) => (error.as_str(), Color::Red),
    };
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new(truncate(headline, inner.width as usize))
                .style(Style::default().fg(colour).bold()),
            row,
        );
    }
    let grid = Rect {
        y: inner.y.saturating_add(2),
        height: inner.height.saturating_sub(3),
        ..inner
    };
    let drawn = matches!(scan.state, ScanState::Showing | ScanState::Scanned)
        && scan
            .code
            .as_ref()
            .is_some_and(|code| draw_qr(frame, grid, code));
    // A window too small for the code is not a dead end: the same link opened
    // on the phone does the same thing.
    if !drawn
        && !scan.link.is_empty()
        && let Some(row) = modal_row(inner, inner.height.saturating_sub(1))
    {
        frame.render_widget(
            Paragraph::new(truncate(
                &format!(
                    "Too small to draw the code here. Open on your phone: {}",
                    scan.link
                ),
                inner.width as usize,
            ))
            .style(Style::default().fg(MUTED)),
            row,
        );
    }
}

/// Paint a QR code into `area`, centred, and say whether there was room.
///
/// Half blocks: the top half of a cell is one module and the bottom half is the
/// module under it. A terminal cell is about twice as tall as it is wide, so
/// this is the only way to get modules that come out square — and a scanner
/// wants them square.
///
/// Black on white whatever the terminal's own colours are. A scanner reads
/// contrast and does not know about themes; a code painted in a dark palette's
/// foreground on its background is a code that will not scan.
fn draw_qr(frame: &mut Frame<'_>, area: Rect, code: &crate::qr::Code) -> bool {
    let (columns, rows) = (code.columns(), code.rows());
    let (Ok(width), Ok(height)) = (u16::try_from(columns), u16::try_from(rows)) else {
        return false;
    };
    if area.width < width || area.height < height {
        return false;
    }
    let dark = Style::default().fg(Color::Black).bg(Color::White);
    let lines = (0..rows)
        .map(|row| {
            Line::from(
                (0..columns)
                    .map(|column| {
                        // The upper half block is drawn in the foreground and
                        // the lower half is whatever shows through behind it.
                        Span::styled(
                            "▀",
                            match (code.dark(column, row * 2), code.dark(column, row * 2 + 1)) {
                                (true, true) => dark.fg(Color::Black).bg(Color::Black),
                                (true, false) => dark,
                                (false, true) => dark.fg(Color::White).bg(Color::Black),
                                (false, false) => dark.fg(Color::White).bg(Color::White),
                            },
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        ),
    );
    true
}

/// Lark's two strings, and the one honest sentence about why they have to be
/// typed at all.
fn draw_channel_keys(frame: &mut Frame<'_>, form: &ChannelsForm, keys: &ChannelKeys, inner: Rect) {
    // The address on its own line rather than folded into the sentence: it is
    // the one thing on this screen somebody might have to type by hand, and a
    // line with nothing else on it is a line that survives a narrow panel.
    for (offset, (line, colour)) in [
        (
            "Only Lark can make an app. Scan below, or open it yourself:",
            MUTED,
        ),
        (crate::channel::LARK_CONSOLE, ACCENT),
    ]
    .iter()
    .enumerate()
    {
        if let Some(row) = modal_row(inner, offset as u16) {
            frame.render_widget(
                Paragraph::new(truncate(line, inner.width as usize))
                    .style(Style::default().fg(*colour)),
                row,
            );
        }
    }
    let label_width = 14;
    let mut cursor = None;
    for (index, field) in KeysField::ALL.iter().enumerate() {
        let Some(row) = modal_row(inner, 3 + index as u16) else {
            break;
        };
        let active = index == keys.selected.min(KeysField::ALL.len() - 1);
        let value = keys.value(*field);
        // A secret is shown as bullets whether or not the cursor is on it: the
        // person typing it knows what they pasted, and everyone behind them
        // does not need to.
        let shown = if field.hidden() {
            "•".repeat(value.chars().count().min(28))
        } else {
            value.to_string()
        };
        let mut spans = vec![
            Span::styled(
                format!("{:<label_width$}", field.label()),
                Style::default().fg(if active { ACCENT } else { Color::Gray }),
            ),
            Span::styled(
                shown.clone(),
                if active {
                    Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
                } else {
                    Style::default().fg(Color::White)
                },
            ),
        ];
        if active {
            // Cut to what is left of the row rather than to the row, or a long
            // secret pushes the hint off the edge mid-word.
            let room = (inner.width as usize)
                .saturating_sub(label_width + UnicodeWidthStr::width(shown.as_str()) + 2);
            spans.push(Span::styled(
                format!("  {}", truncate(field.hint(), room)),
                Style::default().fg(MUTED),
            ));
            cursor = Some((
                inner
                    .x
                    .saturating_add(label_width as u16)
                    .saturating_add(UnicodeWidthStr::width(shown.as_str()) as u16)
                    .min(inner.x + inner.width.saturating_sub(1)),
                row.y,
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
    }
    let footnote = match &keys.borrowed {
        Some(borrowed) => (
            format!("Filled in from {borrowed} — check it, then Enter"),
            Color::Yellow,
        ),
        None if form.set.bindings.is_empty() => (
            "The first channel bound is the one a message that names none goes to.".into(),
            MUTED,
        ),
        None => (String::new(), MUTED),
    };
    let below = 3 + KeysField::ALL.len() as u16 + 1;
    if let Some(row) = modal_row(inner, below) {
        frame.render_widget(
            Paragraph::new(truncate(&footnote.0, inner.width as usize))
                .style(Style::default().fg(footnote.1)),
            row,
        );
    }
    // Under everything, and only if the window left room. A code that will not
    // fit is simply absent: the address two rows from the top is the part that
    // cannot go missing, and it is text, so it survives any panel at all.
    let grid = Rect {
        y: inner.y.saturating_add(below).saturating_add(2),
        height: inner.height.saturating_sub(below.saturating_add(2)),
        ..inner
    };
    if let Some(code) = console_code() {
        draw_qr(frame, grid, code);
    }
    if let Some((x, y)) = cursor {
        frame.set_cursor_position((x, y));
    }
}

/// The console as a code, encoded once for the whole run.
///
/// Unlike every other code in this panel it says the same thing every time —
/// there is one open platform and it is at one address — so encoding it per
/// frame would be work done sixty times a second to draw the same squares.
fn console_code() -> Option<&'static crate::qr::Code> {
    static CODE: LazyLock<Option<crate::qr::Code>> =
        LazyLock::new(|| crate::qr::encode(crate::channel::LARK_CONSOLE).ok());
    CODE.as_ref()
}

/// The chats that app turned out to be in.
///
/// A list rather than a box to paste an `oc_…` into: those ids exist nowhere a
/// person can copy them from, and the app already knows both the ids and the
/// names the chats are known by.
fn draw_channel_chats(frame: &mut Frame<'_>, chats: &ChannelChats, inner: Rect) {
    let Some(found) = chats.found.as_deref() else {
        if let Some(row) = modal_row(inner, 0) {
            frame.render_widget(
                Paragraph::new("Asking Lark which chats this app is in…")
                    .style(Style::default().fg(MUTED)),
                row,
            );
        }
        return;
    };
    if found.is_empty() {
        draw_channel_bot_code(frame, chats, inner);
        return;
    }
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new("Which chat should agents reach you in?")
                .style(Style::default().fg(Color::Gray).bold()),
            row,
        );
    }
    let rows = usize::from(inner.height.saturating_sub(2)).max(1);
    let first = chats.selected.saturating_add(1).saturating_sub(rows);
    for (visible, (index, chat)) in found.iter().enumerate().skip(first).take(rows).enumerate() {
        let Some(row) = modal_row(inner, 2 + visible as u16) else {
            break;
        };
        let active = index == chats.selected;
        frame.render_widget(
            Paragraph::new(truncate(
                &format!("{} {}", if active { "▸" } else { " " }, chat.label()),
                inner.width as usize,
            ))
            .style(
                Style::default()
                    .fg(if active { Color::White } else { Color::Gray })
                    .bg(if active {
                        Color::Rgb(42, 48, 58)
                    } else {
                        Color::Reset
                    }),
            ),
            row,
        );
    }
}

/// The way out of "that app is in no chats": the bot itself, as a code.
///
/// This is the one screen in the Lark flow with nothing on it to choose and
/// nothing to type — a chat id exists nowhere a person can copy it from, so the
/// route to a bindable chat is to talk to the bot: a direct message opens the
/// one-to-one chat, which the list picks up and can bind. Adding the bot to a
/// group still works, but a private message is the shortest path. Scanning
/// opens the bot in Lark; the link under it is the same thing for a window
/// with no room to draw a code, or for somebody who would rather paste.
///
/// Nothing here asks for a keystroke. The panel keeps asking Lark on its own
/// while this is up, so the hands that are holding the phone never have to come
/// back to the keyboard to say they are done.
fn draw_channel_bot_code(frame: &mut Frame<'_>, chats: &ChannelChats, inner: Rect) {
    for (offset, (line, colour)) in [
        (
            "No chat with this bot yet — that's normal for a fresh bot.".to_string(),
            Color::Yellow,
        ),
        (String::new(), MUTED),
        (
            "Scan to open its bot in Lark, then just send it a message —".to_string(),
            MUTED,
        ),
        (
            "直接私聊即可；群聊可选：群设置 › 群机器人 › 添加机器人.".to_string(),
            MUTED,
        ),
        (watch_line(chats), ACCENT),
    ]
    .iter()
    .enumerate()
    {
        let Some(row) = modal_row(inner, offset as u16) else {
            return;
        };
        frame.render_widget(
            Paragraph::new(truncate(line, inner.width as usize))
                .style(Style::default().fg(*colour)),
            row,
        );
    }
    let grid = Rect {
        y: inner.y.saturating_add(6),
        height: inner.height.saturating_sub(7),
        ..inner
    };
    let drawn = chats
        .code
        .as_ref()
        .is_some_and(|code| draw_qr(frame, grid, code));
    if !drawn
        && !chats.link.is_empty()
        && let Some(row) = modal_row(inner, inner.height.saturating_sub(1))
    {
        frame.render_widget(
            Paragraph::new(truncate(
                &format!(
                    "No room for the code here. Open on your phone: {}",
                    chats.link
                ),
                inner.width as usize,
            ))
            .style(Style::default().fg(MUTED)),
            row,
        );
    }
}

/// The line that says the screen is doing something, so an empty list does not
/// read as a screen that has given up.
///
/// It counts from the last answer rather than showing a spinner, because what
/// somebody standing there with a phone wants to know is not "is it busy" but
/// "would it have noticed yet".
fn watch_line(chats: &ChannelChats) -> String {
    if chats.asking {
        return "Watching for it — asking Lark…".into();
    }
    match chats.checked {
        Some(at) => format!(
            "Watching for it — last checked {}s ago",
            at.elapsed().as_secs()
        ),
        None => "Watching for it…".into(),
    }
}

fn draw_channel_rename(frame: &mut Frame<'_>, label: &str, inner: Rect) {
    if let Some(row) = modal_row(inner, 0) {
        frame.render_widget(
            Paragraph::new(truncate(
                "What a message says it came through. Nothing else about a binding is typed.",
                inner.width as usize,
            ))
            .style(Style::default().fg(MUTED)),
            row,
        );
    }
    let label_width = 14;
    if let Some(row) = modal_row(inner, 2) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{:<label_width$}", "Name"),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    label.to_string(),
                    Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58)),
                ),
            ])),
            row,
        );
        frame.set_cursor_position((
            inner
                .x
                .saturating_add(label_width as u16)
                .saturating_add(UnicodeWidthStr::width(label) as u16)
                .min(inner.x + inner.width.saturating_sub(1)),
            row.y,
        ));
    }
}

fn draw_port_forward_modal(frame: &mut Frame<'_>, form: &PortForwardForm, outer: Rect) {
    let area = centered_rect(78, 20, outer);
    frame.render_widget(Clear, area);
    let title = format!(
        " Port forwarding - {} ",
        truncate(&form.target.label, area.width.saturating_sub(24) as usize)
    );
    let block = panel(&title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let labels = ["Remote host", "Remote port", "Local port"];
    let values = [&form.remote_host, &form.remote_port, &form.local_port];
    let label_width = 15;
    for (index, (label, value)) in labels.iter().zip(values).enumerate() {
        let active = form.selected == index;
        let Some(row) = modal_row(inner, index as u16) else {
            continue;
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{label:<label_width$}"),
                    Style::default().fg(if active { ACCENT } else { Color::Gray }),
                ),
                Span::styled(
                    value.as_str(),
                    if active {
                        Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
                    } else {
                        Style::default().fg(Color::White)
                    },
                ),
            ])),
            row,
        );
    }

    let detected = if form.loading {
        "Detecting listeners...".into()
    } else if form.detected_ports.is_empty() {
        "Detected: none".into()
    } else {
        format!(
            "Detected: {}",
            form.detected_ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join("  ")
        )
    };
    if let Some(row) = modal_row(inner, 4) {
        frame.render_widget(
            Paragraph::new(truncate(&detected, inner.width as usize))
                .style(Style::default().fg(MUTED)),
            row,
        );
    }

    if let Some(row) = modal_row(inner, 6) {
        frame.render_widget(
            Paragraph::new("Active forwards").style(Style::default().fg(Color::Gray).bold()),
            row,
        );
    }
    let selected_active = form.active_index();
    // Two trailing rows are reserved for the error line and the key hints.
    let active_rows = usize::from(inner.height.saturating_sub(9)).min(6);
    let active_start = selected_active
        .map(|index| index.saturating_add(1).saturating_sub(active_rows.max(1)))
        .unwrap_or(0);
    if form.active.is_empty()
        && let Some(row) = modal_row(inner, 7)
    {
        frame.render_widget(
            Paragraph::new("None").style(Style::default().fg(MUTED)),
            row,
        );
    }
    for (visible, (index, forward)) in form
        .active
        .iter()
        .enumerate()
        .skip(active_start)
        .take(active_rows)
        .enumerate()
    {
        let (status, color) = match &forward.state {
            PortForwardState::Starting => ("starting".to_string(), Color::Yellow),
            PortForwardState::Active => ("active".to_string(), Color::Green),
            PortForwardState::Error(error) => {
                (format!("error: {}", truncate(error, 28)), Color::Red)
            }
        };
        let Some(row) = modal_row(inner, 7 + visible as u16) else {
            break;
        };
        let selected = selected_active == Some(index);
        let folder = forward
            .folder
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&forward.folder);
        let mapping = format!(
            "127.0.0.1:{} -> {}:{}  {}  {status}",
            forward.local_port, forward.remote_host, forward.remote_port, folder
        );
        frame.render_widget(
            Paragraph::new(truncate(&mapping, inner.width as usize)).style(
                Style::default().fg(color).bg(if selected {
                    Color::Rgb(42, 48, 58)
                } else {
                    Color::Reset
                }),
            ),
            row,
        );
    }

    let message = form
        .error
        .as_ref()
        .or(form.detection_error.as_ref())
        .map(|error| truncate(error, inner.width as usize))
        .unwrap_or_default();
    if let Some(row) = modal_row(inner, inner.height.saturating_sub(2)) {
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::Red)),
            row,
        );
    }
    if let Some(row) = modal_row(inner, inner.height.saturating_sub(1)) {
        frame.render_widget(
            Paragraph::new(truncate(
                "Tab field/forward   Left/Right detected port   Enter start   d stop   Esc close",
                inner.width as usize,
            ))
            .style(Style::default().fg(MUTED)),
            row,
        );
    }
    if form.selected < PortForwardForm::FIELD_COUNT && (form.selected as u16) < inner.height {
        let value = values[form.selected];
        let cursor = inner
            .x
            .saturating_add(label_width as u16)
            .saturating_add(UnicodeWidthStr::width(value.as_str()) as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor, inner.y + form.selected as u16));
    }
}

fn panel<'a>(title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
}

fn list_highlight_style(focused: bool, bold: bool) -> Style {
    if !focused {
        return Style::default();
    }
    let style = Style::default().bg(Color::Rgb(42, 48, 58));
    if bold {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// A machine row's capability marker: it animates while that runtime has a
/// working session on the machine, mirroring the agent row's spinner.
fn runtime_capability(
    kind: AgentKind,
    available: bool,
    working: bool,
    frame: u64,
) -> Span<'static> {
    let (idle, _, color) = agent_visual(kind);
    let label = if working {
        running_agent_effect(kind, frame)
    } else {
        idle
    };
    Span::styled(
        label,
        Style::default()
            .fg(if available || working { color } else { MUTED })
            .add_modifier(Modifier::BOLD),
    )
}

pub fn agent_visual(kind: AgentKind) -> (&'static str, &'static str, Color) {
    match kind {
        AgentKind::Codex => ("◉", "Codex", CODEX),
        AgentKind::Claude => ("✻", "Claude Code", CLAUDE),
        AgentKind::OpenCode => ("◈", "OpenCode", OPENCODE),
        AgentKind::Pi => ("π", "Pi", PI),
        AgentKind::Terminal => ("▣", "Terminal", TERMINAL),
    }
}

fn running_agent_effect(kind: AgentKind, frame: u64) -> &'static str {
    // Codex is a cyan rotating braille spinner (single-column, so it keeps a
    // stable footprint); Claude keeps its own orange sparkle, matching the
    // asterisk glyphs Claude Code itself cycles through. Both advance one frame
    // per `frame`, so with a time-based counter they animate at a constant rate
    // regardless of how often the UI redraws.
    // The newer runtimes borrow the same idea: a single-column glyph cycle in
    // their own shape, so a busy row reads as movement whichever agent it is.
    const CODEX_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠇"];
    const CLAUDE_FRAMES: [&str; 6] = ["✻", "✽", "✶", "✳", "✶", "✽"];
    const OPENCODE_FRAMES: [&str; 4] = ["◈", "◇", "◆", "◇"];
    const PI_FRAMES: [&str; 4] = ["π", "ᴨ", "π", "∏"];
    match kind {
        AgentKind::Codex => CODEX_FRAMES[(frame % CODEX_FRAMES.len() as u64) as usize],
        AgentKind::Claude => CLAUDE_FRAMES[(frame % CLAUDE_FRAMES.len() as u64) as usize],
        AgentKind::OpenCode => OPENCODE_FRAMES[(frame % OPENCODE_FRAMES.len() as u64) as usize],
        AgentKind::Pi => PI_FRAMES[(frame % PI_FRAMES.len() as u64) as usize],
        AgentKind::Terminal => "▣",
    }
}

fn segment(label: impl Into<String>, selected: bool, color: Color) -> Span<'static> {
    let label = label.into();
    if selected {
        Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(label, Style::default().fg(MUTED))
    }
}

fn field_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
    } else {
        Style::default().fg(Color::Gray)
    }
}

/// A single full-width row at a fixed offset inside a modal body.
///
/// Modal bodies are laid out at constant offsets, but `centered_rect` shrinks
/// the modal to fit a short terminal. Rendering a widget outside the frame
/// buffer panics, so rows that fall past the bottom are dropped instead.
fn modal_row(inner: Rect, offset: u16) -> Option<Rect> {
    (offset < inner.height && inner.width > 0)
        .then(|| Rect::new(inner.x, inner.y + offset, inner.width, 1))
}

fn centered_rect(width: u16, height: u16, outer: Rect) -> Rect {
    let width = width.min(outer.width.saturating_sub(2)).max(1);
    let height = height.min(outer.height.saturating_sub(2)).max(1);
    Rect {
        x: outer.x + outer.width.saturating_sub(width) / 2,
        y: outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn wrap_display(value: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut result = Vec::new();
    for logical_line in value.split('\n') {
        let mut line = String::new();
        let mut width = 0;
        for character in logical_line.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if !line.is_empty() && width + character_width > max_width {
                result.push(std::mem::take(&mut line));
                width = 0;
            }
            line.push(character);
            width += character_width;
        }
        result.push(line);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn tail_display(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    let mut width = 0;
    let mut reversed = Vec::new();
    for character in value.chars().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_width.saturating_sub(3) {
            break;
        }
        reversed.push(character);
        width += character_width;
    }
    reversed.reverse();
    format!("...{}", reversed.into_iter().collect::<String>())
}

fn truncate(value: &str, max: usize) -> String {
    if UnicodeWidthStr::width(value) <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let content_width = max - 3;
    let mut width = 0;
    let mut result = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push_str("...");
    result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        app::{App, FileManagerOrigin},
        config::{Config, State},
        model::{AgentKind, AgentSession, Target, TaskProgress},
        runtime::Runtime,
        worker::{TaskKind, Worker},
    };

    #[test]
    fn renders_at_compact_and_wide_sizes() {
        for (width, height) in [(50, 14), (100, 25), (160, 40)] {
            let config = Config::default();
            let worker = Worker::start(Runtime::new(&config));
            let mut state = State::default();
            state.enabled_hosts.insert("local".into());
            let mut app = App::new(
                config,
                PathBuf::from("unused-config.toml"),
                state,
                PathBuf::from("unused-state.json"),
                vec![Target::local(), Target::ssh("very-long-gpu-machine-name")],
                worker,
            );
            app.sessions.push(AgentSession {
                id: "ad-codex-1-1-1".into(),
                target_id: "local".into(),
                kind: AgentKind::Codex,
                path: "/work/terminal".into(),
                label: "build".into(),
                created_at: 1,
                archived_at: None,
                dead: false,
                pid: Some(100),
                working: false,
                needs_attention: true,
                attention_reason: Some("approve".into()),
                recap: None,
                title: None,
                thread: None,
                parent: None,
            });
            app.selected_session_id = Some("ad-codex-1-1-1".into());

            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(rendered.contains("MUXLOOM"));
            assert!(rendered.contains("INPUT REQUIRED"));
            assert!(rendered.contains("build") || width == 50);
        }
    }

    #[test]
    fn wraps_machine_names_by_display_width() {
        assert_eq!(wrap_display("machine-long", 7), vec!["machine", "-long"]);
        assert_eq!(wrap_display("机器名称", 4), vec!["机器", "名称"]);
        assert_eq!(wrap_display("first\nsecond", 20), vec!["first", "second"]);
    }

    #[test]
    fn truncates_using_display_width() {
        assert_eq!(truncate("机器-alpha", 9), "机器-a...");
        assert_eq!(
            UnicodeWidthStr::width(truncate("机器-alpha", 9).as_str()),
            9
        );
    }

    #[test]
    fn ansi_history_preserves_colors_and_attributes() {
        let text =
            ansi_history_text("plain \x1b[31;1mred\x1b[0m \x1b[48;2;1;2;3mbackground\x1b[0m");
        assert_eq!(text.lines.len(), 1);
        assert!(text.lines[0].spans.iter().any(|span| span.content == "red"
            && span.style.fg == Some(Color::Red)
            && span.style.add_modifier.contains(Modifier::BOLD)));
        assert!(text.lines[0].spans.iter().any(|span| {
            span.content == "background" && span.style.bg == Some(Color::Rgb(1, 2, 3))
        }));
        assert_ne!(
            running_agent_effect(AgentKind::Codex, 0),
            running_agent_effect(AgentKind::Codex, 20)
        );
    }

    /// The shapes here are taken off real captures on this machine: Codex
    /// writes its spinner into the window title, and one session's capture
    /// carries 464,500 of those. Every one of them used to be printed.
    #[test]
    fn ansi_history_shows_what_was_on_the_screen_and_not_what_was_said_to_the_terminal() {
        let flat = |text: &Text<'_>| {
            text.lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        // A title, ended by a bell, and one ended by a string terminator.
        let titled = ansi_history_text("\x1b]0;\u{2839} m5stack-tools\x07real output\n");
        assert_eq!(flat(&titled), vec!["real output"]);
        let marked = ansi_history_text("\x1b]133;A\x1b\\prompt\n");
        assert_eq!(flat(&marked), vec!["prompt"]);

        // A charset designation and a keypad mode: the byte after the escape is
        // part of the escape, not a letter that belongs on the line.
        let designated = ansi_history_text("\x1b(B\x1b=text\n");
        assert_eq!(flat(&designated), vec!["text"]);

        // A line written over is the line as it was left, and a carriage
        // return that only ends a line does not eat it.
        let spun = ansi_history_text("50%\r100% done\n");
        assert_eq!(flat(&spun), vec!["100% done"]);
        let dos = ansi_history_text("kept\r\nalso kept\r\n");
        assert_eq!(flat(&dos), vec!["kept", "also kept"]);

        // Colour still survives all of that, and survives being written over.
        let coloured = ansi_history_text("\x1b[31mgone\r\x1b[32mstayed\n");
        assert!(
            coloured.lines[0]
                .spans
                .iter()
                .any(|span| span.content == "stayed" && span.style.fg == Some(Color::Green)),
            "the line rewrote its text, not the terminal's colour"
        );
        assert_eq!(flat(&coloured), vec!["stayed"]);
    }

    #[test]
    fn a_machine_only_another_muxloom_reaches_is_shown_and_can_be_looked_at() {
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
        app.forwarded = vec![crate::relay::Forwarded {
            peer: crate::relay::RelayPeer {
                id: "gpu".into(),
                label: "gpu".into(),
                via: "desk".into(),
                own: false,
            },
            through: "local".into(),
        }];

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_machines(frame, &mut app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        // It is on screen, marked for what it is and named with the way there.
        assert!(screen.contains("» "), "{screen}");
        assert!(screen.contains("gpu"), "{screen}");
        assert!(screen.contains("via desk"), "{screen}");
        // It is a row of its own — the cursor lands on it and its agents can be
        // listed — but never a `Machine`, which is what enabling and starting
        // sessions are keyed on. There is no route here to do either over.
        assert!(
            app.machine_rows
                .iter()
                .all(|(row, _)| *row != MachineRow::Machine(1)),
            "{:?}",
            app.machine_rows
        );
        assert_eq!(
            app.machine_rows.last().map(|(row, _)| *row),
            Some(MachineRow::Forwarded(0)),
            "{:?}",
            app.machine_rows
        );
    }

    #[test]
    fn animation_stays_on_agent_rows_and_folder_rows_carry_state_as_colour() {
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
        for (id, kind) in [("codex", AgentKind::Codex), ("claude", AgentKind::Claude)] {
            app.sessions.push(AgentSession {
                id: format!("muxloomd-{id}-working"),
                target_id: "local".into(),
                kind,
                path: "/work/project".into(),
                label: format!("{id} task"),
                created_at: 1,
                archived_at: None,
                dead: false,
                pid: Some(1),
                working: true,
                needs_attention: false,
                attention_reason: None,
                recap: None,
                title: None,
                thread: None,
                parent: None,
            });
        }
        app.targets[0].probe.set(AgentKind::Codex, true);
        app.targets[0].probe.set(AgentKind::Claude, true);

        // Each row as its text plus the colour at a given marker substring.
        let rows = |terminal: &Terminal<TestBackend>| -> Vec<String> {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect()
                })
                .collect()
        };
        let colour_at = |terminal: &Terminal<TestBackend>, marker: &str| -> Option<Color> {
            let buffer = terminal.backend().buffer();
            for y in 0..buffer.area.height {
                let text: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if let Some(column) = text.find(marker) {
                    return buffer[(column as u16, y)].style().fg;
                }
            }
            None
        };

        // The machine row's icons mirror the working spinner for the runtimes
        // busy on that machine.
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_machines(frame, &mut app, frame.area()))
            .unwrap();
        let machines: String = rows(&terminal).concat();
        assert!(
            machines.contains('⠋') && machines.contains('✻'),
            "machine icons mirror the working animation"
        );

        // The folder row does not animate either; it turns green while a
        // child works, and the agent rows keep the only animation.
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        let agent_rows = rows(&terminal);
        let folder = agent_rows
            .iter()
            .find(|text| text.contains("/work/project"))
            .expect("folder row rendered");
        assert!(!folder.contains('⠋'), "folder rows must not animate");
        assert_eq!(colour_at(&terminal, "/work/project"), Some(Color::Green));
        assert!(
            agent_rows
                .iter()
                .any(|text| text.contains('⠋') && text.contains("codex task")),
            "the agent row itself must animate"
        );

        // Attention outranks working in the folder colour.
        app.sessions[1].needs_attention = true;
        app.sessions[1].working = false;
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        assert_eq!(colour_at(&terminal, "/work/project"), Some(Color::Yellow));
    }

    /// The agent list draws a task as a task: the subagents indented under the
    /// agent that started them, in that agent's block whatever folder they run
    /// in, and a count on the row that can put them away.
    #[test]
    fn subagents_are_drawn_under_their_agent_with_a_count_that_folds_them() {
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
        let session = |id: &str, path: &str, parent: Option<&str>, created_at: u64| AgentSession {
            id: id.into(),
            target_id: "local".into(),
            kind: AgentKind::Claude,
            path: path.into(),
            label: id.into(),
            created_at,
            archived_at: None,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            thread: None,
            parent: parent.map(Into::into),
        };
        app.sessions = vec![
            session("lead", "/work/project", None, 30),
            session("helper", "/work/project", Some("lead"), 20),
            // A subagent sent off to another folder is still the lead's work.
            session("scout", "/work/other", Some("lead"), 10),
        ];

        let rows = |terminal: &Terminal<TestBackend>| -> Vec<String> {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        };
        let backend = TestBackend::new(46, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        let drawn = rows(&terminal);
        let row = |needle: &str| {
            drawn
                .iter()
                .find(|text| text.contains(needle))
                .unwrap_or_else(|| panic!("no row for {needle} in {drawn:?}"))
                .clone()
        };
        assert!(row("lead").contains("[-] 2"), "{drawn:?}");
        assert!(row("helper").contains('├'), "{drawn:?}");
        assert!(row("scout").contains('└'), "{drawn:?}");
        assert!(
            row("helper").find('├') < row("helper").find("helper"),
            "the elbow comes before the name"
        );
        // The subagent's own folder never opens a band of its own: it belongs
        // to the block the lead opened.
        assert!(
            !drawn.iter().any(|text| text.contains("/work/other")),
            "{drawn:?}"
        );

        // Folded, the row says how many it is holding and the rows are gone.
        app.state.folded_tasks.insert("lead".into());
        app.sessions[2].needs_attention = true;
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        let drawn = rows(&terminal);
        assert!(
            drawn.iter().any(|text| text.contains("[+] 2 !")),
            "a fold must say when it is hiding a prompt: {drawn:?}"
        );
        assert!(
            !drawn.iter().any(|text| text.contains("helper")),
            "{drawn:?}"
        );
    }

    /// The board's Task tab answers "what is my team doing", so the tree the
    /// agent list draws has to survive into it: a subagent's line sits under
    /// the line of whoever started it.
    #[test]
    fn the_task_tab_indents_a_subagent_under_the_agent_that_started_it() {
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
        let session = |id: &str, parent: Option<&str>| AgentSession {
            id: id.into(),
            target_id: "local".into(),
            kind: AgentKind::Claude,
            path: "/work".into(),
            label: id.into(),
            created_at: 1,
            archived_at: None,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            thread: None,
            parent: parent.map(Into::into),
        };
        app.sessions = vec![
            session("lead", None),
            session("scout", Some("lead")),
            session("digger", Some("scout")),
        ];
        app.selected_session_id = Some("lead".into());
        let said = |seq: u64, who: &str, text: &str| TalkMessage {
            id: format!("mars:{seq}"),
            origin: "mars".into(),
            seq,
            ts: seq * 1000,
            scope: TalkScope::Task {
                machine: "mars".into(),
                root_session: "lead".into(),
            },
            author: crate::talk::TalkAuthor {
                machine: "mars".into(),
                machine_label: "mars".into(),
                voice: crate::talk::TalkVoice {
                    session_id: Some(who.into()),
                    label: Some(who.into()),
                    kind: Some("claude".into()),
                    human: false,
                    channel: None,
                    channel_quote: None,
                },
            },
            kind: TalkKind::Message,
            to: None,
            reply_to: None,
            text: text.into(),
        };
        app.board.merge(vec![
            said(1, "lead", "take the parser"),
            said(2, "scout", "on it"),
            said(3, "digger", "found the retry"),
        ]);

        let mut form = BoardForm {
            tab: BoardTab::Task,
            ..BoardForm::default()
        };
        let backend = TestBackend::new(114, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_board_modal(frame, &app, &mut form, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let drawn: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let indent = |needle: &str| -> usize {
            let row = drawn
                .iter()
                .find(|text| text.contains(needle))
                .unwrap_or_else(|| panic!("no row for {needle} in {drawn:?}"));
            row.find(needle).expect("just matched")
        };
        // Two spaces per step down the chain, so the shape reads the same way
        // the agent list does.
        assert_eq!(indent("scout@mars"), indent("lead@mars") + 2, "{drawn:?}");
        assert_eq!(indent("digger@mars"), indent("lead@mars") + 4, "{drawn:?}");
        // And the tab says which task it is showing, since the answer changes
        // with whatever the agent list is standing on.
        assert!(
            drawn.iter().any(|text| text.contains("[Task]")),
            "{drawn:?}"
        );
    }

    #[test]
    fn waiting_agent_item_is_entirely_yellow_bold() {
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
        app.focus = Focus::Agents;
        app.sessions.push(AgentSession {
            id: "muxloomd-temporal-codex-waiting".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work/hidden-for-temporal".into(),
            label: "approval needed".into(),
            created_at: 1,
            archived_at: None,
            dead: false,
            pid: Some(1),
            working: false,
            needs_attention: true,
            attention_reason: Some("command approval".into()),
            recap: Some("approve the command".into()),
            title: None,
            thread: None,
            parent: None,
        });
        app.selected_session_id = Some("muxloomd-temporal-codex-waiting".into());

        let backend = TestBackend::new(70, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Temporal Chat"));
        assert!(!rendered.contains("hidden-for-temporal"));

        // Row 1 is the folder heading; the selected agent occupies rows 2-4.
        for y in 2..=4 {
            for x in 1..69 {
                let cell = buffer.cell((x, y)).unwrap();
                assert_eq!(cell.fg, Color::Yellow, "cell {x},{y} was not yellow");
                assert!(
                    cell.modifier.contains(Modifier::BOLD),
                    "cell {x},{y} was not bold"
                );
            }
        }
    }

    #[test]
    fn the_agent_at_the_top_of_a_scrolled_list_still_says_which_folder_it_is_in() {
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
        for folder in ["/work/alpha", "/work/beta"] {
            for index in 0..4 {
                app.sessions.push(AgentSession {
                    id: format!("muxloomd-codex{}-{index}", folder.replace('/', "-")),
                    target_id: "local".into(),
                    kind: AgentKind::Codex,
                    path: folder.into(),
                    label: format!("{folder} agent {index}"),
                    created_at: index,
                    archived_at: None,
                    dead: false,
                    pid: Some(1),
                    working: false,
                    needs_attention: false,
                    attention_reason: None,
                    recap: None,
                    title: None,
                    thread: None,
                    parent: None,
                });
            }
        }
        // The last agent of the second folder: reaching it scrolls the first
        // folder's band, and every one of its rows, off the top.
        app.selected_session_id = Some("muxloomd-codex-work-beta-0".into());

        let border = |terminal: &Terminal<TestBackend>| -> String {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect()
        };

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        assert!(app.agent_list_state.offset() > 0, "the list has to scroll");
        let scrolled = border(&terminal);
        assert!(
            scrolled.contains("/work/alpha"),
            "the top row's folder belongs on the pane edge: {scrolled:?}"
        );

        // Climbing back up to the first agent of a folder is the case that
        // stung: the list stops with that agent's own band one row too high.
        app.selected_session_id = Some("muxloomd-codex-work-alpha-3".into());
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        assert_eq!(app.agent_list_state.offset(), 1);
        assert!(border(&terminal).contains("/work/alpha"));

        // Nothing scrolled off: the bands are all on screen and the edge says
        // nothing they do not already say.
        app.agent_list_state = Default::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        assert_eq!(app.agent_list_state.offset(), 0);
        let whole = border(&terminal);
        assert!(!whole.contains("/work/"), "nothing scrolled off: {whole:?}");
    }

    #[test]
    fn running_agent_effect_glyphs_are_single_column() {
        for kind in [AgentKind::Codex, AgentKind::Claude] {
            for frame in 0..16 {
                let glyph = running_agent_effect(kind, frame);
                assert_eq!(
                    UnicodeWidthStr::width(glyph),
                    1,
                    "spinner glyph {glyph:?} must occupy exactly one column"
                );
            }
        }
    }

    #[test]
    fn preview_wrapping_windows_rows_and_drops_control_chars() {
        // A single long logical line hard-wraps into ceil(len / width) rows and
        // the height estimate matches what is rendered.
        let long = "a".repeat(250);
        let mut render = PreviewRender::lines(Vec::new(), vec![Line::raw(long)]);
        render.measure(80);
        assert_eq!(render.height(), 4);

        // The window returns exactly the visible rows, none wider than the pane.
        let top = render.window(0, 2);
        assert_eq!(top.len(), 2);
        assert!(top.iter().all(|line| line.width() == 80));

        // Scrolling past the full rows yields only the short trailing remainder.
        let tail = render.window(3, 2);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].width(), 250 - 80 * 3);

        // Control / zero-width characters are stripped so they cannot shift the
        // column accounting and leave stray glyphs behind while scrolling.
        let mut dirty = PreviewRender::lines(Vec::new(), vec![Line::raw("a\u{1b}b")]);
        dirty.measure(80);
        assert_eq!(dirty.height(), 1);
        let rendered: String = dirty.window(0, 1)[0]
            .spans
            .iter()
            .flat_map(|span| span.content.chars())
            .collect();
        assert_eq!(rendered, "ab");
    }

    #[test]
    fn preview_window_scrolls_to_the_requested_row() {
        let mut render = PreviewRender::lines(
            Vec::new(),
            (0..6)
                .map(|index| Line::raw(format!("line{index}")))
                .collect::<Vec<_>>(),
        );
        render.measure(80);
        assert_eq!(render.height(), 6);
        assert_eq!(
            preview_rows(&render.window(2, 3)),
            ["line2", "line3", "line4"]
        );
    }

    /// A large body is kept as source and only the visible rows are built, so
    /// scrolling stays cheap; the rows still have to be exact.
    #[test]
    fn plain_previews_window_without_materialising_the_file() {
        let content = (0..2_000)
            .map(|index| format!("line-{index}\n"))
            .collect::<String>();
        let mut render = plain_render(Vec::new(), &content);
        render.measure(80);
        assert_eq!(render.height(), 2_000);
        assert_eq!(
            preview_rows(&render.window(1_998, 4)),
            ["line-1998", "line-1999"]
        );
        // Re-measuring at the same width must not rebuild the row index.
        render.measure(80);
        assert_eq!(render.height(), 2_000);
    }

    fn preview_rows(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn markdown_preview_renders_headings_bold_tables_and_rules() {
        let lines = markdown_lines(concat!(
            "# One\n## Two\n### Three\n#### Four\n",
            "plain **bold** text\n\n",
            "| Name | Value |\n| --- | ---: |\n| alpha | 1 |\n",
            "---\n"
        ));
        assert!(lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content == "One"
                    && span.style.add_modifier.contains(Modifier::UNDERLINED)
                    && span.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
        assert!(lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
        assert!(
            lines
                .iter()
                .any(|line| { line.spans.iter().any(|span| span.content.contains('┼')) })
        );
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.starts_with("────────────────"))
        }));
    }

    #[test]
    fn a_wide_but_short_window_keeps_all_three_panes() {
        // 200x15 used to collapse to a single pane because one flag covered
        // both axes, hiding the machine list on a screen with room to spare.
        assert!(!compact_layout(false, Rect::new(0, 0, 200, 15), false));
        assert!(compact_layout(false, Rect::new(0, 0, 60, 40), false));
        // Hysteresis: a window already compact holds that layout for a few
        // more columns rather than flipping on every cell of a resize.
        assert!(compact_layout(true, Rect::new(0, 0, 74, 40), false));
        assert!(!compact_layout(false, Rect::new(0, 0, 74, 40), false));
        assert!(!compact_layout(true, Rect::new(0, 0, 76, 40), false));
    }

    #[test]
    fn moving_focus_leaves_every_pane_where_it_was() {
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
        // The focused pane used to grow by 8-10 columns, which moved both
        // dividers and resized the attached PTY on every pane switch.
        for (area, portrait) in [
            (Rect::new(0, 0, 160, 30), false),
            (Rect::new(0, 0, 60, 100), true),
        ] {
            app.focus = Focus::Recap;
            let unfocused = compute_layout(&app, area, portrait, false);
            for focus in [Focus::Machines, Focus::Agents] {
                app.focus = focus;
                let layout = compute_layout(&app, area, portrait, false);
                assert_eq!(layout.machines, unfocused.machines, "{focus:?} {portrait}");
                assert_eq!(layout.agents, unfocused.agents, "{focus:?} {portrait}");
                assert_eq!(layout.recap, unfocused.recap, "{focus:?} {portrait}");
            }
        }
    }

    #[test]
    fn portrait_layout_places_terminal_above_machine_and_folder_lists() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            state,
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );

        let layout = compute_layout(&app, Rect::new(0, 0, 60, 100), true, false);
        let terminal = layout.recap.unwrap();
        let machines = layout.machines.unwrap();
        let folders = layout.agents.unwrap();
        assert_eq!(terminal, Rect::new(0, 0, 60, 65));
        assert_eq!(machines.y, terminal.height);
        assert_eq!(folders.y, terminal.height);
        assert_eq!(machines.height, 35);
        assert_eq!(folders.x, machines.width);
        assert_eq!(machines.width + folders.width, 60);
    }

    /// The handle is what tells you the split can be dragged, and a horizontal
    /// divider is a full-width line: one accent cell in the middle of it was
    /// indistinguishable from the border it sits on.
    #[test]
    fn a_divider_handle_is_a_bar_wide_enough_to_notice() {
        assert_eq!(grip(80, 6, 9, 21), 13);
        assert_eq!(grip(30, 6, 9, 21), 9, "short dividers keep the minimum");
        assert_eq!(grip(300, 6, 9, 21), 21, "long ones stop being a border");
        assert_eq!(grip(4, 6, 9, 21), 4, "never longer than what it sits on");

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
        let backend = TestBackend::new(60, 100);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let divider = app
            .pane_layout
            .portrait_terminal_divider
            .expect("a tall window splits horizontally");
        let buffer = terminal.backend().buffer();
        let bar: Vec<u16> = (0..60)
            .filter(|x| buffer.cell((*x, divider.y)).unwrap().symbol() == "━")
            .collect();
        assert_eq!(bar.len(), grip(divider.width, 6, 9, 21) as usize);
        // Centred, and unbroken.
        assert_eq!(bar.last().unwrap() - bar[0], bar.len() as u16 - 1);
        assert_eq!(bar[0] + *bar.last().unwrap(), 59);
        assert_eq!(buffer.cell((bar[0], divider.y)).unwrap().fg, ACCENT);
    }

    #[test]
    fn portrait_detection_prefers_pixels_and_uses_cell_aspect_as_fallback() {
        let cells = Rect::new(0, 0, 180, 110);
        assert!(portrait_layout(cells, Some((1200, 1800))));
        assert!(!portrait_layout(cells, Some((1800, 1200))));
        assert!(portrait_layout(cells, None));
    }

    #[test]
    fn compact_layout_only_fullscreens_the_focused_terminal() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::local()],
            worker,
        );
        let area = Rect::new(0, 0, 40, 20);
        let mut app = app;
        app.focus = Focus::Recap;
        let terminal = compute_layout(&app, area, true, true);
        assert_eq!(terminal.recap, Some(area));
        assert!(terminal.machines.is_none());
        assert!(terminal.agents.is_none());

        app.focus = Focus::Agents;
        let agents = compute_layout(&app, area, true, true);
        assert!(agents.agents.is_some());
        assert!(agents.recap.is_some());
    }

    #[test]
    fn renders_archives_search_and_common_footer_actions() {
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
        app.focus = Focus::Agents;
        app.sessions.push(AgentSession {
            id: "ad-codex-dead".into(),
            target_id: "local".into(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "optional-name".into(),
            created_at: 1,
            archived_at: None,
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
            title: None,
            thread: None,
            parent: None,
        });
        let backend = TestBackend::new(150, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Archived (1)"));
        assert!(rendered.contains("a expand"));
        assert!(rendered.contains("/ search"));
        assert!(!rendered.contains("GROUPED"));
        assert!(!rendered.contains("ENABLED ONLY"));
        assert!(!rendered.contains('▣'));
        assert!(!rendered.contains(", settings"));

        app.modal = Some(Modal::Search(SearchForm {
            query: "needle".into(),
            submitted_query: "needle".into(),
            results: vec![crate::model::SearchResult {
                session_id: "ad-codex-dead".into(),
                target_id: "local".into(),
                kind: AgentKind::Codex,
                label: "optional-name".into(),
                path: "/work".into(),
                match_kind: SearchMatchKind::Name,
                snippet: "optional-name".into(),
                line_number: None,
                created_at: 1,
                dead: true,
            }],
            result_rows: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            edited_at: std::time::Instant::now(),
            reading: None,
        }));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Search all agent history"));
        assert!(rendered.contains("exact optional name/path, recap, then newest history"));
        assert!(rendered.contains("optional-name"));

        // Mid-search the same list is on screen with a bar over it: what has
        // been read of what there is, and what has turned up so far.
        if let Some(Modal::Search(form)) = app.modal.as_mut() {
            form.loading = true;
            form.reading = Some((3, 12));
        }
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("3/12 histories read"));
        assert!(rendered.contains("1 matches so far"));
        assert!(rendered.contains('·'), "the unread part of the bar shows");
        assert!(
            rendered.contains("optional-name"),
            "and the names found already stay listed while it runs"
        );

        app.modal = Some(Modal::PathPicker(PathPickerForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Codex,
                path: "/work".into(),
                label: String::new(),
                temporary: false,
                field: LaunchField::Path,
            },
            path: "/work".into(),
            directories: vec!["src".into(), "tests".into()],
            query: String::new(),
            selected: 0,
            loading: false,
            error: None,
        }));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // The title truncates to the bar, so on a machine with a longer name
        // than this one only the start of it survives: that the name is what
        // goes there is the claim, not that every letter fits.
        let named: String = crate::model::own_machine_name().chars().take(6).collect();
        assert!(
            rendered.contains(&format!("Folders on {named}")),
            "{rendered}"
        );
        assert!(rendered.contains("src/"));
        assert!(rendered.contains("Type to match"));
        assert!(rendered.contains("Enter use"));

        app.modal = Some(Modal::Resume(ResumeForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Claude,
                path: "/work".into(),
                label: String::new(),
                temporary: false,
                field: LaunchField::Path,
            },
            candidates: vec![crate::model::ResumeCandidate {
                id: "resume-id".into(),
                kind: AgentKind::Claude,
                source_path: "/home/test/.claude/projects/resume-id.jsonl".into(),
                recap: None,
                first_message: Some("first user message".into()),
                last_message: Some("last user message".into()),
                updated_at: "2026-07-21T12:00:00Z".into(),
            }],
            revive: None,
            selected: 0,
            loading: false,
            error: None,
            query: String::new(),
            history_hits: Vec::new(),
            history_selected: 0,
            searched_query: String::new(),
            search_edited_at: None,
            history_loading: false,
        }));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("New session"));
        assert!(rendered.contains("first user message"));

        app.modal = None;
        app.file_manager = Some(FileManagerForm {
            origin: FileManagerOrigin::TerminalPane,
            target: Target::local(),
            session_id: None,
            path: "/work".into(),
            entries: vec![crate::model::FileEntry {
                name: "README.md".into(),
                path: "/work/README.md".into(),
                kind: crate::model::FileEntryKind::File,
                symlink: false,
                size: 42,
                mtime: 0,
            }],
            selected: 0,
            loading: false,
            error: None,
            directory_cache: std::collections::HashMap::new(),
            return_path: None,
            preview_path: Some("/work/README.md".into()),
            preview: Some(crate::model::FilePreview {
                path: "/work/README.md".into(),
                mime: "text/markdown".into(),
                kind: crate::model::FilePreviewKind::Markdown,
                size: 42,
                content: "# File preview\n\n- item".into(),
                truncated: false,
            }),
            preview_requested_path: Some("/work/README.md".into()),
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
            preview_cache: std::collections::HashMap::new(),
            preload_pending: std::collections::HashSet::new(),
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
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // Same as the folders title, and tighter: this one is drawn into the
        // agents pane rather than a centred dialog, so on a machine with a long
        // enough name the path after it is off the bar too. The name is what is
        // being checked for.
        assert!(rendered.contains(&format!("Files  {named}")), "{rendered}");
        assert!(rendered.contains("README.md"));
        assert!(rendered.contains("File preview"));
        assert!(rendered.contains("Enter close"));
        assert!(app.pane_layout.machines.is_none());
        assert!(app.pane_layout.agents.is_some());
        assert!(app.pane_layout.recap.is_some());

        let form = app.file_manager.as_mut().unwrap();
        form.origin = FileManagerOrigin::AgentPane;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.pane_layout.machines.is_some());
        assert!(app.pane_layout.agents.is_some());
        assert!(app.pane_layout.recap.is_some());

        app.modal = Some(Modal::Help(HelpForm {
            offset: HELP_CONTENT_ROWS - 1,
        }));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Touch Screens"));
        assert!(rendered.contains("View And Configuration"));
        assert!(rendered.contains("Home/End jump"));
    }

    #[test]
    fn footer_renders_controller_task_progress_at_bottom_right() {
        let config = Config::default();
        let worker = Worker::start(Runtime::new(&config));
        let mut app = App::new(
            config,
            PathBuf::from("unused-config.toml"),
            State::default(),
            PathBuf::from("unused-state.json"),
            vec![Target::ssh("gpu-box")],
            worker,
        );
        app.task_progress.push((
            "gpu-box".into(),
            TaskKind::Install,
            TaskProgress::bytes("Downloading Claude", 1, Some(2)),
        ));
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("gpu-box: Downloading Claude 50%"));
        assert!(rendered.contains('█'));
    }

    #[test]
    fn a_preview_parked_at_the_bottom_follows_the_file_as_it_grows() {
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
        let mut form = FileManagerForm {
            origin: FileManagerOrigin::TerminalPane,
            target: Target::local(),
            session_id: None,
            path: "/work".into(),
            entries: vec![crate::model::FileEntry {
                name: "notes.log".into(),
                path: "/work/notes.log".into(),
                kind: crate::model::FileEntryKind::File,
                symlink: false,
                size: 42,
                mtime: 1,
            }],
            selected: 0,
            loading: false,
            error: None,
            directory_cache: std::collections::HashMap::new(),
            return_path: None,
            preview_path: Some("/work/notes.log".into()),
            preview: None,
            preview_requested_path: None,
            preview_loading: false,
            preview_error: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_page_rows: 1,
            preview_follow_tail: true,
            preview_stamp: Some((42, 1)),
            preview_rendered: None,
            query: String::new(),
            search_request_id: None,
            searching: false,
            search_truncated: false,
            search_edited_at: None,
            preview_cache: std::collections::HashMap::new(),
            preload_pending: std::collections::HashSet::new(),
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
        };
        let log = |lines: usize| crate::model::FilePreview {
            path: "/work/notes.log".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 42,
            content: (1..=lines)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            truncated: false,
        };
        form.preview = Some(log(80));
        app.file_manager = Some(form);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = |terminal: &Terminal<TestBackend>| -> String {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        };
        assert!(rendered(&terminal).contains("line-80"));

        // The monitor swaps in a longer file: the view must move on to the new
        // last line rather than staying on the old one.
        let form = app.file_manager.as_mut().unwrap();
        form.preview = Some(log(120));
        form.preview_rendered = None;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains("line-120"));
        assert!(!screen.contains("line-80"), "the old tail scrolled away");

        // Scrolled off the bottom, the same refresh leaves the reader alone.
        let form = app.file_manager.as_mut().unwrap();
        form.preview_follow_tail = false;
        form.preview_scroll = 0;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains("line-1 "));
        assert!(!screen.contains("line-120"));
    }

    #[test]
    fn structured_previews_parse_and_highlight_common_text_formats() {
        let json = file_preview_render(&crate::model::FilePreview {
            path: "/tmp/data.json".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 20,
            content: r#"{"name":"muxloom","count":2}"#.into(),
            truncated: false,
        });
        let json_text = rendered_text(&json);
        assert!(json_text.contains("\"name\""));
        assert!(json_text.contains("\"muxloom\""));
        assert!(json.body.len() > 3, "JSON should be pretty printed");

        let jsonl = file_preview_render(&crate::model::FilePreview {
            path: "/tmp/events.jsonl".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 30,
            content: "{\"id\":1}\n{\"id\":2}".into(),
            truncated: false,
        });
        let jsonl_text = rendered_text(&jsonl);
        assert!(jsonl_text.contains("record 1"));
        assert!(jsonl_text.contains("record 2"));

        let csv = file_preview_render(&crate::model::FilePreview {
            path: "/tmp/data.csv".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 30,
            content: "name,count\nmuxloom,2".into(),
            truncated: false,
        });
        let csv_text = rendered_text(&csv);
        assert!(csv_text.contains("name"));
        assert!(csv_text.contains("muxloom"));
        assert!(csv.pinned.iter().any(|line| line.spans.iter().any(|span| {
            span.content.contains("name") && span.style.add_modifier.contains(Modifier::BOLD)
        })));

        let rust = file_preview_render(&crate::model::FilePreview {
            path: "/tmp/main.rs".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 12,
            content: "fn main() {}".into(),
            truncated: false,
        });
        assert!(preview_lines(&rust).iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("fn") && span.style.fg.is_some())
        }));
    }

    /// A header stays pinned above the viewport while the body pages, and both
    /// the ruler and the row gutter number what is on screen.
    #[test]
    fn delimited_previews_pin_the_header_and_number_rows_and_columns() {
        let content = std::iter::once("name,count".to_string())
            .chain((1..=40).map(|index| format!("row{index},{index}")))
            .collect::<Vec<_>>()
            .join("\n");
        let mut render = file_preview_render(&crate::model::FilePreview {
            path: "/tmp/data.csv".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: content.len() as u64,
            content,
            truncated: false,
        });
        render.measure(60);
        // Metadata, the column ruler, the header and the rule sit above the body.
        let pinned = preview_rows(&render.pinned_window(render.pinned_height()));
        assert!(
            pinned
                .iter()
                .any(|row| row.contains("# ") && row.contains(" 1 ") && row.contains(" 2 "))
        );
        assert!(
            pinned
                .iter()
                .any(|row| row.contains("name") && row.contains("count"))
        );

        // The header is not repeated in the body, and every row is numbered.
        assert_eq!(render.height(), 40);
        let top = preview_rows(&render.window(0, 2));
        assert!(top[0].contains(" 1 ") && top[0].contains("row1"));
        assert!(top[1].contains(" 2 ") && top[1].contains("row2"));

        // Paging keeps the pinned block intact and renumbers the visible rows.
        let paged = preview_rows(&render.window(38, 2));
        assert!(paged[0].contains("39") && paged[0].contains("row39"));
        assert!(paged[1].contains("40") && paged[1].contains("row40"));
        assert_eq!(render.pinned_height(), pinned.len());
    }

    /// Without a header every record is data, so nothing is stolen from the top
    /// of the file and the numbering starts at the first row.
    #[test]
    fn delimited_previews_without_a_header_number_every_record() {
        let mut render = file_preview_render(&crate::model::FilePreview {
            path: "/tmp/values.csv".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 12,
            content: "1,2\n3,4".into(),
            truncated: false,
        });
        render.measure(40);
        assert_eq!(render.height(), 2);
        let rows = preview_rows(&render.window(0, 2));
        assert!(rows[0].contains("1") && rows[0].contains("2"));
        assert!(rows[1].contains("3") && rows[1].contains("4"));
    }

    #[test]
    fn markdown_table_renders_inline_bold_spans() {
        let lines = markdown_lines("| name | status |\n| --- | --- |\n| muxloom | **ready** |");
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.contains("ready") && span.style.add_modifier.contains(Modifier::BOLD)
        })));
        assert!(
            !lines
                .iter()
                .any(|line| line.spans.iter().any(|span| span.content.contains("**")))
        );
    }

    #[test]
    fn media_frame_uses_half_blocks_for_two_pixel_rows() {
        let text = media_frame_text(&crate::media::MediaFrame {
            width: 1,
            height: 2,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
            sequence: 0,
        });
        assert_eq!(text.height(), 1);
        let span = &text.lines[0].spans[0];
        assert_eq!(span.content, "▄");
        assert_eq!(span.style.bg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(span.style.fg, Some(Color::Rgb(0, 0, 255)));
    }

    /// Every row of a rendered preview, pinned block first, so assertions can
    /// look at the whole thing without going through the viewport.
    fn preview_lines(render: &PreviewRender) -> Vec<Line<'static>> {
        let mut lines = render.pinned.clone();
        lines.extend((0..render.body.len()).map(|index| render.body.line(index)));
        lines
    }

    fn rendered_text(render: &PreviewRender) -> String {
        preview_rows(&preview_lines(render)).join("\n")
    }
}
