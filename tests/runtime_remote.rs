use std::{collections::BTreeMap, thread, time::Duration};

use muxloom::{
    config::{CommandConfig, Config, HostConfig},
    model::{AgentKind, LaunchRequest, Target},
    runtime::Runtime,
};

#[test]
#[ignore = "requires MUXLOOM_REMOTE_TEST_ALIAS and a target-native muxloomd"]
fn target_native_companion_launches_and_recovers_history_over_one_bridge() {
    let alias = std::env::var("MUXLOOM_REMOTE_TEST_ALIAS")
        .expect("MUXLOOM_REMOTE_TEST_ALIAS must name an SSH config host");
    let command = std::env::var("MUXLOOM_REMOTE_COMPANION_COMMAND")
        .unwrap_or_else(|_| "definitely-missing-muxloomd".into());
    let companion_binary = std::env::var("MUXLOOM_REMOTE_COMPANION_ASSET").ok();
    let expects_deployment = companion_binary.is_some();
    let mut hosts = BTreeMap::new();
    hosts.insert(
        alias.clone(),
        HostConfig {
            companion_command: Some(command),
            companion_binary,
            ..HostConfig::default()
        },
    );
    let config = Config {
        hosts,
        ..Config::default()
    };
    let runtime = Runtime::new(&config);
    let target = Target::ssh(&alias);
    let marker = format!("muxloom-remote-smoke-{}", std::process::id());
    let request = LaunchRequest {
        target: target.clone(),
        kind: AgentKind::Codex,
        path: "/tmp".into(),
        label: "remote native companion smoke".into(),
        resume_id: None,
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
    assert!(session_id.starts_with("muxloomd-"));
    assert_eq!(runtime.bridge_pool().connected_targets(), 1);
    let notice = runtime.take_bridge_notice(&alias);
    if expects_deployment {
        assert!(
            notice
                .as_deref()
                .is_some_and(|notice| notice.contains("deployed configured")),
            "provision notice: {notice:?}"
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
