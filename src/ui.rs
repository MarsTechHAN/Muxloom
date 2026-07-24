use std::{collections::HashMap, path::Path, sync::LazyLock};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
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
        App, FileManagerForm, FileManagerOrigin, Focus, HELP_CONTENT_ROWS, HelpForm, LaunchField,
        LaunchForm, Modal, PaneLayout, PathPickerForm, ResumeForm, SearchForm, SettingsForm,
        SettingsScope,
    },
    debug,
    model::{AgentKind, ConnectionState, FileEntryKind, FilePreviewKind, SearchMatchKind},
};

const ACCENT: Color = Color::Rgb(112, 184, 255);
const CODEX: Color = Color::Cyan;
const CLAUDE: Color = Color::Rgb(215, 119, 87);
const TERMINAL: Color = Color::Green;
const MUTED: Color = Color::DarkGray;
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
    let compact = if portrait {
        area.width < 48 || area.height < 28
    } else {
        area.width < 72 || area.height < 16
    };
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
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, app, vertical[0]);
    draw_content(frame, app, vertical[1], portrait, compact);
    draw_footer(frame, app, vertical[2]);

    if let Some(modal) = app.modal.as_mut() {
        draw_modal(frame, modal, area);
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
        .filter(|session| session.dead && session.kind != AgentKind::Terminal)
        .count();
    let launch_target = app
        .targets
        .get(app.selected_target)
        .map(|target| target.target.label.as_str())
        .unwrap_or("none");
    let first = Line::from(vec![
        Span::styled(
            " MUXLOOM ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "persistent multi-machine agent sessions",
            Style::default().fg(Color::Gray),
        ),
    ]);
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
        app.attention_banner = Some(Rect::new(area.x, area.y + 2, area.width, 1));
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
        app.attention_banner = None;
        Line::raw("")
    };
    frame.render_widget(Paragraph::new(vec![first, second, third]), area);
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
    if app
        .file_manager
        .as_ref()
        .is_some_and(|form| form.origin == FileManagerOrigin::TerminalPane)
    {
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
        let base_machine_percent = app.state.portrait_machine_percent.clamp(25, 75);
        let machine_percent = match app.focus {
            Focus::Machines => base_machine_percent.saturating_add(10),
            Focus::Agents => base_machine_percent.saturating_sub(10),
            Focus::Recap => base_machine_percent,
        }
        .clamp(20, 80);
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
        let mut agents_width = base_width + if app.focus == Focus::Agents { 10 } else { 0 };
        agents_width = agents_width.min(area.width.saturating_sub(28));
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

    let mut machine_width =
        app.state.machine_width.clamp(16, 52) + if app.focus == Focus::Machines { 8 } else { 0 };
    let base_agents_width = if app.file_manager.is_some() {
        app.state.file_width.clamp(22, 72)
    } else {
        app.state.agents_width.clamp(24, 72)
    };
    let mut agents_width = base_agents_width + if app.focus == Focus::Agents { 10 } else { 0 };
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
        let y = divider.y.saturating_add(divider.height / 2);
        frame.render_widget(
            Paragraph::new("│").style(style),
            Rect::new(divider.x, y, 1, 1),
        );
    }
    if let Some(divider) = panes.portrait_terminal_divider {
        let x = divider.x.saturating_add(divider.width / 2);
        frame.render_widget(
            Paragraph::new("─").style(style),
            Rect::new(x, divider.y, 1, 1),
        );
    }
}

