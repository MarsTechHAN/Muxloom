//! Modal bodies are laid out at fixed row offsets, but `centered_rect` shrinks
//! a modal to fit the terminal. Rendering a widget outside the frame buffer
//! panics and takes the whole dashboard down, so every modal is swept across
//! degenerate terminal sizes here.

use std::path::PathBuf;

use muxloom::{
    app::{
        App, HelpForm, LaunchForm, Modal, PathPickerForm, PortForwardForm, ResumeForm, SearchForm,
        SettingsForm,
    },
    config::{Config, State},
    model::{AgentKind, AgentSession, Target},
    port_forward::{PortForwardState, PortForwardSummary},
    runtime::Runtime,
    ui,
    worker::Worker,
};
use ratatui::{Terminal, backend::TestBackend};

fn make_app() -> App {
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
    app
}

const SIZES: &[(u16, u16)] = &[
    (1, 1),
    (2, 2),
    (5, 2),
    (10, 3),
    (20, 5),
    (30, 8),
    (40, 10),
    (47, 27),
    (48, 28),
    (60, 4),
    (80, 6),
    (80, 8),
    (100, 12),
    (120, 40),
];

fn forward(id: u64, port: u16) -> PortForwardSummary {
    PortForwardSummary {
        id,
        target_id: "local".into(),
        session_id: "s".into(),
        folder: "/work".into(),
        remote_host: "127.0.0.1".into(),
        remote_port: port,
        local_port: port,
        state: PortForwardState::Active,
    }
}

fn port_forward_form(count: u64) -> PortForwardForm {
    PortForwardForm {
        target: Target::local(),
        session_id: "s".into(),
        folder: "/work".into(),
        remote_host: "127.0.0.1".into(),
        remote_port: String::new(),
        local_port: String::new(),
        detected_ports: vec![3000, 3001],
        active: (1..=count).map(|i| forward(i, 3000 + i as u16)).collect(),
        selected: 0,
        loading: false,
        error: Some("something went wrong while starting the tunnel".into()),
        detection_error: None,
    }
}

fn draw_with(modal: Option<Modal>, w: u16, h: u16) {
    let mut app = make_app();
    app.modal = modal;
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
}

fn render_rows(modal: Option<Modal>, w: u16, h: u16) -> (App, Vec<String>) {
    let mut app = make_app();
    app.modal = modal;
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect();
    (app, rows)
}

fn render_text(modal: Option<Modal>, w: u16, h: u16) -> String {
    render_rows(modal, w, h).1.join("")
}

#[test]
fn no_modal_never_panics() {
    for (w, h) in SIZES {
        draw_with(None, *w, *h);
    }
}

#[test]
fn port_forward_modal_never_panics() {
    for (w, h) in SIZES {
        for count in [0u64, 1, 4, 12] {
            for selected in [0usize, 2, 3] {
                let mut form = port_forward_form(count);
                form.selected = selected;
                draw_with(Some(Modal::PortForward(form)), *w, *h);
            }
        }
    }
}

#[test]
fn help_modal_never_panics() {
    for (w, h) in SIZES {
        draw_with(Some(Modal::Help(HelpForm::default())), *w, *h);
    }
}

#[test]
fn search_modal_never_panics() {
    for (w, h) in SIZES {
        let form = SearchForm {
            query: "needle".into(),
            submitted_query: "needle".into(),
            results: Vec::new(),
            result_rows: Vec::new(),
            selected: 0,
            loading: false,
            error: Some("machine unreachable".into()),
            edited_at: std::time::Instant::now(),
        };
        draw_with(Some(Modal::Search(form)), *w, *h);
    }
}

