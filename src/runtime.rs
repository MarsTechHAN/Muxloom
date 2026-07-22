use std::{
    collections::HashMap,
    fs,
    io::Read,
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
use sha2::{Digest, Sha256};

use crate::{
    config::{CommandConfig, Config},
    debug,
    model::{
        AgentKind, AgentSession, DirectoryListing, HistoryMatch, HistoryPage, LaunchRequest, Probe,
        ResumeCandidate, Target, Transport,
    },
};

const SESSION_PREFIX: &str = "muxloom-";
const LEGACY_SESSION_PREFIX: &str = "ad-";
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

#[derive(Debug, Clone)]
struct TargetPlatform {
    os: String,
    arch: String,
    musl: bool,
}

impl TargetPlatform {
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
}

#[derive(Debug, Clone)]
pub struct Runtime {
    ssh_connect_timeout_secs: u64,
    history_limit: usize,
    reverse_tunnel: String,
    host_reverse_tunnels: HashMap<String, String>,
    tunnel_checks: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Runtime {
    pub fn new(config: &Config) -> Self {
        Self {
            ssh_connect_timeout_secs: config.ssh_connect_timeout_secs,
            history_limit: config.history_limit.max(2_000),
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
        }
    }

    pub fn probe_and_discover(
        &self,
        target: &Target,
        codex_command: &str,
        claude_command: &str,
        environment: &[(String, String)],
    ) -> Result<(Probe, Vec<AgentSession>)> {
        debug::log("runtime", format!("probe start target={}", target.id));
        let exports = environment_exports(environment);
        let codex_probe = login_shell_command(&format!(
            "{exports} command -v {} >/dev/null 2>&1",
            shell_quote(codex_command)
        ));
        let claude_probe = login_shell_command(&format!(
            "{exports} command -v {} >/dev/null 2>&1",
            shell_quote(claude_command)
        ));
        let probe = format!(
            "if {codex_probe} >/dev/null 2>&1; then printf 'codex=1\\n'; else printf 'codex=0\\n'; fi; \
             if {claude_probe} >/dev/null 2>&1; then printf 'claude=1\\n'; else printf 'claude=0\\n'; fi; \
             if command -v tmux >/dev/null 2>&1; then printf 'tmux=1\\n'; else printf 'tmux=0\\n'; fi",
        );
        let script = format!(
            "{}; {}",
            probe,
            shell_join(&[
                "tmux",
                "list-panes",
                "-a",
                "-F",
                FORMAT,
                "-f",
                "#{m/r:^(muxloom-|ad-),#{session_name}}",
            ]) + " 2>/dev/null || true"
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "target probe")?;
        let (probe, mut sessions) =
            parse_discovery(&target.id, &String::from_utf8_lossy(&output.stdout))?;
        let mut dead_terminals: Vec<_> = sessions
            .iter()
            .filter(|session| session.dead && session.kind == AgentKind::Terminal)
            .map(|session| session.id.clone())
            .collect();
        dead_terminals.sort();
        dead_terminals.dedup();
        sessions.retain(|session| !(session.dead && session.kind == AgentKind::Terminal));
        for session_id in &dead_terminals {
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
                "probe done target={} tmux={} codex={} claude={} sessions={} dead_terminals_cleaned={}",
                target.id,
                probe.tmux,
                probe.codex,
                probe.claude,
                sessions.len(),
                dead_terminals.len()
            ),
        );
        Ok((probe, sessions))
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sequence = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let session_id = format!(
            "{SESSION_PREFIX}{}-{now}-{}-{sequence}",
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
                    command_line(command, request.kind, request.resume_id.as_deref())
                ))
            };
        let label = request.label.replace(['\t', '\n', '\r'], " ");
        let metadata_path = request.path.replace(['\t', '\n', '\r'], " ");
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
                &request.path,
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
        let script = commands.join(" && ");
        let output = self.run_shell(&request.target, &script, false)?;
        ensure_success(&output, "launch agent")?;
        debug::log(
            "runtime",
            format!(
                "launch done target={} session={session_id}",
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
        if kind == AgentKind::Terminal {
            bail!("ordinary terminals do not require a runtime install");
        }
        let executable_name = kind.as_str();
        let exports = environment_exports(environment);
        let platform = if matches!(target.transport, Transport::Ssh { .. }) {
            Some(self.target_platform(target)?)
        } else {
            None
        };
        let mut installed_source = None;
        let mut controller_download_error = None;

        if matches!(target.transport, Transport::Ssh { .. })
            && !command.command.contains('/')
            && command.command == executable_name
            && platform.as_ref().is_some_and(TargetPlatform::matches_local)
            && let Some(local_binary) = find_local_native_executable(&command.command)
            && local_runtime_can_copy(kind, &local_binary)
        {
            match self.upload_runtime_binary(target, &local_binary, executable_name) {
                Ok(()) => installed_source = Some("compatible controller binary".to_string()),
                Err(error) => debug::log(
                    "install",
                    format!(
                        "local binary upload failed target={} kind={kind}: {error:#}; falling back",
                        target.id
                    ),
                ),
            }
        }

        if installed_source.is_none()
            && matches!(target.transport, Transport::Ssh { .. })
            && !command.command.contains('/')
            && command.command == executable_name
            && let Some(platform) = &platform
        {
            match self.download_and_upload_runtime(target, kind, platform, environment) {
                Ok(source) => installed_source = Some(source),
                Err(error) => {
                    controller_download_error = Some(error.to_string());
                    debug::log(
                        "install",
                        format!(
                            "controller-side download failed target={} kind={kind}: {error:#}; trying configured target installer",
                            target.id
                        ),
                    );
                }
            }
        }

        if installed_source.is_none() {
            if command.install.trim().is_empty() {
                bail!(
                    "{} is unavailable and no install command is configured for {}",
                    command.command,
                    target.id
                );
            }
            let script = login_shell_command(&format!("{exports} {}", command.install));
            let output = self.run_shell(target, &script, false)?;
            if let Err(error) = ensure_success(&output, &format!("install {kind}")) {
                if let Some(controller_error) = controller_download_error {
                    bail!(
                        "{error}; controller-side offline install also failed: {controller_error}"
                    );
                }
                return Err(error);
            }
            installed_source = Some("configured target installer".into());
        }

        let synced = if matches!(target.transport, Transport::Ssh { .. }) {
            self.sync_local_config_files(target, &command.sync_files)?
        } else {
            0
        };
        let verify = login_shell_command(&format!(
            "{exports} command -v {} >/dev/null 2>&1",
            shell_quote(&command.command)
        ));
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

    fn download_and_upload_runtime(
        &self,
        target: &Target,
        kind: AgentKind,
        platform: &TargetPlatform,
        environment: &[(String, String)],
    ) -> Result<String> {
        let controller_environment = self.controller_download_environment(target, environment);
        match kind {
            AgentKind::Claude => {
                let platform_name = platform.claude_name()?;
                let version = validate_release_name(
                    self.controller_fetch_text(
                        &format!("{CLAUDE_RELEASES}/latest"),
                        &controller_environment,
                    )?
                    .trim(),
                )?;
                let manifest = self.controller_fetch_text(
                    &format!("{CLAUDE_RELEASES}/{version}/manifest.json"),
                    &controller_environment,
                )?;
                let manifest: Value = serde_json::from_str(&manifest)
                    .context("Claude release manifest is invalid JSON")?;
                let checksum = manifest
                    .get("platforms")
                    .and_then(|platforms| platforms.get(&platform_name))
                    .and_then(|platform| platform.get("checksum"))
                    .and_then(Value::as_str)
                    .context("Claude manifest has no checksum for the target platform")?;
                validate_sha256(checksum)?;
                let cache = controller_download_cache()
                    .join("claude")
                    .join(&version)
                    .join(&platform_name)
                    .join("claude");
                self.controller_download_verified(
                    &format!("{CLAUDE_RELEASES}/{version}/{platform_name}/claude"),
                    &cache,
                    checksum,
                    &controller_environment,
                )?;
                self.upload_runtime_binary(target, &cache, "claude")?;
                Ok(format!(
                    "controller-downloaded Claude {version} ({platform_name})"
                ))
            }
            AgentKind::Codex => {
                let platform_name = platform.codex_name()?;
                let effective =
                    self.controller_effective_url(CODEX_LATEST, &controller_environment)?;
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
                    &controller_environment,
                )?;
                let checksum = checksum_for_asset(&checksums, &asset)
                    .context("Codex checksum manifest has no target package")?;
                let cache = controller_download_cache()
                    .join("codex")
                    .join(&version)
                    .join(&platform_name)
                    .join(&asset);
                self.controller_download_verified(
                    &format!("{release_root}/{asset}"),
                    &cache,
                    &checksum,
                    &controller_environment,
                )?;
                self.upload_codex_archive(target, &cache, &version)?;
                Ok(format!(
                    "controller-downloaded Codex {version} ({platform_name})"
                ))
            }
            AgentKind::Terminal => bail!("terminal has no downloadable agent runtime"),
        }
    }

    fn controller_download_environment(
        &self,
        target: &Target,
        environment: &[(String, String)],
    ) -> Vec<(String, String)> {
        let tunnel = self
            .host_reverse_tunnels
            .get(&target.id)
            .map(String::as_str)
            .unwrap_or(&self.reverse_tunnel);
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
        let output = controller_curl(environment)
            .args(["-fsSL", "--retry", "3", url])
            .output()
            .with_context(|| format!("failed to download {url} on the controller"))?;
        ensure_success(&output, "controller download")?;
        String::from_utf8(output.stdout).context("controller download was not UTF-8")
    }

    fn controller_effective_url(
        &self,
        url: &str,
        environment: &[(String, String)],
    ) -> Result<String> {
        let output = controller_curl(environment)
            .args([
                "-fsSL",
                "--retry",
                "3",
                "-o",
                null_device(),
                "-w",
                "%{url_effective}",
                url,
            ])
            .output()
            .with_context(|| format!("failed to resolve {url} on the controller"))?;
        ensure_success(&output, "resolve controller download URL")?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn controller_download_verified(
        &self,
        url: &str,
        destination: &Path,
        expected_sha256: &str,
        environment: &[(String, String)],
    ) -> Result<()> {
        if destination.is_file()
            && sha256_file(destination).is_ok_and(|digest| digest == expected_sha256)
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
        let output = controller_curl(environment)
            .args(["-fsSL", "--retry", "3", "--output"])
            .arg(&partial)
            .arg(url)
            .output()
            .with_context(|| format!("failed to download {url} on the controller"))?;
        if !output.status.success() {
            let _ = fs::remove_file(&partial);
            ensure_success(&output, "controller runtime download")?;
        }
        let actual = sha256_file(&partial)?;
        if actual != expected_sha256 {
            let _ = fs::remove_file(&partial);
            bail!("download checksum mismatch: expected {expected_sha256}, got {actual}");
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

    fn upload_codex_archive(
        &self,
        target: &Target,
        local_archive: &Path,
        version: &str,
    ) -> Result<()> {
        let Transport::Ssh { alias } = &target.transport else {
            bail!("Codex package upload requires an SSH target");
        };
        let remote_home = self.remote_home(target)?;
        let install_cache = format!("{remote_home}/.cache/muxloom/install");
        let remote_archive = format!("{install_cache}/codex-package.tar.gz");
        let releases = format!("{remote_home}/.local/share/muxloom/codex/releases");
        let release_dir = format!("{releases}/{version}");
        let staging = format!("{release_dir}.partial-{}", std::process::id());
        let bin_dir = format!("{remote_home}/.local/bin");
        let prepare = format!(
            "mkdir -p {} {} {}",
            shell_quote(&install_cache),
            shell_quote(&releases),
            shell_quote(&bin_dir)
        );
        let output = self.run_shell(target, &prepare, false)?;
        ensure_success(&output, "prepare remote Codex package install")?;
        self.scp_to(alias, local_archive, &remote_archive)?;
        let activate = format!(
            "rm -rf {staging}; mkdir -p {staging}; \
             tar -xzf {archive} -C {staging} && \
             test -f {staging}/bin/codex && \
             chmod 755 {staging}/bin/codex && \
             if [ -f {staging}/codex-path/rg ]; then chmod 755 {staging}/codex-path/rg; fi && \
             if [ -f {staging}/codex-resources/bwrap ]; then chmod 755 {staging}/codex-resources/bwrap; fi && \
             rm -rf {release}; mv {staging} {release} && \
             ln -sfn {release}/bin/codex {bin}/codex && rm -f {archive}",
            staging = shell_quote(&staging),
            archive = shell_quote(&remote_archive),
            release = shell_quote(&release_dir),
            bin = shell_quote(&bin_dir),
        );
        let output = self.run_shell(target, &activate, false)?;
        ensure_success(&output, "activate controller-downloaded Codex package")
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

    fn remote_home(&self, target: &Target) -> Result<String> {
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
        let capture = shell_join(&["tmux", "capture-pane", "-p", "-t", session_id]);
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

    pub fn capture(&self, target: &Target, session_id: &str, lines: usize) -> Result<String> {
        Ok(self
            .capture_page(target, session_id, 0, lines, 80, 24)?
            .text)
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
        let script = format!(
            "cd {} && pwd -P && find -L . -mindepth 1 -maxdepth 1 -type d -print0",
            shell_quote(path)
        );
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "list directory")?;
        parse_directory_listing(&output.stdout)
    }

    pub fn scan_resumes(
        &self,
        target: &Target,
        kind: AgentKind,
        path: &str,
    ) -> Result<Vec<ResumeCandidate>> {
        if kind == AgentKind::Terminal {
            return Ok(Vec::new());
        }
        let root = match kind {
            AgentKind::Codex => "$HOME/.codex/sessions",
            AgentKind::Claude => "$HOME/.claude/projects",
            AgentKind::Terminal => unreachable!(),
        };
        let index = if kind == AgentKind::Codex {
            r#"printf '\036INDEX\n'; if [ -f "$HOME/.codex/session_index.jsonl" ]; then cat "$HOME/.codex/session_index.jsonl"; fi;"#
        } else {
            ""
        };
        let collect = r#"query=$1; shift; for file do if grep -F -q -- "$query" "$file"; then printf '\036SESSION\n'; sed -n '1,60p' "$file"; tail -n 80 "$file"; fi; done"#;
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

    pub fn kill(&self, target: &Target, session_id: &str) -> Result<()> {
        debug::log(
            "runtime",
            format!("kill target={} session={session_id}", target.id),
        );
        validate_session_id(session_id)?;
        let script = shell_join(&["tmux", "kill-session", "-t", session_id]);
        let output = self.run_shell(target, &script, false)?;
        ensure_success(&output, "delete agent session")
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

    fn run_shell(&self, target: &Target, script: &str, interactive: bool) -> Result<Output> {
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

fn controller_download_cache() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/muxloom/downloads")
}

fn controller_curl(environment: &[(String, String)]) -> Command {
    let mut command = Command::new("curl");
    command
        .args([
            "--connect-timeout",
            "10",
            "--speed-limit",
            "1024",
            "--speed-time",
            "60",
        ])
        .stdin(Stdio::null())
        .envs(environment.iter().cloned());
    command
}

fn parse_reverse_tunnel(value: &str) -> Option<(u16, &str, u16)> {
    let mut fields = value.trim().split(':');
    let remote_port = fields.next()?.parse().ok()?;
    let local_host = fields.next()?;
    let local_port = fields.next()?.parse().ok()?;
    (fields.next().is_none() && remote_port > 0 && local_port > 0 && !local_host.is_empty())
        .then_some((remote_port, local_host, local_port))
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
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

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!("release manifest returned an invalid SHA-256 digest")
    }
}

fn checksum_for_asset(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let checksum = fields.next()?;
        let filename = fields.next()?.trim_start_matches('*');
        (filename == asset && validate_sha256(checksum).is_ok())
            .then(|| checksum.to_ascii_lowercase())
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for checksum", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
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
    })
}

fn attention_reason(kind: AgentKind, screen: &str, patterns: &[String]) -> Option<String> {
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
                &["run this command", "run the following command"],
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
                &["allow this", "allow command", "permission"],
            ),
            ("confirmation", &["do you want to proceed", "esc to cancel"]),
        ],
        AgentKind::Terminal => &[],
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
    None
}

fn attention_tail(screen: &str) -> String {
    let lines: Vec<_> = screen.lines().collect();
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
        let candidate = match kind {
            AgentKind::Codex => parse_codex_resume(session, &normalized_path, &titles),
            AgentKind::Claude => parse_claude_resume(session, &normalized_path),
            AgentKind::Terminal => None,
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
                        .filter(|message| !message.is_empty());
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
                        .filter(|message| {
                            !message.is_empty() && !message.starts_with("<environment_context>")
                        });
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
        recap,
        first_message: first_message.or(fallback_first),
        last_message: last_message.or(fallback_last),
        updated_at,
    })
}

fn parse_claude_resume(session: &str, path: &str) -> Option<ResumeCandidate> {
    let mut id = None;
    let mut cwd = None;
    let mut updated_at = String::new();
    let mut first_message = None;
    let mut last_message = None;
    let mut summary = None;
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
        if summary.is_none() {
            summary = value
                .get("summary")
                .or_else(|| value.get("customTitle"))
                .and_then(Value::as_str)
                .map(clean_recap);
        }
        if value.get("type").and_then(Value::as_str) == Some("user") {
            let message = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(extract_message_text)
                .map(|message| clean_recap(&message))
                .filter(|message| !message.is_empty());
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
        recap: summary.filter(|message| !message.is_empty()),
        first_message,
        last_message,
        updated_at,
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

fn command_line(command: &CommandConfig, kind: AgentKind, resume_id: Option<&str>) -> String {
    let mut values = Vec::with_capacity(command.args.len() + 3);
    values.push(command.command.as_str());
    values.extend(command.args.iter().map(String::as_str));
    if let Some(resume_id) = resume_id {
        match kind {
            AgentKind::Codex => values.extend(["resume", resume_id]),
            AgentKind::Claude => values.extend(["--resume", resume_id]),
            AgentKind::Terminal => {}
        }
    }
    shell_join(&values)
}

fn interactive_shell_command(command: &str) -> String {
    format!("exec {}", login_shell_command(command))
}

fn login_shell_command(command: &str) -> String {
    format!("\"${{SHELL:-/bin/sh}}\" -lc {}", shell_quote(command))
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

fn local_runtime_can_copy(kind: AgentKind, binary: &Path) -> bool {
    kind != AgentKind::Codex
        || std::env::consts::OS != "linux"
        || find_codex_resource(binary, "bwrap").is_some()
}

fn find_codex_resource(binary: &Path, name: &str) -> Option<PathBuf> {
    for ancestor in binary.parent()?.ancestors().take(7) {
        for relative in [
            PathBuf::from("codex-resources").join(name),
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

fn parse_discovery(target_id: &str, output: &str) -> Result<(Probe, Vec<AgentSession>)> {
    let mut probe = Probe::default();
    let mut sessions = Vec::new();
    for line in output.lines() {
        match line {
            "tmux=1" => probe.tmux = true,
            "codex=1" => probe.codex = true,
            "claude=1" => probe.claude = true,
            "tmux=0" | "codex=0" | "claude=0" => {}
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
                    needs_attention: false,
                    attention_reason: None,
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

fn validate_session_id(session_id: &str) -> Result<()> {
    if is_managed_session_id(session_id) {
        Ok(())
    } else {
        bail!("refusing invalid Muxloom session id")
    }
}

pub(crate) fn is_managed_session_id(session_id: &str) -> bool {
    (session_id.starts_with(SESSION_PREFIX) || session_id.starts_with(LEGACY_SESSION_PREFIX))
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub fn shell_join(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
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
    fn maps_release_platforms_and_checks_checksum_manifests() {
        let linux = TargetPlatform {
            os: "linux".into(),
            arch: "x86_64".into(),
            musl: false,
        };
        assert_eq!(linux.claude_name().unwrap(), "linux-x64");
        assert_eq!(linux.codex_name().unwrap(), "x86_64-unknown-linux-musl");
        let alpine = TargetPlatform {
            musl: true,
            ..linux
        };
        assert_eq!(alpine.claude_name().unwrap(), "linux-x64-musl");
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
            "muxloom-codex-10-2\tcodex\t/work/a b\talpha\t10\t\t\t\t\t0\t123\n",
            "ad-claude-11-2\t\t\t\t\tclaude\t/work/remote\tdone\t11\t1\t456\n"
        );
        let (probe, sessions) = parse_discovery("gpu", output).unwrap();
        assert!(probe.tmux && probe.codex && !probe.claude);
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
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T09:00:00Z\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first codex prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Codex, "/work/project/", codex);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "codex-id");
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
            "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/work/project\",\"timestamp\":\"2026-07-20T11:00:00Z\",\"message\":{\"content\":\"first claude prompt\"}}\n"
        );
        let candidates = parse_resume_candidates(AgentKind::Claude, "/work/project", claude);
        assert_eq!(candidates.len(), 1);
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

    #[test]
    fn builds_runtime_specific_resume_commands() {
        let command = CommandConfig {
            command: "codex".into(),
            args: vec!["--full-auto".into()],
            ..CommandConfig::default()
        };
        assert_eq!(
            command_line(&command, AgentKind::Codex, Some("session id")),
            "codex --full-auto resume 'session id'"
        );
        let command = CommandConfig {
            command: "claude".into(),
            args: Vec::new(),
            ..CommandConfig::default()
        };
        assert_eq!(
            command_line(&command, AgentKind::Claude, Some("abc")),
            "claude --resume abc"
        );
    }
}
