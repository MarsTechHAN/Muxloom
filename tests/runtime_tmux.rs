use std::{process::Command, sync::Mutex, thread, time::Duration};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use muxloom::{
    config::{CommandConfig, Config},
    model::{AgentKind, LaunchRequest, Target},
    runtime::Runtime,
    terminal_session::TerminalSession,
};

struct SessionGuard<'a> {
    runtime: &'a Runtime,
    target: &'a Target,
    session_id: String,
}

static TMUX_TEST_LOCK: Mutex<()> = Mutex::new(());

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let _ = self.runtime.kill(self.target, &self.session_id);
    }
}

#[test]
fn local_session_survives_agent_exit_and_is_discoverable() {
    let _test_lock = TMUX_TEST_LOCK.lock().unwrap();
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping integration check");
        return;
    }

    let config = Config::default();
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let request = LaunchRequest {
        target: target.clone(),
        kind: AgentKind::Codex,
        path: std::env::temp_dir().display().to_string(),
        label: "integration smoke".into(),
        resume_id: None,
    };
    let command = CommandConfig {
        command: "sh".into(),
        args: vec!["-c".into(), "printf 'muxloom-smoke\\n'; sleep 0.1".into()],
    };

    let session_id = runtime.launch(&request, &command).unwrap();
    let _guard = SessionGuard {
        runtime: &runtime,
        target: &target,
        session_id: session_id.clone(),
    };

    let mut found = None;
    for _ in 0..20 {
        let (_, sessions) = runtime
            .probe_and_discover(&target, "sh", "definitely-not-an-agent-command")
            .unwrap();
        found = sessions
            .into_iter()
            .find(|session| session.id == session_id);
        if found.as_ref().is_some_and(|session| session.dead) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let session = found.expect("launched session should be discoverable");
    assert!(session.dead, "tmux should preserve the exited pane");
    assert_eq!(session.path, request.path);
    assert_eq!(session.label, request.label);
    let recap = runtime.capture(&target, &session_id, 20).unwrap();
    assert!(
        recap.contains("muxloom-smoke"),
        "recap did not contain command output: {recap:?}"
    );
}

#[test]
fn discovers_legacy_agent_deck_sessions_after_the_rename() {
    let _test_lock = TMUX_TEST_LOCK.lock().unwrap();
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping integration check");
        return;
    }

    let config = Config::default();
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let session_id = format!("ad-codex-legacy-{}", std::process::id());
    let path = std::env::temp_dir().display().to_string();
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session_id, "-c", &path])
        .status()
        .unwrap();
    assert!(status.success());
    let _guard = SessionGuard {
        runtime: &runtime,
        target: &target,
        session_id: session_id.clone(),
    };
    for (name, value) in [
        ("@agentdeck_kind", "codex"),
        ("@agentdeck_path", path.as_str()),
        ("@agentdeck_label", "legacy session"),
        ("@agentdeck_created", "123"),
    ] {
        let status = Command::new("tmux")
            .args(["set-option", "-t", &session_id, name, value])
            .status()
            .unwrap();
        assert!(status.success());
    }

    let (_, sessions) = runtime.probe_and_discover(&target, "sh", "sh").unwrap();
    let session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("legacy agent-deck session should remain discoverable");
    assert_eq!(session.kind, AgentKind::Codex);
    assert_eq!(session.path, path);
    assert_eq!(session.label, "legacy session");
    assert_eq!(session.created_at, 123);
}

#[test]
fn embedded_pty_attaches_renders_and_accepts_input() {
    let _test_lock = TMUX_TEST_LOCK.lock().unwrap();
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping integration check");
        return;
    }

    let config = Config::default();
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let request = LaunchRequest {
        target: target.clone(),
        kind: AgentKind::Claude,
        path: std::env::temp_dir().display().to_string(),
        label: "pty smoke".into(),
        resume_id: None,
    };
    let command = CommandConfig {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            concat!(
                "printf '\\033[?1049h\\033[31mREADY\\033[0m'; ",
                "IFS= read -r line; ",
                "printf '\\033[2J\\033[HREPLY:%s' \"$line\"; sleep 1"
            )
            .into(),
        ],
    };

    let session_id = runtime.launch(&request, &command).unwrap();
    let _guard = SessionGuard {
        runtime: &runtime,
        target: &target,
        session_id: session_id.clone(),
    };
    let mut terminal = TerminalSession::attach(&target, &session_id, 60, 12).unwrap();

    wait_for_screen(&mut terminal, "READY");
    for character in "hello".chars() {
        terminal
            .write_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            .unwrap();
    }
    terminal
        .write_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    wait_for_screen(&mut terminal, "REPLY:hello");
}

