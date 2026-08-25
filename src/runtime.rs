use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

use crate::{
    bridge::{BridgeOptions, BridgePool},
    config::{CommandConfig, Config},
    daemon_protocol::DaemonSession,
    debug,
    model::{
        AgentKind, AgentSession, Composer, DirectoryListing, FileEntry, FileEntryKind, FileListing,
        FilePreview, FilePreviewKind, HistoryMatch, HistoryPage, LOCAL_TARGET_ID, LaunchRequest,
        Probe, ResumeCandidate, Target, TaskProgress, Transport,
    },
    recap::extract_recap,
};

const SESSION_PREFIX: &str = "muxloom-";
pub(crate) const DAEMON_SESSION_PREFIX: &str = "muxloomd-";
const LEGACY_SESSION_PREFIX: &str = "ad-";
const TEMPORARY_SESSION_MARKER: &str = "temporal-";
pub const SSH_CONTROL_PERSIST_OPTION: &str = "ControlPersist=600";
pub const SSH_SERVER_ALIVE_INTERVAL_OPTION: &str = "ServerAliveInterval=15";
pub const SSH_SERVER_ALIVE_COUNT_OPTION: &str = "ServerAliveCountMax=3";
pub const SSH_CONNECTION_ATTEMPTS_OPTION: &str = "ConnectionAttempts=3";
const FORMAT: &str = "#{session_name}\t#{@muxloom_kind}\t#{@muxloom_path}\t#{@muxloom_label}\t#{@muxloom_created}\t#{@agentdeck_kind}\t#{@agentdeck_path}\t#{@agentdeck_label}\t#{@agentdeck_created}\t#{pane_dead}\t#{pane_pid}";
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
static DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);
static TUNNEL_START_LOCK: Mutex<()> = Mutex::new(());

const CLAUDE_RELEASES: &str = "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases";
const CODEX_RELEASES: &str = "https://github.com/openai/codex/releases/download";
const CODEX_LATEST: &str = "https://github.com/openai/codex/releases/latest";
const PI_RELEASES: &str = "https://github.com/earendil-works/pi/releases/download";
const PI_LATEST: &str = "https://github.com/earendil-works/pi/releases/latest";
/// OpenCode's own release carries no checksum manifest for the command-line
/// build, but the npm registry publishes one for the very bytes `npm install`
/// would fetch: every `opencode-<platform>` package names its own integrity
/// digest. So the registry is where the release is resolved from — the payload
/// is the same self-contained binary, and it arrives with something to check
/// it against.
const NPM_REGISTRY: &str = "https://registry.npmjs.org";
const CODEX_NO_ALT_SCREEN_ARG: &str = "--no-alt-screen";
const CODEX_NO_HISTORY_CONFIG: &str = "history.persistence=\"none\"";

/// The most of a text file a preview will pull across. A preview is something
/// to glance through, and reading a log of any size whole — over SSH, into
/// memory, behind a spinner that cannot say how long it will take — is not that.
/// Past this the reader is told the preview stops short.
const PREVIEW_BYTE_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
struct TargetPlatform {
    os: String,
    arch: String,
    musl: bool,
}

impl TargetPlatform {
    fn local() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "windows_nt",
            other => other,
        };
        Self {
            os: os.into(),
            arch: normalize_arch(std::env::consts::ARCH).into(),
            musl: cfg!(target_env = "musl"),
        }
    }

    fn matches_local(&self) -> bool {
        let local_os = match std::env::consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            "windows" => "windows_nt",
            other => other,
        };
        self.os == local_os
            && self.arch == normalize_arch(std::env::consts::ARCH)
            && (self.os != "linux" || self.musl == cfg!(target_env = "musl"))
    }

    fn claude_name(&self) -> Result<String> {
        let os = match self.os.as_str() {
            "linux" => "linux",
            "darwin" => "darwin",
            other => bail!("Claude controller download does not support target OS {other}"),
        };
        let arch = match self.arch.as_str() {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => bail!("Claude controller download does not support architecture {other}"),
        };
        Ok(format!(
            "{os}-{arch}{}",
            if self.os == "linux" && self.musl {
                "-musl"
            } else {
                ""
            }
        ))
    }

    fn codex_name(&self) -> Result<String> {
        match (self.os.as_str(), self.arch.as_str()) {
            ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl".into()),
            ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl".into()),
            ("darwin", "x86_64") => Ok("x86_64-apple-darwin".into()),
            ("darwin", "aarch64") => Ok("aarch64-apple-darwin".into()),
            (os, arch) => bail!("Codex controller download does not support {os}/{arch}"),
        }
    }

    /// Pi publishes one bundle per platform and no musl build at all, so a
    /// machine running musl is told plainly rather than handed a binary that
    /// cannot start — the install then falls through to Pi's own installer.
    fn pi_name(&self) -> Result<String> {
        if self.os == "linux" && self.musl {
            bail!("Pi publishes no musl build");
        }
        let os = match self.os.as_str() {
            "linux" => "linux",
            "darwin" => "darwin",
            other => bail!("Pi controller download does not support target OS {other}"),
        };
        let arch = match self.arch.as_str() {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => bail!("Pi controller download does not support architecture {other}"),
        };
        Ok(format!("{os}-{arch}"))
    }

    /// The npm package holding OpenCode's binary for this machine. There are
    /// also `-baseline` variants, built for processors too old for the vector
    /// instructions the ordinary build uses; nothing a machine tells us says
    /// whether it is one of those, so the ordinary build is what it gets.
    fn opencode_name(&self) -> Result<String> {
        let os = match self.os.as_str() {
            "linux" => "linux",
            "darwin" => "darwin",
            other => bail!("OpenCode controller download does not support target OS {other}"),
        };
        let arch = match self.arch.as_str() {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => bail!("OpenCode controller download does not support architecture {other}"),
        };
        Ok(format!(
            "{os}-{arch}{}",
            if self.os == "linux" && self.musl {
                "-musl"
            } else {
                ""
            }
        ))
    }
}

/// Which hash a publisher offers for its own payload. Nobody picks this:
/// whichever manifest the release could be resolved from names the algorithm,
/// and the controller and the target must then check the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestAlgorithm {
    Sha256,
    Sha512,
}

impl DigestAlgorithm {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Hex characters a digest of this width is written with.
    fn hex_len(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }
}

/// The release a target should end up running. The controller resolves this
/// much — a version, a URL, a digest — and either side can then act on it: the
/// controller downloading the payload for an upload, or the target pulling it
/// down itself and checking it against the same digest.
#[derive(Debug, Clone)]
struct RemoteRelease {
    version: String,
    platform_name: String,
    /// The file name the payload is cached under, which is also the last
    /// segment of `url`.
    asset: String,
    url: String,
    digest: String,
    algorithm: DigestAlgorithm,
}

#[derive(Debug, Clone)]
pub struct Runtime {
    ssh_connect_timeout_secs: u64,
    history_limit: usize,
    reverse_tunnel: String,
    host_reverse_tunnels: HashMap<String, String>,
    tunnel_checks: Arc<Mutex<HashMap<String, Instant>>>,
    bridges: BridgePool,
    bridge_failures: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Runtime {
    pub fn new(config: &Config) -> Self {
        let default_download_environment =
            Self::controller_environment_for_config(config, LOCAL_TARGET_ID);
        let default_bridge = BridgeOptions {
            connect_timeout_secs: config.ssh_connect_timeout_secs,
            command: config.companion_command.clone(),
            reverse_tunnel: config.reverse_tunnel.clone(),
            bootstrap_binary: config.companion_binary.clone(),
            download_environment: default_download_environment,
            remote_environment: config.environment_for(LOCAL_TARGET_ID).unwrap_or_default(),
            attention_patterns: config.attention_patterns_for(LOCAL_TARGET_ID).to_vec(),
        };
        let bridge_options = config
            .hosts
            .iter()
            .map(|(host, host_config)| {
                (
                    host.clone(),
                    BridgeOptions {
                        connect_timeout_secs: config.ssh_connect_timeout_secs,
                        command: host_config
                            .companion_command
                            .clone()
                            .unwrap_or_else(|| config.companion_command.clone()),
                        reverse_tunnel: host_config
                            .reverse_tunnel
                            .clone()
                            .unwrap_or_else(|| config.reverse_tunnel.clone()),
                        bootstrap_binary: host_config
                            .companion_binary
                            .clone()
                            .unwrap_or_else(|| config.companion_binary.clone()),
                        download_environment: Self::controller_environment_for_config(config, host),
                        remote_environment: config.environment_for(host).unwrap_or_default(),
                        attention_patterns: config.attention_patterns_for(host).to_vec(),
                    },
                )
            })
            .collect();
        Self {
            ssh_connect_timeout_secs: config.ssh_connect_timeout_secs,
            history_limit: config.history_limit,
            reverse_tunnel: config.reverse_tunnel.clone(),
            host_reverse_tunnels: config
                .hosts
                .iter()
                .filter_map(|(host, config)| {
                    config
                        .reverse_tunnel
                        .as_ref()
                        .map(|tunnel| (host.clone(), tunnel.clone()))
                })
                .collect(),
            tunnel_checks: Arc::new(Mutex::new(HashMap::new())),
            bridges: BridgePool::new(default_bridge, bridge_options),
            bridge_failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn bridge_pool(&self) -> BridgePool {
        self.bridges.clone()
    }

    pub fn take_bridge_notice(&self, target_id: &str) -> Option<String> {
        self.bridges.take_notice(target_id)
    }

    pub fn probe_and_discover(
        &self,
        target: &Target,
        commands: &[(AgentKind, String)],
        environment: &[(String, String)],
    ) -> Result<(Probe, Vec<AgentSession>)> {
        self.probe_and_discover_with_progress(target, commands, environment, |_| {})
    }

    pub fn probe_and_discover_with_progress(
        &self,
        target: &Target,
        commands: &[(AgentKind, String)],
        environment: &[(String, String)],
        progress: impl FnMut(TaskProgress),
    ) -> Result<(Probe, Vec<AgentSession>)> {
        debug::log("runtime", format!("probe start target={}", target.id));
        if let Ok(available) = self.bridges.probe_executables_with_progress(
            target,
            commands
                .iter()
                .map(|(_, command)| command.clone())
                .collect(),
            progress,
        ) {
            let mut sessions = self
                .bridges
                .list_sessions(target)?
                .into_iter()
                .filter_map(|session| daemon_agent_session(&target.id, session))
                .collect::<Vec<_>>();
            let disposable = sessions
                .iter()
                .filter(|session| {
                    session.dead
                        && (session.kind == AgentKind::Terminal
                            || is_temporary_session_id(&session.id))
                })
                .map(|session| session.id.clone())
                .collect::<Vec<_>>();
            sessions.retain(|session| {
                !(session.dead
                    && (session.kind == AgentKind::Terminal
                        || is_temporary_session_id(&session.id)))
            });
            for session_id in disposable {
                let _ = self.bridges.delete(target, session_id);
            }
            let mut probe = Probe::default();
            for (kind, command) in commands {
                probe.set(*kind, available.iter().any(|item| item == command));
            }
            debug::log(
                "runtime",
                format!(
                    "probe done target={} backend=muxloomd runtimes={} sessions={}",
                    target.id,
                    describe_runtimes(&probe),
                    sessions.len()
                ),
            );
            for session in &sessions {
                debug::log(
                    "activity",
                    format!(
                        "source=muxloomd target={} session={} kind={} working={} attention={}",
                        target.id,
                        session.id,
                        session.kind,
                        session.working,
                        session.needs_attention
                    ),
                );
            }
            return Ok((probe, sessions));
        }
        self.bridge_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target.id.clone(), Instant::now());
        let exports = environment_exports(environment);
        let mut probe = String::new();
        for (kind, command) in commands {
            let lookup = login_shell_command(&format!(
                "{exports} command -v {} >/dev/null 2>&1",
                shell_quote(command)
            ));
            let name = kind.as_str();
            probe.push_str(&format!(
                "if {lookup} >/dev/null 2>&1; then printf '{name}=1\\n'; else printf '{name}=0\\n'; fi; "
            ));
        }
        probe.push_str(
            "if command -v tmux >/dev/null 2>&1; then printf 'tmux=1\\n'; else printf 'tmux=0\\n'; fi",
        );
        let managed_panes = shell_join(&[
            "tmux",
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}",
            "-f",
            "#{m/r:^(muxloom-|ad-),#{session_name}}",
        ]);
        let enable_archive = format!(
            "{managed_panes} 2>/dev/null | while IFS= read -r pane; do \
             tmux set-option -w -t \"$pane\" remain-on-exit on 2>/dev/null || true; done"
        );
        let discover = shell_join(&[
            "tmux",
            "list-panes",
            "-a",
            "-F",
            FORMAT,
            "-f",
            "#{m/r:^(muxloom-|ad-),#{session_name}}",
        ]) + " 2>/dev/null || true";
        let script = format!("{probe}; {enable_archive}; {discover}");
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "target probe")?;
        let (probe, mut sessions) =
            parse_discovery(&target.id, &String::from_utf8_lossy(&output.stdout))?;
        let mut disposable: Vec<_> = sessions
            .iter()
            .filter(|session| {
                session.dead
                    && (session.kind == AgentKind::Terminal || is_temporary_session_id(&session.id))
            })
            .map(|session| session.id.clone())
            .collect();
        disposable.sort();
        disposable.dedup();
        sessions.retain(|session| {
            !(session.dead
                && (session.kind == AgentKind::Terminal || is_temporary_session_id(&session.id)))
        });
        for session_id in &disposable {
            if let Err(error) = self.kill(target, session_id) {
                debug::log(
                    "runtime",
                    format!(
                        "dead terminal cleanup failed target={} session={session_id}: {error}",
                        target.id
                    ),
                );
            }
        }
        debug::log(
            "runtime",
            format!(
                "probe done target={} tmux={} runtimes={} sessions={} disposable_sessions_cleaned={}",
                target.id,
                probe.tmux,
                describe_runtimes(&probe),
                sessions.len(),
                disposable.len()
            ),
        );
        Ok((probe, sessions))
    }

    /// Refresh muxloomd-owned session metadata without running executable
    /// probes or touching the legacy tmux compatibility path.
    pub fn daemon_sessions(&self, target: &Target) -> Result<Vec<AgentSession>> {
        let sessions = self
            .bridges
            .list_sessions(target)?
            .into_iter()
            .filter_map(|session| daemon_agent_session(&target.id, session))
            .collect();
        Ok(sessions)
    }

    pub fn launch(
        &self,
        request: &LaunchRequest,
        command: &CommandConfig,
        environment: &[(String, String)],
    ) -> Result<String> {
        if request.path.trim().is_empty() {
            bail!("working directory cannot be empty");
        }
        if command.command.trim().is_empty() && request.kind != AgentKind::Terminal {
            bail!("command for {} is empty", request.kind);
        }

        debug::log(
            "runtime",
            format!(
                "launch start target={} kind={} path={} executable={}",
                request.target.id, request.kind, request.path, command.command
            ),
        );
        let (session_id, now) = new_daemon_session_id(request.kind, request.temporary);
        let label = request.label.replace(['\t', '\n', '\r'], " ");
        let args = launch_arguments(
            command,
            request.kind,
            request.temporary,
            request.resume_id.as_deref(),
            request.initial_prompt.as_deref(),
        );
        let daemon_launch = self.bridges.launch(
            &request.target,
            session_id.clone(),
            request.kind.as_str().into(),
            request.path.clone(),
            label,
            request.temporary,
            command.command.clone(),
            args,
            environment.to_vec(),
            now,
            request.parent.clone(),
        );
        if let Err(daemon_error) = daemon_launch {
            debug::log(
                "runtime",
                format!(
                    "launch target={} muxloomd unavailable: {daemon_error:#}; trying explicit legacy tmux fallback",
                    request.target.id
                ),
            );
            self.bridge_failures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(request.target.id.clone(), Instant::now());
            let legacy_session = self
                .launch_legacy_tmux(request, command, environment, now)
                .with_context(|| {
                    format!(
                        "muxloomd launch failed ({daemon_error:#}); legacy tmux fallback also failed"
                    )
                })?;
            self.bridges.record_notice(
                &request.target.id,
                format!(
                    "WARNING: muxloomd was unavailable, so this session uses legacy tmux; {daemon_error:#}"
                ),
            );
            return Ok(legacy_session);
        }
        debug::log(
            "runtime",
            format!(
                "launch done target={} session={session_id}",
                request.target.id
            ),
        );
        Ok(session_id)
    }