#[test]
fn settings_modal_never_panics() {
    for (w, h) in SIZES {
        for (scope, rows) in [
            (
                muxloom::app::SettingsScope::Global,
                &muxloom::app::GLOBAL_SETTINGS[..],
            ),
            (
                muxloom::app::SettingsScope::Host("gpu".into()),
                &muxloom::app::HOST_SETTINGS[..],
            ),
        ] {
            let values: Vec<String> = rows
                .iter()
                .filter_map(|row| match row {
                    muxloom::app::SettingsRow::Field(label) => Some(format!("{label} value")),
                    _ => None,
                })
                .collect();
            let notes: Vec<String> = rows
                .iter()
                .filter_map(|row| match row {
                    muxloom::app::SettingsRow::Note(label) => Some(format!("{label} note")),
                    _ => None,
                })
                .collect();
            let focusable = rows
                .iter()
                .filter(|row| {
                    matches!(
                        row,
                        muxloom::app::SettingsRow::Field(_) | muxloom::app::SettingsRow::Action(_)
                    )
                })
                .count();
            for selected in [0usize, 5, 9, focusable - 1] {
                let form = SettingsForm {
                    scope: scope.clone(),
                    selected: selected.min(focusable - 1),
                    values: values.clone(),
                    notes: notes.clone(),
                    error: Some("bad value".into()),
                };
                draw_with(Some(Modal::Settings(form)), *w, *h);
            }
        }
    }
}

#[test]
fn launch_and_picker_modals_never_panic() {
    for (w, h) in SIZES {
        let launch = LaunchForm {
            target: Target::local(),
            kind: AgentKind::Codex,
            path: "/some/very/long/path/that/keeps/going/on/and/on/forever".into(),
            label: "a fairly long label for the agent".into(),
            field: muxloom::app::LaunchField::Path,
            temporary: false,
        };
        draw_with(Some(Modal::Launch(launch.clone())), *w, *h);

        let picker = PathPickerForm {
            launch: launch.clone(),
            path: "/home/user".into(),
            directories: (0..40).map(|i| format!("folder-{i}")).collect(),
            query: String::new(),
            selected: 30,
            loading: false,
            error: Some("permission denied".into()),
        };
        draw_with(Some(Modal::PathPicker(picker)), *w, *h);

        let resume = ResumeForm {
            launch,
            candidates: Vec::new(),
            selected: 0,
            loading: false,
            error: Some("scan failed".into()),
            query: String::new(),
            history_hits: Vec::new(),
            history_selected: 0,
            searched_query: String::new(),
            search_edited_at: None,
        };
        draw_with(Some(Modal::Resume(resume)), *w, *h);
    }
}

/// A short terminal used to drop the launch form's value rows on the floor:
/// the captions rendered and the fields the user was typing into did not.
#[test]
fn launch_modal_keeps_its_fields_on_a_short_terminal() {
    for (w, h) in [(40u16, 10u16), (30, 8), (60, 5)] {
        let launch = LaunchForm {
            target: Target::local(),
            kind: AgentKind::Codex,
            path: "/work".into(),
            label: "build".into(),
            field: muxloom::app::LaunchField::Path,
            temporary: false,
        };
        let text = render_text(Some(Modal::Launch(launch)), w, h);
        assert!(
            text.contains("Dir /work"),
            "the focused directory field vanished at {w}x{h}: {text}"
        );
    }
}

/// The root split gave the content pane five rows before the footer got its
/// one, so on a short terminal the only on-screen list of keys was the first
/// thing dropped.
#[test]
fn footer_keeps_its_row_on_a_short_terminal() {
    for (w, h) in [(80u16, 6u16), (80, 8), (80, 9), (100, 12), (120, 40)] {
        let (_, rows) = render_rows(None, w, h);
        let footer = rows.last().unwrap();
        assert!(
            footer.contains("quit"),
            "the key hints vanished at {w}x{h}: {footer:?}"
        );
    }
}

/// The banner was registered as clickable at a fixed offset into the header,
/// which on a squeezed header pointed at a row the alert was never drawn on.
#[test]
fn attention_banner_is_only_clickable_where_it_is_drawn() {
    for (w, h) in [(80u16, 6u16), (80, 8), (80, 10), (100, 12), (120, 40)] {
        let (app, rows) = render_rows(None, w, h);
        let banner = app
            .attention_banner
            .unwrap_or_else(|| panic!("no banner registered at {w}x{h}"));
        assert!(
            rows[banner.y as usize].contains("INPUT REQUIRED"),
            "the banner rect at row {} holds {:?} at {w}x{h}",
            banner.y,
            rows[banner.y as usize]
        );
    }
}
