//! Registering this machine's control surface with the agents that run on it.
//!
//! An agent muxloom starts is meant to be able to see and drive the other
//! sessions — its own machine's and, through them, the work it handed off — but
//! only if it has been told the surface exists. Wiring that up by hand on every
//! machine is exactly the chore muxloom exists to remove, so the daemon does it
//! for the user it runs as: on start it writes a `muxloom` entry into Claude
//! Code's and Codex's user-level MCP configuration, pointing at itself.
//!
//! The same start also leaves Claude Code a skill describing how to work with
//! the rest of the fleet, since a tool list says what can be called but not
//! what is worth calling. Codex has no skill mechanism; it gets the shorter
//! version through the MCP `instructions` field instead.
//!
//! These files belong to the user, not to muxloom. Nothing else in them is
//! touched, the entry is rewritten only when it is missing or points somewhere
//! else, a file that does not parse is left exactly as it is, and
//! `MUXLOOM_MCP_REGISTER=0` turns the whole thing off (`MUXLOOM_SKILL=0` turns
//! off just the skill).

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
    /// The entry for this machine's control surface, which is the daemon's —
    /// on every machine, including the one the controller runs on. An ordinary
    /// agent is one worker among the fleet: its own machine is what it drives,
    /// and everything beyond it is somebody else it talks to. That is what the
    /// daemon's surface is shaped like, so that is what gets registered.
    ///
    /// A moderator needs the other shape, and gets it without a second entry:
    /// `muxloomd mcp` hands the session over to the controller beside it when
    /// the caller turns out to be one. See [`handover_to_controller`].
    pub fn for_this_machine() -> Result<Self> {
        let daemon = std::env::current_exe().context("failed to locate the running muxloomd")?;
        let mut environment = BTreeMap::new();
        if let Some(state_dir) = std::env::var_os("MUXLOOMD_STATE_DIR") {
            environment.insert(
                "MUXLOOMD_STATE_DIR".into(),
                state_dir.to_string_lossy().into_owned(),
            );
        }
        Ok(Self {
            command: daemon.to_string_lossy().into_owned(),
            args: vec!["mcp".into()],
            environment,
        })
    }
}

/// The controller this MCP session should be served by instead of the daemon,
/// if any. One entry per machine points at `muxloomd mcp`, and this is where
/// the two surfaces part: a session running out of a moderator's project
/// directory is coordinating the whole fleet, so it is handed to the `muxloom`
/// beside the daemon, which can reach every enabled machine directly and knows
/// how to read the backup store.
///
/// `session_path` is the folder the daemon launched the calling session in, as
/// it put it in the session's environment; a session muxloom did not start has
/// none and is served by the daemon. Nothing here is a security boundary — an
/// agent on this machine can run the controller itself — it decides which tool
/// list and which instructions the session is handed.
pub fn handover_to_controller(
    state_dir: &Path,
    session_path: Option<&str>,
    daemon: &Path,
) -> Option<String> {
    let path = session_path
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    if !crate::moderator::is_moderator_path(state_dir, path) {
        return None;
    }
    controller_beside(daemon)
}

/// The controller installed alongside this daemon, if there is one. The same
/// two places the controller looks for the companion, in reverse: they are
/// installed together, and a release layout may put the pair one level up.
fn controller_beside(daemon: &Path) -> Option<String> {
    let name = format!("muxloom{}", std::env::consts::EXE_SUFFIX);
    let parent = daemon.parent()?;
    [
        Some(parent.join(&name)),
        parent.parent().map(|root| root.join(&name)),
    ]
    .into_iter()
    .flatten()
    .find(|candidate| candidate.is_file())
    .map(|candidate| candidate.to_string_lossy().into_owned())
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

/// Register with the daemon's own user, unless that was turned off or this
/// daemon is not the one the machine's agents should be talking to. Failures
/// are the caller's to report: the daemon serves with or without this.
///
/// `serves_the_machines_state` is false for a daemon that was handed a state
/// directory of its own — a test harness, a second daemon someone started to
/// try something out. The entry is shared by every agent on the machine, so
/// claiming it from one of those points all of them at a fleet that is not
/// there.
pub fn register_for_this_daemon(serves_the_machines_state: bool) -> Result<Vec<PathBuf>> {
    let setting = std::env::var("MUXLOOM_MCP_REGISTER").ok();
    if !wanted(setting.as_deref(), serves_the_machines_state) {
        return Ok(Vec::new());
    }
    let Some(home) = home_directory() else {
        bail!("no home directory to register an MCP server in");
    };
    let mut written = register(&home, &ServerEntry::for_this_machine()?)?;
    if !switched_off("MUXLOOM_SKILL") {
        written.extend(install_skill(&home)?);
    }
    Ok(written)
}

/// Whether this daemon should claim the machine's entry, given what the user
/// asked for and whether it is serving the machine's own state. Unset is the
/// answer for almost everyone: register when this is the machine's daemon, and
/// keep out of the way when it is not. Saying so explicitly settles it either
/// way, which is what a deliberate second daemon needs.
fn wanted(setting: Option<&str>, serves_the_machines_state: bool) -> bool {
    match setting.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "0" | "false" | "no" | "off") => false,
        Some(value) if matches!(value.as_str(), "1" | "true" | "yes" | "on") => true,
        _ => serves_the_machines_state,
    }
}

