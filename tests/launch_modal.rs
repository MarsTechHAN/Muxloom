//! The new-agent modal has three rows but only two of them are text. Every
//! editing key has to survive the cursor sitting on the runtime row, which is a
//! choice rather than a field.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use muxloom::{
    app::{App, LaunchField, LaunchForm, Modal},
    config::{Config, State},
    model::{AgentKind, Target},
    runtime::Runtime,
    worker::Worker,
};

fn make_app() -> App {
    let config = Config::default();
    let worker = Worker::start(Runtime::new(&config));
    let mut state = State::default();
    state.enabled_hosts.insert("local".into());
    App::new(
        config,
        PathBuf::from("unused-config.toml"),
        state,
        PathBuf::from("unused-state.json"),
        vec![Target::local()],
        worker,
    )
}

fn launch_form(field: LaunchField) -> LaunchForm {
    LaunchForm {
        target: Target::local(),
        kind: AgentKind::Codex,
        path: "/work/terminal".into(),
        label: "build".into(),
        temporary: false,
        field,
    }
}

fn form_of(app: &App) -> &LaunchForm {
    match app.modal.as_ref() {
        Some(Modal::Launch(form)) => form,
        other => panic!("launch modal expected, got {other:?}"),
    }
}

#[test]
fn editing_keys_on_the_runtime_row_do_nothing() {
    for key in [
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    ] {
        let mut app = make_app();
        app.modal = Some(Modal::Launch(launch_form(LaunchField::Kind)));
        app.handle_key(key);
        let form = form_of(&app);
        assert_eq!(form.field, LaunchField::Kind, "{key:?} moved the cursor");
        assert_eq!(form.path, "/work/terminal", "{key:?} edited the path");
        assert_eq!(form.label, "build", "{key:?} edited the label");
    }
}

#[test]
fn pasting_onto_the_runtime_row_does_nothing() {
    let mut app = make_app();
    app.modal = Some(Modal::Launch(launch_form(LaunchField::Kind)));
    app.handle_paste("pasted".into());
    let form = form_of(&app);
    assert_eq!(form.path, "/work/terminal");
    assert_eq!(form.label, "build");
}

#[test]
fn editing_keys_still_reach_the_text_rows() {
    let mut app = make_app();
    app.modal = Some(Modal::Launch(launch_form(LaunchField::Label)));
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
    assert_eq!(form_of(&app).label, "built");

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(form_of(&app).label.is_empty());

    app.handle_paste("release".into());
    assert_eq!(form_of(&app).label, "release");
}