#[test]
fn ordinary_terminal_with_empty_command_stays_running() {
    let _test_lock = TMUX_TEST_LOCK.lock().unwrap();
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping integration check");
        return;
    }
    let config = Config::default();
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let request = LaunchRequest {
        target: target.clone(),
        kind: AgentKind::Terminal,
        path: std::env::temp_dir().display().to_string(),
        label: "ordinary terminal".into(),
        resume_id: None,
    };
    let session_id = runtime
        .launch(&request, config.agents.get(AgentKind::Terminal))
        .unwrap();
    let _guard = SessionGuard {
        runtime: &runtime,
        target: &target,
        session_id: session_id.clone(),
    };
    thread::sleep(Duration::from_millis(150));
    let (_, sessions) = runtime.probe_and_discover(&target, "sh", "sh").unwrap();
    let session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("ordinary terminal should be discoverable");
    assert_eq!(session.kind, AgentKind::Terminal);
    assert!(!session.dead, "login shell should remain interactive");
}

#[test]
fn history_reads_do_not_resize_attached_pane_and_full_search_finds_matches() {
    let _test_lock = TMUX_TEST_LOCK.lock().unwrap();
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping integration check");
        return;
    }
    let config = Config::default();
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let request = LaunchRequest {
        target: target.clone(),
        kind: AgentKind::Codex,
        path: std::env::temp_dir().display().to_string(),
        label: "history and resize".into(),
        resume_id: None,
    };
    let command = CommandConfig {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "i=0; while [ $i -lt 80 ]; do printf 'line-%s\\n' \"$i\"; i=$((i+1)); done; printf 'full-history-needle\\nREADY\\n'; IFS= read -r line"
                .into(),
        ],
    };
    let session_id = runtime.launch(&request, &command).unwrap();
    let _guard = SessionGuard {
        runtime: &runtime,
        target: &target,
        session_id: session_id.clone(),
    };
    let mut terminal = TerminalSession::attach(&target, &session_id, 73, 17).unwrap();
    wait_for_screen(&mut terminal, "READY");
    thread::sleep(Duration::from_millis(100));
    let before = pane_size(&session_id);

    let page = runtime
        .capture_page(&target, &session_id, 0, 50, 140, 40)
        .unwrap();
    let matches = runtime
        .search_history(&target, &session_id, "full-history-needle", 8)
        .unwrap();
    let after = pane_size(&session_id);

    assert_eq!(before, after, "history capture changed the tmux pane size");
    assert!(page.text.contains("READY"));
    assert!(
        matches
            .iter()
            .any(|item| item.text.contains("full-history-needle"))
    );
}

#[test]
fn local_directory_listing_and_resume_scan_commands_execute() {
    let _test_lock = TMUX_TEST_LOCK.lock().unwrap();
    let config = Config::default();
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let root = std::env::temp_dir().join(format!("muxloom-picker-{}", std::process::id()));
    std::fs::create_dir_all(root.join("alpha")).unwrap();
    std::fs::create_dir_all(root.join("beta")).unwrap();
    let listing = runtime
        .list_directory(&target, &root.display().to_string())
        .unwrap();
    assert_eq!(listing.directories, ["alpha", "beta"]);

    let cwd = std::env::current_dir().unwrap();
    runtime
        .scan_resumes(&target, AgentKind::Claude, &cwd.display().to_string())
        .unwrap();
    runtime
        .scan_resumes(&target, AgentKind::Codex, &cwd.display().to_string())
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

fn pane_size(session_id: &str) -> String {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            session_id,
            "#{pane_width}x#{pane_height}",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn wait_for_screen(terminal: &mut TerminalSession, expected: &str) {
    for _ in 0..100 {
        terminal.drain();
        if terminal.screen().contents().contains(expected) {
            return;
        }
        assert!(
            !terminal.is_closed(),
            "embedded terminal closed unexpectedly"
        );
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "embedded terminal never rendered {expected:?}; screen was {:?}",
        terminal.screen().contents()
    );
}
