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
            },
            claude: CommandConfig {
                command: "claude".into(),
                args: Vec::new(),
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    pub codex: Option<CommandConfig>,
    pub claude: Option<CommandConfig>,
    pub terminal: Option<CommandConfig>,
    pub attention_patterns: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub enabled_hosts: BTreeSet<String>,
    pub flatten: bool,
    pub hide_disabled: bool,
    pub machine_width: u16,
    pub agents_width: u16,
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

[agents.codex]
command = "codex"
args = []

[agents.claude]
command = "claude"
args = []

# Empty command means the user's login shell.
[agents.terminal]
command = ""
args = []

# Commands can be overridden per SSH alias. Arguments are passed literally.
# [hosts.gpu-box]
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
                }),
                claude: None,
                terminal: None,
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
}