fn draw_machines(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.visible_target_indices();
    let name_width = area.width.saturating_sub(10).max(1) as usize;
    let mut items = Vec::new();
    let mut rows = Vec::new();
    for target_index in &visible {
        let status = &app.targets[*target_index];
        let codex_working = app.sessions.iter().any(|session| {
            session.target_id == status.target.id
                && session.kind == AgentKind::Codex
                && session.working
                && !session.dead
        });
        let claude_working = app.sessions.iter().any(|session| {
            session.target_id == status.target.id
                && session.kind == AgentKind::Claude
                && session.working
                && !session.dead
        });
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
            Line::from(vec![
                Span::raw("    "),
                runtime_capability(
                    AgentKind::Codex,
                    status.probe.codex,
                    codex_working,
                    app.animation_frame,
                ),
                Span::raw(" "),
                runtime_capability(
                    AgentKind::Claude,
                    status.probe.claude,
                    claude_working,
                    app.animation_frame,
                ),
            ])
        } else {
            Line::styled("    disabled", Style::default().fg(MUTED))
        };
        lines.push(detail);
        rows.push((*target_index, lines.len() as u16));
        items.push(ListItem::new(lines));
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "No enabled machines. Press v to show all.",
            Style::default().fg(MUTED),
        )));
    }
    app.machine_rows = rows;
    let selected = visible
        .iter()
        .position(|target_index| *target_index == app.selected_target);
    app.machine_list_state.select(selected);
    let title = if app.state.hide_disabled {
        " Machines - enabled "
    } else {
        " Machines "
    };
    let list = List::new(items)
        .block(panel(title, app.focus == Focus::Machines))
        .highlight_style(Style::default().bg(Color::Rgb(42, 48, 58)))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.machine_list_state);
}

