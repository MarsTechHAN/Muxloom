use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, LOCAL_TARGET_ID};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub refresh_interval_ms: u64,
    pub ssh_connect_timeout_secs: u64,
    pub history_limit: usize,
    pub history_chunk_lines: usize,
    pub attention_patterns: Vec<String>,
    pub ssh_config: String,
    pub environment: String,
    pub reverse_tunnel: String,
    pub companion_command: String,
    pub agents: AgentCommands,
    pub hosts: BTreeMap<String, HostConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_interval_ms: 5_000,
            ssh_connect_timeout_secs: 5,
            history_limit: 1_000_000,
            history_chunk_lines: 500,
            attention_patterns: vec![
                "do you want to".into(),
                "would you like to".into(),
                "allow command".into(),
                "approve".into(),
                "waiting for your input".into(),
                "press enter to confirm".into(),
            ],
            ssh_config: "~/.ssh/config".into(),
            environment: String::new(),
            reverse_tunnel: String::new(),
            companion_command: "muxloomd".into(),
            agents: AgentCommands::default(),
            hosts: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCommands {
    pub codex: CommandConfig,
    pub claude: CommandConfig,
    pub terminal: CommandConfig,
}

impl Default for AgentCommands {
    fn default() -> Self {
        Self {
            codex: CommandConfig {
                command: "codex".into(),
                args: Vec::new(),
                install: "curl -fsSL https://chatgpt.com/codex/install.sh | sh".into(),
                sync_files: vec!["~/.codex/config.toml".into(), "~/.codex/auth.json".into()],
            },
            claude: CommandConfig {
                command: "claude".into(),
                args: Vec::new(),
                install: "curl -fsSL https://claude.ai/install.sh | bash".into(),
                sync_files: vec!["~/.claude/settings.json".into()],
            },
            terminal: CommandConfig::default(),
        }
    }
}

