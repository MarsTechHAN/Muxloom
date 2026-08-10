use std::{collections::BTreeMap, thread, time::Duration};

use muxloom::{
    config::{CommandConfig, Config, HostConfig},
    model::{AgentKind, LaunchRequest, Target},
    runtime::Runtime,
};

fn remote_runtime(alias: &str) -> Runtime {
    let mut hosts = BTreeMap::new();
    hosts.insert(
        alias.to_string(),
        HostConfig {
            companion_command: Some(
                std::env::var("MUXLOOM_REMOTE_COMPANION_COMMAND")
                    .unwrap_or_else(|_| "definitely-missing-muxloomd".into()),
            ),
            companion_binary: std::env::var("MUXLOOM_REMOTE_COMPANION_ASSET").ok(),
            ..HostConfig::default()
        },
    );
    Runtime::new(&Config {
        hosts,
        ..Config::default()
    })
}

#[test]
#[ignore = "requires MUXLOOM_REMOTE_TEST_ALIAS and a target-native muxloomd"]
fn target_native_companion_launches_and_recovers_history_over_one_bridge() {
    let alias = std::env::var("MUXLOOM_REMOTE_TEST_ALIAS")
        .expect("MUXLOOM_REMOTE_TEST_ALIAS must name an SSH config host");
    let runtime = remote_runtime(&alias);
    let target = Target::ssh(&alias);
    let marker = format!("muxloom-remote-smoke-{}", std::process::id());
    let request = LaunchRequest {
        target: target.clone(),
        kind: AgentKind::Codex,
        path: "/tmp".into(),
        label: "remote native companion smoke".into(),
        temporary: false,
        resume_id: None,
        initial_prompt: None,
    };
    let agent = CommandConfig {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "printf '\\033[2J\\033[H• Working (1s • esc to interrupt)\\n{marker}\\n'; sleep 10"
            ),
        ],
        ..CommandConfig::default()
    };

    let session_id = runtime.launch(&request, &agent, &[]).unwrap();
    let notice = runtime.take_bridge_notice(&alias);
    assert!(
        session_id.starts_with("muxloomd-"),
        "native companion was not used: session={session_id} notice={notice:?}"
    );
    assert_eq!(runtime.bridge_pool().connected_targets(), 1);
    if let Some(notice) = notice {
        assert!(
            notice.contains("deployed configured"),
            "unexpected provision notice: {notice}"
        );
    }
    let mut history = String::new();
    for _ in 0..20 {
        history = runtime.capture(&target, &session_id, 40).unwrap();
        if history.contains(&marker) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(history.contains(&marker), "remote history: {history:?}");
    if let Ok(path) = std::env::var("MUXLOOM_REMOTE_TEST_PATH") {
        let listing = runtime.list_files(&target, &path).unwrap();
        assert!(!listing.path.is_empty());
        assert!(
            !listing.entries.is_empty(),
            "remote file listing was empty for {path}"
        );
    }
    let mut sessions = Vec::new();
    let mut working_seen = false;
    for _ in 0..40 {
        (_, sessions) = runtime
            .probe_and_discover(&target, "sh", "sh", &[])
            .unwrap();
        working_seen = sessions
            .iter()
            .any(|session| session.id == session_id && session.working);
        if working_seen {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(sessions.iter().any(|session| session.id == session_id));
    assert_eq!(runtime.bridge_pool().connected_targets(), 1);
    runtime.kill(&target, &session_id).unwrap();
    assert!(
        working_seen,
        "remote daemon never reported a working session"
    );
}

/// Forwarding has to work against whatever daemon the machine happens to be
/// running, including one that live sessions pin to a generation older than
/// the capability: the bridge serves it there, so the client never sees it
/// missing. Reads a banner from a port the far end listens on — 22 by
/// default, since a machine reached over SSH is listening on it.
#[test]
#[ignore = "requires MUXLOOM_REMOTE_TEST_ALIAS and a target-native muxloomd"]
fn tcp_forwarding_reaches_a_remote_listener_over_one_bridge() {
    let alias = std::env::var("MUXLOOM_REMOTE_TEST_ALIAS")
        .expect("MUXLOOM_REMOTE_TEST_ALIAS must name an SSH config host");
    let port: u16 = std::env::var("MUXLOOM_REMOTE_TEST_TCP_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(22);
    let runtime = remote_runtime(&alias);
    let target = Target::ssh(&alias);

    runtime
        .bridge_pool()
        .ensure_tcp_forward(&target)
        .unwrap_or_else(|error| {
            panic!(
                "forwarding was unavailable: {error:#} notice={:?}",
                runtime.take_bridge_notice(&alias)
            )
        });
    // Privileged ports are left out of this report, so the port the banner is
    // read from need not appear in it.
    let ports = runtime.tcp_listener_ports(&target).unwrap();
    assert!(
        ports.iter().all(|port| *port >= 1024),
        "privileged ports were reported as forwardable: {ports:?}"
    );

    let mut stream = runtime
        .bridge_pool()
        .open_tcp(&target, "127.0.0.1".into(), port)
        .unwrap();
    let mut banner = Vec::new();
    for _ in 0..100 {
        while let Some(bytes) = stream.try_read_result().unwrap() {
            banner.extend_from_slice(&bytes);
        }
        if !banner.is_empty() || stream.is_closed() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !banner.is_empty(),
        "nothing came back from 127.0.0.1:{port}"
    );
    assert_eq!(runtime.bridge_pool().connected_targets(), 1);
}
