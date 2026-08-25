#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct TestState {
    root: PathBuf,
}

impl TestState {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("mxh-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_muxloomd"));
        command.env("MUXLOOMD_STATE_DIR", &self.root);
        // Never the machine's daemon, and never allowed to write itself into
        // the agent configuration of whoever is running the tests.
        command.env("MUXLOOM_MCP_REGISTER", "0");
        command
    }

    fn pid(&self) -> u32 {
        fs::read_to_string(self.root.join("muxloomd.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
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

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn active_client_defers_generation_handover_then_idle_daemon_upgrades() {
    let state = TestState::new();
    let mut serve = state
        .command()
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for(&state.root.join("muxloomd.sock"), &mut serve);
    let old_pid = state.pid();

    let bridge = state
        .command()
        .arg("bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    fs::write(state.root.join("muxloomd.generation"), "stale\n").unwrap();

    assert_eq!(status_pid(&status(&state)), old_pid);
    stop(bridge);
    thread::sleep(Duration::from_millis(100));

    let deadline = Instant::now() + Duration::from_secs(4);
    let new_pid = loop {
        let pid = status_pid(&status(&state));
        if pid != old_pid {
            break pid;
        }
        assert!(Instant::now() < deadline, "idle handover never completed");
        thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(new_pid, old_pid);

    let _ = serve.wait_with_output();
}

/// A superseded daemon that never stands down must not get to serve forever.
/// Builds from before the retirement watcher refuse a handover outright while
/// more than one client is attached, and on a machine with agents on it there
/// always is one — every bridge holds a connection open. The fix for that runs
/// in the daemon being replaced, which is exactly the daemon too old to have
/// it, so the build arriving keeps its own patience: once the ask has been
/// outstanding long enough it stops the running daemon and serves in its place.
#[test]
fn a_daemon_that_keeps_deferring_is_replaced_once_the_ask_is_overdue() {
    let state = TestState::new();
    let mut serve = state
        .command()
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for(&state.root.join("muxloomd.sock"), &mut serve);
    let old_pid = state.pid();

    // The client that never lets go, and the reason the daemon defers.
    let bridge = state
        .command()
        .arg("bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(150));
    fs::write(state.root.join("muxloomd.generation"), "stale\n").unwrap();

    // Someone has been asking this generation to make way since long before
    // any daemon able to retire itself would still be here.
    let long_ago = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 11 * 60 * 1000;
    fs::write(
        state.root.join("muxloomd.handover"),
        format!("stale\t{long_ago}"),
    )
    .unwrap();

    // One call, with the bridge still attached: whoever answers is the new
    // build. Waiting for the daemon to stand down on its own would take
    // seconds, so this cannot be that path by accident.
    let new_pid = status_pid(&status(&state));
    assert_ne!(
        new_pid, old_pid,
        "an overdue ask must be answered by taking the daemon's place"
    );
    assert!(
        !state.root.join("muxloomd.handover").exists(),
        "the ask is over once it has been answered"
    );

    stop(bridge);
    let _ = serve.wait_with_output();
}