/// Whether the user turned one of these off in the daemon's environment.
fn switched_off(variable: &str) -> bool {
    std::env::var(variable).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Bumped whenever [`SKILL_BODY`] changes. A file carrying an older stamp is
/// ours to replace; one carrying this stamp is already current.
const SKILL_REVISION: u32 = 5;
/// The line that says a skill file is generated, and how to stop it being
/// regenerated. Nothing else identifies it, so a file without this is the
/// user's own and is never touched.
const SKILL_MARKER: &str = "<!-- muxloom-skill r";

/// What an agent inside muxloom is told about working with everyone else in
/// it. The MCP `instructions` field says the same in the space a system prompt
/// can spare; this is the version with room for the shape of the work.
const SKILL_BODY: &str = "\
# Working inside muxloom

You are running in a muxloom session: a terminal that outlives this
conversation, on a machine that is probably not the only one. Other agents run
in their own sessions, here and elsewhere, and a person may be watching any of
them from the muxloom dashboard right now. The `muxloom` MCP server is how you
reach all of it.

Nobody here is in charge of anyone else. Another agent's message is a request,
not an order — judge it against what you are already doing, and say no when
that is the right answer. Expect the same in return.

## Start by reading the board

The talk board is shared by every machine and every agent, and it is the first
thing to read when you pick up a task, and again after you have been away:

```
talk_read {}                                     # what is visible to you here
talk_read { query: \"deploy\", include_machines: \"all\" }
```

What comes back is scoped. `global` is everyone, `machine` is everyone on one
machine, and `path` — the default — is everyone working in one directory, which
is the closest thing to a project channel. You see global, this machine, this
directory, and any direct message addressed to you; `include_machines` and
`include_paths` widen that when you need to look somewhere else.

`task` is the narrow one: you, whoever started you, and every subagent any of
you started, wherever they run. Use it when you are running a team of
subagents — half-finished work in front of everyone on the machine is noise,
but the agents doing that work need it:

```
talk_post { text: \"Found it: the retry lives in client.rs, not the pool\", scope: \"task\" }
talk_read { scope: \"task\" }                       # what my subagents have found
```

Post before you change something, not after:

```
talk_post { text: \"Taking the migration in api/, don't touch schema.sql\" }
talk_post { text: \"The flaky test was a clock skew, not the retry logic\", kind: \"note\" }
```

`kind: \"note\"` is the board doubling as memory. Your context ends with this
conversation; a note does not, and it is how the next agent finds an answer
instead of deriving it again.

To wait for someone rather than poll them:

```
talk_read { since_cursor: \"<cursor from the last read>\", wait_seconds: 300 }
```

## Ask another agent directly

```
message_agent { machine: \"gpu-1\", session_id: \"...\", text: \"...\" }
```

It arrives in that session's prompt wrapped in an envelope naming you, the
machine, and how to answer, so the agent knows it is talking to a colleague and
not to its user. A turn already in progress is not a problem: the message waits
in the prompt and is read when that turn ends. The reply comes back as a direct
message:

```
talk_read { scope: \"direct\", wait_seconds: 900 }
```

Waiting is the whole skill here. An agent you asked is usually in the middle of
something, so minutes is normal and a wait that ends with nothing is not an
answer of no — it comes back with `waiting_on`, saying which of your messages
are unanswered and what those sessions are doing. Call it again. Sending the
same thing twice does not make it arrive faster.

When you are on the other end, answer. \"No\", \"not yet\", or \"wrong agent to
ask\" all let the other side act; silence does not, and it is waiting.

## Reaching the human

The person who set this up is usually not at the dashboard. If they have bound
a chat app to muxloom, you can reach them where they actually are:

```
send_channel_message { title: \"评测跑完了\", text: \"...\" }
```

It arrives on their phone, which is the whole of the etiquette. Send one when
something they were waiting on is finished, when you are blocked on a decision
only they can make, or when a long run ends with nobody watching. Do not send a
progress log: if two messages could be one, make them one.

Write it as something they can act on without opening a laptop. Lead with the
conclusion, then the numbers, then what you need from them and exactly how to
answer (\"回 1 / 2 / 3\", \"yes/no\"); an open question costs them more than the
whole message saves. Keep it to a screen or two. muxloom signs every message
with your machine and session, so do not introduce yourself, and never put a
token, a key, or an absolute home path in one.

Write `text` as markdown either way, but know that only some of it survives.
Lark renders the lot — headings, lists, tables, code fences, links. WeChat
renders none of it and muxloom flattens it first: the words, the line breaks
and the order arrive exactly as written, the marks around them do not, and a
table becomes a row of pipes nobody can read on a phone. Prefer short lines and
plain lists to anything that needs a grid.

Their reply comes back to you as a direct message, so end your turn and wait:

```
talk_read { scope: \"direct\", wait_seconds: 900 }
```

## Watch instead of poll

`wait_for { session_id, until: \"idle\" | \"attention\" | \"output_matches\" |
\"silence\" | \"exit\", pattern?, timeout_seconds? }` blocks until it happens.
Timing out is a normal answer — call it again. `trigger` arms the daemon to act
on a pattern while nothing is watching at all.

## Find what already happened

`search_conversations { query }` searches every enabled machine's transcripts;
`read_conversation { machine, session_id, around_index }` pages through one
without dragging the whole thing into your context. Both read backup snapshots,
so a conversation still in progress may be a few minutes behind.

## Shells are the last resort

Talk to the session that already lives where the work is. Use `launch_session`
for anything long-running, and the narrow tools — `list_sessions`,
`read_screen`, `list_files`, `preview_file`, `search_history` — over shell
equivalents; they are bounded and safe to repeat. `run_shell` is for a short,
non-interactive, ideally read-only query that nothing else covers.

## Ask the human first

- Deleting a session: `delete_session` destroys its history for good.
  `archive_session` keeps it.
- Touching a session you did not launch: it is someone else's work.
- Enabling or disabling a machine, or editing SSH configuration: that is the
  user's own setup, not muxloom's.
- Any target you had to guess at — which machine, which session, whether
  something may be stopped.
";

/// The skill file as it is written: Claude Code's frontmatter, the line that
/// says who generated it, then the body.
fn skill_document() -> String {
    format!(
        "---\n\
         name: muxloom\n\
         description: >-\n\
         \x20 Collaborate with the other agents and people in a muxloom fleet: read and post to\n\
         \x20 the shared talk board, message another agent on any machine, reach the human on\n\
         \x20 their phone through a bound chat app, search history across machines, and work\n\
         \x20 through long-lived sessions. Use whenever the muxloom MCP tools are available.\n\
         ---\n\n\
         {SKILL_MARKER}{SKILL_REVISION} — written by muxloomd. Delete this line to keep your own \
         edits. -->\n\n\
         {SKILL_BODY}"
    )
}

/// Leave the skill under `home`, unless the file there is not ours to write.
/// Returns the path when it was written.
pub fn install_skill(home: &Path) -> Result<Option<PathBuf>> {
    let path = home
        .join(".claude")
        .join("skills")
        .join(SERVER_NAME)
        .join("SKILL.md");
    match fs::read_to_string(&path) {
        Ok(existing) => match skill_revision(&existing) {
            // No stamp: someone else wrote this, and it is not ours to replace.
            None => return Ok(None),
            Some(revision) if revision >= SKILL_REVISION => return Ok(None),
            Some(_) => {}
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_atomically(&path, skill_document().as_bytes())?;
    Ok(Some(path))
}

/// Which revision of the skill a file holds, if it holds one of ours at all.
fn skill_revision(text: &str) -> Option<u32> {
    let rest = text.split_once(SKILL_MARKER)?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
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

    #[test]
    fn only_a_moderator_is_handed_to_the_controller_beside_the_daemon() {
        let state = scratch("handover");
        let bin = state.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let daemon = bin.join(format!("muxloomd{}", std::env::consts::EXE_SUFFIX));
        fs::write(&daemon, "").unwrap();
        let controller = bin.join(format!("muxloom{}", std::env::consts::EXE_SUFFIX));
        fs::write(&controller, "").unwrap();
        let moderator = state.join("projects/fleet-lead");
        let moderator = moderator.to_string_lossy().into_owned();

        assert_eq!(
            handover_to_controller(&state, Some(&moderator), &daemon).as_deref(),
            Some(controller.to_string_lossy().as_ref()),
        );
        // Everyone else keeps the daemon's own surface, and so does a session
        // muxloom did not start, which has no folder to judge.
        for path in [Some("/home/me/Works/Terminal"), Some("  "), Some(""), None] {
            assert_eq!(
                handover_to_controller(&state, path, &daemon),
                None,
                "{path:?}"
            );
        }
        // A machine that only ever received the companion has nothing to hand
        // the session to, and serves it itself rather than failing.
        fs::remove_file(&controller).unwrap();
        assert_eq!(
            handover_to_controller(&state, Some(&moderator), &daemon),
            None
        );
        let _ = fs::remove_dir_all(&state);
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
    fn the_skill_is_written_once_and_refreshed_when_it_goes_stale() {
        let home = scratch("skill");
        let path = install_skill(&home)
            .unwrap()
            .expect("a fresh install writes");
        assert_eq!(path, home.join(".claude/skills/muxloom/SKILL.md"));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: muxloom\n"), "{text}");
        assert!(text.contains("talk_read"), "{text}");
        assert_eq!(skill_revision(&text), Some(SKILL_REVISION));

        // Current: nothing to do, and the file is not touched.
        assert!(install_skill(&home).unwrap().is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        // Stale: ours to replace.
        fs::write(&path, text.replace(&format!("r{SKILL_REVISION}"), "r0")).unwrap();
        assert!(install_skill(&home).unwrap().is_some());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_skill_the_user_wrote_themselves_is_never_overwritten() {
        let home = scratch("skill-mine");
        let path = home.join(".claude/skills/muxloom/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\nname: muxloom\n---\n\nMy own notes.\n").unwrap();

        assert!(install_skill(&home).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "---\nname: muxloom\n---\n\nMy own notes.\n"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_skill_says_what_the_instructions_say() {
        let text = skill_document();
        for tool in [
            "talk_read",
            "talk_post",
            "message_agent",
            "wait_for",
            "trigger",
            "search_conversations",
            "read_conversation",
            "launch_session",
            "run_shell",
            "delete_session",
        ] {
            assert!(text.contains(tool), "the skill never mentions {tool}");
        }
        // The two halves the whole layer rests on: read the board before you
        // act, and shells are what you reach for last.
        assert!(text.contains("Start by reading the board"), "{text}");
        assert!(text.contains("Shells are the last resort"), "{text}");
    }

    /// One machine, one entry. The controller's surface reaches the whole
    /// fleet without relaying, so where both are installed it is the one the
    /// agents on that machine should be given.
    #[test]
    fn the_controller_takes_the_entry_on_a_machine_that_has_one() {
        let root = scratch("beside");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let daemon = bin.join(format!("muxloomd{}", std::env::consts::EXE_SUFFIX));
        fs::write(&daemon, "").unwrap();

        // A target that only ever received the companion has nothing else to
        // point at.
        assert_eq!(controller_beside(&daemon), None);

        let controller = bin.join(format!("muxloom{}", std::env::consts::EXE_SUFFIX));
        fs::write(&controller, "").unwrap();
        assert_eq!(
            controller_beside(&daemon).as_deref(),
            Some(controller.to_string_lossy().as_ref())
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The entry belongs to the machine, not to whoever started a daemon: a
    /// test harness or a second daemon in a scratch directory must not send
    /// every agent here to a fleet that does not exist.
    #[test]
    fn only_the_machines_own_daemon_claims_the_entry_unless_told_otherwise() {
        assert!(wanted(None, true));
        assert!(!wanted(None, false));
        // Asked for, in either direction, settles it.
        assert!(wanted(Some("1"), false));
        assert!(wanted(Some(" yes "), false));
        assert!(!wanted(Some("0"), true));
        assert!(!wanted(Some("off"), true));
        // Anything else is not an instruction.
        assert!(wanted(Some("maybe"), true));
        assert!(!wanted(Some("maybe"), false));
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