impl AgentCommands {
    pub fn get(&self, kind: AgentKind) -> &CommandConfig {
        match kind {
            AgentKind::Codex => &self.codex,
            AgentKind::Claude => &self.claude,
            AgentKind::Terminal => &self.terminal,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandConfig {
    pub command: String,
    pub args: Vec<String>,
    pub install: String,
    pub sync_files: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    pub codex: Option<CommandConfig>,
    pub claude: Option<CommandConfig>,
    pub terminal: Option<CommandConfig>,
    pub environment: Option<String>,
    pub reverse_tunnel: Option<String>,
    pub companion_command: Option<String>,
    pub attention_patterns: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let mut parsed = BTreeMap::new();
        parse_environment(&self.environment, &mut parsed)?;
        validate_reverse_tunnel(&self.reverse_tunnel)?;
        if self.companion_command.trim().is_empty() {
            anyhow::bail!("companion command cannot be empty");
        }
        for (host, host_config) in &self.hosts {
            if let Some(environment) = &host_config.environment {
                parse_environment(environment, &mut parsed)
                    .with_context(|| format!("invalid environment for host {host}"))?;
            }
            if let Some(tunnel) = &host_config.reverse_tunnel {
                validate_reverse_tunnel(tunnel)
                    .with_context(|| format!("invalid reverse tunnel for host {host}"))?;
            }
            if host_config
                .companion_command
                .as_deref()
                .is_some_and(|command| command.trim().is_empty())
            {
                anyhow::bail!("companion command cannot be empty for host {host}");
            }
        }
        Ok(())
    }

    pub fn command_for(&self, host: &str, kind: AgentKind) -> &CommandConfig {
        let override_command = self.hosts.get(host).and_then(|host| match kind {
            AgentKind::Codex => host.codex.as_ref(),
            AgentKind::Claude => host.claude.as_ref(),
            AgentKind::Terminal => host.terminal.as_ref(),
        });
        override_command.unwrap_or_else(|| self.agents.get(kind))
    }

    pub fn attention_patterns_for(&self, host: &str) -> &[String] {
        self.hosts
            .get(host)
            .and_then(|host| host.attention_patterns.as_deref())
            .unwrap_or(&self.attention_patterns)
    }

    pub fn environment_for(&self, host: &str) -> Result<Vec<(String, String)>> {
        let mut environment = BTreeMap::new();
        parse_environment(&self.environment, &mut environment)?;
        if let Some(host_environment) = self
            .hosts
            .get(host)
            .and_then(|config| config.environment.as_deref())
        {
            parse_environment(host_environment, &mut environment)?;
        }
        Ok(environment.into_iter().collect())
    }

    pub fn reverse_tunnel_for(&self, host: &str) -> &str {
        self.hosts
            .get(host)
            .and_then(|config| config.reverse_tunnel.as_deref())
            .unwrap_or(&self.reverse_tunnel)
    }

    pub fn companion_command_for(&self, host: &str) -> &str {
        self.hosts
            .get(host)
            .and_then(|config| config.companion_command.as_deref())
            .unwrap_or(&self.companion_command)
    }

    pub fn ssh_config_path(&self) -> PathBuf {
        expand_tilde(&self.ssh_config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(path, format!("{text}\n"))
            .with_context(|| format!("failed to write config {}", path.display()))
    }
}

fn parse_environment(value: &str, output: &mut BTreeMap<String, String>) -> Result<()> {
    for assignment in shell_words::split(value).context("invalid environment quoting")? {
        let Some((name, value)) = assignment.split_once('=') else {
            anyhow::bail!("environment entry must use NAME=value: {assignment}");
        };
        if !valid_environment_name(name) {
            anyhow::bail!("invalid environment variable name: {name}");
        }
        output.insert(name.to_string(), value.to_string());
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn validate_reverse_tunnel(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let fields: Vec<_> = value.split(':').collect();
    if fields.len() != 3 {
        anyhow::bail!("reverse tunnel must use REMOTE_PORT:LOCAL_HOST:LOCAL_PORT");
    }
    let remote_port: u16 = fields[0]
        .parse()
        .context("invalid reverse tunnel remote port")?;
    let local_port: u16 = fields[2]
        .parse()
        .context("invalid reverse tunnel local port")?;
    if remote_port == 0 || local_port == 0 || fields[1].trim().is_empty() {
        anyhow::bail!("reverse tunnel ports and local host must be non-empty");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub enabled_hosts: BTreeSet<String>,
    pub flatten: bool,
    pub hide_disabled: bool,
    pub machine_width: u16,
    pub agents_width: u16,
    pub file_width: u16,
    pub portrait_terminal_percent: u16,
    pub portrait_machine_percent: u16,
    pub show_archived: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            enabled_hosts: BTreeSet::new(),
            flatten: false,
            hide_disabled: false,
            machine_width: 24,
            agents_width: 40,
            file_width: 34,
            portrait_terminal_percent: 65,
            portrait_machine_percent: 45,
            show_archived: false,
        }
    }
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let mut state = Self::default();
            state.enabled_hosts.insert(LOCAL_TARGET_ID.into());
            return Ok(state);
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read state {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("invalid state JSON in {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, format!("{text}\n"))
            .with_context(|| format!("failed to write state {}", path.display()))
    }
}

pub fn default_config_path() -> PathBuf {
    home_dir().join(".config/muxloom/config.toml")
}

pub fn default_state_path() -> PathBuf {
    home_dir().join(".local/state/muxloom/state.json")
}

pub fn default_debug_log_path() -> PathBuf {
    home_dir().join(".local/state/muxloom/debug.log")
}

pub fn legacy_config_path() -> PathBuf {
    home_dir().join(".config/agent-deck/config.toml")
}

pub fn legacy_state_path() -> PathBuf {
    home_dir().join(".local/state/agent-deck/state.json")
}

pub fn migrate_legacy_file(legacy: &Path, current: &Path) -> Result<bool> {
    if current.exists() || !legacy.exists() {
        return Ok(false);
    }
    if let Some(parent) = current.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(legacy, current).with_context(|| {
        format!(
            "failed to migrate legacy file {} to {}",
            legacy.display(),
            current.display()
        )
    })?;
    Ok(true)
}

pub fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        home_dir()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(value)
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub const EXAMPLE_CONFIG: &str = r#"# muxloom configuration
refresh_interval_ms = 5000
ssh_connect_timeout_secs = 5
history_limit = 1000000
history_chunk_lines = 500
attention_patterns = ["do you want to", "would you like to", "allow command", "approve", "waiting for your input", "press enter to confirm"]
ssh_config = "~/.ssh/config"
environment = ""
reverse_tunnel = ""
companion_command = "muxloomd"

[agents.codex]
command = "codex"
args = []
install = "curl -fsSL https://chatgpt.com/codex/install.sh | sh"
sync_files = ["~/.codex/config.toml", "~/.codex/auth.json"]

[agents.claude]
command = "claude"
args = []
install = "curl -fsSL https://claude.ai/install.sh | bash"
sync_files = ["~/.claude/settings.json"]

# Empty command means the user's SHELL (or /bin/sh).
[agents.terminal]
command = ""
args = []
install = ""
sync_files = []

# Commands can be overridden per SSH alias. Arguments are passed literally.
# [hosts.gpu-box]
# environment = 'HTTP_PROXY=http://proxy:8118 HTTPS_PROXY=http://proxy:8118 NO_PROXY="localhost,.internal"'
# reverse_tunnel = "18118:127.0.0.1:8118"
# companion_command = "~/.local/bin/muxloomd"
# attention_patterns = ["approve", "do you want to proceed"]

# [hosts.gpu-box.claude]
# command = "/opt/claude/bin/claude"
# args = ["--dangerously-skip-permissions"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_command_overrides_default() {
        let mut config = Config::default();
        config.hosts.insert(
            "gpu".into(),
            HostConfig {
                codex: Some(CommandConfig {
                    command: "/opt/codex".into(),
                    args: vec!["--yolo".into()],
                    ..CommandConfig::default()
                }),
                claude: None,
                terminal: None,
                environment: None,
                reverse_tunnel: None,
                companion_command: None,
                attention_patterns: None,
            },
        );
        assert_eq!(
            config.command_for("gpu", AgentKind::Codex).command,
            "/opt/codex"
        );
        assert_eq!(
            config.command_for("gpu", AgentKind::Claude).command,
            "claude"
        );
        assert_eq!(
            config.attention_patterns_for("gpu"),
            config.attention_patterns.as_slice()
        );
        config.hosts.get_mut("gpu").unwrap().attention_patterns =
            Some(vec!["machine prompt".into()]);
        assert_eq!(config.attention_patterns_for("gpu"), ["machine prompt"]);
    }

    #[test]
    fn migrates_legacy_files_without_overwriting_current_state() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("muxloom-migration-{nonce}"));
        let legacy = root.join("agent-deck/state.json");
        let current = root.join("muxloom/state.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "legacy-state").unwrap();

        assert!(migrate_legacy_file(&legacy, &current).unwrap());
        assert_eq!(fs::read_to_string(&current).unwrap(), "legacy-state");

        fs::write(&current, "current-state").unwrap();
        assert!(!migrate_legacy_file(&legacy, &current).unwrap());
        assert_eq!(fs::read_to_string(&current).unwrap(), "current-state");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn machine_environment_overrides_global_shell_assignments() {
        let mut config = Config {
            environment: "HTTP_PROXY=http://global TOKEN='two words'".into(),
            reverse_tunnel: "18118:127.0.0.1:8118".into(),
            ..Config::default()
        };
        config.hosts.insert(
            "gpu".into(),
            HostConfig {
                environment: Some("HTTP_PROXY=http://gpu EXTRA=yes".into()),
                reverse_tunnel: None,
                ..HostConfig::default()
            },
        );
        assert_eq!(
            config.environment_for("gpu").unwrap(),
            [
                ("EXTRA".into(), "yes".into()),
                ("HTTP_PROXY".into(), "http://gpu".into()),
                ("TOKEN".into(), "two words".into()),
            ]
        );
        config.environment = "NOT_AN_ASSIGNMENT".into();
        assert!(config.environment_for("gpu").is_err());
    }

    #[test]
    fn validates_and_overrides_reverse_tunnels() {
        let mut config = Config {
            reverse_tunnel: "18118:127.0.0.1:8118".into(),
            ..Config::default()
        };
        assert!(config.validate().is_ok());
        assert_eq!(config.reverse_tunnel_for("gpu"), "18118:127.0.0.1:8118");
        config.hosts.insert(
            "gpu".into(),
            HostConfig {
                reverse_tunnel: Some("28118:proxy.local:8118".into()),
                ..HostConfig::default()
            },
        );
        assert_eq!(config.reverse_tunnel_for("gpu"), "28118:proxy.local:8118");
        config.hosts.get_mut("gpu").unwrap().reverse_tunnel = Some("bad tunnel".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn old_state_files_receive_new_portrait_divider_defaults() {
        let state: State =
            serde_json::from_str(r#"{"enabled_hosts":[],"machine_width":30,"agents_width":44}"#)
                .unwrap();
        assert_eq!(state.portrait_terminal_percent, 65);
        assert_eq!(state.portrait_machine_percent, 45);
        assert_eq!(state.file_width, 34);
    }
}
