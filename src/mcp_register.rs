//! Registering this machine's control surface with the agents that run on it.
//!
//! An agent muxloom starts is meant to be able to see and drive the other
//! sessions — its own machine's and, through them, the work it handed off — but
//! only if it has been told the surface exists. Wiring that up by hand on every
//! machine is exactly the chore muxloom exists to remove, so the daemon does it
//! for the user it runs as: on start it writes a `muxloom` entry into Claude
//! Code's and Codex's user-level MCP configuration, pointing at itself.
//!
//! These files belong to the user, not to muxloom. Nothing else in them is
//! touched, the entry is rewritten only when it is missing or points somewhere
//! else, a file that does not parse is left exactly as it is, and
//! `MUXLOOM_MCP_REGISTER=0` turns the whole thing off.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The name the entry carries in both agents' configuration.
const SERVER_NAME: &str = "muxloom";

/// What the daemon wants both agents to know about it.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    pub command: String,
    pub args: Vec<String>,
    /// Passed to the server process; carries the state directory when the
    /// daemon is not running out of the default one.
    pub environment: BTreeMap<String, String>,
}

impl ServerEntry {
    /// The entry for the currently running binary, serving its own state.
    pub fn for_this_daemon() -> Result<Self> {
        let command = std::env::current_exe()
            .context("failed to locate the running muxloomd")?
            .to_string_lossy()
            .into_owned();
        let mut environment = BTreeMap::new();
        if let Some(state_dir) = std::env::var_os("MUXLOOMD_STATE_DIR") {
            environment.insert(
                "MUXLOOMD_STATE_DIR".into(),
                state_dir.to_string_lossy().into_owned(),
            );
        }
        Ok(Self {
            command,
            args: vec!["mcp".into()],
            environment,
        })
    }
}

/// Write the entry into every agent configuration under `home`, reporting the
/// files that were changed. Agents that are not installed are set up anyway:
/// the file is theirs to read whenever they first run.
pub fn register(home: &Path, entry: &ServerEntry) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    let claude = home.join(".claude.json");
    if register_with_claude(&claude, entry)? {
        written.push(claude);
    }
    let codex = home.join(".codex").join("config.toml");
    if register_with_codex(&codex, entry)? {
        written.push(codex);
    }
    Ok(written)
}

/// Register with the daemon's own user, unless that was turned off. Failures
/// are the caller's to report: the daemon serves with or without this.
pub fn register_for_this_daemon() -> Result<Vec<PathBuf>> {
    if std::env::var("MUXLOOM_MCP_REGISTER").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    }) {
        return Ok(Vec::new());
    }
    let Some(home) = home_directory() else {
        bail!("no home directory to register an MCP server in");
    };
    register(&home, &ServerEntry::for_this_daemon()?)
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Claude Code keeps its user-scope servers under `mcpServers` in
/// `~/.claude.json`, a file it also uses for its own state — so it is read,
/// amended and written back whole rather than generated.
fn register_with_claude(path: &Path, entry: &ServerEntry) -> Result<bool> {
    let mut root = match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => json!({}),
        Ok(text) => serde_json::from_str::<Value>(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let Some(object) = root.as_object_mut() else {
        bail!("{} does not hold a JSON object", path.display());
    };
    let mut desired = json!({
        "type": "stdio",
        "command": entry.command,
        "args": entry.args,
    });
    if !entry.environment.is_empty() {
        desired["env"] = json!(entry.environment);
    }
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("mcpServers in {} is not an object", path.display()))?;
    if servers.get(SERVER_NAME) == Some(&desired) {
        return Ok(false);
    }
    servers.insert(SERVER_NAME.into(), desired);
    let mut text = serde_json::to_string_pretty(&root).context("failed to encode the config")?;
    text.push('\n');
    write_atomically(path, text.as_bytes())?;
    Ok(true)
}