fn draw_agents(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if let Some(form) = app.file_manager.as_mut() {
        app.agent_rows.clear();
        app.archive_row = None;
        draw_file_browser(frame, form, area, app.focus == Focus::Agents);
        return;
    }
    let sessions: Vec<_> = app.visible_sessions().into_iter().cloned().collect();
    let mut working_by_group = HashMap::<String, (bool, bool)>::new();
    for session in &sessions {
        if session.dead || !session.working {
            continue;
        }
        let group = if app.state.flatten {
            format!("{}  {}", session.target_id, session.path)
        } else {
            session.path.clone()
        };
        let active = working_by_group.entry(group).or_default();
        match session.kind {
            AgentKind::Codex => active.0 = true,
            AgentKind::Claude => active.1 = true,
            AgentKind::Terminal => {}
        }
    }
    let archived_count = app.archived_count();
    let mut items = Vec::new();
    let mut row_ids = Vec::new();
    let mut selected_row = None;
    let mut previous_group = String::new();
    let mut archive_header_added = false;
    app.archive_row = None;

    for session in sessions {
        if session.dead && !archive_header_added {
            app.archive_row = Some(items.len());
            items.push(archive_item(archived_count, true));
            row_ids.push((None, 1));
            previous_group.clear();
            archive_header_added = true;
        }
        let group = if app.state.flatten {
            format!("{}  {}", session.target_id, session.path)
        } else {
            session.path.clone()
        };
        if group != previous_group {
            let active = working_by_group.get(&group).copied().unwrap_or_default();
            let activity_width = usize::from(active.0) * 2 + usize::from(active.1) * 2;
            let mut spans = vec![Span::styled(
                truncate(
                    &group,
                    area.width
                        .saturating_sub(4)
                        .saturating_sub(activity_width as u16) as usize,
                ),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )];
            if active.0 {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    running_agent_effect(AgentKind::Codex, app.animation_frame),
                    Style::default().fg(CODEX).add_modifier(Modifier::BOLD),
                ));
            }
            if active.1 {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    running_agent_effect(AgentKind::Claude, app.animation_frame),
                    Style::default().fg(CLAUDE).add_modifier(Modifier::BOLD),
                ));
            }
            items.push(ListItem::new(Line::from(spans)));
            row_ids.push((None, 1));
            previous_group = group;
        }

        let row = items.len();
        if app.selected_session_id.as_deref() == Some(&session.id) {
            selected_row = Some(row);
        }
        let (icon, runtime_name, _) = agent_visual(session.kind);
        let state = if session.dead {
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
        let activity = if session.needs_attention {
            "!"
        } else if session.working && !session.dead {
            running_agent_effect(session.kind, app.animation_frame)
        } else {
            icon
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(
                activity,
                Style::default()
                    .fg(if session.needs_attention {
                        Color::Yellow
                    } else if session.kind == AgentKind::Claude {
                        CLAUDE
                    } else {
                        CODEX
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                truncate(
                    session.display_label(),
                    area.width.saturating_sub(10) as usize,
                ),
                Style::default().fg(Color::White),
            ),
        ])];
        if selected {
            let value_width = area.width.saturating_sub(14) as usize;
            lines.push(Line::from(vec![
                Span::styled("    folder  ", Style::default().fg(MUTED)),
                Span::styled(
                    truncate(&session.path, value_width),
                    Style::default().fg(Color::Gray),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    recap   ", Style::default().fg(MUTED)),
                Span::styled(
                    truncate(&app.recap_for(&session), value_width),
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    status  ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{icon} {runtime_name}  {state}"),
                    Style::default().fg(state_color),
                ),
            ]));
        }
        let height = lines.len() as u16;
        items.push(ListItem::new(lines));
        row_ids.push((Some(session.id), height));
    }
    if archived_count > 0 && !app.state.show_archived {
        app.archive_row = Some(items.len());
        items.push(archive_item(archived_count, false));
        row_ids.push((None, 1));
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "No sessions. Press n to launch one.",
            Style::default().fg(MUTED),
        )));
        row_ids.push((None, 1));
    }

    app.agent_rows = row_ids;
    app.agent_list_state.select(selected_row);
    let title = if app.state.flatten {
        " Agents - all machines "
    } else {
        " Agents by folder "
    };
    let list = List::new(items)
        .block(panel(title, app.focus == Focus::Agents))
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(42, 48, 58))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.agent_list_state);
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
    let selected = app.selected_session().cloned();
    let current_matches = app.terminal_session_id.as_deref() == app.selected_session_id.as_deref();
    let pending_matches =
        app.pending_terminal_session_id.as_deref() == app.selected_session_id.as_deref();
    let loading = if app.history_loading {
        " [loading]"
    } else {
        ""
    };
    let title = if pending_matches && !current_matches && app.history_offset == 0 {
        " Switching terminal - keeping previous frame ".into()
    } else if current_matches && app.terminal.is_some() && app.history_offset == 0 {
        format!(
            " Attached terminal [{}] ",
            if app.interactive {
                "INPUT"
            } else {
                "CONNECTED"
            }
        )
    } else if app.history_offset > 0 {
        format!(
            " Terminal history - {} lines from bottom{loading} ",
            app.history_offset,
        )
    } else if let Some(session) = &selected {
        if session.dead {
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

    if app.attached_terminal_for_selected() {
        // A live emulator scrolls through its own rendered scrollback, so
        // back-scroll shows real lines instead of a mangled raw-log replay.
        let offset = app.history_offset;
        let show_cursor = app.interactive && offset == 0;
        let selection = app.terminal_selection;
        if let Some(terminal) = app.terminal.as_mut() {
            terminal.set_scrollback(offset);
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

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let busy = if app.busy_operations > 0 {
        "  [working]"
    } else {
        ""
    };
    let help = if app.interactive {
        "  Cmd/Opt+Arrow panes  Shift/Opt+Enter newline  PgUp history"
    } else if area.width < 88 {
        "  n new  Enter open  Ctrl-f files  / search  q quit"
    } else {
        match app.focus {
            Focus::Machines => "  Space toggle  n new  / search  q quit  ? more",
            Focus::Agents => {
                if app.archived_count() > 0 {
                    if app.state.show_archived {
                        "  Enter open  a collapse  / search  n new  q quit  ? more"
                    } else {
                        "  Enter open  a expand  / search  n new  q quit  ? more"
                    }
                } else {
                    "  Enter open  / search  n new  q quit  ? more"
                }
            }
            Focus::Recap => "  Cmd/Opt+Arrow panes  PgUp history  / search  q quit  ? more",
        }
    };
    let help_width = UnicodeWidthStr::width(help);
    let status_width = (area.width as usize).saturating_sub(help_width + busy.len() + 2);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {}{busy}", truncate(&app.status_message, status_width)),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(help, Style::default().fg(MUTED)),
        ])),
        area,
    );
}