    fn launch_legacy_tmux(
        &self,
        request: &LaunchRequest,
        command: &CommandConfig,
        environment: &[(String, String)],
        now: u64,
    ) -> Result<String> {
        let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let session_id = format!(
            "{SESSION_PREFIX}{}{}-{now}-{}-{sequence}",
            if request.temporary {
                TEMPORARY_SESSION_MARKER
            } else {
                ""
            },
            request.kind.as_str(),
            std::process::id()
        );
        let exports = environment_exports(environment);
        let agent_command =
            if request.kind == AgentKind::Terminal && command.command.trim().is_empty() {
                interactive_shell_command(&format!("{exports} exec \"${{SHELL:-/bin/sh}}\" -l"))
            } else {
                interactive_shell_command(&format!(
                    "{exports} exec {}",
                    command_line(
                        command,
                        request.kind,
                        request.temporary,
                        request.resume_id.as_deref(),
                        request.initial_prompt.as_deref(),
                    )
                ))
            };
        let working_path = if request.path == "~" {
            let output = self.run_shell(&request.target, "printf '%s' \"$HOME\"", false)?;
            ensure_success(&output, "resolve target home directory")?;
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            request.path.clone()
        };
        let label = request.label.replace(['\t', '\n', '\r'], " ");
        let metadata_path = working_path.replace(['\t', '\n', '\r'], " ");
        let bootstrap = format!("{session_id}-bootstrap");
        let agent_target = format!("{session_id}:agent");
        let commands = [
            shell_join(&[
                "tmux",
                "new-session",
                "-d",
                "-s",
                &session_id,
                "-n",
                &bootstrap,
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "history-limit",
                &self.history_limit.to_string(),
            ]),
            shell_join(&[
                "tmux",
                "new-window",
                "-a",
                "-d",
                "-t",
                &format!("{session_id}:"),
                "-n",
                "agent",
                "-c",
                &working_path,
            ]),
            shell_join(&[
                "tmux",
                "kill-window",
                "-t",
                &format!("{session_id}:{bootstrap}"),
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-w",
                "-t",
                &agent_target,
                "remain-on-exit",
                "on",
            ]),
            shell_join(&["tmux", "set-option", "-t", &session_id, "status", "off"]),
            shell_join(&["tmux", "set-option", "-t", &session_id, "mouse", "on"]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_kind",
                request.kind.as_str(),
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_path",
                &metadata_path,
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_label",
                &label,
            ]),
            shell_join(&[
                "tmux",
                "set-option",
                "-t",
                &session_id,
                "@muxloom_created",
                &now.to_string(),
            ]),
            shell_join(&[
                "tmux",
                "respawn-pane",
                "-k",
                "-t",
                &agent_target,
                &agent_command,
            ]),
        ];
        let output = self.run_shell(&request.target, &commands.join(" && "), false)?;
        ensure_success(&output, "launch agent with legacy tmux fallback")?;
        debug::log(
            "runtime",
            format!(
                "launch target={} session={session_id} backend=legacy-tmux",
                request.target.id
            ),
        );
        Ok(session_id)
    }

    pub fn install_runtime(
        &self,
        target: &Target,
        kind: AgentKind,
        command: &CommandConfig,
        environment: &[(String, String)],
    ) -> Result<String> {
        self.install_runtime_with_progress(target, kind, command, environment, |_| {})
    }

    pub fn install_runtime_with_progress(
        &self,
        target: &Target,
        kind: AgentKind,
        command: &CommandConfig,
        environment: &[(String, String)],
        mut progress: impl FnMut(TaskProgress),
    ) -> Result<String> {
        if kind == AgentKind::Terminal {
            bail!("ordinary terminals do not require a runtime install");
        }
        progress(TaskProgress::pending(format!("Preparing {kind} install")));
        let executable_name = kind.as_str();
        let exports = environment_exports(environment);
        let platform = match &target.transport {
            Transport::Ssh { .. } => self.target_platform(target)?,
            Transport::Local => TargetPlatform::local(),
        };
        let mut installed_source = None;
        // Every built-in attempt that failed, in the order they were made, so
        // a target that ends up with nothing installed says what was tried
        // rather than only how the last hop went.
        let mut attempts: Vec<String> = Vec::new();
        // A configured command that names something else — a wrapper script, an
        // absolute path — is the operator's business, not a release we know how
        // to fetch, so that install goes straight to the configured command.
        let built_in = kind.has_release_download()
            && !command.command.contains('/')
            && command.command == executable_name;
        let remote = matches!(target.transport, Transport::Ssh { .. });

        // The target fetches its own runtime first. The payload then never
        // crosses the controller, and a machine with its own route to the
        // release does not wait on ours.
        if remote && built_in {
            match self.pull_runtime_on_target(target, kind, &platform, environment, &mut progress) {
                Ok(source) => installed_source = Some(source),
                Err(error) => {
                    debug::log(
                        "install",
                        format!(
                            "target-side pull failed target={} kind={kind}: {error:#}; falling back to shipping it from here",
                            target.id
                        ),
                    );
                    attempts.push(format!("target-side download failed: {error:#}"));
                }
            }
        }

        if installed_source.is_none()
            && remote
            && built_in
            && platform.matches_local()
            && let Some(local_binary) = find_local_native_executable(&command.command)
            && local_runtime_can_copy(kind, &local_binary)
        {
            progress(TaskProgress::pending(format!("Uploading {kind} runtime")));
            match self.upload_runtime_binary(target, &local_binary, executable_name) {
                Ok(()) => installed_source = Some("compatible controller binary".to_string()),
                Err(error) => {
                    debug::log(
                        "install",
                        format!(
                            "local binary upload failed target={} kind={kind}: {error:#}; falling back",
                            target.id
                        ),
                    );
                    attempts.push(format!("uploading the local binary failed: {error:#}"));
                }
            }
        }

        if installed_source.is_none() && remote && built_in {
            match self.download_and_install_runtime(
                target,
                kind,
                &platform,
                environment,
                &mut progress,
            ) {
                Ok(source) => installed_source = Some(source),
                Err(error) => {
                    debug::log(
                        "install",
                        format!(
                            "built-in package install failed target={} kind={kind}: {error:#}; trying configured target installer",
                            target.id
                        ),
                    );
                    attempts.push(format!("the controller download failed: {error:#}"));
                }
            }
        }

        if installed_source.is_none() {
            if command.install.trim().is_empty() {
                if !attempts.is_empty() {
                    bail!(
                        "{} is unavailable on {}; every built-in install failed: {}",
                        command.command,
                        target.id,
                        attempts.join("; ")
                    );
                }
                bail!(
                    "{} is unavailable and no install command is configured for {}",
                    command.command,
                    target.id
                );
            }
            progress(TaskProgress::pending(format!("Running {kind} installer")));
            let script = login_shell_command(&format!("{exports} {}", command.install));
            let output = self.run_shell(target, &script, false)?;
            if let Err(error) = ensure_success(&output, &format!("install {kind}")) {
                if !attempts.is_empty() {
                    bail!(
                        "{error}; the built-in installs also failed: {}",
                        attempts.join("; ")
                    );
                }
                return Err(error);
            }
            installed_source = Some("configured target installer".into());
        }

        let synced = if matches!(target.transport, Transport::Ssh { .. }) {
            progress(TaskProgress::pending(format!(
                "Syncing {kind} configuration"
            )));
            self.sync_local_config_files(target, &command.sync_files)?
        } else {
            0
        };
        let verify = login_shell_command(&format!(
            "{exports} command -v {} >/dev/null 2>&1",
            shell_quote(&command.command)
        ));
        progress(TaskProgress::pending(format!("Verifying {kind} install")));
        let output = self.run_shell(target, &verify, false)?;
        ensure_success(&output, &format!("verify {kind} install"))?;
        let source = installed_source.unwrap_or_else(|| "runtime installer".into());
        Ok(format!(
            "Installed {kind} on {} from {source}; synced {synced} local config file(s)",
            target.label
        ))
    }

    fn target_platform(&self, target: &Target) -> Result<TargetPlatform> {
        let output = self.run_shell(
            target,
            "uname -s; uname -m; if [ -e /etc/alpine-release ] || (ldd --version 2>&1 | grep -qi musl); then printf 'musl\\n'; else printf 'gnu\\n'; fi",
            false,
        )?;
        ensure_success(&output, "detect target platform")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        Ok(TargetPlatform {
            os: lines.next().unwrap_or_default().trim().to_ascii_lowercase(),
            arch: normalize_arch(lines.next().unwrap_or_default()).into(),
            musl: lines.next().is_some_and(|line| line.trim() == "musl"),
        })
    }

    fn download_and_install_runtime(
        &self,
        target: &Target,
        kind: AgentKind,
        platform: &TargetPlatform,
        environment: &[(String, String)],
        progress: &mut impl FnMut(TaskProgress),
    ) -> Result<String> {
        let controller_environment = self.controller_download_environment(target, environment);
        let release = self.resolve_release(kind, platform, &controller_environment, progress)?;
        let cache = controller_download_cache()
            .join(kind.as_str())
            .join(&release.version)
            .join(&release.platform_name)
            .join(&release.asset);
        let download_label = format!("Downloading {kind} {}", release.version);
        self.controller_download_verified(
            &release,
            &cache,
            &controller_environment,
            &download_label,
            progress,
        )?;
        let version = &release.version;
        match (kind, &target.transport) {
            (AgentKind::Claude, Transport::Local) => {
                progress(TaskProgress::pending(format!(
                    "Installing {kind} {version}"
                )));
                install_local_runtime_binary(&cache, "claude")?;
            }
            (AgentKind::Claude, Transport::Ssh { .. }) => {
                progress(TaskProgress::pending(format!("Uploading {kind} {version}")));
                self.upload_runtime_binary(target, &cache, "claude")?;
            }
            // Codex and OpenCode each ship one self-contained executable
            // inside their package, so only that file need cross the wire.
            (AgentKind::Codex | AgentKind::OpenCode, Transport::Ssh { .. }) => {
                progress(TaskProgress::pending(format!(
                    "Extracting {kind} {version}"
                )));
                let executable = extract_cached_bundle_executable(&cache, kind)?;
                progress(TaskProgress::pending(format!("Uploading {kind} {version}")));
                self.upload_runtime_binary(target, &executable, kind.as_str())?;
            }
            (AgentKind::OpenCode, Transport::Local) => {
                progress(TaskProgress::pending(format!(
                    "Installing {kind} {version}"
                )));
                let executable = extract_cached_bundle_executable(&cache, kind)?;
                install_local_runtime_binary(&executable, "opencode")?;
            }
            (AgentKind::Codex | AgentKind::Pi, Transport::Local) => {
                progress(TaskProgress::pending(format!(
                    "Installing {kind} {version}"
                )));
                install_local_bundle(&cache, kind, version)?;
            }
            // Pi's executable is no use on its own: the themes, assets and
            // modules beside it are part of the runtime, so the package itself
            // is what the machine is handed.
            (AgentKind::Pi, Transport::Ssh { .. }) => {
                progress(TaskProgress::pending(format!("Uploading {kind} {version}")));
                self.upload_runtime_bundle(target, kind, &cache, version)?;
            }
            (kind, _) => bail!("{kind} has no downloadable agent runtime"),
        }
        Ok(format!(
            "controller-downloaded {kind} {version} ({})",
            release.platform_name
        ))
    }

    /// Have the target fetch its own runtime. The controller resolves the
    /// release metadata — a few hundred bytes — and the target spends its own
    /// bandwidth on the payload, checking it against the digest we resolved
    /// before anything is moved into place. Bounded on both ends: a target
    /// with no route to the release says so in seconds instead of holding the
    /// install open, and the caller falls back to shipping the bytes from here.
    fn pull_runtime_on_target(
        &self,
        target: &Target,
        kind: AgentKind,
        platform: &TargetPlatform,
        environment: &[(String, String)],
        progress: &mut impl FnMut(TaskProgress),
    ) -> Result<String> {
        let controller_environment = self.controller_download_environment(target, environment);
        let release = self.resolve_release(kind, platform, &controller_environment, progress)?;
        progress(TaskProgress::pending(format!(
            "{} is downloading {kind} {}",
            target.label, release.version
        )));
        let script = login_shell_command(&remote_pull_script(
            kind,
            &release,
            &environment_exports(environment),
        ));
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, &format!("download {kind} on {}", target.label))?;
        Ok(format!(
            "{} downloaded {kind} {} ({}) itself",
            target.label, release.version, release.platform_name
        ))
    }

    /// Resolve which file a target needs and what it must hash to. Every
    /// request here is small: a version string, a manifest, a checksum list.
    fn resolve_release(
        &self,
        kind: AgentKind,
        platform: &TargetPlatform,
        controller_environment: &[(String, String)],
        progress: &mut impl FnMut(TaskProgress),
    ) -> Result<RemoteRelease> {
        match kind {
            AgentKind::Claude => {
                let platform_name = platform.claude_name()?;
                progress(TaskProgress::pending("Resolving Claude release"));
                let version = validate_release_name(
                    self.controller_fetch_text(
                        &format!("{CLAUDE_RELEASES}/latest"),
                        controller_environment,
                    )?
                    .trim(),
                )?;
                let manifest = self.controller_fetch_text(
                    &format!("{CLAUDE_RELEASES}/{version}/manifest.json"),
                    controller_environment,
                )?;
                let manifest: Value = serde_json::from_str(&manifest)
                    .context("Claude release manifest is invalid JSON")?;
                let checksum = manifest
                    .get("platforms")
                    .and_then(|platforms| platforms.get(&platform_name))
                    .and_then(|platform| platform.get("checksum"))
                    .and_then(Value::as_str)
                    .context("Claude manifest has no checksum for the target platform")?;
                validate_digest(checksum, DigestAlgorithm::Sha256)?;
                Ok(RemoteRelease {
                    url: format!("{CLAUDE_RELEASES}/{version}/{platform_name}/claude"),
                    digest: checksum.to_ascii_lowercase(),
                    algorithm: DigestAlgorithm::Sha256,
                    asset: "claude".into(),
                    version,
                    platform_name,
                })
            }
            AgentKind::Codex => {
                let platform_name = platform.codex_name()?;
                progress(TaskProgress::pending("Resolving Codex release"));
                let effective =
                    self.controller_effective_url(CODEX_LATEST, controller_environment)?;
                let version = effective
                    .rsplit("/tag/rust-v")
                    .next()
                    .filter(|value| *value != effective)
                    .map(validate_release_name)
                    .transpose()?
                    .context("could not resolve the latest Codex release")?;
                let asset = format!("codex-package-{platform_name}.tar.gz");
                let release_root = format!("{CODEX_RELEASES}/rust-v{version}");
                let checksums = self.controller_fetch_text(
                    &format!("{release_root}/codex-package_SHA256SUMS"),
                    controller_environment,
                )?;
                let checksum = checksum_for_asset(&checksums, &asset)
                    .context("Codex checksum manifest has no target package")?;
                Ok(RemoteRelease {
                    url: format!("{release_root}/{asset}"),
                    digest: checksum,
                    algorithm: DigestAlgorithm::Sha256,
                    asset,
                    version,
                    platform_name,
                })
            }
            AgentKind::Pi => {
                let platform_name = platform.pi_name()?;
                progress(TaskProgress::pending("Resolving Pi release"));
                let effective = self.controller_effective_url(PI_LATEST, controller_environment)?;
                let version = effective
                    .rsplit("/tag/v")
                    .next()
                    .filter(|value| *value != effective)
                    .map(validate_release_name)
                    .transpose()?
                    .context("could not resolve the latest Pi release")?;
                let asset = format!("pi-{platform_name}.tar.gz");
                let release_root = format!("{PI_RELEASES}/v{version}");
                let checksums = self.controller_fetch_text(
                    &format!("{release_root}/SHA256SUMS"),
                    controller_environment,
                )?;
                let checksum = checksum_for_asset(&checksums, &asset)
                    .context("Pi checksum manifest has no target package")?;
                Ok(RemoteRelease {
                    url: format!("{release_root}/{asset}"),
                    digest: checksum,
                    algorithm: DigestAlgorithm::Sha256,
                    asset,
                    version,
                    platform_name,
                })
            }
            AgentKind::OpenCode => {
                let platform_name = platform.opencode_name()?;
                progress(TaskProgress::pending("Resolving OpenCode release"));
                let latest = self.controller_fetch_text(
                    &format!("{NPM_REGISTRY}/opencode-ai/latest"),
                    controller_environment,
                )?;
                let latest: Value = serde_json::from_str(&latest)
                    .context("the OpenCode registry entry is invalid JSON")?;
                let version = validate_release_name(
                    latest
                        .get("version")
                        .and_then(Value::as_str)
                        .context("the OpenCode registry entry names no version")?,
                )?;
                let package = format!("opencode-{platform_name}");
                let manifest = self.controller_fetch_text(
                    &format!("{NPM_REGISTRY}/{package}/{version}"),
                    controller_environment,
                )?;
                let manifest: Value = serde_json::from_str(&manifest)
                    .context("the OpenCode package manifest is invalid JSON")?;
                let (url, digest, algorithm) = registry_distribution(&manifest)?;
                Ok(RemoteRelease {
                    asset: format!("{package}-{version}.tgz"),
                    url,
                    digest,
                    algorithm,
                    version,
                    platform_name,
                })
            }
            // An ordinary terminal is whatever shell the machine already has.
            kind => bail!("{kind} has no downloadable agent runtime"),
        }
    }

    fn controller_download_environment(
        &self,
        target: &Target,
        environment: &[(String, String)],
    ) -> Vec<(String, String)> {
        if matches!(target.transport, Transport::Local) {
            return environment.to_vec();
        }
        let tunnel = self
            .host_reverse_tunnels
            .get(&target.id)
            .map(String::as_str)
            .unwrap_or(&self.reverse_tunnel);
        Self::map_controller_proxy_environment(environment, tunnel)
    }

    fn controller_environment_for_config(config: &Config, host: &str) -> Vec<(String, String)> {
        let environment = config.environment_for(host).unwrap_or_default();
        Self::map_controller_proxy_environment(&environment, config.reverse_tunnel_for(host))
    }

    fn map_controller_proxy_environment(
        environment: &[(String, String)],
        tunnel: &str,
    ) -> Vec<(String, String)> {
        let Some((remote_port, local_host, local_port)) = parse_reverse_tunnel(tunnel) else {
            return environment.to_vec();
        };
        let remote_loopback = format!("127.0.0.1:{remote_port}");
        let remote_localhost = format!("localhost:{remote_port}");
        let local_endpoint = format!("{local_host}:{local_port}");
        environment
            .iter()
            .map(|(name, value)| {
                let value = if name.to_ascii_uppercase().ends_with("_PROXY") {
                    value
                        .replace(&remote_loopback, &local_endpoint)
                        .replace(&remote_localhost, &local_endpoint)
                } else {
                    value.clone()
                };
                (name.clone(), value)
            })
            .collect()
    }

    fn controller_fetch_text(&self, url: &str, environment: &[(String, String)]) -> Result<String> {
        #[cfg(feature = "controller")]
        {
            crate::http::fetch_text(url, environment)
                .with_context(|| format!("failed to download {url} on the controller"))
        }
        #[cfg(not(feature = "controller"))]
        {
            let _ = (url, environment);
            bail!("agent downloads require the controller feature")
        }
    }

    fn controller_effective_url(
        &self,
        url: &str,
        environment: &[(String, String)],
    ) -> Result<String> {
        #[cfg(feature = "controller")]
        {
            crate::http::effective_url(url, environment)
                .with_context(|| format!("failed to resolve {url} on the controller"))
        }
        #[cfg(not(feature = "controller"))]
        {
            let _ = (url, environment);
            bail!("agent downloads require the controller feature")
        }
    }

    fn controller_download_verified(
        &self,
        release: &RemoteRelease,
        destination: &Path,
        environment: &[(String, String)],
        progress_label: &str,
        progress: &mut impl FnMut(TaskProgress),
    ) -> Result<()> {
        let RemoteRelease {
            url,
            digest: expected,
            algorithm,
            ..
        } = release;
        let algorithm = *algorithm;
        if destination.is_file()
            && digest_file(destination, algorithm).is_ok_and(|digest| &digest == expected)
        {
            return Ok(());
        }
        let parent = destination
            .parent()
            .context("controller download path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let download_id = DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let partial =
            destination.with_extension(format!("partial-{}-{download_id}", std::process::id()));
        let result = controller_download(url, &partial, environment, |completed, total| {
            progress(TaskProgress::bytes(progress_label, completed, total));
        })
        .with_context(|| format!("failed to download {url} on the controller"));
        if let Err(error) = result {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
        let actual = digest_file(&partial, algorithm)?;
        if &actual != expected {
            let _ = fs::remove_file(&partial);
            bail!("download checksum mismatch: expected {expected}, got {actual}");
        }
        if destination.exists() {
            fs::remove_file(destination).with_context(|| {
                format!("failed to replace stale cache {}", destination.display())
            })?;
        }
        fs::rename(&partial, destination).with_context(|| {
            format!(
                "failed to move verified download into {}",
                destination.display()
            )
        })?;
        Ok(())
    }

    /// Hand a machine a runtime that is a directory rather than a file. The
    /// package goes across whole and is unpacked there by the same script the
    /// machine would have run had it fetched the package itself, so a runtime
    /// ends up in the same place either way.
    fn upload_runtime_bundle(
        &self,
        target: &Target,
        kind: AgentKind,
        archive: &Path,
        version: &str,
    ) -> Result<()> {
        let Transport::Ssh { alias } = &target.transport else {
            return Ok(());
        };
        let remote_home = self.remote_home(target)?;
        let staging_dir = format!("{remote_home}/.cache/muxloom/install");
        let payload = format!("{staging_dir}/{}.package", kind.as_str());
        let prepare = format!("mkdir -p {}", shell_quote(&staging_dir));
        let output = self.run_shell(target, &prepare, false)?;
        ensure_success(&output, "prepare remote install directory")?;
        self.scp_to(alias, archive, &payload)?;
        let script = login_shell_command(&remote_unpack_script(kind, &payload, version));
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, &format!("unpack {kind} on {}", target.label))?;
        Ok(())
    }

    fn upload_runtime_binary(
        &self,
        target: &Target,
        local_binary: &Path,
        executable_name: &str,
    ) -> Result<()> {
        let Transport::Ssh { alias } = &target.transport else {
            return Ok(());
        };
        let remote_home = self.remote_home(target)?;
        let remote_dir = format!("{remote_home}/.local/bin");
        let staging = format!("{remote_home}/.cache/muxloom/install/{executable_name}.tmp");
        let bundle_dir = format!("{remote_home}/.local/share/muxloom/{executable_name}");
        let prepare = format!(
            "mkdir -p {} {} {} {}",
            shell_quote(&remote_dir),
            shell_quote(&format!("{remote_home}/.cache/muxloom/install")),
            shell_quote(&bundle_dir),
            shell_quote(&format!("{bundle_dir}/codex-resources"))
        );
        let output = self.run_shell(target, &prepare, false)?;
        ensure_success(&output, "prepare remote install directory")?;
        self.scp_to(alias, local_binary, &staging)?;
        let destination = if executable_name == "codex" {
            format!("{bundle_dir}/codex")
        } else {
            format!("{remote_dir}/{executable_name}")
        };
        let install = format!(
            "chmod 755 {} && mv -f {} {}",
            shell_quote(&staging),
            shell_quote(&staging),
            shell_quote(&destination)
        );
        let output = self.run_shell(target, &install, false)?;
        ensure_success(&output, "activate uploaded runtime")?;
        if executable_name == "codex" {
            for resource in ["bwrap", "rg"] {
                let Some(local_resource) = find_codex_resource(local_binary, resource) else {
                    continue;
                };
                let staging_resource =
                    format!("{remote_home}/.cache/muxloom/install/{resource}.tmp");
                self.scp_to(alias, &local_resource, &staging_resource)?;
                let destination_resource = format!("{bundle_dir}/codex-resources/{resource}");
                let activate = format!(
                    "chmod 755 {} && mv -f {} {}",
                    shell_quote(&staging_resource),
                    shell_quote(&staging_resource),
                    shell_quote(&destination_resource)
                );
                let output = self.run_shell(target, &activate, false)?;
                ensure_success(&output, "activate Codex runtime resource")?;
            }
            let link = format!(
                "ln -sfn {} {}",
                shell_quote(&destination),
                shell_quote(&format!("{remote_dir}/codex"))
            );
            let output = self.run_shell(target, &link, false)?;
            ensure_success(&output, "link uploaded Codex runtime")?;
        }
        Ok(())
    }

    fn sync_local_config_files(&self, target: &Target, files: &[String]) -> Result<usize> {
        let Transport::Ssh { alias } = &target.transport else {
            return Ok(0);
        };
        let local_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is unavailable while syncing local config")?;
        let remote_home = self.remote_home(target)?;
        let staging_dir = format!("{remote_home}/.cache/muxloom/install");
        let mut synced = 0;
        for (index, configured) in files.iter().enumerate() {
            let local_path = expand_home_path(configured, &local_home);
            if !local_path.is_file() {
                debug::log(
                    "install",
                    format!("skip missing local config {}", local_path.display()),
                );
                continue;
            }
            let relative = local_path.strip_prefix(&local_home).with_context(|| {
                format!(
                    "config sync path must be inside HOME: {}",
                    local_path.display()
                )
            })?;
            let remote_path = Path::new(&remote_home).join(relative);
            let remote_path = remote_path.to_string_lossy().to_string();
            let parent = Path::new(&remote_path)
                .parent()
                .context("config sync destination has no parent")?
                .to_string_lossy()
                .to_string();
            let staging = format!("{staging_dir}/config-{index}.tmp");
            let prepare = format!(
                "mkdir -p {} {}; if [ -f {} ]; then cp -p {} {}.muxloom-backup-$(date +%Y%m%d-%H%M%S); fi",
                shell_quote(&staging_dir),
                shell_quote(&parent),
                shell_quote(&remote_path),
                shell_quote(&remote_path),
                shell_quote(&remote_path),
            );
            let output = self.run_shell(target, &prepare, false)?;
            ensure_success(&output, "prepare config sync")?;
            self.scp_to(alias, &local_path, &staging)?;
            let activate = format!(
                "chmod 600 {} && mv -f {} {}",
                shell_quote(&staging),
                shell_quote(&staging),
                shell_quote(&remote_path)
            );
            let output = self.run_shell(target, &activate, false)?;
            ensure_success(&output, "activate synced config")?;
            synced += 1;
        }
        Ok(synced)
    }

    pub fn remote_home(&self, target: &Target) -> Result<String> {
        let output = self.run_shell(target, "printf '%s\\n' \"$HOME\"", false)?;
        ensure_success(&output, "resolve remote home")?;
        let home = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if home.is_empty() {
            bail!("target returned an empty HOME");
        }
        Ok(home)
    }

    fn scp_to(&self, alias: &str, local_path: &Path, remote_path: &str) -> Result<()> {
        let control_path = ssh_control_path();
        let output = Command::new("scp")
            .args([
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("ConnectTimeout={}", self.ssh_connect_timeout_secs),
                "-o",
                "ControlMaster=auto",
                "-o",
                SSH_CONTROL_PERSIST_OPTION,
                "-o",
                SSH_SERVER_ALIVE_INTERVAL_OPTION,
                "-o",
                SSH_SERVER_ALIVE_COUNT_OPTION,
                "-o",
                SSH_CONNECTION_ATTEMPTS_OPTION,
                "-o",
                &format!("ControlPath={control_path}"),
            ])
            .arg(local_path)
            .arg(format!("{alias}:{}", shell_quote(remote_path)))
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to upload {}", local_path.display()))?;
        ensure_success(&output, "upload runtime file")
    }

    fn scp_from(&self, alias: &str, remote_path: &str, local_path: &Path) -> Result<()> {
        let control_path = ssh_control_path();
        let output = Command::new("scp")
            .args([
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("ConnectTimeout={}", self.ssh_connect_timeout_secs),
                "-o",
                "ControlMaster=auto",
                "-o",
                SSH_CONTROL_PERSIST_OPTION,
                "-o",
                SSH_SERVER_ALIVE_INTERVAL_OPTION,
                "-o",
                SSH_SERVER_ALIVE_COUNT_OPTION,
                "-o",
                SSH_CONNECTION_ATTEMPTS_OPTION,
                "-o",
                &format!("ControlPath={control_path}"),
            ])
            .arg(format!("{alias}:{}", shell_quote(remote_path)))
            .arg(local_path)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to download {remote_path}"))?;
        ensure_success(&output, "download remote file")
    }

    pub fn capture_page(
        &self,
        target: &Target,
        session_id: &str,
        offset_from_bottom: usize,
        lines: usize,
        _width: u16,
        _height: u16,
    ) -> Result<HistoryPage> {
        validate_session_id(session_id)?;
        let lines = lines.max(1);
        if is_daemon_session_id(session_id) {
            // Pages are for looking at, so they are asked for as rendered rows:
            // the same unit tmux captures in, and the one an attached emulator
            // scrolls through.
            let history = self.bridges.read_history(
                target,
                session_id.into(),
                offset_from_bottom,
                lines,
                true,
            )?;
            let pane_height = usize::from(history.rows);
            return Ok(HistoryPage {
                text: String::from_utf8_lossy(&history.bytes)
                    .trim_end()
                    .to_string(),
                history_size: history.total_lines.saturating_sub(pane_height),
                pane_height,
                pane_width: usize::from(history.columns),
                offset_from_bottom: history.offset_from_bottom,
                rendered: history.rendered,
                // A rendered page is replayed only as deep as it was asked to
                // go, so reaching the offset that was asked for says nothing
                // about the log ending there; falling short of it does. Neither
                // does reading the log from the beginning leave anything above,
                // however far the page was asked to reach — without that the
                // size a page reports is never a boundary and the view scrolls
                // off the top of a session that has already been read whole.
                more_history: history.rendered
                    && !history.reached_start
                    && history.offset_from_bottom >= offset_from_bottom,
            });
        }
        // Derive capture coordinates from the pane's actual height. History
        // reads must never resize the tmux window: doing so races the attached
        // PTY and produces the familiar vertical-bar/dot resize artifacts.
        let pane_height = shell_join(&[
            "tmux",
            "display-message",
            "-p",
            "-t",
            session_id,
            "#{pane_height}",
        ]);
        let history_size = shell_join(&[
            "tmux",
            "display-message",
            "-p",
            "-t",
            session_id,
            "#{history_size}",
        ]);
        let pane_width = shell_join(&[
            "tmux",
            "display-message",
            "-p",
            "-t",
            session_id,
            "#{pane_width}",
        ]);
        let capture = shell_join(&["tmux", "capture-pane", "-p", "-e", "-t", session_id]);
        let script = format!(
            "history_size=$({history_size}) || exit $?; \
             pane_height=$({pane_height}) || exit $?; \
             pane_width=$({pane_width}) || exit $?; \
             offset={offset_from_bottom}; \
             if [ \"$offset\" -gt \"$history_size\" ]; then offset=$history_size; fi; \
             printf '__AD_INFO__%s\\t%s\\t%s\\t%s\\n' \"$history_size\" \"$pane_height\" \"$pane_width\" \"$offset\"; \
             start=$((-{lines} - offset)); \
             end=$((pane_height - 1 - offset)); \
             {capture} -S \"$start\" -E \"$end\""
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "capture recap")?;
        parse_history_page(&String::from_utf8_lossy(&output.stdout), offset_from_bottom)
    }

    pub fn tcp_listener_ports(&self, target: &Target) -> Result<Vec<u16>> {
        self.bridges.tcp_listener_ports(target)
    }

    /// The newest `lines` rows the session actually drew.
    ///
    /// A page is anchored to the bottom of the emulator's screen, so asking for
    /// fewer rows than the pane is tall lands entirely among the rows below a
    /// short session's output — a recap of a command that printed one line came
    /// back as a screen's worth of blanks. Reach past the pane once that has
    /// happened and keep the rows something was drawn on.
    pub fn capture(&self, target: &Target, session_id: &str, lines: usize) -> Result<String> {
        let lines = lines.max(1);
        let page = self.capture_page(target, session_id, 0, lines, 80, 24)?;
        let deep = lines.saturating_add(page.pane_height);
        let page = if deep > lines && drawn_rows(&page.text).len() < lines {
            self.capture_page(target, session_id, 0, deep, 80, 24)?
        } else {
            page
        };
        Ok(newest_drawn_rows(&page.text, lines))
    }

    pub fn detect_attention(
        &self,
        target: &Target,
        session_id: &str,
        kind: AgentKind,
        patterns: &[String],
    ) -> Result<Option<String>> {
        validate_session_id(session_id)?;
        let script = shell_join(&["tmux", "capture-pane", "-p", "-t", session_id]);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "inspect agent attention")?;
        let screen = String::from_utf8_lossy(&output.stdout);
        let reason = attention_reason(kind, &screen, patterns);
        if let Some(reason) = &reason {
            debug::log(
                "attention",
                format!(
                    "matched target={} session={} kind={} reason={} tail={}",
                    target.id,
                    session_id,
                    kind,
                    reason,
                    attention_debug_tail(&screen)
                ),
            );
        }
        Ok(reason)
    }

    pub fn inspect_agent(
        &self,
        target: &Target,
        session_id: &str,
        kind: AgentKind,
        patterns: &[String],
    ) -> Result<(bool, Option<String>, Option<String>)> {
        validate_session_id(session_id)?;
        if is_daemon_session_id(session_id) {
            return Ok((false, None, None));
        }
        let script = shell_join(&["tmux", "capture-pane", "-p", "-S", "-200", "-t", session_id]);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "inspect agent state")?;
        let screen = String::from_utf8_lossy(&output.stdout);
        let attention = attention_reason(kind, &screen, patterns);
        let working = attention.is_none() && agent_is_working(kind, &screen);
        let recap = extract_recap(kind, &screen);
        if let Some(reason) = &attention {
            debug::log(
                "attention",
                format!(
                    "matched target={} session={} kind={} reason={} tail={}",
                    target.id,
                    session_id,
                    kind,
                    reason,
                    attention_debug_tail(&screen)
                ),
            );
        }
        debug::log(
            "activity",
            format!(
                "target={} session={} kind={} working={} attention={}",
                target.id,
                session_id,
                kind,
                working,
                attention.is_some()
            ),
        );
        Ok((working, attention, recap))
    }

    pub fn search_history(
        &self,
        target: &Target,
        session_id: &str,
        query: &str,
        max_matches: usize,
    ) -> Result<Vec<HistoryMatch>> {
        validate_session_id(session_id)?;
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let max_matches = max_matches.clamp(1, 50);
        if is_daemon_session_id(session_id) {
            return Ok(self
                .bridges
                .search_history(target, session_id.into(), query.into(), max_matches)?
                .into_iter()
                .map(|item| HistoryMatch {
                    recap: item.recap,
                    line_number: item.line_number,
                    text: item.text,
                })
                .collect());
        }
        let recap = shell_join(&["tmux", "capture-pane", "-p", "-J", "-t", session_id]);
        let history = shell_join(&[
            "tmux",
            "capture-pane",
            "-p",
            "-J",
            "-S",
            "-",
            "-t",
            session_id,
        ]);
        let awk_program = r#"BEGIN { term_count = split(tolower(q), terms, /[[:space:]]+/) }
{
    lowered = tolower($0)
    matched = 1
    for (term = 1; term <= term_count; term++) {
        if (terms[term] != "" && index(lowered, terms[term]) == 0) {
            matched = 0
            break
        }
    }
    if (matched) {
        slot = found % limit
        numbers[slot] = NR
        lines[slot] = $0
        found++
    }
}
END {
    start = found > limit ? found - limit : 0
    for (item = start; item < found; item++) {
        slot = item % limit
        printf "%s%d\t%s\n", prefix, numbers[slot], lines[slot]
    }
}"#;
        let awk_recap = shell_join(&[
            "awk",
            "-v",
            &format!("q={query}"),
            "-v",
            "prefix=__AD_RECAP__",
            "-v",
            &format!("limit={max_matches}"),
            awk_program,
        ]);
        let awk_history = shell_join(&[
            "awk",
            "-v",
            &format!("q={query}"),
            "-v",
            "prefix=__AD_HISTORY__",
            "-v",
            &format!("limit={max_matches}"),
            awk_program,
        ]);
        let script = format!("{recap} | {awk_recap}; {history} | {awk_history}");
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "search agent history")?;
        Ok(parse_history_matches(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    pub fn list_directory(&self, target: &Target, path: &str) -> Result<DirectoryListing> {
        let path = if path.trim().is_empty() { "." } else { path };
        if !self.bridge_recently_failed(&target.id) {
            match self.bridges.list_directory(target, path.into()) {
                Ok(listing) => return Ok(listing),
                Err(error) if self.bridges.is_connected(&target.id) => return Err(error),
                Err(_) => self.mark_bridge_failed(&target.id),
            }
        }
        let script = format!(
            "cd {} && pwd -P && find -L . -mindepth 1 -maxdepth 1 -type d -print0",
            shell_quote(path)
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "list directory")?;
        parse_directory_listing(&output.stdout)
    }

    pub fn list_files(&self, target: &Target, path: &str) -> Result<FileListing> {
        let path = if path.trim().is_empty() { "." } else { path };
        if !self.bridge_recently_failed(&target.id) {
            match self.bridges.list_files(target, path.into()) {
                Ok(listing) => return Ok(listing),
                Err(error) if self.bridges.is_connected(&target.id) => return Err(error),
                Err(_) => self.mark_bridge_failed(&target.id),
            }
        }
        // Size and modification time come from a single stat so the browser can
        // tell that an open file changed without paying a second call per entry.
        // Link entries report what they resolve to (test follows links), with the
        // kind letter upper-cased so the browser can still mark them as links:
        // d/D directory, f/F regular file, o/O anything else or a broken link.
        let collect = r#"for entry do
            if [ -L "$entry" ]; then link=1; else link=0; fi
            if [ -d "$entry" ]; then kind=d; size=0; mtime=0;
            elif [ -f "$entry" ]; then
                kind=f
                meta=$(stat -c '%s %Y' -- "$entry" 2>/dev/null || stat -f '%z %m' "$entry" 2>/dev/null)
                case "$meta" in
                    *' '*) size=${meta%% *}; mtime=${meta##* } ;;
                    *) size=$(wc -c < "$entry" | tr -d '[:space:]'); mtime=0 ;;
                esac
            else kind=o; size=0; mtime=0; fi
            if [ "$link" = 1 ]; then
                case "$kind" in d) kind=D ;; f) kind=F ;; *) kind=O ;; esac
            fi
            name=${entry#./}
            printf '%s\0%s\0%s\0%s\0' "$kind" "$size" "$mtime" "$name"
        done"#;
        let find = shell_join(&[
            "find",
            ".",
            "-mindepth",
            "1",
            "-maxdepth",
            "1",
            "-exec",
            "sh",
            "-c",
            collect,
            "sh",
            "{}",
            "+",
        ]);
        let script = format!(
            "cd {} && printf '%s\\0' \"$(pwd -P)\" && {find}",
            shell_quote(path)
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "list files")?;
        parse_file_listing(&output.stdout)
    }

    pub fn search_files(&self, target: &Target, root: &str, pattern: &str) -> Result<FileListing> {
        const MAX_VISITED: usize = 100_000;
        const MAX_RESULTS: usize = 2_000;

        let root_listing = self.list_files(target, root)?;
        let canonical_root = root_listing.path.clone();
        let mut directories = VecDeque::<String>::new();
        let mut listing = Some(root_listing);
        let mut results = Vec::new();
        let mut visited = 0usize;
        // The walk gives up on huge trees rather than running forever, and the
        // caller has to be able to say so instead of showing a partial answer
        // as if it were the whole one.
        let mut truncated = false;

        loop {
            let current = if let Some(listing) = listing.take() {
                listing
            } else if let Some(path) = directories.pop_front() {
                match self.list_files(target, &path) {
                    Ok(listing) => listing,
                    Err(error) => {
                        debug::log(
                            "files",
                            format!("recursive search skipped {path}: {error:#}"),
                        );
                        continue;
                    }
                }
            } else {
                break;
            };

            for mut entry in current.entries {
                visited += 1;
                if visited > MAX_VISITED || results.len() >= MAX_RESULTS {
                    truncated = true;
                    break;
                }
                match entry.kind {
                    // Links are not descended into: a link back up the tree
                    // would make the walk loop until the visit budget runs out.
                    FileEntryKind::Directory if !entry.symlink => directories.push_back(entry.path),
                    FileEntryKind::File | FileEntryKind::Symlink | FileEntryKind::Other
                        if filename_matches_pattern(&entry.name, pattern) =>
                    {
                        entry.name = relative_search_path(&canonical_root, &entry.path);
                        results.push(entry);
                    }
                    _ => {}
                }
            }
            if visited > MAX_VISITED || results.len() >= MAX_RESULTS {
                truncated = true;
                break;
            }
        }
        results.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(FileListing {
            truncated,
            path: canonical_root,
            entries: results,
        })
    }

    pub fn preview_file(&self, target: &Target, path: &str) -> Result<FilePreview> {
        // One round trip covers most files. Anything larger comes back flagged
        // and is completed over the chunked file stream, so a preview shows the
        // whole file while no single frame carries more than this.
        const FIRST_READ: u64 = 1024 * 1024;
        let limit = PREVIEW_BYTE_LIMIT;
        if !self.bridge_recently_failed(&target.id) {
            match self
                .bridges
                .preview_file(target, path.into(), FIRST_READ as usize)
            {
                Ok(preview) => return Ok(self.complete_preview(target, path, preview)),
                Err(error) if self.bridges.is_connected(&target.id) => return Err(error),
                Err(_) => self.mark_bridge_failed(&target.id),
            }
        }
        let quoted = shell_quote(path);
        let script = format!(
            r#"path={quoted}
            test -f "$path" || {{ printf 'not a regular file\n' >&2; exit 2; }}
            size=$(stat -c %s -- "$path" 2>/dev/null || stat -f %z "$path" 2>/dev/null || wc -c < "$path" | tr -d '[:space:]')
            if command -v file >/dev/null 2>&1; then
                mime=$(file -b --mime-type -- "$path" 2>/dev/null || printf application/octet-stream)
                description=$(file -b -- "$path" 2>/dev/null || true)
            else
                mime=
                description=
            fi
            lower=$(printf '%s' "$path" | tr '[:upper:]' '[:lower:]')
            case "$lower" in
                *.md|*.markdown|*.mdown|*.mkd) kind=markdown ;;
                *.png|*.jpg|*.jpeg|*.gif|*.webp|*.bmp|*.ico|*.tif|*.tiff|*.pnm|*.pbm|*.pgm|*.ppm|*.qoi) kind=image ;;
                *.mp3|*.wav|*.flac|*.aac|*.m4a|*.ogg|*.opus) kind=audio ;;
                *.mp4|*.m4v|*.mov|*.mkv|*.webm|*.avi|*.mpeg|*.mpg) kind=video ;;
                *) case "$mime" in
                    text/*|application/json|application/xml|application/javascript|application/x-sh|application/toml) kind=text ;;
                    image/*) kind=image ;;
                    audio/*) kind=audio ;;
                    video/*) kind=video ;;
                    *) kind=binary ;;
                esac ;;
            esac
            if [ "$kind" = binary ] && {{ [ ! -s "$path" ] || {{ command -v grep >/dev/null 2>&1 && LC_ALL=C grep -Iq . "$path"; }}; }}; then
                kind=text
                [ -n "$mime" ] || mime=text/plain
            fi
            limit={limit}
            truncated=0
            case "$kind" in
                text|markdown) [ "${{size:-0}}" -gt "$limit" ] 2>/dev/null && truncated=1 ;;
            esac
            printf '%s\0%s\0%s\0%s\0%s\0' "$path" "$mime" "$kind" "$size" "$truncated"
            case "$kind" in
                text|markdown)
                    if head -c 1 </dev/null >/dev/null 2>&1; then
                        head -c "$limit" < "$path"
                    else
                        cat -- "$path"
                    fi ;;
                audio|video)
                    if command -v ffprobe >/dev/null 2>&1; then
                        ffprobe -v error -show_entries format=format_name,duration,size,bit_rate:stream=index,codec_name,codec_type,width,height,sample_rate,channels -of default=noprint_wrappers=1 -- "$path" 2>&1 | head -n 160
                    else
                        printf '%s\n' "$description"
                        printf 'ffprobe is not installed on the target\n'
                    fi ;;
                *) printf '%s\n' "$description" ;;
            esac"#
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "preview file")?;
        parse_file_preview(&output.stdout)
    }

    /// Pull the rest of a text preview whose first response hit the per-frame
    /// limit. The partial body is dropped rather than joined onto the stream:
    /// the companion decodes lossily, so the length of the string it returned
    /// is not the byte offset it stopped reading at. A failed completion keeps
    /// the partial preview — the reader sees the start of the file and a note —
    /// instead of turning a readable file into an error.
    fn complete_preview(
        &self,
        target: &Target,
        path: &str,
        mut preview: FilePreview,
    ) -> FilePreview {
        if !preview.truncated
            || !matches!(
                preview.kind,
                FilePreviewKind::Text | FilePreviewKind::Markdown
            )
        {
            return preview;
        }
        match self
            .bridges
            .read_file(target, path.into(), PREVIEW_BYTE_LIMIT)
        {
            Ok(bytes) => {
                preview.truncated = bytes.len() as u64 >= PREVIEW_BYTE_LIMIT;
                preview.content = String::from_utf8_lossy(&bytes).into_owned();
            }
            Err(error) => debug::log(
                "runtime",
                format!("preview completion failed for {path}: {error}"),
            ),
        }
        preview
    }

    pub fn download_file(
        &self,
        target: &Target,
        remote_path: &str,
        local_directory: &Path,
    ) -> Result<PathBuf> {
        self.download_file_with_progress(target, remote_path, local_directory, |_| {})
    }

    pub fn download_file_with_progress(
        &self,
        target: &Target,
        remote_path: &str,
        local_directory: &Path,
        mut progress: impl FnMut(u64),
    ) -> Result<PathBuf> {
        let name = Path::new(remote_path)
            .file_name()
            .filter(|name| !name.is_empty())
            .context("selected file has no filename")?;
        fs::create_dir_all(local_directory).with_context(|| {
            format!(
                "failed to create download directory {}",
                local_directory.display()
            )
        })?;
        let destination = unique_destination(local_directory, name);
        let temporary = destination.with_file_name(format!(
            ".{}.muxloom-part-{}",
            name.to_string_lossy(),
            std::process::id()
        ));
        let transfer_result = match &target.transport {
            Transport::Local => {
                let mut source = File::open(remote_path)
                    .with_context(|| format!("failed to open {remote_path}"))?;
                (|| -> Result<()> {
                    let mut output = File::create(&temporary)
                        .with_context(|| format!("failed to create {}", temporary.display()))?;
                    let mut buffer = vec![0; 128 * 1024];
                    let mut transferred = 0u64;
                    loop {
                        let read = source.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        output.write_all(&buffer[..read])?;
                        transferred = transferred.saturating_add(read as u64);
                        progress(transferred);
                    }
                    output.flush()?;
                    Ok(())
                })()
            }
            Transport::Ssh { alias } => {
                match self.bridges.download_file(
                    target,
                    remote_path.into(),
                    &temporary,
                    &mut progress,
                ) {
                    Ok(()) => Ok(()),
                    Err(error) if self.bridges.is_connected(&target.id) => Err(error),
                    Err(_) => (|| -> Result<()> {
                        let check = format!("test -f {}", shell_quote(remote_path));
                        let output = self.run_shell(target, &check, false)?;
                        ensure_success(&output, "validate remote download")?;
                        self.scp_from(alias, remote_path, &temporary)?;
                        if let Ok(metadata) = fs::metadata(&temporary) {
                            progress(metadata.len());
                        }
                        Ok(())
                    })(),
                }
            }
        };
        if let Err(error) = transfer_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| {
                format!(
                    "failed to finalize download {} -> {}",
                    temporary.display(),
                    destination.display()
                )
            });
        }
        Ok(destination)
    }

    pub fn upload_files(
        &self,
        target: &Target,
        local_paths: &[PathBuf],
        remote_directory: &str,
    ) -> Result<Vec<String>> {
        self.upload_files_with_progress(target, local_paths, remote_directory, |_, _, _| {})
    }

    /// Upload while reporting `(name, transferred, size)` as each file goes
    /// across. Only the daemon's own stream can say how far along it is; an scp
    /// or a local copy reports the file once it has landed, so a drop of many
    /// files still moves rather than sits there.
    pub fn upload_files_with_progress(
        &self,
        target: &Target,
        local_paths: &[PathBuf],
        remote_directory: &str,
        mut progress: impl FnMut(&str, u64, u64),
    ) -> Result<Vec<String>> {
        if local_paths.is_empty() {
            bail!("no local files were provided");
        }
        // An upload never replaces what is already there: a dropped file that
        // collides with an existing name lands beside it as "name (1).ext", the
        // same way a download does locally.
        let mut taken: HashSet<String> = HashSet::new();
        let daemon_upload = if matches!(target.transport, Transport::Ssh { .. }) {
            match self.bridges.list_files(target, remote_directory.into()) {
                Ok(listing) => {
                    taken.extend(listing.entries.into_iter().map(|entry| entry.name));
                    true
                }
                Err(error) if self.bridges.is_connected(&target.id) => return Err(error),
                Err(_) => {
                    let check = format!("test -d {}", shell_quote(remote_directory));
                    let output = self.run_shell(target, &check, false)?;
                    ensure_success(&output, "validate upload directory")?;
                    if let Ok(listing) = self.list_files(target, remote_directory) {
                        taken.extend(listing.entries.into_iter().map(|entry| entry.name));
                    }
                    false
                }
            }
        } else {
            let directory = Path::new(remote_directory);
            if !directory.is_dir() {
                bail!("upload directory does not exist: {remote_directory}");
            }
            if let Ok(entries) = fs::read_dir(directory) {
                taken.extend(
                    entries
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned()),
                );
            }
            false
        };
        let mut uploaded = Vec::new();
        for local_path in local_paths {
            if !local_path.is_file() {
                bail!(
                    "upload source is not a regular file: {}",
                    local_path.display()
                );
            }
            let name = local_path
                .file_name()
                .filter(|name| !name.is_empty())
                .context("upload source has no filename")?
                .to_string_lossy()
                .into_owned();
            // Dropping a file back onto the directory it already lives in is a
            // no-op, not a reason to make a second copy of it.
            let in_place = matches!(target.transport, Transport::Local)
                && fs::canonicalize(local_path).ok()
                    == fs::canonicalize(remote_child_path(remote_directory, &name)).ok();
            let stored = if in_place {
                name.clone()
            } else {
                unique_upload_name(&mut taken, &name)
            };
            let destination = remote_child_path(remote_directory, &stored);
            let size = local_path.metadata().map(|data| data.len()).unwrap_or(0);
            match &target.transport {
                Transport::Local => {
                    let source = fs::canonicalize(local_path).with_context(|| {
                        format!("failed to resolve upload source {}", local_path.display())
                    })?;
                    let destination_path = PathBuf::from(&destination);
                    if !in_place {
                        fs::copy(&source, &destination_path).with_context(|| {
                            format!("failed to upload to {}", destination_path.display())
                        })?;
                    }
                    progress(&stored, size, size);
                }
                Transport::Ssh { .. } if daemon_upload => {
                    self.bridges.upload_file(
                        target,
                        local_path,
                        destination,
                        |transferred, size| progress(&stored, transferred, size),
                    )?;
                }
                Transport::Ssh { alias } => {
                    self.scp_to(alias, local_path, &destination)?;
                    progress(&stored, size, size);
                }
            }
            uploaded.push(stored);
        }
        Ok(uploaded)
    }

    /// Resolve a `$HOME`-relative path against the target's own home directory.
    pub fn home_relative_path(&self, target: &Target, relative: &str) -> Result<String> {
        let home = match target.transport {
            Transport::Local => std::env::var("HOME")
                .context("HOME is unavailable while resolving a target path")?,
            Transport::Ssh { .. } => self.remote_home(target)?,
        };
        Ok(format!(
            "{}/{}",
            home.trim_end_matches('/'),
            relative.trim_start_matches('/')
        ))
    }

    /// Copy a local file onto a target at one exact absolute path, creating the
    /// parent directory first and keeping whatever was already there as a dated
    /// sibling. Unlike [`Runtime::upload_files`] the destination name is chosen
    /// by the caller, which is what restoring an agent transcript needs: the
    /// agent only finds its own history under the name it wrote it as.
    pub fn place_file(&self, target: &Target, local_path: &Path, remote_path: &str) -> Result<()> {
        let parent = Path::new(remote_path)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .context("destination path has no parent directory")?
            .to_path_buf();
        match &target.transport {
            Transport::Local => {
                // No shell for a machine we are already on: the same three steps
                // as the remote script, done directly.
                let destination = PathBuf::from(remote_path);
                fs::create_dir_all(&parent).with_context(|| {
                    format!("failed to create {} for a restore", parent.display())
                })?;
                if fs::canonicalize(&destination).ok().as_deref()
                    == fs::canonicalize(local_path).ok().as_deref()
                {
                    return Ok(());
                }
                if destination.is_file() {
                    let kept = format!("{remote_path}.muxloom-replaced-{}", unix_seconds());
                    fs::copy(&destination, &kept)
                        .with_context(|| format!("failed to keep the file already at {kept}"))?;
                }
                fs::copy(local_path, &destination)
                    .with_context(|| format!("failed to copy into {}", destination.display()))?;
            }
            Transport::Ssh { alias } => {
                let prepare = format!(
                    "mkdir -p {} && if [ -f {} ]; then cp -p {} {}.muxloom-replaced-$(date +%Y%m%d-%H%M%S); fi",
                    shell_quote(&parent.to_string_lossy()),
                    shell_quote(remote_path),
                    shell_quote(remote_path),
                    shell_quote(remote_path),
                );
                let output = self.run_shell(target, &prepare, false)?;
                ensure_success(&output, "prepare destination directory")?;
                // muxloomd writes through a temp file in the destination
                // directory and renames, so a half-transferred transcript never
                // becomes visible to the agent. scp is the fallback when the
                // daemon is not reachable.
                if let Err(error) =
                    self.bridges
                        .upload_file(target, local_path, remote_path.to_string(), |_, _| {})
                {
                    debug::log(
                        "runtime",
                        format!(
                            "place_file target={} muxloomd upload failed: {error:#}; using scp",
                            target.id
                        ),
                    );
                    self.scp_to(alias, local_path, remote_path)?;
                }
            }
        }
        Ok(())
    }

    pub fn scan_resumes(
        &self,
        target: &Target,
        kind: AgentKind,
        path: &str,
    ) -> Result<Vec<ResumeCandidate>> {
        if !kind.has_native_history() {
            return Ok(Vec::new());
        }
        if kind == AgentKind::OpenCode {
            return self.scan_opencode_resumes(target, path);
        }
        let root = match kind {
            AgentKind::Codex => "$HOME/.codex/sessions",
            AgentKind::Claude => "$HOME/.claude/projects",
            AgentKind::Pi => "$HOME/.pi/agent/sessions",
            _ => unreachable!("only a runtime that writes transcripts is scanned for files"),
        };
        let index = if kind == AgentKind::Codex {
            r#"printf '\036INDEX\n'; if [ -f "$HOME/.codex/session_index.jsonl" ]; then cat "$HOME/.codex/session_index.jsonl"; fi;"#
        } else {
            ""
        };
        let collect = r#"query=$1; shift; for file do if grep -F -q -- "$query" "$file"; then printf '\036SESSION\n%s\n' "$file"; sed -n '1,60p' "$file"; tail -n 80 "$file"; fi; done"#;
        let find_args = shell_join(&[
            "-type", "f", "-name", "*.jsonl", "-exec", "sh", "-c", collect, "sh", path, "{}", "+",
        ]);
        let find = format!("find \"{root}\" {find_args}");
        let script = format!("{index} if [ -d \"{root}\" ]; then {find}; fi");
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "scan resumable sessions")?;
        Ok(parse_resume_candidates(
            kind,
            path,
            &String::from_utf8_lossy(&output.stdout),
        ))
    }

    /// The conversations OpenCode has had in `path`, asked for rather than
    /// found.
    ///
    /// Nothing OpenCode writes is a file muxloom may read: its sessions and its
    /// provider credentials are rows in one store. So this asks OpenCode about
    /// itself, in a single query - see
    /// [`crate::native_history::opencode_query`] - and the answer is the
    /// conversations and nothing else. A machine with no OpenCode on it answers
    /// nothing, which is the same as having no conversations to resume.
    fn scan_opencode_resumes(&self, target: &Target, path: &str) -> Result<Vec<ResumeCandidate>> {
        let query = crate::native_history::opencode_query(crate::native_history::OPENCODE_SCANNED);
        let script = format!(
            "PATH=\"$HOME/.local/bin:$PATH\"; command -v opencode >/dev/null 2>&1 || exit 0; {} </dev/null 2>/dev/null",
            shell_join(&["opencode", "db", "--format", "json", &query])
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "scan resumable sessions")?;
        let normalized_path = normalize_path(path);
        let mut candidates: Vec<ResumeCandidate> =
            crate::native_history::opencode_rows(&String::from_utf8_lossy(&output.stdout))
                .iter()
                .filter_map(|row| parse_opencode_resume(row, &normalized_path))
                .collect();
        candidates.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        candidates.truncate(50);
        Ok(candidates)
    }

    /// One OpenCode session as a document, asked for by id.
    ///
    /// The other three runtimes keep a transcript file and a backup is that
    /// file copied. OpenCode keeps rows in one store shared by every folder on
    /// the machine, and that same file holds the provider tokens it signs in
    /// with, so copying it is out of the question — what comes back here is
    /// what `opencode export` says about one session and nothing else.
    ///
    /// `Ok(None)` when the machine has no `opencode` on it or the session is
    /// gone: a backup run sweeps every session it knows of, and a machine that
    /// has since dropped the CLI is not an error worth failing the sweep over.
    pub fn export_opencode_session(&self, target: &Target, id: &str) -> Result<Option<Vec<u8>>> {
        let script = format!(
            "PATH=\"$HOME/.local/bin:$PATH\"; command -v opencode >/dev/null 2>&1 || exit 0; {} \
             </dev/null 2>/dev/null",
            shell_join(&["opencode", "export", id])
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "export an opencode session")?;
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        Ok(Some(output.stdout))
    }

    /// Put an exported document back into OpenCode's store on a machine.
    ///
    /// `opencode import` keeps the session's own id, which is what makes the
    /// restored session resumable by the id the backup recorded, and it files
    /// the session under the folder it is run from — hence the `cd`, because a
    /// session restored into the wrong folder is a session the agent working
    /// there will never be offered.
    ///
    /// The document goes through a staging file under the cache directory
    /// rather than down a pipe, because [`Runtime::place_file`] is the one
    /// tested way muxloom gets bytes onto a machine. It is removed again
    /// whether or not the import worked. Returns where OpenCode says its store
    /// is, for the record of what was written.
    pub fn import_opencode_session(
        &self,
        target: &Target,
        cwd: &str,
        id: &str,
        document: &Path,
    ) -> Result<String> {
        let safe: String = id
            .chars()
            .map(|char| {
                if char.is_ascii_alphanumeric() || matches!(char, '.' | '_' | '-') {
                    char
                } else {
                    '-'
                }
            })
            .collect();
        let staged =
            self.home_relative_path(target, &format!(".cache/muxloom/opencode-{safe}.json"))?;
        self.place_file(target, document, &staged)?;
        // The import runs from the folder the session belongs to, and the
        // staging file is cleaned up before the exit status is honoured so a
        // failed import does not leave someone's conversation lying around.
        let script = format!(
            "PATH=\"$HOME/.local/bin:$PATH\"; command -v opencode >/dev/null 2>&1 || {{ echo \
             'opencode is not installed on this machine, so there is nowhere to put the session \
             back' >&2; exit 127; }}; mkdir -p {cwd} && cd {cwd} && {import} </dev/null \
             >/dev/null 2>&1; status=$?; rm -f {staged}; [ $status -eq 0 ] || exit $status; \
             opencode db path </dev/null 2>/dev/null || true",
            cwd = shell_quote(cwd),
            import = shell_join(&["opencode", "import", &staged]),
            staged = shell_quote(&staged),
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "import an opencode session")?;
        let store = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .rfind(|line| line.starts_with('/'))
            .map(str::to_string);
        match store {
            Some(store) => Ok(store),
            // Older builds may not answer `db path`. The default location is
            // still the truth for all but a machine with XDG_DATA_HOME moved,
            // and this string is a record of what happened, not an address
            // anything reads back.
            None => self.home_relative_path(target, ".local/share/opencode/opencode.db"),
        }
    }

    pub fn kill(&self, target: &Target, session_id: &str) -> Result<()> {
        debug::log(
            "runtime",
            format!("kill target={} session={session_id}", target.id),
        );
        validate_session_id(session_id)?;
        if session_id.starts_with(DAEMON_SESSION_PREFIX) {
            return self.bridges.delete(target, session_id.into());
        }
        let script = shell_join(&["tmux", "kill-session", "-t", session_id]);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "delete agent session")
    }

    /// Type bytes into a session without attaching a PTY stream, which would
    /// resize the session under any attached terminal.
    pub fn send_input(&self, target: &Target, session_id: &str, bytes: &[u8]) -> Result<()> {
        debug::log(
            "runtime",
            format!(
                "send_input target={} session={session_id} bytes={}",
                target.id,
                bytes.len()
            ),
        );
        validate_session_id(session_id)?;
        if bytes.is_empty() {
            return Ok(());
        }
        if session_id.starts_with(DAEMON_SESSION_PREFIX) {
            return self
                .bridges
                .send_input(target, session_id.into(), bytes.to_vec());
        }
        let hex: Vec<String> = bytes.iter().map(|byte| format!("0x{byte:02x}")).collect();
        let mut command = vec!["tmux", "send-keys", "-t", session_id, "-H"];
        command.extend(hex.iter().map(String::as_str));
        let script = shell_join(&command);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "send input to agent session")
    }

    pub fn archive(&self, target: &Target, session_id: &str) -> Result<()> {
        debug::log(
            "runtime",
            format!("archive target={} session={session_id}", target.id),
        );
        validate_session_id(session_id)?;
        if session_id.starts_with(DAEMON_SESSION_PREFIX) {
            return self.bridges.archive(target, session_id.into());
        }
        let script = format!(
            "{} && {}",
            shell_join(&[
                "tmux",
                "set-option",
                "-w",
                "-t",
                session_id,
                "remain-on-exit",
                "on",
            ]),
            shell_join(&["tmux", "respawn-pane", "-k", "-t", session_id, "exit 0",])
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "archive agent session")
    }

    pub fn attach(&self, target: &Target, session_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        let status = match &target.transport {
            Transport::Local => Command::new("tmux")
                .args(["attach-session", "-t", session_id])
                .status()
                .context("failed to run tmux")?,
            Transport::Ssh { alias } => {
                let control_option = format!("ControlPath={}", ssh_control_path());
                Command::new("ssh")
                    .args([
                        "-t",
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        "ControlMaster=auto",
                        "-o",
                        SSH_CONTROL_PERSIST_OPTION,
                        "-o",
                        SSH_SERVER_ALIVE_INTERVAL_OPTION,
                        "-o",
                        SSH_SERVER_ALIVE_COUNT_OPTION,
                        "-o",
                        SSH_CONNECTION_ATTEMPTS_OPTION,
                        "-o",
                        &control_option,
                        alias,
                        "tmux",
                        "attach-session",
                        "-t",
                        session_id,
                    ])
                    .status()
                    .with_context(|| format!("failed to run ssh for {alias}"))?
            }
        };
        if status.success() {
            Ok(())
        } else {
            bail!("attach exited with {status}")
        }
    }

    pub(crate) fn run_shell(
        &self,
        target: &Target,
        script: &str,
        interactive: bool,
    ) -> Result<Output> {
        if let Transport::Ssh { alias } = &target.transport
            && !interactive
            && !self.bridge_recently_failed(&target.id)
        {
            match self.bridges.run_shell(&target.id, alias, script, &[]) {
                Ok(output) => return Ok(output),
                Err(error) => {
                    debug::log(
                        "bridge",
                        format!(
                            "target={} companion unavailable, using legacy ssh temporarily: {error:#}",
                            target.id
                        ),
                    );
                    self.bridge_failures
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(target.id.clone(), Instant::now());
                }
            }
        }
        self.ensure_reverse_tunnel(target)?;
        let mut command = match &target.transport {
            Transport::Local => {
                let mut command = Command::new("sh");
                command.args(["-lc", script]);
                command
            }
            Transport::Ssh { alias } => {
                let mut command = Command::new("ssh");
                let control_path = ssh_control_path();
                let control_option = format!("ControlPath={control_path}");
                command.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    &format!("ConnectTimeout={}", self.ssh_connect_timeout_secs),
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    SSH_CONTROL_PERSIST_OPTION,
                    "-o",
                    SSH_SERVER_ALIVE_INTERVAL_OPTION,
                    "-o",
                    SSH_SERVER_ALIVE_COUNT_OPTION,
                    "-o",
                    SSH_CONNECTION_ATTEMPTS_OPTION,
                    "-o",
                    &control_option,
                    alias,
                    "sh",
                    "-lc",
                    &shell_quote(script),
                ]);
                command
            }
        };
        if !interactive {
            command.stdin(Stdio::null());
        }
        command
            .output()
            .with_context(|| format!("failed to execute command on {}", target.id))
    }

    /// Best-effort `uname -s` / `uname -m` for a target's OS/arch, recorded once
    /// in the backup machine registry. Returns None on any failure.
    #[cfg(feature = "controller")]
    pub(crate) fn probe_platform(&self, target: &Target) -> Option<(String, String)> {
        let output = self.run_shell(target, "uname -s; uname -m", false).ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let os = lines.next()?.trim().to_string();
        let arch = lines.next().unwrap_or("").trim().to_string();
        (!os.is_empty()).then_some((os, arch))
    }

    fn bridge_recently_failed(&self, target_id: &str) -> bool {
        self.bridge_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(target_id)
            .is_some_and(|failed| failed.elapsed() < Duration::from_secs(30))
    }

    fn mark_bridge_failed(&self, target_id: &str) {
        self.bridge_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target_id.to_string(), Instant::now());
    }

    fn ensure_reverse_tunnel(&self, target: &Target) -> Result<()> {
        let Transport::Ssh { alias } = &target.transport else {
            return Ok(());
        };
        let tunnel = self
            .host_reverse_tunnels
            .get(&target.id)
            .map(String::as_str)
            .unwrap_or(&self.reverse_tunnel)
            .trim();
        if tunnel.is_empty() {
            return Ok(());
        }
        let cache_key = format!("{}\0{tunnel}", target.id);
        if self
            .tunnel_checks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&cache_key)
            .is_some_and(|checked| checked.elapsed() < Duration::from_secs(5))
        {
            return Ok(());
        }
        let _start_guard = TUNNEL_START_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self
            .tunnel_checks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&cache_key)
            .is_some_and(|checked| checked.elapsed() < Duration::from_secs(5))
        {
            return Ok(());
        }
        let control_path = tunnel_control_path(tunnel);
        let check = Command::new("ssh")
            .args(["-S", &control_path, "-O", "check", alias])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if check.is_ok_and(|status| status.success()) {
            self.tunnel_checks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(cache_key, Instant::now());
            return Ok(());
        }
        debug::log(
            "tunnel",
            format!(
                "opening reverse tunnel target={} spec={tunnel} control={control_path}",
                target.id
            ),
        );
        let output = Command::new("ssh")
            .args([
                "-fN",
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("ConnectTimeout={}", self.ssh_connect_timeout_secs),
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath={control_path}"),
                "-R",
                tunnel,
                alias,
            ])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to start reverse tunnel for {}", target.id))?;
        ensure_success(&output, &format!("open reverse tunnel for {}", target.id))?;
        self.tunnel_checks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(cache_key, Instant::now());
        Ok(())
    }
}

pub fn ssh_control_path() -> String {
    format!("/tmp/muxloom-{}-%C", std::process::id())
}

fn tunnel_control_path(tunnel: &str) -> String {
    let digest = Sha256::digest(tunnel.as_bytes());
    let short = hex_digest(&digest[..6]);
    format!("/tmp/muxloom-tunnel-{short}-%C")
}

/// The script a target runs to fetch its own agent runtime. Everything the
/// controller resolved is baked in, so the target only has to reach the
/// release, hash what it got, and move it into place — and it lands in exactly
/// the directories an upload from here would have used.
///
/// The transfer is bounded twice over: eight seconds to connect, and an abort
/// if it drops under 4 KiB/s for twenty. A machine with no route to the
/// release fails in seconds rather than holding the install open until the
/// bridge request times out.
fn remote_pull_script(kind: AgentKind, release: &RemoteRelease, exports: &str) -> String {
    let prelude = format!(
        r#"{exports}
set -e
url={url}
sum={sum}
version={version}
cache="$HOME/.cache/muxloom/install"
mkdir -p "$cache" "$HOME/.local/bin"
payload="$cache/{name}.pull.$$"
rm -f "$payload"
fetch() {{
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --connect-timeout 8 --max-time 900 --speed-limit 4096 --speed-time 20 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --timeout=20 --tries=1 -O "$2" "$1"
    else
        printf 'this machine has neither curl nor wget\n' >&2
        return 69
    fi
}}
digest() {{
    if command -v {sum_tool} >/dev/null 2>&1; then
        {sum_tool} "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a {bits} "$1" | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -{name_of_hash} "$1" | awk '{{print $NF}}'
    else
        printf 'this machine has no {name_of_hash} tool\n' >&2
        return 69
    fi
}}
fetch "$url" "$payload"
actual=$(digest "$payload")
if [ "$actual" != "$sum" ]; then
    rm -f "$payload"
    printf 'checksum mismatch: expected %s, got %s\n' "$sum" "$actual" >&2
    exit 65
fi
"#,
        url = shell_quote(&release.url),
        sum = shell_quote(&release.digest),
        version = shell_quote(&release.version),
        name = kind.as_str(),
        sum_tool = format_args!("{}sum", release.algorithm.as_str()),
        bits = release.algorithm.hex_len() * 4,
        name_of_hash = release.algorithm.as_str(),
    );
    format!("{prelude}{}", remote_install_snippet(kind))
}

/// Put a package that is already on the machine into place. The controller
/// uses this after handing a machine a payload it had no route to fetch for
/// itself; everything past the download is the same either way.
fn remote_unpack_script(kind: AgentKind, payload: &str, version: &str) -> String {
    format!(
        r#"set -e
payload={payload}
version={version}
mkdir -p "$HOME/.local/bin"
{install}"#,
        payload = shell_quote(payload),
        version = shell_quote(version),
        install = remote_install_snippet(kind),
    )
}

/// What a machine does with a verified payload sitting at `$payload`, for the
/// release named by `$version`. Written once so a runtime lands in the same
/// directories whether the machine fetched it or the controller shipped it.
fn remote_install_snippet(kind: AgentKind) -> String {
    match kind {
        // Codex and Pi both ship a directory: the executable alone is not the
        // runtime, so a whole release is unpacked and linked to.
        AgentKind::Codex | AgentKind::Pi => {
            let name = kind.as_str();
            let inner = bundle_executable(kind);
            format!(
                r#"releases="$HOME/.local/share/muxloom/{name}/releases"
stage="$releases/.pull.$$"
rm -rf "$stage"
mkdir -p "$stage"
if ! tar -xzf "$payload" -C "$stage"; then
    rm -rf "$stage" "$payload"
    printf 'could not unpack the {name} package\n' >&2
    exit 65
fi
if [ ! -f "$stage/{inner}" ]; then
    rm -rf "$stage" "$payload"
    printf 'the {name} package did not contain {inner}\n' >&2
    exit 65
fi
chmod 755 "$stage/{inner}"
rm -rf "$releases/$version"
mv "$stage" "$releases/$version"
ln -sfn "$releases/$version/{inner}" "$HOME/.local/bin/{name}"
rm -f "$payload"
"#
            )
        }
        // OpenCode arrives as an npm package wrapped around one binary.
        AgentKind::OpenCode => {
            let inner = bundle_executable(kind);
            format!(
                r#"stage="$HOME/.cache/muxloom/install/opencode.unpack.$$"
rm -rf "$stage"
mkdir -p "$stage"
if ! tar -xzf "$payload" -C "$stage"; then
    rm -rf "$stage" "$payload"
    printf 'could not unpack the opencode package\n' >&2
    exit 65
fi
if [ ! -f "$stage/{inner}" ]; then
    rm -rf "$stage" "$payload"
    printf 'the opencode package did not contain {inner}\n' >&2
    exit 65
fi
chmod 755 "$stage/{inner}"
mv -f "$stage/{inner}" "$HOME/.local/bin/opencode"
rm -rf "$stage" "$payload"
"#
            )
        }
        _ => r#"chmod 755 "$payload"
mv -f "$payload" "$HOME/.local/bin/claude"
"#
        .to_string(),
    }
}

/// Where a runtime's own executable sits inside the package its publisher
/// ships. Empty for a runtime that publishes the bare executable.
fn bundle_executable(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Codex => "bin/codex",
        AgentKind::Pi => "pi/pi",
        AgentKind::OpenCode => "package/bin/opencode",
        _ => "",
    }
}

fn controller_download_cache() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/muxloom/downloads")
}

#[cfg(feature = "controller")]
fn local_install_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable while installing the local agent runtime")
}

/// Unpack a downloaded package beside its cache entry and hand back the one
/// executable inside it, for the runtimes where that file is the whole runtime
/// and is all a machine needs to be sent.
#[cfg(feature = "controller")]
fn extract_cached_bundle_executable(archive: &Path, kind: AgentKind) -> Result<PathBuf> {
    let inner = bundle_executable(kind);
    if inner.is_empty() {
        bail!("{kind} publishes no package to unpack");
    }
    let parent = archive
        .parent()
        .with_context(|| format!("{kind} package cache path has no parent"))?;
    let extracted = parent.join("extracted");
    let executable = extracted.join(inner);
    if executable.is_file() {
        return Ok(executable);
    }
    let staging = parent.join(format!(".extracted.partial-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = (|| -> Result<PathBuf> {
        let file =
            File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut package = tar::Archive::new(decoder);
        package
            .unpack(&staging)
            .with_context(|| format!("failed to unpack {}", archive.display()))?;
        let staged_executable = staging.join(inner);
        if !staged_executable.is_file() {
            bail!("{kind} package did not contain {inner}");
        }
        set_executable(&staged_executable)?;
        if extracted.exists() {
            fs::remove_dir_all(&extracted)?;
        }
        fs::rename(&staging, &extracted)?;
        Ok(executable)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

#[cfg(not(feature = "controller"))]
fn extract_cached_bundle_executable(_archive: &Path, _kind: AgentKind) -> Result<PathBuf> {
    bail!("agent package extraction requires the controller feature")
}

#[cfg(feature = "controller")]
fn install_local_runtime_binary(source: &Path, executable: &str) -> Result<()> {
    install_local_runtime_binary_at(source, executable, &local_install_home()?)
}

#[cfg(not(feature = "controller"))]
fn install_local_runtime_binary(_source: &Path, _executable: &str) -> Result<()> {
    bail!("local agent installation requires the controller feature")
}

#[cfg(feature = "controller")]
fn install_local_runtime_binary_at(source: &Path, executable: &str, home: &Path) -> Result<()> {
    let bin_dir = home.join(".local/bin");
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let destination = bin_dir.join(executable);
    let staging = bin_dir.join(format!(".{executable}.partial-{}", std::process::id()));
    let _ = fs::remove_file(&staging);
    fs::copy(source, &staging).with_context(|| {
        format!(
            "failed to stage {} as {}",
            source.display(),
            staging.display()
        )
    })?;
    set_executable(&staging)?;
    fs::rename(&staging, &destination)
        .with_context(|| format!("failed to activate local runtime {}", destination.display()))?;
    Ok(())
}

#[cfg(feature = "controller")]
fn install_local_bundle(archive: &Path, kind: AgentKind, version: &str) -> Result<()> {
    install_local_bundle_at(archive, kind, version, &local_install_home()?)
}

#[cfg(not(feature = "controller"))]
fn install_local_bundle(_archive: &Path, _kind: AgentKind, _version: &str) -> Result<()> {
    bail!("local agent installation requires the controller feature")
}

/// Unpack a runtime that ships as a directory into a release of its own and
/// point this machine's `~/.local/bin` at it. Keeping each version whole is
/// what lets the executable find the files its publisher put beside it.
#[cfg(all(feature = "controller", unix))]
fn install_local_bundle_at(
    archive: &Path,
    kind: AgentKind,
    version: &str,
    home: &Path,
) -> Result<()> {
    let name = kind.as_str();
    let inner = bundle_executable(kind);
    if inner.is_empty() {
        bail!("{kind} publishes no package to unpack");
    }
    let releases = home.join(format!(".local/share/muxloom/{name}/releases"));
    let release = releases.join(version);
    let staging = releases.join(format!(".{version}.partial-{}", std::process::id()));
    let bin_dir = home.join(".local/bin");
    fs::create_dir_all(&releases)?;
    fs::create_dir_all(&bin_dir)?;
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    let result = (|| -> Result<()> {
        let file =
            File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut package = tar::Archive::new(decoder);
        package
            .unpack(&staging)
            .with_context(|| format!("failed to unpack {}", archive.display()))?;
        let executable = staging.join(inner);
        if !executable.is_file() {
            bail!("{kind} package did not contain {inner}");
        }
        set_executable(&executable)?;
        if release.exists() {
            fs::remove_dir_all(&release).with_context(|| {
                format!(
                    "failed to replace local {kind} release {}",
                    release.display()
                )
            })?;
        }
        fs::rename(&staging, &release).with_context(|| {
            format!(
                "failed to activate local {kind} release {}",
                release.display()
            )
        })?;
        activate_local_runtime_link(&release.join(inner), &bin_dir.join(name))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

#[cfg(all(feature = "controller", not(unix)))]
fn install_local_bundle_at(
    _archive: &Path,
    _kind: AgentKind,
    _version: &str,
    _home: &Path,
) -> Result<()> {
    bail!("local agent package installation is unsupported on this platform")
}

#[cfg(all(feature = "controller", unix))]
fn activate_local_runtime_link(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let staging = destination.with_extension(format!("partial-{}", std::process::id()));
    let _ = fs::remove_file(&staging);
    symlink(source, &staging).with_context(|| {
        format!(
            "failed to stage local runtime link from {}",
            source.display()
        )
    })?;
    fs::rename(&staging, destination)
        .with_context(|| format!("failed to activate {}", destination.display()))?;
    Ok(())
}

#[cfg(feature = "controller")]
fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    let _ = path;
    Ok(())
}

fn controller_download<F>(
    url: &str,
    destination: &Path,
    environment: &[(String, String)],
    on_progress: F,
) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    #[cfg(feature = "controller")]
    {
        crate::http::download(url, destination, environment, on_progress)
    }
    #[cfg(not(feature = "controller"))]
    {
        let _ = (url, destination, environment, on_progress);
        bail!("agent downloads require the controller feature")
    }
}

fn parse_reverse_tunnel(value: &str) -> Option<(u16, &str, u16)> {
    let mut fields = value.trim().split(':');
    let remote_port = fields.next()?.parse().ok()?;
    let local_host = fields.next()?;
    let local_port = fields.next()?.parse().ok()?;
    (fields.next().is_none() && remote_port > 0 && local_port > 0 && !local_host.is_empty())
        .then_some((remote_port, local_host, local_port))
}

fn validate_release_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("release server returned an invalid version name");
    }
    Ok(value.to_string())
}

fn validate_digest(value: &str, algorithm: DigestAlgorithm) -> Result<()> {
    if value.len() == algorithm.hex_len() && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!(
            "release manifest returned an invalid {} digest",
            algorithm.as_str()
        )
    }
}

fn checksum_for_asset(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let checksum = fields.next()?;
        let filename = fields.next()?.trim_start_matches('*');
        (filename == asset && validate_digest(checksum, DigestAlgorithm::Sha256).is_ok())
            .then(|| checksum.to_ascii_lowercase())
    })
}

/// What a registry package manifest says to download, and what it must hash
/// to. A manifest says *what* to fetch, not *where from*: only the registry
/// the manifest was itself read from may serve the payload, so a tarball URL
/// pointing anywhere else is refused rather than followed.
fn registry_distribution(manifest: &Value) -> Result<(String, String, DigestAlgorithm)> {
    let distribution = manifest
        .get("dist")
        .context("the package manifest carries no distribution")?;
    let url = distribution
        .get("tarball")
        .and_then(Value::as_str)
        .context("the package manifest names no tarball")?;
    if !url.starts_with(&format!("{NPM_REGISTRY}/")) {
        bail!("the package manifest points somewhere other than the registry");
    }
    let integrity = distribution
        .get("integrity")
        .and_then(Value::as_str)
        .context("the package manifest carries no integrity digest")?;
    let (digest, algorithm) = digest_from_integrity(integrity)?;
    Ok((url.to_string(), digest, algorithm))
}

/// Turn an npm `integrity` string into the plain hex digest the rest of the
/// install path speaks. The registry writes it as `<algorithm>-<base64>`,
/// which is the same number in another alphabet — and it, not us, picks which
/// algorithm that is, so the caller has to be told.
fn digest_from_integrity(integrity: &str) -> Result<(String, DigestAlgorithm)> {
    let (name, encoded) = integrity
        .trim()
        .split_once('-')
        .context("package manifest carries no integrity digest")?;
    let algorithm = match name {
        "sha512" => DigestAlgorithm::Sha512,
        "sha256" => DigestAlgorithm::Sha256,
        other => bail!("package manifest uses an unsupported {other} integrity digest"),
    };
    let digest = hex_digest(&base64_decode(encoded)?);
    validate_digest(&digest, algorithm)?;
    Ok((digest, algorithm))
}

/// Standard base64, padded, no line breaks — the shape npm writes integrity
/// digests in. Anything else is a manifest we should not be trusting.
fn base64_decode(value: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let value = value.trim().as_bytes();
    if value.is_empty() || value.len() % 4 != 0 {
        bail!("package manifest carries a malformed base64 digest");
    }
    let body = value.strip_suffix(b"==").unwrap_or(value);
    let body = body.strip_suffix(b"=").unwrap_or(body);
    let padding = value.len() - body.len();
    if padding > 2 || body.len() % 4 == 1 {
        bail!("package manifest carries a malformed base64 digest");
    }

    let mut output = Vec::with_capacity(body.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    for byte in body {
        let index = ALPHABET
            .iter()
            .position(|candidate| candidate == byte)
            .context("package manifest carries a malformed base64 digest")?;
        accumulator = (accumulator << 6) | index as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
        }
    }
    Ok(output)
}

fn digest_file(path: &Path, algorithm: DigestAlgorithm) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for checksum", path.display()))?;
        if read == 0 {
            break;
        }
        match algorithm {
            DigestAlgorithm::Sha256 => sha256.update(&buffer[..read]),
            DigestAlgorithm::Sha512 => sha512.update(&buffer[..read]),
        }
    }
    Ok(match algorithm {
        DigestAlgorithm::Sha256 => hex_digest(&sha256.finalize()),
        DigestAlgorithm::Sha512 => hex_digest(&sha512.finalize()),
    })
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn parse_history_page(output: &str, offset_from_bottom: usize) -> Result<HistoryPage> {
    let mut lines = output.splitn(2, '\n');
    let info = lines.next().unwrap_or_default();
    let Some(info) = info.strip_prefix("__AD_INFO__") else {
        bail!("tmux returned malformed history metadata");
    };
    let fields: Vec<_> = info.split('\t').collect();
    if fields.len() < 3 {
        bail!("tmux returned incomplete history metadata");
    }
    Ok(HistoryPage {
        text: lines.next().unwrap_or_default().trim_end().to_string(),
        history_size: fields[0].parse().unwrap_or(0),
        pane_height: fields[1].parse().unwrap_or(0),
        pane_width: fields[2].parse().unwrap_or(0),
        offset_from_bottom: fields
            .get(3)
            .and_then(|value| value.parse().ok())
            .unwrap_or(offset_from_bottom),
        // tmux keeps its scrollback as rendered rows and captures it that way,
        // so a pane it hands back is already in the unit rows are counted in.
        rendered: true,
        // `#{history_size}` measures the whole pane, so it already says where
        // the history ends.
        more_history: false,
    })
}

pub(crate) fn attention_reason(
    kind: AgentKind,
    screen: &str,
    patterns: &[String],
) -> Option<String> {
    let screen = attention_tail(screen).to_lowercase();
    if let Some(pattern) = patterns.iter().find(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty() && screen.contains(&pattern.to_lowercase())
    }) {
        return Some(pattern.clone());
    }

    let has_yes = screen.lines().any(|line| choice_line(line, "yes"));
    let has_no = screen.lines().any(|line| choice_line(line, "no"));
    let has_allow = screen.lines().any(|line| choice_line(line, "allow"));
    let has_deny = screen.lines().any(|line| {
        choice_line(line, "deny") || choice_line(line, "reject") || choice_line(line, "cancel")
    });
    let has_choice = (has_yes && has_no)
        || (has_allow && has_deny)
        || (has_yes && (screen.contains("esc to cancel") || screen.contains("enter to confirm")));
    let builtins: &[(&str, &[&str])] = match kind {
        AgentKind::Codex => &[
            (
                "command approval",
                &[
                    "run this command",
                    "run the following command",
                    "allow command",
                    "wants to run",
                ],
            ),
            (
                "file change approval",
                &["apply this patch", "make this change"],
            ),
            (
                "confirmation",
                &["press enter to confirm", "enter to confirm"],
            ),
        ],
        AgentKind::Claude => &[
            (
                "permission request",
                &[
                    "allow this",
                    "allow command",
                    "permission",
                    "trust the files",
                ],
            ),
            (
                "confirmation",
                &[
                    "do you want to proceed",
                    "do you want to make this edit",
                    "esc to cancel",
                ],
            ),
        ],
        // OpenCode and Pi have no wording of their own here yet; the generic
        // choice detection below still catches the menus they open.
        AgentKind::OpenCode | AgentKind::Pi | AgentKind::Terminal => &[],
    };
    for (reason, markers) in builtins {
        if markers.iter().any(|marker| screen.contains(marker)) && has_choice {
            return Some((*reason).into());
        }
    }
    if has_choice
        && [
            "would you like",
            "do you want",
            "choose an option",
            "select an option",
            "permission",
        ]
        .iter()
        .any(|marker| screen.contains(marker))
    {
        return Some("interactive choice".into());
    }
    // A selection menu open at the bottom of the screen is a question being
    // asked even when no option says yes or no — the answers to a
    // model-authored question rarely do. Menus the model merely printed in
    // its reply scroll away above the input line, so only the bottom rows
    // count, and a real menu shows several numbered options, exactly one
    // cursor, and a key hint. The interrupt marker rules out the working
    // phase, whose panels can also draw pointed lists.
    if !screen.contains("esc to interrupt") && bottom_menu_is_open(&screen) {
        return Some("interactive choice".into());
    }
    None
}

fn bottom_menu_is_open(tail: &str) -> bool {
    let lines: Vec<&str> = tail
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let window = &lines[lines.len().saturating_sub(10)..];
    let numbered = window
        .iter()
        .filter(|line| numbered_option_line(line))
        .count();
    let cursors = window.iter().filter(|line| selector_line(line)).count();
    let hint = window.iter().any(|line| {
        let line = line.trim();
        line.chars().count() <= 80 && (line.contains("enter") || line.contains("esc"))
    });
    numbered >= 2 && cursors == 1 && hint
}

/// A numbered option row, with or without the selection cursor.
fn numbered_option_line(line: &str) -> bool {
    let value = line
        .trim_start()
        .trim_start_matches(['❯', '›'])
        .trim_start();
    let digits = value.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && matches!(value.chars().nth(digits), Some('.') | Some(')') | Some(':'))
}

/// A line like `❯ 2. Fix the test` — a selection cursor on a numbered option.
fn selector_line(line: &str) -> bool {
    let value = line.trim_start();
    let Some(rest) = value.strip_prefix('❯').or_else(|| value.strip_prefix('›')) else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && matches!(rest.chars().nth(digits), Some('.') | Some(')') | Some(':'))
}

pub(crate) fn agent_is_working(kind: AgentKind, screen: &str) -> bool {
    if kind == AgentKind::Terminal {
        return false;
    }
    let tail = attention_tail(screen);
    // "esc to interrupt" is the marker both CLIs keep on screen for the whole
    // of an interruptible turn: the early phase before a token count appears,
    // tool runs, and parallel subagent displays included. Anything stricter
    // reads those phases as idle.
    if tail.to_lowercase().contains("esc to interrupt") {
        return true;
    }
    // Not every phase offers an interrupt, though. Claude Code drops the hint
    // while it compacts a conversation — which can run for minutes — and while
    // it waits out a rate limit, and it drops it whenever the footer is too
    // narrow for the hint. The spinner keeps turning throughout, so the status
    // line it heads is the other half of the answer.
    tail.lines().any(spinner_status_line)
}

/// The frames Claude Code cycles at the head of its status line.
const SPINNER_FRAMES: [char; 6] = ['✻', '✽', '✶', '✳', '✢', '·'];

/// A turn's live status line: a spinner frame, the phase, and the counter it
/// ticks — `✶ Compacting conversation… (11m 4s · ↓ 27.7k tokens)`. The phase
/// itself is no help, being anything from `Cogitating` to the title of the task
/// in hand, so the elapsed time is what makes this a line only a running turn
/// draws rather than prose that happens to trail off. The line is drawn hard
/// against the left edge, which is what keeps a transcript quoting one — every
/// transcript row is indented under its marker — from reading as a live turn.
fn spinner_status_line(line: &str) -> bool {
    if !line.starts_with(SPINNER_FRAMES) {
        return false;
    }
    let Some((_, counter)) = line.split_once('…') else {
        return false;
    };
    let Some(counter) = counter.trim_start().strip_prefix('(') else {
        return false;
    };
    let elapsed = counter
        .split(['·', '•', ')'])
        .next()
        .unwrap_or_default()
        .trim();
    elapsed.starts_with(|character: char| character.is_ascii_digit())
        && elapsed.ends_with(['h', 'm', 's'])
        && elapsed
            .chars()
            .all(|character| character.is_ascii_digit() || " hms".contains(character))
}

/// How far up from the bottom of a screen a prompt box is looked for. Both
/// CLIs draw theirs against the footer; anything higher is transcript.
const COMPOSER_TAIL_LINES: usize = 16;
/// Rows Claude Code draws below its prompt box — a hint line, sometimes two.
const CLAUDE_FOOTER_LINES: usize = 3;
/// Rows its box may hold before what is between two rules stops being a prompt
/// and starts being transcript that happens to sit between two separators.
const CLAUDE_BOX_LINES: usize = 10;
/// The shortest run of box-drawing horizontal that reads as a rule rather than
/// as prose that happens to contain one.
const RULE_MIN_LEN: usize = 20;

/// The placeholders Codex greys into an empty composer. Text there is not text
/// anybody typed.
const CODEX_PLACEHOLDERS: [&str; 3] = [
    "Ask Codex to do anything",
    "Send a message",
    "Type a message",
];

/// Read a session's prompt box off its screen.
///
/// [`Composer::Absent`] means the box was looked for and is not there — a
/// dialog is up, the CLI is still starting, something else has the pty. `None`
/// means it was never looked for, because this is a runtime whose box muxloom
/// has not learned; callers read that as "no reason to hold anything back",
/// which is what muxloom did for every runtime before it could read one.
///
/// Only the daemon reads a prompt box, and there is no daemon off Unix, so
/// this and everything under it are unreachable there. Allowed rather than
/// `cfg`'d out: it is plain text work, and the tests for it are worth running
/// wherever they can run.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn composer(kind: AgentKind, screen: &str) -> Option<Composer> {
    let mut lines: Vec<&str> = screen.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let tail = &lines[lines.len().saturating_sub(COMPOSER_TAIL_LINES)..];
    match kind {
        AgentKind::Claude => Some(claude_composer(tail)),
        AgentKind::Codex => Some(codex_composer(tail)),
        _ => None,
    }
}

/// Claude Code fences its prompt between two full-width rules, with its hint
/// line below the lower one:
///
/// ```text
/// ────────────────────────────────────────
/// ❯ half a sentence nobody has sent yet
/// ────────────────────────────────────────
///   ⏵⏵ bypass permissions on · esc to interrupt
/// ```
fn claude_composer(tail: &[&str]) -> Composer {
    let Some(bottom) = tail.iter().rposition(|line| is_rule(line)) else {
        return Composer::Absent;
    };
    // A rule with a screenful under it is a separator in the transcript, not
    // the underside of the prompt.
    if tail.len() - bottom - 1 > CLAUDE_FOOTER_LINES {
        return Composer::Absent;
    }
    let Some(top) = tail[..bottom].iter().rposition(|line| is_rule(line)) else {
        return Composer::Absent;
    };
    let rows = &tail[top + 1..bottom];
    if rows.is_empty() || rows.len() > CLAUDE_BOX_LINES {
        return Composer::Absent;
    }
    let typed: String = rows
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let line = line.trim();
            if index == 0 {
                line.strip_prefix(['❯', '›', '>']).unwrap_or(line).trim()
            } else {
                line
            }
        })
        .collect();
    if typed.is_empty() {
        Composer::Ready
    } else {
        Composer::Occupied
    }
}

/// Codex draws its prompt on a `»` line above its own footer. `›` is not the
/// same thing: it marks a message already taken, and a dialog's choices.
fn codex_composer(tail: &[&str]) -> Composer {
    let Some(row) = tail
        .iter()
        .rposition(|line| line.trim_start().starts_with('»'))
    else {
        return Composer::Absent;
    };
    let typed = tail[row].trim_start().trim_start_matches('»').trim();
    if typed.is_empty()
        || CODEX_PLACEHOLDERS
            .iter()
            .any(|placeholder| typed.starts_with(placeholder))
    {
        Composer::Ready
    } else {
        Composer::Occupied
    }
}

fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= RULE_MIN_LEN && line.chars().all(|character| character == '─')
}

fn attention_tail(screen: &str) -> String {
    let mut lines: Vec<_> = screen.lines().collect();
    // Drop trailing blank rows first. A full-height TUI (e.g. Claude Code in an
    // 86-row pane with a short transcript) draws its status/spinner line well
    // above the empty bottom of the grid; without this the last 24 raw lines are
    // all blank and a working/waiting agent is misread as idle.
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines[lines.len().saturating_sub(24)..].join("\n")
}

fn attention_debug_tail(screen: &str) -> String {
    let lines: Vec<_> = screen.lines().collect();
    lines[lines.len().saturating_sub(10)..]
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
        .chars()
        .take(600)
        .collect()
}

fn choice_line(line: &str, label: &str) -> bool {
    if line.chars().count() > 120 {
        return false;
    }
    let value = line.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '›' | '❯' | '>' | '•' | '*' | '-')
    });
    let value = value.trim_start_matches(|character: char| {
        character.is_whitespace()
            || character.is_ascii_digit()
            || matches!(character, '.' | ')' | '(' | '[' | ']')
    });
    value.strip_prefix(label).is_some_and(|rest| {
        rest.chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric())
    })
}

fn parse_history_matches(output: &str) -> Vec<HistoryMatch> {
    output
        .lines()
        .filter_map(|line| {
            let (recap, rest) = if let Some(rest) = line.strip_prefix("__AD_RECAP__") {
                (true, rest)
            } else {
                (false, line.strip_prefix("__AD_HISTORY__")?)
            };
            let (number, text) = rest.split_once('\t')?;
            Some(HistoryMatch {
                recap,
                line_number: number.parse().ok()?,
                text: sanitize_field(text),
            })
        })
        .collect()
}

fn parse_directory_listing(output: &[u8]) -> Result<DirectoryListing> {
    let Some(newline) = output.iter().position(|byte| *byte == b'\n') else {
        bail!("directory listing did not include its canonical path");
    };
    let path = String::from_utf8_lossy(&output[..newline])
        .trim()
        .to_string();
    let mut directories: Vec<_> = output[newline + 1..]
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).to_string())
        .filter_map(|entry| entry.strip_prefix("./").map(str::to_string))
        .filter(|entry| !entry.is_empty() && !entry.contains('/'))
        .collect();
    directories.sort_by_key(|entry| entry.to_lowercase());
    directories.dedup();
    Ok(DirectoryListing { path, directories })
}

fn parse_file_listing(output: &[u8]) -> Result<FileListing> {
    let mut fields = output.split(|byte| *byte == 0);
    let path = fields
        .next()
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).to_string())
        .context("file listing did not include its canonical path")?;
    let values: Vec<_> = fields.filter(|field| !field.is_empty()).collect();
    if values.len() % 4 != 0 {
        bail!("file listing returned incomplete metadata");
    }
    let mut entries = Vec::new();
    for fields in values.chunks_exact(4) {
        let (kind, symlink) = match fields[0] {
            b"d" => (FileEntryKind::Directory, false),
            b"f" => (FileEntryKind::File, false),
            b"D" => (FileEntryKind::Directory, true),
            b"F" => (FileEntryKind::File, true),
            // "l" is what older targets emit for any link, resolved or not.
            b"O" | b"l" => (FileEntryKind::Other, true),
            _ => (FileEntryKind::Other, false),
        };
        let size = String::from_utf8_lossy(fields[1]).parse().unwrap_or(0);
        let mtime = String::from_utf8_lossy(fields[2]).parse().unwrap_or(0);
        let name = String::from_utf8_lossy(fields[3]).to_string();
        if name.is_empty() || name.contains('/') {
            continue;
        }
        entries.push(FileEntry {
            path: remote_child_path(&path, &name),
            name,
            kind,
            symlink,
            size,
            mtime,
        });
    }
    entries.sort_by(|left, right| {
        (left.kind != FileEntryKind::Directory)
            .cmp(&(right.kind != FileEntryKind::Directory))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(FileListing {
        path,
        entries,
        truncated: false,
    })
}

fn parse_file_preview(output: &[u8]) -> Result<FilePreview> {
    let mut fields = output.splitn(6, |byte| *byte == 0);
    let path = fields
        .next()
        .map(|value| String::from_utf8_lossy(value).to_string())
        .context("file preview did not include a path")?;
    let mime = fields
        .next()
        .map(|value| String::from_utf8_lossy(value).to_string())
        .context("file preview did not include a MIME type")?;
    let kind = match fields.next().map(String::from_utf8_lossy).as_deref() {
        Some("text") => FilePreviewKind::Text,
        Some("markdown") => FilePreviewKind::Markdown,
        Some("image") => FilePreviewKind::Image,
        Some("audio") => FilePreviewKind::Audio,
        Some("video") => FilePreviewKind::Video,
        Some("binary") => FilePreviewKind::Binary,
        _ => bail!("file preview returned an unknown type"),
    };
    let size = fields
        .next()
        .map(String::from_utf8_lossy)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let truncated = fields.next().is_some_and(|value| value == b"1");
    let content = fields
        .next()
        .map(|value| String::from_utf8_lossy(value).to_string())
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect();
    Ok(FilePreview {
        path,
        mime,
        kind,
        size,
        content,
        truncated,
    })
}

fn remote_child_path(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", directory.trim_end_matches('/'))
    }
}

fn relative_search_path(root: &str, path: &str) -> String {
    if root == "/" {
        path.trim_start_matches('/').to_string()
    } else {
        path.strip_prefix(root.trim_end_matches('/'))
            .unwrap_or(path)
            .trim_start_matches('/')
            .to_string()
    }
}

fn filename_matches_pattern(filename: &str, pattern: &str) -> bool {
    let filename = filename.to_lowercase();
    let pattern = pattern.to_lowercase();
    if !pattern.contains('*') {
        return filename.contains(&pattern);
    }

    let text: Vec<_> = filename.chars().collect();
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    let mut previous_was_star = false;
    for token in pattern.chars() {
        if token == '*' && previous_was_star {
            continue;
        }
        let mut current = vec![false; text.len() + 1];
        if token == '*' {
            current[0] = previous[0];
            for index in 1..=text.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=text.len() {
                current[index] = previous[index - 1] && text[index - 1] == token;
            }
        }
        previous = current;
        previous_was_star = token == '*';
    }
    previous[text.len()]
}

/// The name an upload should take in a directory that already holds `taken`.
///
/// Names chosen here are recorded, so a batch of drops that collide with each
/// other still lands as separate files rather than overwriting one another.
fn unique_upload_name(taken: &mut HashSet<String>, name: &str) -> String {
    let chosen = if taken.contains(name) {
        let path = Path::new(name);
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.to_string());
        let extension = path.extension().map(|value| value.to_string_lossy());
        (1..10_000)
            .map(|index| match &extension {
                Some(extension) => format!("{stem} ({index}).{extension}"),
                None => format!("{stem} ({index})"),
            })
            .find(|candidate| !taken.contains(candidate))
            .unwrap_or_else(|| format!("{stem}-{}", std::process::id()))
    } else {
        name.to_string()
    };
    taken.insert(chosen.clone());
    chosen
}

fn unique_destination(directory: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let original = directory.join(name);
    if !original.exists() {
        return original;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .unwrap_or(name)
        .to_string_lossy()
        .to_string();
    let extension = path.extension().map(|value| value.to_string_lossy());
    for index in 1..10_000 {
        let candidate = if let Some(extension) = &extension {
            directory.join(format!("{stem} ({index}).{extension}"))
        } else {
            directory.join(format!("{stem} ({index})"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(format!("{stem}-{}", std::process::id()))
}

fn parse_resume_candidates(kind: AgentKind, path: &str, output: &str) -> Vec<ResumeCandidate> {
    let mut titles = HashMap::new();
    let chunks: Vec<_> = output.split('\u{1e}').collect();
    for chunk in &chunks {
        let Some(index) = chunk.strip_prefix("INDEX\n") else {
            continue;
        };
        for value in index.lines().filter_map(parse_json_line) {
            if let (Some(id), Some(title)) = (
                value.get("id").and_then(Value::as_str),
                value.get("thread_name").and_then(Value::as_str),
            ) {
                titles.insert(id.to_string(), title.to_string());
            }
        }
    }

    let normalized_path = normalize_path(path);
    let mut candidates = HashMap::<String, ResumeCandidate>::new();
    for chunk in chunks {
        let Some(session) = chunk.strip_prefix("SESSION\n") else {
            continue;
        };
        let (source_path, session) = match session.split_once('\n') {
            Some((first, _)) if first.starts_with('{') => ("", session),
            Some(parts) => parts,
            None => ("", session),
        };
        let candidate = match kind {
            AgentKind::Codex => parse_codex_resume(session, &normalized_path, source_path, &titles),
            AgentKind::Claude => parse_claude_resume(session, &normalized_path, source_path),
            AgentKind::Pi => parse_pi_resume(session, &normalized_path, source_path),
            _ => None,
        };
        if let Some(candidate) = candidate {
            candidates
                .entry(candidate.id.clone())
                .and_modify(|existing| {
                    if candidate.updated_at > existing.updated_at {
                        *existing = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
    }
    let mut candidates: Vec<_> = candidates.into_values().collect();
    candidates.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    candidates.truncate(50);
    candidates
}

fn parse_codex_resume(
    session: &str,
    path: &str,
    source_path: &str,
    titles: &HashMap<String, String>,
) -> Option<ResumeCandidate> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = String::new();
    let mut first_message = None;
    let mut last_message = None;
    let mut fallback_first = None;
    let mut fallback_last = None;
    for value in session.lines().filter_map(parse_json_line) {
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = value.get("payload")?;
                if payload
                    .get("source")
                    .and_then(|source| source.get("subagent"))
                    .is_some()
                {
                    return None;
                }
                id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(normalize_path);
                updated_at = payload
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            Some("event_msg") => {
                let payload = value.get("payload");
                if payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("user_message")
                {
                    let message = payload
                        .and_then(|payload| payload.get("message"))
                        .and_then(Value::as_str)
                        .map(clean_recap)
                        .filter(|message| crate::native_history::is_spoken(message));
                    if let Some(message) = message {
                        first_message.get_or_insert_with(|| message.clone());
                        last_message = Some(message);
                    }
                }
            }
            Some("response_item") => {
                let payload = value.get("payload");
                if payload
                    .and_then(|payload| payload.get("role"))
                    .and_then(Value::as_str)
                    == Some("user")
                {
                    let message = payload
                        .and_then(|payload| payload.get("content"))
                        .and_then(extract_message_text)
                        .map(|message| clean_recap(&message))
                        .filter(|message| crate::native_history::is_spoken(message));
                    if let Some(message) = message {
                        fallback_first.get_or_insert_with(|| message.clone());
                        fallback_last = Some(message);
                    }
                }
            }
            _ => {}
        }
    }
    let id = id?;
    if cwd.as_deref() != Some(path) {
        return None;
    }
    let recap = titles
        .get(&id)
        .cloned()
        .map(|title| clean_recap(&title))
        .filter(|title| !title.is_empty());
    Some(ResumeCandidate {
        id,
        kind: AgentKind::Codex,
        source_path: source_path.to_string(),
        recap,
        first_message: first_message.or(fallback_first),
        last_message: last_message.or(fallback_last),
        updated_at,
    })
}

fn parse_claude_resume(session: &str, path: &str, source_path: &str) -> Option<ResumeCandidate> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = String::new();
    let mut first_message = None;
    let mut last_message = None;
    let mut title = None;
    let mut legacy_title = None;
    for value in session.lines().filter_map(parse_json_line) {
        if id.is_none() {
            id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(Value::as_str).map(normalize_path);
        }
        if let Some(timestamp) = value.get("timestamp").and_then(Value::as_str)
            && timestamp > updated_at.as_str()
        {
            updated_at = timestamp.to_string();
        }
        // The name is rewritten as the conversation goes on, so the last one
        // in the file is the one that describes it now.
        if let Some(named) = crate::native_history::claude_ai_title(&value) {
            let named = clean_recap(named);
            if !named.is_empty() {
                title = Some(named);
            }
        }
        if legacy_title.is_none() {
            legacy_title = crate::native_history::claude_legacy_title(&value)
                .map(clean_recap)
                .filter(|title| !title.is_empty());
        }
        if value.get("type").and_then(Value::as_str) == Some("user") {
            let message = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(extract_message_text)
                .map(|message| clean_recap(&message))
                .filter(|message| crate::native_history::is_spoken(message));
            if let Some(message) = message {
                first_message.get_or_insert_with(|| message.clone());
                last_message = Some(message);
            }
        }
    }
    if cwd.as_deref() != Some(path) {
        return None;
    }
    Some(ResumeCandidate {
        id: id?,
        kind: AgentKind::Claude,
        source_path: source_path.to_string(),
        recap: title.or(legacy_title),
        first_message,
        last_message,
        updated_at,
    })
}

fn parse_pi_resume(session: &str, path: &str, source_path: &str) -> Option<ResumeCandidate> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = String::new();
    let mut title = None;
    let mut first_message = None;
    let mut last_message = None;
    for value in session.lines().filter_map(parse_json_line) {
        if let Some(timestamp) = value.get("timestamp").and_then(Value::as_str)
            && timestamp > updated_at.as_str()
        {
            updated_at = timestamp.to_string();
        }
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                id = value.get("id").and_then(Value::as_str).map(str::to_string);
                cwd = value.get("cwd").and_then(Value::as_str).map(normalize_path);
            }
            // The name can be written again whenever it changes, so the last
            // one wins.
            Some("session_info") => {
                if let Some(named) = crate::native_history::pi_session_name(&value) {
                    let named = clean_recap(named);
                    if !named.is_empty() {
                        title = Some(named);
                    }
                }
            }
            Some("message") => {
                // A tool's answer is filed as a message too, under a role of
                // its own; only what the person typed is worth showing here.
                let message = value.get("message");
                if message
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    != Some("user")
                {
                    continue;
                }
                let text = message
                    .and_then(|message| message.get("content"))
                    .and_then(extract_message_text)
                    .map(|text| clean_recap(&text))
                    .filter(|text| crate::native_history::is_spoken(text));
                if let Some(text) = text {
                    first_message.get_or_insert_with(|| text.clone());
                    last_message = Some(text);
                }
            }
            _ => {}
        }
    }
    if cwd.as_deref() != Some(path) {
        return None;
    }
    Some(ResumeCandidate {
        id: id?,
        kind: AgentKind::Pi,
        source_path: source_path.to_string(),
        recap: title,
        first_message,
        last_message,
        updated_at,
    })
}

/// One row of OpenCode's answer as a conversation that could be resumed, or
/// nothing if it was held somewhere else.
///
/// The source path is left empty on purpose. Every other runtime can point at
/// the file it wrote, and a reader - a person, or an agent of another kind
/// being handed this conversation as context - can go and read it. OpenCode's
/// is a row in a store that also holds its credentials, so there is no path
/// here worth giving out, and what needs the conversation asks OpenCode for it
/// by id.
fn parse_opencode_resume(row: &Value, path: &str) -> Option<ResumeCandidate> {
    if row
        .get("directory")
        .and_then(Value::as_str)
        .map(normalize_path)
        .as_deref()
        != Some(path)
    {
        return None;
    }
    let said = |field: &str| {
        row.get(field)
            .and_then(Value::as_str)
            .map(clean_recap)
            .filter(|text| crate::native_history::is_spoken(text))
    };
    Some(ResumeCandidate {
        id: row.get("id").and_then(Value::as_str)?.to_string(),
        kind: AgentKind::OpenCode,
        source_path: String::new(),
        recap: crate::native_history::opencode_title(row).map(clean_recap),
        first_message: said("first_text"),
        last_message: said("last_text"),
        updated_at: crate::native_history::iso_timestamp(
            row.get("updated").and_then(Value::as_u64).unwrap_or(0),
        ),
    })
}

fn parse_json_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

fn extract_message_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let parts = value.as_array()?.iter().filter_map(|part| {
        part.get("text")
            .or_else(|| part.get("content"))
            .and_then(Value::as_str)
    });
    Some(parts.collect::<Vec<_>>().join(" "))
}

fn clean_recap(value: &str) -> String {
    let flattened = value.split_whitespace().collect::<Vec<_>>().join(" ");
    flattened.chars().take(180).collect()
}

fn normalize_path(value: &str) -> String {
    if value == "/" {
        "/".into()
    } else {
        value.trim_end_matches('/').to_string()
    }
}

fn login_shell_command(command: &str) -> String {
    format!("\"${{SHELL:-/bin/sh}}\" -lc {}", shell_quote(command))
}

fn command_line(
    command: &CommandConfig,
    kind: AgentKind,
    temporary: bool,
    resume_id: Option<&str>,
    initial_prompt: Option<&str>,
) -> String {
    let args = launch_arguments(command, kind, temporary, resume_id, initial_prompt);
    let mut values = Vec::with_capacity(args.len() + 1);
    values.push(command.command.as_str());
    values.extend(args.iter().map(String::as_str));
    shell_join(&values)
}

/// A fresh daemon session id and the launch timestamp it embeds. Shared with
/// the control surface so a session launched over MCP is indistinguishable
/// from one launched by the dashboard.
pub(crate) fn new_daemon_session_id(kind: AgentKind, temporary: bool) -> (String, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let session_id = format!(
        "{DAEMON_SESSION_PREFIX}{}{}-{now}-{}-{sequence}",
        if temporary {
            TEMPORARY_SESSION_MARKER
        } else {
            ""
        },
        kind.as_str(),
        std::process::id()
    );
    (session_id, now)
}

pub(crate) fn launch_arguments(
    command: &CommandConfig,
    kind: AgentKind,
    temporary: bool,
    resume_id: Option<&str>,
    initial_prompt: Option<&str>,
) -> Vec<String> {
    let mut args = command.args.clone();
    // Keep this session-local instead of changing the user's global Codex
    // configuration. The flag must precede the `resume` subcommand.
    if kind == AgentKind::Codex
        && !args
            .iter()
            .any(|argument| argument == CODEX_NO_ALT_SCREEN_ARG)
    {
        args.push(CODEX_NO_ALT_SCREEN_ARG.into());
    }
    if kind == AgentKind::Codex
        && temporary
        && !args.iter().any(|argument| {
            argument == CODEX_NO_HISTORY_CONFIG
                || argument.strip_prefix("-c=") == Some(CODEX_NO_HISTORY_CONFIG)
        })
    {
        args.extend(["-c".into(), CODEX_NO_HISTORY_CONFIG.into()]);
    }
    if let Some(resume_id) = resume_id {
        match kind {
            AgentKind::Codex => args.extend(["resume".into(), resume_id.into()]),
            AgentKind::Claude => args.extend(["--resume".into(), resume_id.into()]),
            AgentKind::Pi | AgentKind::OpenCode => {
                args.extend(["--session".into(), resume_id.into()])
            }
            // Only a runtime that remembers its own conversations hands back a
            // resume id.
            AgentKind::Terminal => {}
        }
    }
    if resume_id.is_none()
        && kind != AgentKind::Terminal
        && let Some(prompt) = initial_prompt
    {
        args.push(prompt.into());
    }
    args
}

fn interactive_shell_command(command: &str) -> String {
    format!("exec {}", login_shell_command(command))
}

fn environment_exports(environment: &[(String, String)]) -> String {
    let mut script = "export PATH=\"$HOME/.local/bin:$PATH\";".to_string();
    for (name, value) in environment {
        script.push_str(" export ");
        script.push_str(name);
        script.push('=');
        script.push_str(&shell_quote(value));
        script.push(';');
    }
    script
}

fn find_local_native_executable(command: &str) -> Option<PathBuf> {
    let output = Command::new("sh")
        .args(["-lc", &format!("command -v {}", shell_quote(command))])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()?
            .trim(),
    );
    let path = fs::canonicalize(path).ok()?;
    let magic = fs::read(&path).ok()?;
    let native = magic.starts_with(b"\x7fELF")
        || magic.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || magic.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
        || magic.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
        || magic.starts_with(b"MZ");
    native.then_some(path)
}

/// Whether the copy of this runtime on the controller's own PATH is a thing
/// that would still work somewhere else. Pi's executable is only half of a Pi:
/// its themes, assets and modules sit beside it, so what is on PATH is never
/// worth copying and the package has to be fetched instead.
fn local_runtime_can_copy(kind: AgentKind, binary: &Path) -> bool {
    match kind {
        AgentKind::Pi => false,
        AgentKind::Codex => {
            std::env::consts::OS != "linux" || find_codex_resource(binary, "bwrap").is_some()
        }
        _ => true,
    }
}

fn find_codex_resource(binary: &Path, name: &str) -> Option<PathBuf> {
    for ancestor in binary.parent()?.ancestors().take(7) {
        for relative in [
            PathBuf::from("codex-resources").join(name),
            PathBuf::from("codex-path").join(name),
            PathBuf::from("path").join(name),
        ] {
            let candidate = ancestor.join(relative);
            if candidate.is_file() {
                return fs::canonicalize(candidate).ok();
            }
        }
    }
    None
}

fn expand_home_path(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(value).is_relative() {
        home.join(value)
    } else {
        PathBuf::from(value)
    }
}

fn normalize_arch(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => "unknown",
    }
}

fn daemon_agent_session(target_id: &str, session: DaemonSession) -> Option<AgentSession> {
    let kind = AgentKind::from_str(&session.kind).ok()?;
    Some(AgentSession {
        id: session.id,
        target_id: target_id.into(),
        kind,
        path: session.path,
        label: session.label,
        created_at: session.created_at,
        dead: session.dead || session.archived,
        pid: session.pid,
        working: session.working,
        needs_attention: session.needs_attention,
        attention_reason: session.attention_reason,
        recap: session.recap,
        title: session.title,
        parent: session.parent,
    })
}

/// The installed runtimes as a comma-separated list, for the debug log.
fn describe_runtimes(probe: &Probe) -> String {
    probe
        .runtimes
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_discovery(target_id: &str, output: &str) -> Result<(Probe, Vec<AgentSession>)> {
    let mut probe = Probe::default();
    let mut sessions = Vec::new();
    for line in output.lines() {
        // `<runtime>=0|1` lines answer the executable probe; everything else
        // is a tab-separated pane record.
        if !line.contains('\t')
            && let Some((name, flag)) = line.split_once('=')
            && matches!(flag, "0" | "1")
        {
            let present = flag == "1";
            if name == "tmux" {
                probe.tmux = present;
            } else if let Ok(kind) = AgentKind::from_str(name) {
                probe.set(kind, present);
            }
            continue;
        }
        match line {
            line if is_managed_session_id(line.split('\t').next().unwrap_or_default()) => {
                let fields: Vec<_> = line.split('\t').collect();
                if fields.len() < 11 {
                    continue;
                }
                let metadata = if fields[1].is_empty() {
                    (&fields[5..9], 9, 10)
                } else {
                    (&fields[1..5], 9, 10)
                };
                let Ok(kind) = AgentKind::from_str(metadata.0[0]) else {
                    continue;
                };
                sessions.push(AgentSession {
                    id: sanitize_field(fields[0]),
                    target_id: target_id.into(),
                    kind,
                    path: sanitize_field(metadata.0[1]),
                    label: sanitize_field(metadata.0[2]),
                    created_at: metadata.0[3].parse().unwrap_or(0),
                    dead: fields[metadata.1] == "1",
                    pid: fields[metadata.2].parse().ok(),
                    working: false,
                    needs_attention: false,
                    attention_reason: None,
                    recap: None,
                    // A pane record carries what tmux was told at launch and
                    // nothing the runtime has said since.
                    title: None,
                    parent: None,
                });
            }
            _ => {}
        }
    }
    Ok((probe, sessions))
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn ensure_success(output: &Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(anyhow!(
        "{action} failed ({}): {}",
        output.status,
        if stderr.is_empty() {
            "no error output"
        } else {
            &stderr
        }
    ))
}

/// The newest `wanted` rows of a page that hold something to read.
fn newest_drawn_rows(page: &str, wanted: usize) -> String {
    let rows = drawn_rows(page);
    rows[rows.len().saturating_sub(wanted)..].join("\n")
}

/// The rows of a page up to the last one anything was drawn on. Rows below it
/// carry only the attributes the renderer resets each row with, and standing in
/// for output that a caller asked to see is the one thing they must not do.
fn drawn_rows(page: &str) -> Vec<&str> {
    let mut rows: Vec<&str> = page.lines().collect();
    while rows.last().is_some_and(|row| !row_has_content(row)) {
        rows.pop();
    }
    rows
}

/// Whether a rendered row shows anything, looking past the escape sequences
/// that carry its colours rather than counting them as content.
fn row_has_content(row: &str) -> bool {
    let bytes = row.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => index = escape_end(bytes, index),
            byte if byte < 0x20 || byte == 0x7f || byte == b' ' => index += 1,
            _ => return true,
        }
    }
    false
}

/// Where the escape sequence starting at `start` ends, so a scan can step over
/// it without mistaking its parameters for text.
fn escape_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    match bytes.get(index) {
        Some(b'[') => {
            index += 1;
            while bytes
                .get(index)
                .is_some_and(|byte| (0x20..0x40).contains(byte))
            {
                index += 1;
            }
            index + 1
        }
        // A string sequence runs until BEL or ST rather than a final byte.
        Some(b']' | b'P' | b'X' | b'^' | b'_') => {
            index += 1;
            while index < bytes.len() {
                match bytes[index] {
                    0x07 => return index + 1,
                    0x1b if bytes.get(index + 1) == Some(&b'\\') => return index + 2,
                    _ => index += 1,
                }
            }
            index
        }
        _ => index + 1,
    }
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if is_managed_session_id(session_id) {
        Ok(())
    } else {
        bail!("refusing invalid Muxloom session id")
    }
}

pub(crate) fn is_managed_session_id(session_id: &str) -> bool {
    (session_id.starts_with(SESSION_PREFIX)
        || session_id.starts_with(LEGACY_SESSION_PREFIX)
        || is_daemon_session_id(session_id))
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn is_temporary_session_id(session_id: &str) -> bool {
    session_id.starts_with("muxloomd-temporal-") || session_id.starts_with("muxloom-temporal-")
}

pub fn is_daemon_session_id(session_id: &str) -> bool {
    session_id.starts_with(DAEMON_SESSION_PREFIX)
}

pub fn shell_join(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whole seconds since the epoch, for naming the copy a placement keeps.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_values() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn parses_structured_file_listings_and_previews() {
        let listing = b"/work/project\x00f\x005\x00170\x00z.txt\x00d\x000\x000\x00src\x00f\x0012\x001730000000\x00a.md\x00";
        let listing = parse_file_listing(listing).unwrap();
        assert_eq!(listing.path, "/work/project");
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.entries[0].name, "src");
        assert_eq!(listing.entries[1].name, "a.md");
        assert_eq!(listing.entries[1].size, 12);
        assert_eq!(listing.entries[1].mtime, 1_730_000_000);
        assert_eq!(listing.entries[1].path, "/work/project/a.md");

        let preview = parse_file_preview(
            b"/work/project/a.md\x00text/markdown\x00markdown\x0012\x000\x00# Heading\n- item\n",
        )
        .unwrap();
        assert_eq!(preview.kind, FilePreviewKind::Markdown);
        assert_eq!(preview.mime, "text/markdown");
        assert!(preview.content.contains("Heading"));
        assert!(!preview.truncated);
    }

    #[test]
    fn maps_release_platforms_and_checks_checksum_manifests() {
        let linux = TargetPlatform {
            os: "linux".into(),
            arch: "x86_64".into(),
            musl: false,
        };
        assert_eq!(linux.claude_name().unwrap(), "linux-x64");
        assert_eq!(linux.codex_name().unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(linux.pi_name().unwrap(), "linux-x64");
        assert_eq!(linux.opencode_name().unwrap(), "linux-x64");
        let alpine = TargetPlatform {
            musl: true,
            ..linux
        };
        assert_eq!(alpine.claude_name().unwrap(), "linux-x64-musl");
        // OpenCode publishes a musl build; Pi does not, and saying so is what
        // sends that machine to Pi's own installer instead of a broken binary.
        assert_eq!(alpine.opencode_name().unwrap(), "linux-x64-musl");
        assert!(alpine.pi_name().is_err());
        let mac = TargetPlatform {
            os: "darwin".into(),
            arch: "aarch64".into(),
            musl: false,
        };
        assert_eq!(mac.pi_name().unwrap(), "darwin-arm64");
        assert_eq!(mac.opencode_name().unwrap(), "darwin-arm64");
        let manifest = concat!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  other.tar.gz\n",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  codex-package.tar.gz\n",
        );
        assert_eq!(
            checksum_for_asset(manifest, "codex-package.tar.gz").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn reads_the_digest_a_registry_publishes_for_its_own_tarball() {
        // The registry writes the digest in base64; the target checks it in
        // hex. Same number, and these are the published digests of "muxloom".
        assert_eq!(
            digest_from_integrity(
                "sha512-TNC3gdHJwIJiBN5a8CSFmJ1/aEuMnAs0GdFmGS1vjPqIfO6iqanZZemll91BQ6UZ9m8J/VzGkQjO0A74uQGHbQ=="
            )
            .unwrap(),
            (
                "4cd0b781d1c9c0826204de5af02485989d7f684b8c9c0b3419d166192d6f8cfa\
                 887ceea2a9a9d965e9a597dd4143a519f66f09fd5cc69108ced00ef8b901876d"
                    .to_string(),
                DigestAlgorithm::Sha512,
            )
        );
        // Which algorithm is the registry's call, not ours: the target has to
        // be told to check the same one.
        assert_eq!(
            digest_from_integrity("sha256-n7nOfVn7bhgESGJsuK4hJsE9qrDyKDmiucQ9A7sMgI4=")
                .unwrap()
                .1,
            DigestAlgorithm::Sha256
        );
        for rejected in [
            "",
            "sha512",
            "md5-n7nOfVn7bhgESGJsuK4hJsE9qrDyKDmiucQ9A7sMgI4=",
            // Right alphabet, wrong width: a digest that is not 512 bits long
            "sha512-n7nOfVn7bhgESGJsuK4hJsE9qrDyKDmiucQ9A7sMgI4=",
            // Not the alphabet at all.
            "sha512-****fVn7bhgESGJsuK4hJsE9qrDyKDmiucQ9A7sMgI4=",
        ] {
            assert!(
                digest_from_integrity(rejected).is_err(),
                "accepted {rejected}"
            );
        }

        // A manifest says what to fetch, not where from.
        let manifest = serde_json::json!({
            "dist": {
                "tarball": format!("{NPM_REGISTRY}/opencode-linux-x64/-/opencode-linux-x64-1.0.0.tgz"),
                "integrity": "sha256-n7nOfVn7bhgESGJsuK4hJsE9qrDyKDmiucQ9A7sMgI4=",
            }
        });
        let (url, digest, algorithm) = registry_distribution(&manifest).unwrap();
        assert!(url.starts_with(NPM_REGISTRY));
        assert_eq!(
            digest,
            "9fb9ce7d59fb6e180448626cb8ae2126c13daab0f22839a2b9c43d03bb0c808e"
        );
        assert_eq!(algorithm, DigestAlgorithm::Sha256);
        let elsewhere = serde_json::json!({
            "dist": {
                "tarball": "https://example.invalid/opencode-linux-x64-1.0.0.tgz",
                "integrity": "sha256-n7nOfVn7bhgESGJsuK4hJsE9qrDyKDmiucQ9A7sMgI4=",
            }
        });
        assert!(
            registry_distribution(&elsewhere)
                .unwrap_err()
                .to_string()
                .contains("somewhere other than the registry")
        );
    }

    #[test]
    fn a_shipped_package_is_unpacked_where_a_fetched_one_would_have_been() {
        let pulled = remote_pull_script(
            AgentKind::Pi,
            &RemoteRelease {
                version: "0.84.3".into(),
                platform_name: "linux-x64".into(),
                asset: "pi-linux-x64.tar.gz".into(),
                url: "https://example.invalid/pi-linux-x64.tar.gz".into(),
                digest: "ef".repeat(32),
                algorithm: DigestAlgorithm::Sha256,
            },
            "",
        );
        let shipped = remote_unpack_script(AgentKind::Pi, "/tmp/staging/pi.package", "0.84.3");
        // However the payload got there, what happens to it afterwards is the
        // same text, so a runtime lands in the same place either way.
        let installing = remote_install_snippet(AgentKind::Pi);
        assert!(pulled.ends_with(&installing));
        assert!(shipped.ends_with(&installing));
        assert!(shipped.contains("payload=/tmp/staging/pi.package\n"));
        assert!(shipped.contains("version=0.84.3\n"));
    }

    #[test]
    fn a_target_pulls_its_own_runtime_into_the_directories_an_upload_would_use() {
        let claude = RemoteRelease {
            version: "1.2.3".into(),
            platform_name: "linux-x64".into(),
            asset: "claude".into(),
            url: "https://example.invalid/1.2.3/linux-x64/claude".into(),
            digest: "ab".repeat(32),
            algorithm: DigestAlgorithm::Sha256,
        };
        let script = remote_pull_script(AgentKind::Claude, &claude, "export HTTPS_PROXY='p';");
        // The proxy the operator configured for this host is what the target
        // reaches the release through, so it has to be exported first.
        assert!(script.starts_with("export HTTPS_PROXY='p';\n"));
        assert!(script.contains("url=https://example.invalid/1.2.3/linux-x64/claude\n"));
        assert!(script.contains(&format!("sum={}\n", "ab".repeat(32))));
        // Bounded twice: connecting, and stalling mid-transfer.
        assert!(script.contains("--connect-timeout 8"));
        assert!(script.contains("--speed-limit 4096 --speed-time 20"));
        // Nothing is moved into place until the payload hashes to what the
        // controller resolved.
        let verified = script.find("checksum mismatch").unwrap();
        let installed = script.find(r#"mv -f "$payload" "$HOME/.local/bin/claude""#);
        assert!(installed.is_some_and(|installed| installed > verified));

        let codex = RemoteRelease {
            version: "0.9.0".into(),
            platform_name: "aarch64-apple-darwin".into(),
            asset: "codex-package-aarch64-apple-darwin.tar.gz".into(),
            url: "https://example.invalid/codex-package-aarch64-apple-darwin.tar.gz".into(),
            digest: "cd".repeat(32),
            algorithm: DigestAlgorithm::Sha256,
        };
        let script = remote_pull_script(AgentKind::Codex, &codex, "");
        assert!(script.contains(r#"tar -xzf "$payload" -C "$stage""#));
        assert!(
            script.contains(r#"ln -sfn "$releases/$version/bin/codex" "$HOME/.local/bin/codex""#)
        );

        // Pi keeps its themes and modules beside the executable, so the link
        // has to point into the release rather than at a copied-out file.
        let pi = RemoteRelease {
            version: "0.84.3".into(),
            platform_name: "linux-x64".into(),
            asset: "pi-linux-x64.tar.gz".into(),
            url: "https://example.invalid/pi-linux-x64.tar.gz".into(),
            digest: "ef".repeat(32),
            algorithm: DigestAlgorithm::Sha256,
        };
        let script = remote_pull_script(AgentKind::Pi, &pi, "");
        assert!(script.contains(r#"ln -sfn "$releases/$version/pi/pi" "$HOME/.local/bin/pi""#));

        // OpenCode's package is verified against the digest npm publishes for
        // it, which is a SHA-512, so the target has to check that one.
        let opencode = RemoteRelease {
            version: "0.5.0".into(),
            platform_name: "linux-x64".into(),
            asset: "opencode-linux-x64-0.5.0.tgz".into(),
            url: "https://example.invalid/opencode-linux-x64-0.5.0.tgz".into(),
            digest: "12".repeat(64),
            algorithm: DigestAlgorithm::Sha512,
        };
        let script = remote_pull_script(AgentKind::OpenCode, &opencode, "");
        assert!(script.contains("sha512sum"));
        assert!(script.contains("shasum -a 512"));
        assert!(script.contains(&format!("sum={}\n", "12".repeat(64))));
        assert!(
            script.contains(r#"mv -f "$stage/package/bin/opencode" "$HOME/.local/bin/opencode""#)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_pull_script_verifies_before_it_installs_and_installs_what_it_verified() {
        let has_curl = Command::new("sh")
            .args(["-c", "command -v curl >/dev/null 2>&1"])
            .status()
            .is_ok_and(|status| status.success());
        if !has_curl {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("muxloom-pull-{}-{nonce}", std::process::id()));
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let exports = format!("export HOME={};", shell_quote(&home.to_string_lossy()));

        let binary = root.join("claude-release");
        fs::write(&binary, b"claude-binary").unwrap();
        let claude = RemoteRelease {
            version: "1.2.3".into(),
            platform_name: "linux-x64".into(),
            asset: "claude".into(),
            url: format!("file://{}", binary.display()),
            digest: digest_file(&binary, DigestAlgorithm::Sha256).unwrap(),
            algorithm: DigestAlgorithm::Sha256,
        };

        // A digest that does not match what lands is the one thing that must
        // never leave a runtime behind.
        let tampered = RemoteRelease {
            digest: "ab".repeat(32),
            ..claude.clone()
        };
        let status = Command::new("sh")
            .arg("-c")
            .arg(remote_pull_script(AgentKind::Claude, &tampered, &exports))
            .output()
            .unwrap();
        assert!(!status.status.success());
        assert!(
            String::from_utf8_lossy(&status.stderr).contains("checksum mismatch"),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(!home.join(".local/bin/claude").exists());

        let output = Command::new("sh")
            .arg("-c")
            .arg(remote_pull_script(AgentKind::Claude, &claude, &exports))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let installed = home.join(".local/bin/claude");
        assert_eq!(fs::read(&installed).unwrap(), b"claude-binary");

        let package_root = root.join("package");
        fs::create_dir_all(package_root.join("bin")).unwrap();
        fs::write(package_root.join("bin/codex"), b"codex-binary").unwrap();
        let archive = root.join("codex-package.tar.gz");
        let packed = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&package_root)
            .arg("bin")
            .status()
            .unwrap();
        assert!(packed.success());
        let codex = RemoteRelease {
            version: "0.9.0".into(),
            platform_name: "aarch64-apple-darwin".into(),
            asset: "codex-package.tar.gz".into(),
            url: format!("file://{}", archive.display()),
            digest: digest_file(&archive, DigestAlgorithm::Sha256).unwrap(),
            algorithm: DigestAlgorithm::Sha256,
        };
        let output = Command::new("sh")
            .arg("-c")
            .arg(remote_pull_script(AgentKind::Codex, &codex, &exports))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let linked = home.join(".local/bin/codex");
        assert_eq!(fs::read(&linked).unwrap(), b"codex-binary");
        assert_eq!(
            fs::read_link(&linked).unwrap(),
            home.join(".local/share/muxloom/codex/releases/0.9.0/bin/codex")
        );

        // Pi's executable is no use without the files its publisher put beside
        // it, so the whole release stays together and the link points into it.
        let pi_root = root.join("pi-package");
        fs::create_dir_all(pi_root.join("pi/theme")).unwrap();
        fs::write(pi_root.join("pi/pi"), b"pi-binary").unwrap();
        fs::write(pi_root.join("pi/theme/dark.json"), b"{}").unwrap();
        let pi_archive = root.join("pi-linux-x64.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&pi_archive)
                .arg("-C")
                .arg(&pi_root)
                .arg("pi")
                .status()
                .unwrap()
                .success()
        );
        let pi = RemoteRelease {
            version: "0.84.3".into(),
            platform_name: "linux-x64".into(),
            asset: "pi-linux-x64.tar.gz".into(),
            url: format!("file://{}", pi_archive.display()),
            digest: digest_file(&pi_archive, DigestAlgorithm::Sha256).unwrap(),
            algorithm: DigestAlgorithm::Sha256,
        };
        let output = Command::new("sh")
            .arg("-c")
            .arg(remote_pull_script(AgentKind::Pi, &pi, &exports))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(home.join(".local/bin/pi")).unwrap(), b"pi-binary");
        assert!(
            home.join(".local/share/muxloom/pi/releases/0.84.3/pi/theme/dark.json")
                .is_file()
        );

        // OpenCode is verified against the SHA-512 npm publishes for the very
        // bytes it serves, so the target has to reach for the other tool.
        let opencode_root = root.join("opencode-package");
        fs::create_dir_all(opencode_root.join("package/bin")).unwrap();
        fs::write(
            opencode_root.join("package/bin/opencode"),
            b"opencode-binary",
        )
        .unwrap();
        let opencode_archive = root.join("opencode.tgz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&opencode_archive)
                .arg("-C")
                .arg(&opencode_root)
                .arg("package")
                .status()
                .unwrap()
                .success()
        );
        let opencode = RemoteRelease {
            version: "0.5.0".into(),
            platform_name: "linux-x64".into(),
            asset: "opencode-linux-x64-0.5.0.tgz".into(),
            url: format!("file://{}", opencode_archive.display()),
            digest: digest_file(&opencode_archive, DigestAlgorithm::Sha512).unwrap(),
            algorithm: DigestAlgorithm::Sha512,
        };
        let output = Command::new("sh")
            .arg("-c")
            .arg(remote_pull_script(AgentKind::OpenCode, &opencode, &exports))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(home.join(".local/bin/opencode")).unwrap(),
            b"opencode-binary"
        );

        // Nothing is left in the staging directory the payload passed through.
        assert!(
            fs::read_dir(home.join(".cache/muxloom/install"))
                .unwrap()
                .next()
                .is_none()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "controller")]
    #[test]
    fn installs_downloaded_agent_packages_without_system_tools() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "muxloom-local-install-{}-{nonce}",
            std::process::id()
        ));
        let home = root.join("home");
        fs::create_dir_all(&root).unwrap();

        let claude = root.join("claude");
        fs::write(&claude, b"claude-binary").unwrap();
        install_local_runtime_binary_at(&claude, "claude", &home).unwrap();
        assert_eq!(
            fs::read(home.join(".local/bin/claude")).unwrap(),
            b"claude-binary"
        );

        let archive = root.join("codex.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut package = tar::Builder::new(encoder);
        let payload = b"codex-binary";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        package
            .append_data(&mut header, "bin/codex", &payload[..])
            .unwrap();
        package.into_inner().unwrap().finish().unwrap();

        let extracted = extract_cached_bundle_executable(&archive, AgentKind::Codex).unwrap();
        assert_eq!(fs::read(extracted).unwrap(), b"codex-binary");
        #[cfg(unix)]
        {
            install_local_bundle_at(&archive, AgentKind::Codex, "1.2.3", &home).unwrap();
            assert_eq!(
                fs::read(home.join(".local/bin/codex")).unwrap(),
                b"codex-binary"
            );
            assert!(
                home.join(".local/share/muxloom/codex/releases/1.2.3/bin/codex")
                    .is_file()
            );
        }
        #[cfg(not(unix))]
        {
            let error =
                install_local_bundle_at(&archive, AgentKind::Codex, "1.2.3", &home).unwrap_err();
            assert!(error.to_string().contains("unsupported on this platform"));
            assert!(
                !home
                    .join(".local/share/muxloom/codex/releases/1.2.3")
                    .exists()
            );
        }

        // The same unpacking serves every runtime that ships a directory: only
        // where its executable sits inside the package differs.
        let pi_archive = root.join("pi.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&pi_archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut package = tar::Builder::new(encoder);
        for (name, payload) in [
            ("pi/pi", &b"pi-binary"[..]),
            ("pi/theme/dark.json", &b"{}"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            package.append_data(&mut header, name, payload).unwrap();
        }
        package.into_inner().unwrap().finish().unwrap();
        #[cfg(unix)]
        {
            install_local_bundle_at(&pi_archive, AgentKind::Pi, "0.84.3", &home).unwrap();
            assert_eq!(fs::read(home.join(".local/bin/pi")).unwrap(), b"pi-binary");
            assert!(
                home.join(".local/share/muxloom/pi/releases/0.84.3/pi/theme/dark.json")
                    .is_file()
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tunnel_control_paths_are_stable_and_config_specific() {
        let first = tunnel_control_path("18118:127.0.0.1:8118");
        assert_eq!(first, tunnel_control_path("18118:127.0.0.1:8118"));
        assert_ne!(first, tunnel_control_path("28118:127.0.0.1:8118"));
        assert!(first.ends_with("-%C"));
    }

    #[test]
    fn controller_downloads_translate_remote_loopback_proxy_through_tunnel() {
        let config = Config {
            reverse_tunnel: "18118:127.0.0.1:8118".into(),
            ..Config::default()
        };
        let runtime = Runtime::new(&config);
        let environment = vec![
            ("HTTPS_PROXY".into(), "http://127.0.0.1:18118".into()),
            ("NO_PROXY".into(), "localhost".into()),
        ];
        assert_eq!(
            runtime.controller_download_environment(&Target::ssh("gpu"), &environment),
            [
                ("HTTPS_PROXY".into(), "http://127.0.0.1:8118".into()),
                ("NO_PROXY".into(), "localhost".into()),
            ]
        );
    }

    #[test]
    fn parses_probe_and_sessions() {
        let output = concat!(
            "tmux=1\n",
            "codex=1\n",
            "claude=0\n",
            "pi=1\n",
            "opencode=0\n",
            "muxloom-codex-10-2\tcodex\t/work/a b\talpha\t10\t\t\t\t\t0\t123\n",
            "ad-claude-11-2\t\t\t\t\tclaude\t/work/remote\tdone\t11\t1\t456\n"
        );
        let (probe, sessions) = parse_discovery("gpu", output).unwrap();
        assert!(
            probe.tmux
                && probe.has(AgentKind::Codex)
                && !probe.has(AgentKind::Claude)
                && probe.has(AgentKind::Pi)
        );
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].path, "/work/a b");
        assert!(sessions[0].id.starts_with("muxloom-"));
        assert_eq!(sessions[0].target_id, "gpu");
        assert!(sessions[1].id.starts_with("ad-"));
        assert!(sessions[1].dead, "remote dead panes must be archived");
    }

    #[test]
    fn accepts_current_and_legacy_managed_session_ids() {
        assert!(is_managed_session_id("muxloom-codex-10-2"));
        assert!(is_managed_session_id("ad-claude-10-2"));
        assert!(!is_managed_session_id("other-codex-10-2"));
        assert!(!is_managed_session_id("muxloom-invalid/session"));
    }

    #[test]
    fn parses_paged_history_metadata() {
        let page =
            parse_history_page("__AD_INFO__120\t24\t80\t120\nline one\nline two\n", 999).unwrap();
        assert_eq!(page.history_size, 120);
        assert_eq!(page.pane_height, 24);
        assert_eq!(page.pane_width, 80);
        assert_eq!(page.offset_from_bottom, 120);
        assert_eq!(page.text, "line one\nline two");
        assert!(!page.has_older());
    }

    #[test]
    fn a_recap_shows_what_a_short_session_printed_rather_than_the_blank_rows_under_it() {
        // What a page of a barely-used screen looks like: one printed row, then
        // rows the renderer only resets the attributes on.
        let mut page = String::from("muxloom-smoke\u{1b}[m");
        for _ in 0..19 {
            page.push_str("\n\u{1b}[m");
        }
        assert_eq!(newest_drawn_rows(&page, 20), "muxloom-smoke\u{1b}[m");
    }

    #[test]
    fn a_recap_keeps_the_newest_rows_and_the_blank_ones_between_them() {
        let page = (0..30)
            .map(|row| {
                if row == 27 {
                    "\u{1b}[m".to_string()
                } else {
                    format!("row {row}\u{1b}[m")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            newest_drawn_rows(&page, 4),
            "row 26\u{1b}[m\n\u{1b}[m\nrow 28\u{1b}[m\nrow 29\u{1b}[m"
        );
        assert_eq!(newest_drawn_rows("\u{1b}[m\n\u{1b}[m", 20), "");
    }

    #[test]
    fn styling_and_titles_do_not_count_as_terminal_output() {
        assert!(!row_has_content("\u{1b}[m"));
        assert!(!row_has_content("\u{1b}[K\u{1b}[38;5;24m   \u{1b}[m"));
        assert!(!row_has_content("\u{1b}]0;muxloom\u{7}"));
        assert!(!row_has_content("\u{1b}]8;;https://example.com\u{1b}\\"));
        assert!(row_has_content("\u{1b}[1mdone\u{1b}[m"));
        assert!(row_has_content("\u{1b}[m你好"));
    }

    #[test]
    fn detects_runtime_attention_prompts() {
        let codex = "Would you like to run the following command?\n› 1. Yes\n  2. No\nPress enter to confirm";
        assert_eq!(
            attention_reason(AgentKind::Codex, codex, &[]).as_deref(),
            Some("command approval")
        );
        let claude = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";
        assert_eq!(
            attention_reason(AgentKind::Claude, claude, &[]).as_deref(),
            Some("confirmation")
        );
        let idle_prompt = concat!(
            "Earlier output: 1. Yes\n",
            "Earlier output: 2. No\n",
            "Task completed successfully.\n",
            "› Explain this codebase\n",
            "gpt-5.6-sol max · /work/project\n"
        );
        assert_eq!(attention_reason(AgentKind::Codex, idle_prompt, &[]), None);

        let codex_working = "• Working (7s • esc to interrupt) · 1 background terminal running";
        assert!(agent_is_working(AgentKind::Codex, codex_working));
        assert!(agent_is_working(
            AgentKind::Codex,
            "• Refactoring terminal state (12s • esc to interrupt)"
        ));
        assert!(!agent_is_working(AgentKind::Codex, idle_prompt));
        let claude_working = concat!(
            "Bash(sleep 20)\n",
            "  Running… (7s)\n",
            "✶ Tomfoolering… (9s · ↓ 82 tokens)\n",
            "manual mode on · esc to interrupt"
        );
        assert!(agent_is_working(AgentKind::Claude, claude_working));
        assert!(!agent_is_working(
            AgentKind::Claude,
            "❯ \nmanual mode on · ? for shortcuts"
        ));
        assert!(!agent_is_working(AgentKind::Terminal, codex_working));

        let mut stale_prompt =
            String::from("Would you like to run the following command?\n› 1. Yes\n  2. No\n");
        stale_prompt.push_str(
            &(0..30)
                .map(|index| format!("working output {index}\n"))
                .collect::<String>(),
        );
        assert_eq!(attention_reason(AgentKind::Codex, &stale_prompt, &[]), None);
        assert!(attention_reason(AgentKind::Codex, "working...", &[]).is_none());
    }

    /// A rule as wide as Claude Code draws one.
    fn rule() -> String {
        "─".repeat(120)
    }

    #[test]
    fn an_empty_prompt_box_is_ready_whether_or_not_a_turn_is_running() {
        // Both frames are Claude Code v2.1.233, read off a live session: idle
        // after a turn, and mid-turn with the interrupt hint up. The box is
        // drawn and empty either way, which is the whole point — a message
        // delivered during a turn is held and read when the turn ends.
        let idle = format!(
            "⏺ A\n\n✻ Baked for 6s\n\n{}\n❯\n{}\n  ⏸ manual mode on · ? for shortcuts · ← for agents\n",
            rule(),
            rule()
        );
        assert_eq!(composer(AgentKind::Claude, &idle), Some(Composer::Ready));

        let working = format!(
            "⏺ one\n  two\n\n✻ Discombobulating… (10s · thinking with xhigh effort)\n\n{}\n❯\n{}\n  \
             ⏸ manual mode on · esc to interrupt · ← for agents\n",
            rule(),
            rule()
        );
        assert_eq!(composer(AgentKind::Claude, &working), Some(Composer::Ready));
        assert!(agent_is_working(AgentKind::Claude, &working));
    }

    #[test]
    fn a_prompt_box_somebody_is_still_typing_in_is_occupied() {
        // What muxloom's own send_input used to leave behind: text in the box
        // and no submission. Anything pasted now joins it and goes in as one
        // message.
        let stranded = format!(
            "⏺ A\n\n{}\n❯ Reply with exactly the single word BEE and nothing else, no preamble.\n{}\n \
             ⏸ manual mode on\n",
            rule(),
            rule()
        );
        assert_eq!(
            composer(AgentKind::Claude, &stranded),
            Some(Composer::Occupied)
        );

        // Wrapped over two rows, as a long line is drawn.
        let wrapped = format!(
            "{}\n❯ Without using any tools at all, write out the numbers 1 through 120 as English \
             words,\n  one per line.\n{}\n  ⏸ manual mode on\n",
            rule(),
            rule()
        );
        assert_eq!(
            composer(AgentKind::Claude, &wrapped),
            Some(Composer::Occupied)
        );
    }

    #[test]
    fn a_dialog_leaves_no_prompt_box_to_deliver_into() {
        // Claude Code replaces the box with the question while it asks one, so
        // a paste would answer the question. Read off a live permission prompt.
        let dialog = format!(
            "⏺ Write(probe.txt)\n\n{}\n Create file\n probe.txt\n{}\n  1 hi\n{}\n Do you want to \
             create probe.txt?\n ❯ 1. Yes\n   2. Yes, allow all edits during this session \
             (shift+tab)\n   3. No\n\n Esc to cancel · Tab to amend\n",
            rule(),
            "╌".repeat(120),
            "╌".repeat(120)
        );
        assert_eq!(composer(AgentKind::Claude, &dialog), Some(Composer::Absent));
        assert!(attention_reason(AgentKind::Claude, &dialog, &[]).is_some());
    }

    #[test]
    fn a_runtime_that_is_not_on_the_pty_yet_has_no_prompt_box() {
        // A launch that ran an upgrade first: the kind says codex, the screen
        // is a package manager. Bracketed paste into this lands as literal
        // escape codes in a shell.
        let updating = "Updating Codex via `brew upgrade --cask codex`...\n==> Auto-updating \
                        Homebrew...\nYou have 61 outdated formulae and 3 outdated casks \
                        installed.\n";
        assert_eq!(composer(AgentKind::Codex, updating), Some(Composer::Absent));
        assert_eq!(
            composer(AgentKind::Claude, updating),
            Some(Composer::Absent)
        );
        // A runtime with no box muxloom has learned is not read at all, and
        // says so rather than claiming the box is missing.
        assert_eq!(composer(AgentKind::OpenCode, updating), None);
        assert_eq!(composer(AgentKind::Terminal, "tiger $ "), None);
    }

    #[test]
    fn codex_reads_its_own_prompt_line() {
        // Codex v0.149.1, live: the placeholder is not something anybody typed.
        let empty = "• Reconnecting... 1/5 (4s • esc to interrupt)\n\n» Ask Codex to do \
                     anything\n\n  gpt-5.6-sol ultra · /private/tmp/muxprobe\n";
        assert_eq!(composer(AgentKind::Codex, empty), Some(Composer::Ready));

        let typed = "»  with exactly the single word BEE and nothing else. No tools, no \
                     preamble.\n\n  tab to queue message                        100% context \
                     left\n";
        assert_eq!(composer(AgentKind::Codex, typed), Some(Composer::Occupied));

        // Its trust dialog draws choices with `›`, and no composer at all.
        let trust = "  Do you trust the contents of this directory?\n\n› 1. Yes, continue\n  2. \
                     No, quit\n\n  Press enter to continue\n";
        assert_eq!(composer(AgentKind::Codex, trust), Some(Composer::Absent));
    }

    #[test]
    fn working_status_is_detected_above_a_blank_screen_bottom() {
        // A tall pane whose transcript is short leaves the bottom rows empty, so
        // the status/spinner line sits well above the last raw lines. It must
        // still be classified as working rather than idle.
        let mut claude =
            String::from("✻ Nucleating… (esc to interrupt · 27m 56s · ↓ 24.0k tokens)\n");
        claude.push_str(&"\n".repeat(40));
        assert!(agent_is_working(AgentKind::Claude, &claude));

        let mut codex =
            String::from("• Working (7s • esc to interrupt) · 1 background terminal running\n");
        codex.push_str(&"   \n".repeat(40));
        assert!(agent_is_working(AgentKind::Codex, &codex));
    }

    #[test]
    fn every_interruptible_phase_counts_as_working() {
        // The early phase, before any token count is drawn.
        assert!(agent_is_working(
            AgentKind::Claude,
            "✳ Deliberating… (esc to interrupt)\n"
        ));
        // A tool run.
        assert!(agent_is_working(
            AgentKind::Claude,
            "  Running… (esc to interrupt)\n"
        ));
        // A parallel subagent display.
        assert!(agent_is_working(
            AgentKind::Claude,
            "✻ Task(explore the repo)\n✻ Task(review tests)\n2 agents running · esc to interrupt\n"
        ));
        // A finished turn is not working, whatever the transcript retains.
        assert!(!agent_is_working(
            AgentKind::Claude,
            "✻ Worked for 27s · ↓ 24.0k tokens\n❯ \n? for shortcuts\n"
        ));
    }

    #[test]
    fn a_phase_that_offers_no_interrupt_is_still_working() {
        // Compaction runs for minutes with no interrupt hint anywhere on the
        // screen — the footer shows the mode and the context meter instead —
        // and used to read as a session that had stopped.
        let compacting = concat!(
            "✶ Compacting conversation… (11m 4s · ↓ 27.7k tokens)\n",
            "  ▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱  18%\n",
            "──────────────────────────────\n",
            "❯\n",
            "──────────────────────────────\n",
            "  ⏵⏵ auto mode on (shift+tab to cycle)   100% context used\n"
        );
        assert!(agent_is_working(AgentKind::Claude, compacting));
        // The same shape carries every other phase whose hint the footer had no
        // room for, whether the phase is a spinner word or the task in hand.
        for phase in [
            "· Precipitating… (36m 2s · ↓ 57.0k tokens)",
            "✽ Sprouting… (3m 10s · ↓ 8.3k tokens · thinking with xhigh effort)",
            "✳ Add the waveform lab module… (20m 2s · ↓ 73.0k tokens)",
            "✻ Cogitating… (9s)",
            "✢ 修复 forced update 根因… (1h 2m 3s · ↓ 1.0k tokens)",
        ] {
            assert!(
                agent_is_working(AgentKind::Claude, &format!("{phase}\n❯\n")),
                "{phase}"
            );
        }
        // Prose that trails off, a transcript line quoting a counter, and the
        // collapsed-output hint all keep their session idle.
        for quiet in [
            "· so that is where the parser gave up…\n❯\n",
            "  ⎿  … +138 lines (ctrl+o to expand)\n❯\n",
            "✻ Worked for 27s · ↓ 24.0k tokens\n❯\n",
            "  ✶ Compacting conversation… (11m 4s · ↓ 27.7k tokens)  ← what it looked like\n❯\n",
        ] {
            assert!(!agent_is_working(AgentKind::Claude, quiet), "{quiet}");
        }
    }

    #[test]
    fn a_selection_cursor_on_a_numbered_option_asks_for_attention() {
        let menu = "Which approach should we take?\n\
                    ❯ 1. Refactor the parser\n  2. Patch the renderer\n  3. Other\n\
                    esc to skip\n";
        assert_eq!(
            attention_reason(AgentKind::Claude, menu, &[]).as_deref(),
            Some("interactive choice")
        );
        // The same pointed list while a turn is running is a progress panel,
        // not a question.
        let busy = format!("{menu}✻ Nucleating… (esc to interrupt)\n");
        assert_eq!(attention_reason(AgentKind::Claude, &busy, &[]), None);
        // A bare input caret is not a menu.
        assert_eq!(
            attention_reason(AgentKind::Claude, "❯ \n? for shortcuts\n", &[]),
            None
        );
        // A numbered list the model merely printed in its reply must not
        // trigger: no cursor and no key hint...
        let quoted = "Here are the options I considered:\n\
                      1. Refactor the parser\n  2. Patch the renderer\n\
                      Let me know which you prefer.\n";
        assert_eq!(attention_reason(AgentKind::Claude, quoted, &[]), None);
        // ...and even a cursor-looking quote scrolled above the input line
        // sits outside the bottom window once the reply continues.
        let scrolled = format!("{menu}{}", "output line\n".repeat(12));
        assert_eq!(attention_reason(AgentKind::Claude, &scrolled, &[]), None);
        // A single numbered line with a stray cursor is not a menu either.
        assert_eq!(
            attention_reason(
                AgentKind::Claude,
                "❯ 1. the only line\npress esc maybe\n",
                &[]
            ),
            None
        );
    }

    #[test]
    fn parses_rankable_history_matches() {
        let matches =
            parse_history_matches("__AD_RECAP__3\tapprove now\n__AD_HISTORY__91\tolder mention\n");
        assert_eq!(matches.len(), 2);
        assert!(matches[0].recap);
        assert_eq!(matches[1].line_number, 91);
    }

    #[test]
    fn parses_directory_entries_and_runtime_resume_metadata() {
        let listing =
            parse_directory_listing(b"/work/project\n./src\0./.hidden\0./tests\0").unwrap();
        assert_eq!(listing.path, "/work/project");
        assert_eq!(listing.directories, [".hidden", "src", "tests"]);

        let codex = concat!(
            "\u{1e}INDEX\n",
            "{\"id\":\"codex-id\",\"thread_name\":\"Fix the renderer\",\"updated_at\":\"2026-07-20T10:00:00Z\"}\n",
            "\u{1e}SESSION\n",
            "/home/test/.codex/sessions/rollout-codex-id.jsonl\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T09:00:00Z\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first codex prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Codex, "/work/project/", codex);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "codex-id");
        assert_eq!(candidates[0].kind, AgentKind::Codex);
        assert_eq!(
            candidates[0].source_path,
            "/home/test/.codex/sessions/rollout-codex-id.jsonl"
        );
        assert_eq!(candidates[0].recap.as_deref(), Some("Fix the renderer"));
        assert_eq!(
            candidates[0].first_message.as_deref(),
            Some("first codex prompt")
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("first codex prompt")
        );

        let claude = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.claude/projects/claude-id.jsonl\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\",\"message\":{\"content\":\"first claude prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Claude, "/work/project", claude);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, AgentKind::Claude);
        assert_eq!(candidates[0].recap, None);
        assert_eq!(
            candidates[0].first_message.as_deref(),
            Some("first claude prompt")
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("first claude prompt")
        );
        assert!(
            parse_resume_candidates(AgentKind::Claude, "/other", claude).is_empty(),
            "resume candidates must match the exact working directory"
        );
    }

    /// Claude Code renames a session as it goes, and the name it settled on is
    /// the one worth showing. Transcripts written by older builds name it once,
    /// under a key those builds no longer write.
    #[test]
    fn claude_resume_candidates_carry_the_latest_name_the_session_was_given() {
        let renamed = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.claude/projects/claude-id.jsonl\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\",\"message\":{\"content\":\"first claude prompt\"}}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"Reading the daemon\",\"sessionId\":\"claude-id\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"Recap from the transcript\",\"sessionId\":\"claude-id\"}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Claude, "/work/project", renamed);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].recap.as_deref(),
            Some("Recap from the transcript")
        );

        let legacy = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.claude/projects/old-id.jsonl\n",
            "{\"type\":\"summary\",\"summary\":\"Named by an older build\"}\n",
            "{\"type\":\"user\",\"sessionId\":\"old-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\",\"message\":{\"content\":\"first claude prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Claude, "/work/project", legacy);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].recap.as_deref(),
            Some("Named by an older build")
        );
    }

    /// A conversation the runtime never named is shown by what was said in it,
    /// and the runtime files a good deal under the person's own role that
    /// nobody said. A quarter of the conversations on this machine open with
    /// the caveat below; listed by their first "message" they would all be
    /// called the same thing.
    #[test]
    fn a_conversation_is_never_listed_by_the_machinery_it_opens_with() {
        let sessions = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.claude/projects/claude-id.jsonl\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\",\"message\":{\"content\":\"<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>\"}}\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:01Z\",\"message\":{\"content\":\"<command-name>/model</command-name>\"}}\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:02Z\",\"message\":{\"content\":\"why is the port forward failing?\"}}\n",
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:03Z\",\"message\":{\"content\":\"[Request interrupted by user]\"}}\n",
        );
        let candidates = parse_resume_candidates(AgentKind::Claude, "/work/project", sessions);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].summary(),
            "why is the port forward failing?",
            "the one line anybody actually typed"
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("why is the port forward failing?"),
            "an interruption is not the last word either"
        );

        // Codex files the environment it was handed the same way.
        let codex = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.codex/sessions/codex-id.jsonl\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context> cwd: /work/project </environment_context>\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"trace the memory peak\"}]}}\n",
        );
        let candidates = parse_resume_candidates(AgentKind::Codex, "/work/project", codex);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].summary(), "trace the memory peak");
    }

    /// pi opens a transcript with a header line that carries the id and the
    /// folder, names the conversation on a line of its own whenever the name
    /// changes, and files a tool's answer as a message under a role of its own.
    #[test]
    fn pi_resume_candidates_come_from_the_header_the_name_and_what_the_person_typed() {
        let sessions = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.pi/agent/sessions/--work-project--/2026-07-20T09-00-00-000Z_pi-id.jsonl\n",
            "{\"type\":\"session\",\"version\":3,\"id\":\"pi-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T09:00:00.000Z\"}\n",
            "{\"type\":\"session_info\",\"name\":\"A first guess at a name\",\"timestamp\":\"2026-07-20T09:00:01.000Z\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-07-20T09:00:02.000Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"first pi prompt\"}]}}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-07-20T09:00:03.000Z\",\"message\":{\"role\":\"tool\",\"content\":[{\"type\":\"text\",\"text\":\"a tool answered\"}]}}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-07-20T09:00:04.000Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"second pi prompt\"}]}}\n",
            "{\"type\":\"session_info\",\"name\":\"What it is really about\",\"timestamp\":\"2026-07-20T09:00:05.000Z\"}\n",
        );

        let candidates = parse_resume_candidates(AgentKind::Pi, "/work/project", sessions);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "pi-id");
        assert_eq!(candidates[0].kind, AgentKind::Pi);
        assert_eq!(
            candidates[0].recap.as_deref(),
            Some("What it is really about")
        );
        assert_eq!(
            candidates[0].first_message.as_deref(),
            Some("first pi prompt")
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("second pi prompt"),
            "a tool's answer is not something either party said"
        );
        assert_eq!(candidates[0].updated_at, "2026-07-20T09:00:05.000Z");
        assert!(
            parse_resume_candidates(AgentKind::Pi, "/other", sessions).is_empty(),
            "resume candidates must match the exact working directory"
        );
    }

    #[test]
    fn opencode_resume_candidates_come_from_what_opencode_says_about_itself() {
        let answer = r#"[
          {"id":"ses_here","directory":"/work/project","title":"What it is really about",
           "created":1787659401726,"updated":1787659402199,
           "first_text":"first opencode prompt","last_text":"The keeper spawn is what fails."},
          {"id":"ses_unnamed","directory":"/work/project",
           "title":"New session - 2026-08-25T12:03:21.726Z",
           "created":1787659401726,"updated":1787659402199,
           "first_text":"<system-reminder>a plan file exists</system-reminder>","last_text":null},
          {"id":"ses_elsewhere","directory":"/work/other","title":"Somewhere else entirely",
           "created":1787659401726,"updated":1787659402199,
           "first_text":null,"last_text":null}
        ]"#;
        let rows = crate::native_history::opencode_rows(answer);
        let candidates: Vec<_> = rows
            .iter()
            .filter_map(|row| parse_opencode_resume(row, "/work/project"))
            .collect();

        assert_eq!(candidates.len(), 2, "{candidates:?}");
        assert_eq!(candidates[0].id, "ses_here");
        assert_eq!(candidates[0].kind, AgentKind::OpenCode);
        assert_eq!(
            candidates[0].recap.as_deref(),
            Some("What it is really about")
        );
        assert_eq!(
            candidates[0].first_message.as_deref(),
            Some("first opencode prompt")
        );
        assert_eq!(
            candidates[0].last_message.as_deref(),
            Some("The keeper spawn is what fails.")
        );
        // The one runtime with no file to point anybody at.
        assert!(candidates[0].source_path.is_empty());
        // Written the way every other runtime writes a time, so the sort and
        // the "has this moved on?" check both still work.
        assert_eq!(candidates[0].updated_at, "2026-08-25T12:03:22.199Z");
        // A name it has not given yet is no name, and what the runtime files
        // under the person's own role is not the person talking.
        assert_eq!(candidates[1].recap, None);
        assert_eq!(candidates[1].first_message, None);
    }

    #[test]
    fn codex_resume_candidates_exclude_newer_subagent_threads() {
        let sessions = concat!(
            "\u{1e}SESSION\n",
            "/home/test/.codex/sessions/rollout-main.jsonl\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"main-thread\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T09:00:00Z\",\"source\":\"cli\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"main prompt\"}}\n",
            "\u{1e}SESSION\n",
            "/home/test/.codex/sessions/rollout-subagent.jsonl\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"subagent-thread\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T10:00:00Z\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"main-thread\",\"depth\":1}}}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"delegated prompt\"}}\n"
        );

        let candidates = parse_resume_candidates(AgentKind::Codex, "/work/project", sessions);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "main-thread");
        assert_eq!(candidates[0].first_message.as_deref(), Some("main prompt"));
    }

    #[test]
    fn builds_runtime_specific_resume_commands() {
        let command = CommandConfig {
            command: "codex".into(),
            args: vec!["--full-auto".into()],
            ..CommandConfig::default()
        };
        assert_eq!(
            command_line(&command, AgentKind::Codex, false, Some("session id"), None),
            "codex --full-auto --no-alt-screen resume 'session id'"
        );
        let command = CommandConfig {
            command: "claude".into(),
            args: Vec::new(),
            ..CommandConfig::default()
        };
        assert_eq!(
            command_line(&command, AgentKind::Claude, false, Some("abc"), None),
            "claude --resume abc"
        );
        assert_eq!(
            command_line(
                &command,
                AgentKind::Claude,
                false,
                None,
                Some("Read /tmp/source history.jsonl")
            ),
            "claude 'Read /tmp/source history.jsonl'"
        );
        let command = CommandConfig {
            command: "pi".into(),
            args: Vec::new(),
            ..CommandConfig::default()
        };
        assert_eq!(
            command_line(&command, AgentKind::Pi, false, Some("abc"), None),
            "pi --session abc"
        );
    }

    #[test]
    fn codex_launches_inline_without_duplicate_flags() {
        let command = CommandConfig {
            command: "codex".into(),
            args: vec!["--no-alt-screen".into(), "--full-auto".into()],
            ..CommandConfig::default()
        };
        assert_eq!(
            launch_arguments(
                &command,
                AgentKind::Codex,
                false,
                None,
                Some("keep this history")
            ),
            ["--no-alt-screen", "--full-auto", "keep this history"]
        );
    }

    #[test]
    fn temporary_codex_disables_transcript_persistence_for_only_that_launch() {
        let command = CommandConfig {
            command: "codex".into(),
            args: vec!["--full-auto".into()],
            ..CommandConfig::default()
        };

        assert_eq!(
            launch_arguments(&command, AgentKind::Codex, true, None, None),
            [
                "--full-auto",
                "--no-alt-screen",
                "-c",
                "history.persistence=\"none\""
            ]
        );
        assert_eq!(
            launch_arguments(&command, AgentKind::Codex, false, None, None),
            ["--full-auto", "--no-alt-screen"]
        );
        assert!(is_temporary_session_id("muxloomd-temporal-codex-1"));
        assert!(is_temporary_session_id("muxloom-temporal-codex-1"));
        assert!(!is_temporary_session_id("muxloomd-codex-1"));
    }

    /// Placing a file creates the directories the agent expects and never
    /// silently overwrites: whatever was there is kept beside the new copy.
    #[test]
    fn placing_a_file_creates_its_directory_and_keeps_what_it_replaces() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("muxloom-place-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("restored.jsonl");
        fs::write(&source, b"{\"role\":\"user\"}\n").unwrap();
        let destination = root.join("home/.codex/sessions/2026/08/09/rollout-x.jsonl");

        let runtime = Runtime::new(&crate::config::Config::default());
        let target = Target::local();
        let path = destination.to_string_lossy().to_string();
        runtime.place_file(&target, &source, &path).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"{\"role\":\"user\"}\n");

        // A second placement of different content keeps the first alongside.
        fs::write(&source, b"{\"role\":\"assistant\"}\n").unwrap();
        runtime.place_file(&target, &source, &path).unwrap();
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"{\"role\":\"assistant\"}\n"
        );
        let kept = fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".muxloom-replaced-")
            });
        assert!(
            kept,
            "the replaced transcript must be kept beside the new one"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filename_search_supports_substrings_and_star_globs() {
        assert!(filename_matches_pattern("Main.RS", "main"));
        assert!(filename_matches_pattern("main.rs", "*.rs"));
        assert!(filename_matches_pattern("job.rs", "j**.rs"));
        assert!(!filename_matches_pattern("job.md", "j**.rs"));
        assert_eq!(
            relative_search_path("/work/project", "/work/project/src/main.rs"),
            "src/main.rs"
        );
    }
}
