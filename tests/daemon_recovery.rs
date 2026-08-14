#![cfg(unix)]

//! A daemon that dies must not take its sessions with it. Sessions are owned
//! by keeper processes, so a killed or stopped daemon leaves them running and
//! the next generation adopts them live. Only when the keeper itself is gone
//! is a session retired into the archive with its transcript intact.

use std::{
    fs,
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

    fn delete(&self, session_id: &str) {
        let mut client = self.connect();
        let response = request(
            &mut client,
            99,
            &DaemonRequest::Delete {
                session_id: session_id.into(),
            },
        );
        assert!(
            matches!(response, DaemonResponse::Ack | DaemonResponse::Error { .. }),
            "unexpected delete response: {response:?}"
        );
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

fn launch(stream: &mut UnixStream, request_id: u64, session_id: &str, script: &str) -> Option<u32> {
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
    match response {
        DaemonResponse::Launched { session } => session.pid,
        response => panic!("unexpected launch response: {response:?}"),
    }
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
fn a_killed_daemon_leaves_its_sessions_running_for_the_next_generation() {
    let state = TestState::new();
    let killed = state.serve();
    let daemon_pid = state.pid();
    let session_id = "muxloomd-claude-1700000000-77-0";

    let mut client = state.connect();
    let child_pid = launch(
        &mut client,
        10,
        session_id,
        "printf '\\342\\217\\272 finished the migration\\r\\n'; sleep 300",
    )
    .expect("launched session has a pid");

    // Give the child's first line time to reach the log the keeper appends to.
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

    // The keeper never noticed; the next generation adopts the session live.
    let mut recovered_daemon = state.serve();
    let mut client = state.connect();
    let listed = sessions(&mut client, 20);
    let session = listed
        .iter()
        .find(|session| session.id == session_id)
        .expect("a killed daemon must not lose the sessions it served");
    assert!(
        !session.dead,
        "the keeper owns the session, so a daemon crash must not end it"
    );
    assert_eq!(
        session.pid,
        Some(child_pid),
        "the adopted session is the same process, not a relaunch"
    );
    assert_eq!(session.recap.as_deref(), Some("finished the migration"));
    let transcript = history(&state, session_id);
    assert!(
        transcript.contains("finished the migration"),
        "{transcript}"
    );
    assert!(
        !transcript.contains("muxloomd stopped unexpectedly"),
        "a survived session must not read as a crash victim: {transcript}"
    );
    drop(client);

    state.delete(session_id);
    drop(recovered_daemon.try_wait());
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
fn a_session_whose_keeper_died_is_recovered_into_the_archive() {
    let state = TestState::new();
    let daemon = state.serve();
    let daemon_pid = state.pid();
    let session_id = "muxloomd-claude-1700000000-78-0";

    let mut client = state.connect();
    let child_pid = launch(
        &mut client,
        30,
        session_id,
        "printf '\\342\\217\\272 finished the migration\\r\\n'; sleep 300",
    )
    .expect("launched session has a pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    let history_path = state.root.join(format!("history/{session_id}.ansi"));
    while fs::metadata(&history_path).is_ok_and(|metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    drop(client);

    // The daemon dies without warning, and then so does the session's child:
    // with no keeper left, the next generation can only archive the record.
    unsafe {
        libc::kill(daemon_pid, libc::SIGKILL);
    }
    let _ = daemon.wait_with_output();
    let _ = fs::remove_file(state.root.join("muxloomd.sock"));
    unsafe {
        libc::kill(child_pid as i32, libc::SIGKILL);
    }
    // The keeper notices the child's death, records it, and leaves.
    let deadline = Instant::now() + Duration::from_secs(5);
    let keepers = state.root.join("keepers");
    let socket_gone = |keepers: &Path| {
        fs::read_dir(keepers).is_ok_and(|entries| {
            entries
                .flatten()
                .all(|entry| entry.path().extension().and_then(|ext| ext.to_str()) != Some("sock"))
        })
    };
    while !socket_gone(&keepers) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }

    let mut restarted = state.serve();
    let mut client = state.connect();
    let listed = sessions(&mut client, 31);
    let session = listed
        .iter()
        .find(|session| session.id == session_id)
        .expect("the record must survive its keeper");
    assert!(
        session.dead,
        "with the keeper gone there is nothing to adopt"
    );
    assert!(session.pid.is_none() && !session.working && !session.needs_attention);
    assert_eq!(session.recap.as_deref(), Some("finished the migration"));
    let transcript = history(&state, session_id);
    assert!(
        transcript.contains("finished the migration"),
        "{transcript}"
    );

    // The archived session reads back like any other.
    match request(
        &mut client,
        32,
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
    while restarted.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        restarted.try_wait().unwrap().is_some(),
        "muxloomd must stop when it is asked to"
    );
}