fn draw_modal(frame: &mut Frame<'_>, modal: &mut Modal, outer: Rect) {
    match modal {
        Modal::Launch(form) => draw_launch_modal(frame, form, outer),
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
        Modal::ConfirmInstall { launch, .. } => {
            let area = centered_rect(68, 11, outer);
            frame.render_widget(Clear, area);
            let text = vec![
                Line::raw(""),
                Line::raw(format!(
                    "{} was not detected on {}.",
                    launch.kind, launch.target.label
                )),
                Line::raw(""),
                Line::raw("Install it now, then continue launching this agent?"),
                Line::styled(
                    "Uses a compatible local binary or downloads the checked target package locally.",
                    Style::default().fg(MUTED),
                ),
                Line::styled(
                    "The target needs no internet; its configured installer is only the final fallback.",
                    Style::default().fg(MUTED),
                ),
                Line::raw(""),
                Line::styled(
                    "Enter/y install    Esc/n cancel",
                    Style::default().fg(MUTED),
                ),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .block(panel(" Install agent runtime ", true)),
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
        Modal::Help(form) => draw_help_modal(frame, form, outer),
        Modal::Settings(form) => draw_settings_modal(frame, form, outer),
        Modal::Search(form) => draw_search_modal(frame, form, outer),
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
        if form.loading {
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
            format!("  match: {}", form.query)
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
    let items: Vec<_> = if form.loading && form.entries.is_empty() {
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
            "Empty directory",
            Style::default().fg(MUTED),
        ))]
    } else {
        form.entries
            .iter()
            .map(|entry| {
                let (icon, color) = match entry.kind {
                    FileEntryKind::Directory => ("▸", ACCENT),
                    FileEntryKind::File => ("·", Color::White),
                    FileEntryKind::Symlink => ("↗", Color::Cyan),
                    FileEntryKind::Other => ("?", MUTED),
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
        .highlight_style(Style::default().bg(Color::Rgb(42, 48, 58)))
        .highlight_symbol("> ");
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

    let footer = if form.preview_path.is_some() {
        "Double-click/Enter close  Scroll or arrows page  Right-click parent"
    } else if form.loading {
        "Click select  Double-click open  Right-click parent  loading"
    } else {
        "Click select  Double-click open  Right-click parent  c copy  d download"
    };
    frame.render_widget(
        Paragraph::new(truncate(footer, rows[1].width as usize)).style(Style::default().fg(MUTED)),
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
    let block = panel(&title, true);
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
    if form.preview_rendered.is_none() {
        form.preview_rendered = form.preview.as_ref().map(file_preview_text);
    }
    let preview = if let Some(error) = &form.preview_error {
        Text::from(Line::styled(error.clone(), Style::default().fg(Color::Red)))
    } else if form.preview_loading {
        Text::from(Line::styled(
            "Loading preview...",
            Style::default().fg(MUTED),
        ))
    } else if let Some(preview) = &form.preview_rendered {
        preview.clone()
    } else {
        Text::from(Line::styled(
            "No preview output",
            Style::default().fg(MUTED),
        ))
    };
    let total_rows = wrapped_text_height(&preview, inner.width);
    form.preview_max_scroll = total_rows
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    form.preview_page_rows = inner.height.max(1);
    form.preview_scroll = form.preview_scroll.min(form.preview_max_scroll);
    // Wrap and window the preview ourselves instead of leaning on ratatui's
    // Paragraph scroll+wrap. That path miscounts wrapped rows for the very long
    // lines in JSON/JSONL session dumps and, once scrolled, leaves stray glyphs
    // on otherwise-empty rows. Emitting exactly the visible rows keeps scroll
    // bounds accurate and drops control characters that would shift columns.
    let window = preview_window(
        &preview,
        inner.width,
        form.preview_scroll as usize,
        inner.height as usize,
    );
    // Remember the plain text of the visible rows so a mouse selection over the
    // preview can be copied, mirroring terminal selection.
    form.preview_text_area = Some(inner);
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
    frame.render_widget(Paragraph::new(Text::from(window)), inner);
    highlight_terminal_selection(frame, inner, selection);
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

fn wrapped_text_height(text: &Text<'_>, width: u16) -> usize {
    let width = width.max(1) as usize;
    text.lines
        .iter()
        .map(|line| wrapped_row_count(line, width))
        .sum()
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
fn preview_window(text: &Text<'_>, width: u16, start: usize, height: usize) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let width = width.max(1) as usize;
    let end = start.saturating_add(height);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut row = 0usize;
    for line in &text.lines {
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

fn file_preview_text(preview: &crate::model::FilePreview) -> Text<'static> {
    let mut lines = vec![file_preview_metadata(preview)];
    if preview.truncated {
        lines.push(Line::styled(
            "Preview truncated at 256 KiB",
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.push(Line::raw(""));
    match preview.kind {
        FilePreviewKind::Markdown => lines.extend(markdown_lines(&preview.content)),
        FilePreviewKind::Text => lines.extend(text_preview_lines(preview)),
        FilePreviewKind::Image => lines.push(Line::styled(
            "Image decoding has not started.",
            Style::default().fg(MUTED),
        )),
        FilePreviewKind::Audio => lines.push(Line::styled(
            "Audio playback is not implemented yet.",
            Style::default().fg(MUTED),
        )),
        FilePreviewKind::Video => lines.push(Line::styled(
            "Video decoding has not started.",
            Style::default().fg(MUTED),
        )),
        FilePreviewKind::Binary => lines.push(Line::styled(
            "Binary content is not rendered.",
            Style::default().fg(MUTED),
        )),
    }
    Text::from(lines)
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

fn text_preview_lines(preview: &crate::model::FilePreview) -> Vec<Line<'static>> {
    let extension = Path::new(&preview.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "csv" => delimited_lines(&preview.content, b','),
        "tsv" => delimited_lines(&preview.content, b'\t'),
        "json" => parsed_json_lines(&preview.content)
            .unwrap_or_else(|| syntax_lines(&preview.content, &preview.path, Some("json"))),
        "jsonl" | "ndjson" => json_lines(&preview.content),
        _ => syntax_lines(&preview.content, &preview.path, None),
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

fn delimited_lines(content: &str, delimiter: u8) -> Vec<Line<'static>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(content.as_bytes());
    let mut rows = Vec::new();
    for record in reader.records() {
        match record {
            Ok(record) => rows.push(
                record
                    .iter()
                    .map(|cell| cell.replace(['\r', '\n'], " "))
                    .collect(),
            ),
            Err(error) => {
                return vec![Line::styled(
                    format!("Could not parse delimited data: {error}"),
                    Style::default().fg(Color::Red),
                )];
            }
        }
    }
    data_table_lines(&rows, false)
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

fn format_bytes(size: u64) -> String {
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
        help_row("Left / Right", "Choose Codex, Claude, or Terminal"),
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
        help_row(
            "Mouse drag",
            "Select and copy terminal text; Alt-drag forwards",
        ),
        help_row(
            "x",
            "Archive live agents; permanently remove archived agents",
        ),
        help_row("a", "Expand or collapse Archived sessions"),
        help_row("e", "Rename the selected agent's display name"),
        help_row("Up twice at top", "Open the first agent waiting for input"),
        Line::raw(""),
        help_header("Machines"),
        help_row("Space", "Enable or disable the selected machine"),
        help_row("v / Ctrl-h", "Hide disabled machines or show all"),
        help_row("r / Ctrl-r", "Refresh enabled machines now"),
        Line::raw(""),
        help_header("History And Search"),
        help_row("Wheel / PageUp", "Scroll one line / move one history page"),
        help_row("PageDown", "Move back toward the live terminal"),
        help_row("/ / Ctrl-p", "Search every discovered agent history"),
        help_row("Enter in search", "Open the selected match"),
        Line::raw(""),
        help_header("File Manager"),
        help_row(
            "Ctrl-f",
            "Open or close files on the selected agent machine",
        ),
        help_row("Arrows / Enter", "Select, move to parent, or open an entry"),
        help_row("d", "Download the selected file to Downloads"),
        help_row("c", "Copy the selected target path to the clipboard"),
        help_row("Drop local files", "Upload them into the visible directory"),
        Line::raw(""),
        help_header("View And Configuration"),
        help_row("f", "Toggle grouped and flat agent views"),
        help_row(",", "Edit configuration for the selected machine"),
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
    frame.render_widget(
        Paragraph::new(truncate(&form.path, inner.width as usize))
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let query_prefix = "Match: ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(query_prefix, Style::default().fg(ACCENT)),
            Span::styled(form.query.as_str(), Style::default().fg(Color::White)),
        ])),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
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
    frame.render_widget(
        Paragraph::new(truncate(status, inner.width as usize)).style(Style::default().fg(
            if form.error.is_some() {
                Color::Red
            } else {
                MUTED
            },
        )),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );
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
        let selected = index == form.selected;
        let row_text = format!("{} {directory}/", if selected { ">" } else { " " });
        frame.render_widget(
            Paragraph::new(truncate(&row_text, inner.width as usize)).style(if selected {
                Style::default().fg(Color::White).bg(Color::Rgb(42, 48, 58))
            } else {
                Style::default().fg(Color::Gray)
            }),
            Rect::new(inner.x, inner.y + 3 + visible as u16, inner.width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(
            "Type to match  Backspace/Ctrl-u edit  Arrows navigate  Enter use  Esc back",
        )
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

    let new_selected = form.selected == 0;
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
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let status = if form.loading {
        "Scanning resumable sessions... Enter starts New immediately"
    } else if let Some(error) = &form.error {
        error
    } else if form.candidates.is_empty() {
        "No matching history; press Enter for a new session"
    } else {
        "Resume history for this exact working directory"
    };
    frame.render_widget(
        Paragraph::new(truncate(status, inner.width as usize)).style(Style::default().fg(
            if form.error.is_some() {
                Color::Yellow
            } else {
                MUTED
            },
        )),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    let available = inner.height.saturating_sub(4) as usize;
    let selected_candidate = form.selected.saturating_sub(1);
    let start = selected_candidate.saturating_sub(available.saturating_sub(4));
    let mut y = inner.y + 2;
    let last_y = inner.y + inner.height.saturating_sub(1);
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
                    "{} Resume  {}",
                    if selected { ">" } else { " " },
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
        Paragraph::new("Up/Down select   Enter launch   Left/Esc edit runtime/path")
            .style(Style::default().fg(MUTED)),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );
}

fn draw_search_modal(frame: &mut Frame<'_>, form: &mut SearchForm, outer: Rect) {
    let area = centered_rect(104, 31, outer);
    frame.render_widget(Clear, area);
    let block = panel(" Search all agent history ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let query_prefix = "Search: ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(query_prefix, Style::default().fg(ACCENT)),
            Span::styled(form.query.as_str(), Style::default().fg(Color::White)),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let status = if form.loading {
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
    let label_width = 27u16.min(inner.width / 2);
    let value_width = inner.width.saturating_sub(label_width + 1) as usize;
    let visible_fields = inner.height.saturating_sub(3) as usize;
    let start = form
        .selected
        .saturating_add(1)
        .saturating_sub(visible_fields);
    for (visible_index, (index, (label, value))) in form
        .labels()
        .iter()
        .zip(&form.values)
        .enumerate()
        .skip(start)
        .take(visible_fields)
        .enumerate()
    {
        let row = Rect::new(inner.x, inner.y + visible_index as u16, inner.width, 1);
        let active = index == form.selected;
        let shown = tail_display(value, value_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{label:<width$}", width = label_width as usize),
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
            ])),
            row,
        );
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
    if form.selected < form.values.len() {
        let shown = tail_display(&form.values[form.selected], value_width);
        let cursor = inner
            .x
            .saturating_add(label_width + 1)
            .saturating_add(UnicodeWidthStr::width(shown.as_str()) as u16)
            .min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cursor, inner.y + form.selected.saturating_sub(start) as u16));
    }
}

fn draw_launch_modal(frame: &mut Frame<'_>, form: &LaunchForm, outer: Rect) {
    let area = centered_rect(70, 13, outer);
    frame.render_widget(Clear, area);
    let inner = Block::default()
        .title(format!(" New agent on {} ", form.target.label))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT));
    let content = inner.inner(area);
    frame.render_widget(inner, area);

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
    let kinds = Line::from(vec![
        segment(" CODEX ", form.kind == AgentKind::Codex, CODEX),
        Span::raw("  "),
        segment(" CLAUDE ", form.kind == AgentKind::Claude, CLAUDE),
        Span::raw("  "),
        segment(" TERMINAL ", form.kind == AgentKind::Terminal, TERMINAL),
        Span::styled("  Left/Right", Style::default().fg(MUTED)),
    ]);
    frame.render_widget(
        Paragraph::new(kinds).style(field_style(form.field == LaunchField::Kind)),
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
    frame.render_widget(
        Paragraph::new("Enter advances runtime -> folder -> New/Resume    Tab edits label")
            .style(Style::default().fg(MUTED)),
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

fn panel<'a>(title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
}

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

fn agent_visual(kind: AgentKind) -> (&'static str, &'static str, Color) {
    match kind {
        AgentKind::Codex => ("◉", "Codex", CODEX),
        AgentKind::Claude => ("✻", "Claude Code", CLAUDE),
        AgentKind::Terminal => ("▣", "Terminal", TERMINAL),
    }
}

fn running_agent_effect(kind: AgentKind, frame: u64) -> &'static str {
    // Codex is a cyan rotating braille spinner (single-column, so it keeps a
    // stable footprint); Claude keeps its own orange sparkle, matching the
    // asterisk glyphs Claude Code itself cycles through. Both advance one frame
    // per `frame`, so with a time-based counter they animate at a constant rate
    // regardless of how often the UI redraws.
    const CODEX_FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠇"];
    const CLAUDE_FRAMES: [&str; 6] = ["✻", "✽", "✶", "✳", "✶", "✽"];
    match kind {
        AgentKind::Codex => CODEX_FRAMES[(frame % CODEX_FRAMES.len() as u64) as usize],
        AgentKind::Claude => CLAUDE_FRAMES[(frame % CLAUDE_FRAMES.len() as u64) as usize],
        AgentKind::Terminal => "▣",
    }
}

fn segment(label: &'static str, selected: bool, color: Color) -> Span<'static> {
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
        app::App,
        config::{Config, State},
        model::{AgentKind, AgentSession, Target},
        runtime::Runtime,
        worker::Worker,
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
                dead: false,
                pid: Some(100),
                working: false,
                needs_attention: true,
                attention_reason: Some("approve".into()),
                recap: None,
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

    #[test]
    fn working_agent_animation_is_visible_in_machine_and_folder_panes() {
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
                dead: false,
                pid: Some(1),
                working: true,
                needs_attention: false,
                attention_reason: None,
                recap: None,
            });
        }

        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_machines(frame, &mut app, frame.area()))
            .unwrap();
        let machines = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(machines.contains('⠋'));
        assert!(machines.contains('✻'));

        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .unwrap();
        let folders = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(folders.contains("/work/project ⠋ ✻"));

        app.animation_frame = 20;
        terminal
            .draw(|frame| draw_machines(frame, &mut app, frame.area()))
            .unwrap();
        let next_frame = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(next_frame.contains('⠼'));
        assert!(next_frame.contains('✶'));
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
        let text = Text::from(vec![Line::raw(long)]);
        assert_eq!(wrapped_row_count(&text.lines[0], 80), 4);
        assert_eq!(wrapped_text_height(&text, 80), 4);

        // The window returns exactly the visible rows, none wider than the pane.
        let top = preview_window(&text, 80, 0, 2);
        assert_eq!(top.len(), 2);
        assert!(top.iter().all(|line| line.width() == 80));

        // Scrolling past the full rows yields only the short trailing remainder.
        let tail = preview_window(&text, 80, 3, 2);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].width(), 250 - 80 * 3);

        // Control / zero-width characters are stripped so they cannot shift the
        // column accounting and leave stray glyphs behind while scrolling.
        let dirty = Text::from(vec![Line::raw("a\u{1b}b")]);
        assert_eq!(wrapped_row_count(&dirty.lines[0], 80), 1);
        let rendered: String = preview_window(&dirty, 80, 0, 1)[0]
            .spans
            .iter()
            .flat_map(|span| span.content.chars())
            .collect();
        assert_eq!(rendered, "ab");
    }

    #[test]
    fn preview_window_scrolls_to_the_requested_row() {
        let text = Text::from(
            (0..6)
                .map(|index| Line::raw(format!("line{index}")))
                .collect::<Vec<_>>(),
        );
        assert_eq!(wrapped_text_height(&text, 80), 6);
        let window = preview_window(&text, 80, 2, 3);
        let rendered: Vec<String> = window
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(rendered, vec!["line2", "line3", "line4"]);
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
    fn focused_sidebars_expand() {
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
        let area = Rect::new(0, 0, 160, 30);
        app.focus = Focus::Machines;
        let machine_focused = compute_layout(&app, area, false, false);
        app.focus = Focus::Agents;
        let agents_focused = compute_layout(&app, area, false, false);
        assert!(machine_focused.machines.unwrap().width > agents_focused.machines.unwrap().width);
        assert!(agents_focused.agents.unwrap().width > machine_focused.agents.unwrap().width);
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
            dead: true,
            pid: None,
            working: false,
            needs_attention: false,
            attention_reason: None,
            recap: None,
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

        app.modal = Some(Modal::PathPicker(PathPickerForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Codex,
                path: "/work".into(),
                label: String::new(),
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
        assert!(rendered.contains("Folders on This machine"));
        assert!(rendered.contains("src/"));
        assert!(rendered.contains("Type to match"));
        assert!(rendered.contains("Enter use"));

        app.modal = Some(Modal::Resume(ResumeForm {
            launch: LaunchForm {
                target: Target::local(),
                kind: AgentKind::Claude,
                path: "/work".into(),
                label: String::new(),
                field: LaunchField::Path,
            },
            candidates: vec![crate::model::ResumeCandidate {
                id: "resume-id".into(),
                recap: None,
                first_message: Some("first user message".into()),
                last_message: Some("last user message".into()),
                updated_at: "2026-07-21T12:00:00Z".into(),
            }],
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
                size: 42,
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
            preview_rendered: None,
            query: String::new(),
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
        assert!(rendered.contains("Files  This machine:/work"));
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
        assert!(rendered.contains("History And Search"));
        assert!(rendered.contains("View And Configuration"));
        assert!(rendered.contains("Home/End jump"));
    }

    #[test]
    fn structured_previews_parse_and_highlight_common_text_formats() {
        let json = file_preview_text(&crate::model::FilePreview {
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
        assert!(json.height() > 3, "JSON should be pretty printed");

        let jsonl = file_preview_text(&crate::model::FilePreview {
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

        let csv = file_preview_text(&crate::model::FilePreview {
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
        assert!(csv.lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.contains("name") && span.style.add_modifier.contains(Modifier::BOLD)
        })));

        let rust = file_preview_text(&crate::model::FilePreview {
            path: "/tmp/main.rs".into(),
            mime: "text/plain".into(),
            kind: FilePreviewKind::Text,
            size: 12,
            content: "fn main() {}".into(),
            truncated: false,
        });
        assert!(rust.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("fn") && span.style.fg.is_some())
        }));
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

    fn rendered_text(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
