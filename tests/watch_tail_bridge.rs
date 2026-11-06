#![cfg(unix)]

//! A watched file larger than the 8 MiB preview budget has to come back as a
//! bounded window at its end when the companion bridge answers. The app-level
//! regression covers the shell fallback a target without a companion takes;
//! this covers the normal data plane, where the tail is read at an offset
//! instead of pulling the file's first eight megabytes on every poll.

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use muxloom::{config::Config, model::Target, runtime::Runtime};

struct Scratch {
    root: PathBuf,
    serve: Child,
}

impl Scratch {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("mxw-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let serve = Command::new(env!("CARGO_BIN_EXE_muxloomd"))
            .arg("serve")
            .env("MUXLOOMD_STATE_DIR", &root)
            // Never the machine's own daemon, and never allowed to write
            // itself into the agent configuration of whoever runs the tests.
            .env("MUXLOOM_MCP_REGISTER", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let socket = root.join("muxloomd.sock");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            socket.exists(),
            "timed out waiting for {}",
            socket.display()
        );
        Self { root, serve }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
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
        let _ = self.serve.kill();
        let _ = self.serve.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_large_watched_file_returns_its_tail_over_the_bridge() {
    let scratch = Scratch::new();
    // The bridge and the daemon it talks to are children of this process, so
    // the scratch state directory has to reach them through the environment.
    // This test binary holds a single test, and the directory is removed by
    // `Scratch` when it finishes.
    unsafe {
        std::env::set_var("MUXLOOMD_STATE_DIR", &scratch.root);
        std::env::set_var("MUXLOOM_MCP_REGISTER", "0");
    }
    // A copy of the companion that belongs to this test alone, so the bridge
    // it starts can never be another test binary's.
    let companion = scratch.path("muxloomd");
    fs::copy(env!("CARGO_BIN_EXE_muxloomd"), &companion).unwrap();

    let file = scratch.path("huge.log");
    // Past the 8 MiB preview budget, with a marker only the tail can hold.
    const PADDING: usize = 9 * 1024 * 1024;
    let line = "bridge watch padding line\n";
    let marker = "BRIDGE-TAIL-MARKER-4c1d\n";
    let mut content = String::with_capacity(PADDING + marker.len());
    content.push_str("start-of-file\n");
    while content.len() < PADDING {
        content.push_str(line);
    }
    content.push_str(marker);
    fs::write(&file, &content).unwrap();

    let config = Config {
        companion_command: companion.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let runtime = Runtime::new(&config);
    let target = Target::local();
    let path = file.to_string_lossy().into_owned();
    let window = 64 * 1024;

    let tail = runtime.preview_file_tail(&target, &path, window).unwrap();
    assert_eq!(tail.size, content.len() as u64);
    assert!(
        tail.starts_mid_file,
        "the window begins after the start of a file this size"
    );
    assert!(!tail.truncated, "a tail shows the end; nothing follows it");
    assert!(
        tail.content.ends_with(marker),
        "a watch must see the file's newest bytes"
    );
    assert!(
        tail.content.len() <= window as usize,
        "the transfer is bounded by the window, not the file: {} bytes",
        tail.content.len()
    );
    assert!(
        !tail.content.starts_with("start-of-file\n"),
        "the head must not be pulled along with the tail"
    );

    // The browser's preview of the same file is still the head, so Ctrl-r on
    // the browser cannot replace a watch's end with the file's beginning.
    let head = runtime.preview_file(&target, &path).unwrap();
    assert!(head.content.starts_with("start-of-file\n"));
    assert!(!head.starts_mid_file);
    assert!(head.truncated);

    drop(runtime);
}
