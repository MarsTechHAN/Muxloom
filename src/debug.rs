use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};

struct DebugLog {
    file: Mutex<File>,
    path: PathBuf,
}

static DEBUG_LOG: OnceLock<DebugLog> = OnceLock::new();

pub fn init(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create debug log directory {}", parent.display())
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open debug log {}", path.display()))?;
    let _ = DEBUG_LOG.set(DebugLog {
        file: Mutex::new(file),
        path: path.to_path_buf(),
    });
    log("debug", format!("logging enabled; {}", tty_state()));
    Ok(())
}

pub fn enabled() -> bool {
    DEBUG_LOG.get().is_some()
}

pub fn path() -> Option<PathBuf> {
    DEBUG_LOG.get().map(|log| log.path.clone())
}

pub fn log(scope: &str, message: impl AsRef<str>) {
    let Some(log) = DEBUG_LOG.get() else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let message: String = message
        .as_ref()
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' {
                ' '
            } else if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect();
    if let Ok(mut file) = log.file.lock() {
        let _ = writeln!(
            file,
            "{timestamp} [{:?}] [{scope}] {message}",
            std::thread::current().id()
        );
        let _ = file.flush();
    }
}

#[cfg(unix)]
pub fn tty_state() -> String {
    // These calls only inspect process and terminal job-control state.
    let process_group = unsafe { libc::getpgrp() };
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let foreground_group = if is_tty {
        unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) }
    } else {
        -1
    };
    format!(
        "pid={} pgrp={process_group} foreground_pgrp={foreground_group} stdin_is_tty={is_tty}",
        std::process::id()
    )
}

#[cfg(not(unix))]
pub fn tty_state() -> String {
    format!("pid={} job_control=unsupported", std::process::id())
}
