#![cfg(unix)]

//! A running session must survive a daemon generation handover: its keeper
//! owns the PTY, the old daemon drains without touching it, and the next
//! generation adopts it with the same child process and a transcript that
//! spans both generations.

use std::{
    collections::HashMap,
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
        let root = PathBuf::from("/tmp").join(format!("mxk-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_muxloomd"));
        command.env("MUXLOOMD_STATE_DIR", &self.root);
        // A daemon in a scratch directory keeps out of the machine's agent
        // configuration on its own, but this is a test writing to the home of
        // whoever is running it. Say it twice.
        command.env("MUXLOOM_MCP_REGISTER", "0");
        command
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
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_for(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating its socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

fn status(state: &TestState) -> String {
    let output = state.command().arg("status").output().unwrap();
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn status_pid(status: &str) -> u32 {
    status
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap()
}

/// One request over an open client connection, collecting stream data.
fn request(
    stream: &mut UnixStream,
    request_id: u64,
    request: &DaemonRequest,
) -> (DaemonResponse, HashMap<u32, Vec<u8>>) {
    Frame::json(FrameKind::Request, 0, request_id, request)
        .unwrap()
        .write_to(stream)
        .unwrap();
    let mut data: HashMap<u32, Vec<u8>> = HashMap::new();
    loop {
        let frame = Frame::read_from(stream).unwrap().unwrap();
        match frame.kind {
            FrameKind::Data if frame.request_id == request_id => {
                data.entry(frame.stream_id)
                    .or_default()
                    .extend(frame.decoded_payload().unwrap());
            }
            FrameKind::Response if frame.request_id == request_id => {
                return (frame.decode_json().unwrap(), data);
            }
            _ => {}
        }
    }
}

fn sessions(state: &TestState) -> Vec<DaemonSession> {
    let mut client = state.connect();
    match request(&mut client, 1, &DaemonRequest::ListSessions).0 {
        DaemonResponse::Sessions { sessions } => sessions,
        response => panic!("unexpected session list: {response:?}"),
    }
}

fn history_text(state: &TestState, session_id: &str) -> String {
    let mut client = state.connect();
    let (response, data) = request(
        &mut client,
        2,
        &DaemonRequest::ReadHistory {
            session_id: session_id.into(),
            offset_from_bottom: 0,
            lines: 200,
            rendered: false,
        },
    );
    assert!(
        matches!(response, DaemonResponse::HistoryComplete { .. }),
        "unexpected history response: {response:?}"
    );
    String::from_utf8_lossy(
        data.get(&muxloom::daemon_protocol::stream::HISTORY)
            .map_or(&[][..], Vec::as_slice),
    )
    .into_owned()
}

fn wait_for_history(state: &TestState, session_id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if history_text(state, session_id).contains(needle) {
            return;
        }
        assert!(Instant::now() < deadline, "history never showed {needle:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_live_session_survives_a_generation_handover_with_its_process() {
    let state = TestState::new();
    let mut serve = state
        .command()
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for(&state.root.join("muxloomd.sock"), &mut serve);
    let old_daemon_pid = status_pid(&status(&state));

    let session_id = "muxloomd-terminal-1700000000-1-0";
    let child_pid = {
        let mut client = state.connect();
        let (response, _) = request(
            &mut client,
            10,
            &DaemonRequest::Launch {
                session_id: session_id.into(),
                kind: "terminal".into(),
                path: "/tmp".into(),
                label: "survivor".into(),
                temporary: false,
                executable: "/bin/cat".into(),
                args: vec![],
                environment: vec![],
                created_at: 1,
                columns: 80,
                rows: 24,
            },
        );
        let DaemonResponse::Launched { session } = response else {
            panic!("launch failed: {response:?}");
        };
        let (ack, _) = request(
            &mut client,
            11,
            &DaemonRequest::SendInput {
                session_id: session_id.into(),
                bytes: b"first-generation-probe\r".to_vec(),
            },
        );
        assert_eq!(ack, DaemonResponse::Ack);
        session.pid.expect("launched session has a child pid")
    };
    wait_for_history(&state, session_id, "first-generation-probe");

    // A stale generation makes the next connecting client demand a handover.
    // The live session must not defer it: its keeper carries it across.
    fs::write(state.root.join("muxloomd.generation"), "stale\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let new_daemon_pid = loop {
        let pid = status_pid(&status(&state));
        if pid != old_daemon_pid {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "handover never happened despite the live session"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(new_daemon_pid, old_daemon_pid);

    // The next generation serves the same session: same child process, the
    // old transcript intact, and input still reaching the PTY.
    let adopted = sessions(&state)
        .into_iter()
        .find(|session| session.id == session_id)
        .expect("the session must survive the handover");
    assert!(!adopted.dead, "the adopted session must still be live");
    assert_eq!(
        adopted.pid,
        Some(child_pid),
        "the adopted session is the same process, not a relaunch"
    );
    {
        let mut client = state.connect();
        let (ack, _) = request(
            &mut client,
            12,
            &DaemonRequest::SendInput {
                session_id: session_id.into(),
                bytes: b"second-generation-probe\r".to_vec(),
            },
        );
        assert_eq!(ack, DaemonResponse::Ack);
    }
    wait_for_history(&state, session_id, "second-generation-probe");
    assert!(
        history_text(&state, session_id).contains("first-generation-probe"),
        "the transcript must span both generations"
    );

    // Deleting the session dismisses its keeper and its child.
    {
        let mut client = state.connect();
        let (ack, _) = request(
            &mut client,
            13,
            &DaemonRequest::Delete {
                session_id: session_id.into(),
            },
        );
        assert_eq!(ack, DaemonResponse::Ack);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(child_pid as i32, 0) } == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_ne!(
        unsafe { libc::kill(child_pid as i32, 0) },
        0,
        "deleting the session must end its child process"
    );

    let _ = serve.wait_with_output();
}