/// Codex keeps its servers in `[mcp_servers.<name>]` tables in
/// `~/.codex/config.toml`. That file is hand-written and commented, so only
/// our own tables are rewritten, textually, and the result has to parse before
/// it is allowed to replace what the user had.
fn register_with_codex(path: &Path, entry: &ServerEntry) -> Result<bool> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let parsed: toml::Value = toml::from_str(&existing)
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    let current = parsed
        .get("mcp_servers")
        .and_then(|servers| servers.get(SERVER_NAME));
    if current.is_some_and(|current| codex_entry_matches(current, entry)) {
        return Ok(false);
    }

    let mut text = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping = is_our_table(trimmed);
        }
        if !skipping {
            text.push_str(line);
            text.push('\n');
        }
    }
    while text.ends_with("\n\n") {
        text.pop();
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&codex_table(entry));

    if toml::from_str::<toml::Value>(&text).is_err() {
        bail!(
            "leaving {} alone: rewriting it would not have parsed",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_atomically(path, text.as_bytes())?;
    Ok(true)
}

fn is_our_table(header: &str) -> bool {
    let Some(name) = header
        .strip_prefix('[')
        .and_then(|rest| rest.split(']').next())
    else {
        return false;
    };
    let name = name.trim().replace('"', "");
    let ours = format!("mcp_servers.{SERVER_NAME}");
    name == ours || name.starts_with(&format!("{ours}."))
}

fn codex_table(entry: &ServerEntry) -> String {
    let mut table = format!(
        "[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [{}]\n",
        toml_string(&entry.command),
        entry
            .args
            .iter()
            .map(|argument| toml_string(argument))
            .collect::<Vec<_>>()
            .join(", "),
    );
    if !entry.environment.is_empty() {
        table.push_str(&format!("\n[mcp_servers.{SERVER_NAME}.env]\n"));
        for (name, value) in &entry.environment {
            table.push_str(&format!("{name} = {}\n", toml_string(value)));
        }
    }
    table
}

fn codex_entry_matches(current: &toml::Value, entry: &ServerEntry) -> bool {
    let command = current.get("command").and_then(toml::Value::as_str);
    if command != Some(entry.command.as_str()) {
        return false;
    }
    let arguments: Vec<&str> = current
        .get("args")
        .and_then(toml::Value::as_array)
        .map(|args| args.iter().filter_map(toml::Value::as_str).collect())
        .unwrap_or_default();
    if arguments != entry.args {
        return false;
    }
    let environment: BTreeMap<String, String> = current
        .get("env")
        .and_then(toml::Value::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    environment == entry.environment
}

fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Replace a file the user also writes to without ever leaving a half-written
/// one behind: the new text lands beside it and is renamed over it.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("muxloom-{}", std::process::id()));
    fs::write(&temporary, bytes)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace {}", path.display()))
        .inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> ServerEntry {
        ServerEntry {
            command: "/opt/muxloomd".into(),
            args: vec!["mcp".into()],
            environment: BTreeMap::new(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("muxloom-register-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn both_agents_learn_about_the_daemon_and_stop_being_rewritten_after_that() {
        let home = scratch("fresh");
        let written = register(&home, &entry()).unwrap();
        assert_eq!(written.len(), 2);

        let claude: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["muxloom"]["command"], "/opt/muxloomd");
        assert_eq!(claude["mcpServers"]["muxloom"]["args"][0], "mcp");
        let codex: toml::Value =
            toml::from_str(&fs::read_to_string(home.join(".codex/config.toml")).unwrap()).unwrap();
        assert_eq!(
            codex["mcp_servers"]["muxloom"]["command"].as_str(),
            Some("/opt/muxloomd")
        );

        // Nothing to say the second time.
        assert!(register(&home, &entry()).unwrap().is_empty());

        // A daemon that moved is followed, not duplicated.
        let moved = ServerEntry {
            command: "/usr/local/bin/muxloomd".into(),
            ..entry()
        };
        assert_eq!(register(&home, &moved).unwrap().len(), 2);
        let text = fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert_eq!(text.matches("[mcp_servers.muxloom]").count(), 1);
        assert!(text.contains("/usr/local/bin/muxloomd"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn everything_the_user_already_had_survives() {
        let home = scratch("populated");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"numStartups": 7, "mcpServers": {"other": {"command": "other-server"}}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"other-server\"\n",
        )
        .unwrap();

        register(&home, &entry()).unwrap();

        let claude: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(claude["numStartups"], 7);
        assert_eq!(claude["mcpServers"]["other"]["command"], "other-server");
        assert_eq!(claude["mcpServers"]["muxloom"]["command"], "/opt/muxloomd");

        let text = fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(text.contains("# my settings"), "{text}");
        let codex: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(codex["model"].as_str(), Some("gpt-5"));
        assert_eq!(
            codex["mcp_servers"]["other"]["command"].as_str(),
            Some("other-server")
        );
        assert_eq!(
            codex["mcp_servers"]["muxloom"]["args"][0].as_str(),
            Some("mcp")
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_state_directory_the_daemon_was_given_is_passed_on() {
        let home = scratch("state-dir");
        let mut entry = entry();
        entry
            .environment
            .insert("MUXLOOMD_STATE_DIR".into(), "/tmp/state".into());
        register(&home, &entry).unwrap();

        let claude: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(
            claude["mcpServers"]["muxloom"]["env"]["MUXLOOMD_STATE_DIR"],
            "/tmp/state"
        );
        let codex: toml::Value =
            toml::from_str(&fs::read_to_string(home.join(".codex/config.toml")).unwrap()).unwrap();
        assert_eq!(
            codex["mcp_servers"]["muxloom"]["env"]["MUXLOOMD_STATE_DIR"].as_str(),
            Some("/tmp/state")
        );
        assert!(register(&home, &entry).unwrap().is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_config_that_does_not_parse_is_left_exactly_as_it_was() {
        let home = scratch("broken");
        fs::write(home.join(".claude.json"), "{ not json").unwrap();
        assert!(register(&home, &entry()).is_err());
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            "{ not json"
        );
        let _ = fs::remove_dir_all(&home);
    }
}
