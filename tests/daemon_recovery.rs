#![cfg(unix)]

//! A daemon that is killed outright must not take its sessions with it. The
//! PTY cannot survive, but the record and the log can, and the next generation
//! is expected to bring them back as archived sessions.

use std::{
    fs,
    io::Read,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use muxloom::daemon_protocol::{DaemonRequest, DaemonResponse, DaemonSession, Frame, FrameKind};

struct TestState {
    root: PathBuf,
}

impl TestState {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("mxr-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn serve(&self) -> Child {
        let mut child = Command::new(env!("CARGO_BIN_EXE_muxloomd"))
            .env("MUXLOOMD_STATE_DIR", &self.root)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(&self.root.join("muxloomd.sock"), &mut child);
        child
    }

    fn pid(&self) -> i32 {
        fs::read_to_string(self.root.join("muxloomd.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(self.root.join("muxloomd.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }
}

impl Drop for TestState {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.root.join("muxloomd.pid"))
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_for(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating its socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

fn request(stream: &mut UnixStream, request_id: u64, request: &DaemonRequest) -> DaemonResponse {
    Frame::json(FrameKind::Request, 0, request_id, request)
        .unwrap()
        .write_to(stream)
        .unwrap();
    loop {
        let frame = Frame::read_from(stream).unwrap().expect("daemon closed");
        if frame.kind == FrameKind::Response && frame.request_id == request_id {
            return frame.decode_json::<DaemonResponse>().unwrap();
        }
    }
}

fn sessions(stream: &mut UnixStream, request_id: u64) -> Vec<DaemonSession> {
    match request(stream, request_id, &DaemonRequest::ListSessions) {
        DaemonResponse::Sessions { sessions } => sessions,
        response => panic!("unexpected list response: {response:?}"),
    }
}

fn launch(stream: &mut UnixStream, request_id: u64, session_id: &str, script: &str) {
    let response = request(
        stream,
        request_id,
        &DaemonRequest::Launch {
            session_id: session_id.into(),
            kind: "claude".into(),
            path: "/tmp".into(),
            label: "recovery smoke".into(),
            temporary: false,
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            environment: vec![],
            created_at: 1_700_000_000,
            columns: 80,
            rows: 24,
        },
    );
    assert!(
        matches!(response, DaemonResponse::Launched { .. }),
        "unexpected launch response: {response:?}"
    );
}

/// Read a session's history back the way an attaching client does.
fn history(state: &TestState, session_id: &str) -> String {
    let mut stream = state.connect();
    Frame::json(
        FrameKind::Request,
        0,
        90,
        &DaemonRequest::ReadHistory {
            session_id: session_id.into(),
            offset_from_bottom: 0,
            lines: 200,
            rendered: false,
        },
    )
    .unwrap()
    .write_to(&mut stream)
    .unwrap();
    let mut history = Vec::new();
    loop {
        let frame = Frame::read_from(&mut stream)
            .unwrap()
            .expect("daemon closed");
        match frame.kind {
            FrameKind::Data if frame.request_id == 90 => {
                history.extend(frame.decoded_payload().unwrap())
            }
            FrameKind::Response if frame.request_id == 90 => {
                match frame.decode_json::<DaemonResponse>().unwrap() {
                    DaemonResponse::HistoryComplete { .. } => break,
                    response => panic!("unexpected history response: {response:?}"),
                }
            }
            _ => {}
        }
    }
    String::from_utf8_lossy(&history).into_owned()
}

#[test]
fn a_killed_daemon_leaves_its_sessions_recoverable_as_archived() {
    let state = TestState::new();
    let killed = state.serve();
    let daemon_pid = state.pid();
    let session_id = "muxloomd-claude-1700000000-77-0";
    let temporary_id = "muxloomd-temporal-claude-1700000000-77-1";

    let mut client = state.connect();
    launch(
        &mut client,
        10,
        session_id,
        "printf '\\342\\217\\272 finished the migration\\r\\n'; sleep 300",
    );
    // A temporary chat keeps no transcript, so a crash must discard it rather
    // than archive it.
    let response = request(
        &mut client,
        11,
        &DaemonRequest::Launch {
            session_id: temporary_id.into(),
            kind: "claude".into(),
            path: "/tmp".into(),
            label: "temporal".into(),
            temporary: true,
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 300".into()],
            environment: vec![],
            created_at: 1_700_000_000,
            columns: 80,
            rows: 24,
        },
    );
    assert!(matches!(response, DaemonResponse::Launched { .. }));

    // Give the child's first line time to reach the log the daemon appends to.
    let deadline = Instant::now() + Duration::from_secs(5);
    let history_path = state.root.join(format!("history/{session_id}.ansi"));
    while fs::metadata(&history_path).is_ok_and(|metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        sessions(&mut client, 12)
            .iter()
            .any(|session| session.id == session_id && !session.dead),
        "the session must be live before the daemon is killed"
    );
    drop(client);

    // No warning, no chance to write anything down.
    unsafe {
        libc::kill(daemon_pid, libc::SIGKILL);
    }
    let _ = killed.wait_with_output();
    let _ = fs::remove_file(state.root.join("muxloomd.sock"));

    let mut recovered_daemon = state.serve();
    let mut client = state.connect();
    let sessions = sessions(&mut client, 20);
    let session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("a killed daemon must not lose the sessions it owned");
    assert!(
        session.dead,
        "an unattachable session must be archived, not reported as running"
    );
    assert!(session.pid.is_none() && !session.working && !session.needs_attention);
    assert_eq!(session.label, "recovery smoke");
    assert_eq!(session.path, "/tmp");
    assert_eq!(
        session.recap.as_deref(),
        Some("finished the migration"),
        "the recap must be rebuilt from the log the daemon left behind"
    );
    assert!(
        !sessions.iter().any(|session| session.id == temporary_id),
        "a temporary session must not be archived"
    );

    let transcript = history(&state, session_id);
    assert!(
        transcript.contains("finished the migration"),
        "{transcript}"
    );
    assert!(
        transcript.contains("muxloomd stopped unexpectedly"),
        "the archived transcript must say why it ends: {transcript}"
    );

    // The recovered session reads back like any other archived one.
    match request(
        &mut client,
        21,
        &DaemonRequest::SearchHistory {
            session_id: session_id.into(),
            query: "migration".into(),
            max_matches: 10,
        },
    ) {
        DaemonResponse::HistoryMatches { matches } => assert_eq!(matches.len(), 1),
        response => panic!("unexpected search response: {response:?}"),
    }
    drop(client);
    unsafe {
        libc::kill(state.pid(), libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while recovered_daemon.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        recovered_daemon.try_wait().unwrap().is_some(),
        "muxloomd must stop when it is asked to"
    );
}

#[test]
fn a_daemon_asked_to_stop_records_its_sessions_before_exiting() {
    let state = TestState::new();
    let mut daemon = state.serve();
    let daemon_pid = state.pid();
    let session_id = "muxloomd-claude-1700000000-78-0";

    let mut client = state.connect();
    launch(&mut client, 30, session_id, "sleep 300");
    drop(client);

    unsafe {
        libc::kill(daemon_pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        daemon.try_wait().unwrap().is_some(),
        "SIGTERM must stop muxloomd"
    );

    let mut metadata = String::new();
    fs::File::open(state.root.join(format!("sessions/{session_id}.json")))
        .unwrap()
        .read_to_string(&mut metadata)
        .unwrap();
    let metadata: DaemonSession = serde_json::from_str(&metadata).unwrap();
    assert!(
        metadata.dead && metadata.pid.is_none(),
        "an orderly stop must record that its sessions ended: {metadata:?}"
    );

    let mut restarted = state.serve();
    let mut client = state.connect();
    let sessions = sessions(&mut client, 31);
    let session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("the session must still be listed after a restart");
    assert!(session.dead);
    let transcript = history(&state, session_id);
    assert!(
        !transcript.contains("muxloomd stopped unexpectedly"),
        "an orderly stop must not be reported as a crash: {transcript}"
    );

    drop(client);
    unsafe {
        libc::kill(state.pid(), libc::SIGTERM);
    }
    let _ = restarted.wait();
}
