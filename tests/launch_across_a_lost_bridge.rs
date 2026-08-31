#![cfg(unix)]

//! A launch whose bridge dies under it must land on the daemon anyway.
//!
//! Losing the bridge mid-request is ordinary: a generation handover on the far
//! side takes every attached client's connection with it, which is precisely
//! what happens while a machine is being upgraded. The keepers carry the
//! sessions across and the next daemon adopts them, so the only thing a client
//! has to do is make a new connection. Treating the gap as "this machine has
//! no companion" instead puts the session on the legacy tmux path for the rest
//! of its life over an interruption measured in milliseconds.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use muxloom::{
    bridge::{BridgeOptions, BridgePool},
    model::Target,
};

struct Scratch {
    root: PathBuf,
}

impl Drop for Scratch {
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

/// Every `muxloomd bridge` process started from this test's own copy of the
/// companion. The copy is what makes that answerable: other test binaries run
/// beside this one and start bridges of their own, and killing one of those
/// would be a failure planted in somebody else's test.
fn bridge_pids(companion: &Path) -> Vec<i32> {
    let Ok(output) = Command::new("ps").args(["-eo", "pid=,args="]).output() else {
        return Vec::new();
    };
    let companion = companion.to_string_lossy().into_owned();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&companion) && line.contains("bridge"))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

#[test]
fn a_launch_that_loses_its_bridge_still_reaches_the_daemon() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let scratch = Scratch {
        root: PathBuf::from("/tmp").join(format!("mxb-{}-{nonce}", std::process::id())),
    };
    fs::create_dir_all(&scratch.root).unwrap();

    // The bridge and the daemon it starts are children of this process, so the
    // scratch state directory reaches them through the environment. Never the
    // machine's own daemon, and never allowed to write itself into the agent
    // configuration of whoever is running the tests.
    unsafe {
        std::env::set_var("MUXLOOMD_STATE_DIR", &scratch.root);
        std::env::set_var("MUXLOOM_MCP_REGISTER", "0");
    }

    // A copy of the companion that belongs to this test alone, so the bridges
    // it starts can be told apart from every other test binary's.
    let companion = scratch.root.join("muxloomd");
    fs::copy(env!("CARGO_BIN_EXE_muxloomd"), &companion).unwrap();
    let pool = BridgePool::new(
        BridgeOptions {
            command: companion.to_string_lossy().into_owned(),
            ..BridgeOptions::default()
        },
        HashMap::new(),
    );
    let target = Target::local();

    // Warm the bridge up, so what the launch loses is a connection that was
    // there rather than one that never opened.
    pool.list_sessions(&target).unwrap();

    // Kill the bridge and launch straight afterwards, with no pause for the
    // reader thread to notice. Whichever way the client finds out — the
    // request failing under it, or the connection already gone by the time it
    // is asked for — the session has to end up on the daemon. Several rounds,
    // because which of the two it is comes down to scheduling.
    for round in 0..5 {
        for pid in bridge_pids(&companion) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let session_id = format!("muxloomd-terminal-lost-bridge-{round}");
        let session = pool
            .launch(
                &target,
                session_id.clone(),
                "terminal".into(),
                "/tmp".into(),
                format!("lost bridge {round}"),
                false,
                "/bin/cat".into(),
                Vec::new(),
                Vec::new(),
                1_700_000_000_000 + round,
                None,
                None,
                None,
                None,
            )
            .unwrap_or_else(|error| panic!("round {round} fell off the daemon: {error:#}"));
        assert_eq!(session.id, session_id);

        // And asking again for a session the daemon already has must return
        // that one rather than a second copy of it: a retry cannot tell
        // whether the reply it lost was a failure or a success.
        let sessions = pool.list_sessions(&target).unwrap();
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.id == session_id)
                .count(),
            1,
            "round {round} left more than one session behind"
        );

        // A terminal session is ephemeral, and this one is a test's: take it
        // and its keeper with us rather than leaving them on the machine.
        pool.delete(&target, session_id).unwrap();
    }
}
