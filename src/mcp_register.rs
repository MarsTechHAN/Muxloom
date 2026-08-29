//! Registering this machine's control surface with the agents that run on it.
//!
//! An agent muxloom starts is meant to be able to see and drive the other
//! sessions — its own machine's and, through them, the work it handed off — but
//! only if it has been told the surface exists. Wiring that up by hand on every
//! machine is exactly the chore muxloom exists to remove, so the daemon does it
//! for the user it runs as: on start it writes a `muxloom` entry into the
//! user-level MCP configuration of every agent that speaks MCP — Claude Code,
//! Codex, and OpenCode — pointing at itself, plus a Pi extension that bridges
//! the same surface to the one agent that does not speak MCP at all.
//!
//! The same start also leaves the agents a skill describing how to work with
//! the rest of the fleet, since a tool list says what can be called but not
//! what is worth calling. Claude Code, Codex, and Pi all load the Agent Skills
//! standard; OpenCode has no skill directory and gets the shorter version
//! through the MCP `instructions` field instead, as everything does.
//!
//! These files belong to the user, not to muxloom. Nothing else in them is
//! touched, the entry is rewritten only when it is missing or points somewhere
//! else, a file that does not parse is left exactly as it is, and
//! `MUXLOOM_MCP_REGISTER=0` turns the whole thing off (`MUXLOOM_SKILL=0` turns
//! off just the skill, `MUXLOOM_PI=0` turns off just the Pi extension).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The name the entry carries in every agent's configuration.
const SERVER_NAME: &str = "muxloom";

/// The agents the daemon registers a skill with. Each is the relative path from
/// `home` to that agent's `SKILL.md` under its own `skills` directory.
const CLAUDE: &str = ".claude/skills/muxloom/SKILL.md";
const CODEX: &str = ".codex/skills/muxloom/SKILL.md";
const PI: &str = ".pi/agent/skills/muxloom/SKILL.md";

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
    let opencode = home.join(".config").join("opencode").join("opencode.json");
    if register_with_opencode(&opencode, entry)? {
        written.push(opencode);
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
        written.extend(install_skills(&home)?);
    }
    if !switched_off("MUXLOOM_PI") {
        written.extend(install_pi(&home)?);
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
const SKILL_REVISION: u32 = 12;
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
talk_read { since_cursor: \"<cursor from the last read>\", wait_seconds: 45 }
```

## Name what you are doing

Your session has a head name — the top line of your row in the dashboard and
the agent list. Keep it saying what you are working on *as a whole*, so a
person watching the board can see your progress without opening your screen:

```
set_head_name { name: \"fixing MCP timeout\" }
```

Update it whenever the shape of your work changes — picking up a new task,
moving to a new sub-goal, delegating to a subagent, getting blocked, or
finishing. Do not update it on every tool call; it should describe the whole
task, not the file you happen to be in. Keep it short (under 60 characters)
and in the language the user works in. A stale name misleads everyone
watching; one that tracks the current task saves them a click.

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
talk_read { scope: \"direct\", wait_seconds: 45 }
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
whole message saves. muxloom signs every message with your machine and session,
so do not introduce yourself, and never put a token, a key, or an absolute home
path in one.

Be brief, and know that this one is enforced: `text` is capped at 1200
characters and `title` at 48, and a message over either is **refused, not
trimmed**. Trimming would take whatever you put last, which is almost always
the ask, and you would never be told. So if it does not fit, the message is too
long rather than the cap too small. Cut it the way you would cut it for
somebody standing at a bus stop:

- the conclusion, in one line;
- the two or three numbers that would change what they do;
- the ask, and how to answer it.

Everything else already exists somewhere — the talk board, your session, the
diff, the CI run. Say where it is instead of repeating it. A report belongs on
the board; what goes to a phone is the sentence that made you send anything at
all.

Write `text` as markdown either way, but know that only some of it survives.
Lark renders the lot — headings, lists, tables, code fences, links. WeChat
renders none of it and muxloom flattens it first: the words, the line breaks
and the order arrive exactly as written, the marks around them do not, and a
table becomes a row of pipes nobody can read on a phone. Prefer short lines and
plain lists to anything that needs a grid.

Their reply comes back to you as a direct message, so end your turn and wait:

```
talk_read { scope: \"direct\", wait_seconds: 45 }
```

### Answer them before you start

A message from the person is not another agent's request to weigh up. They are
on a phone, they cannot see your screen, and until you send something the only
thing they know is that they typed. Reply first — before the search, before the
fix, before the file you were about to read — even when all you have is what you
understood and roughly how long it will take:

```
send_channel_message { text: \"收到。先看 CI 红在哪一格，大概十分钟回你\" }
```

Then do the work and send the result when it is done. One message at each end
is the shape. Going quiet for twenty minutes and coming back with a finished
job reads, from their side, exactly like an agent that never heard them — and
a question they asked in passing (\"干的怎么样了\") is answered now, in a line,
not by a report at the end.

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

## Read a screen

`read_screen { session_id, lines?, offset_from_bottom?, raw? }` returns the
screen's read result by default: borders and the bottom status/footer bar
stripped, whitespace collapsed, content in reading order — the text a person
would read off the screen. Pass `raw: true` for the raw vt100 grid (ANSI
stripped, columns intact) when you need the exact layout.

## Shells are the last resort

Talk to the session that already lives where the work is. Use `launch_session`
for anything long-running, and the narrow tools — `list_sessions`,
`read_screen`, `list_files`, `preview_file`, `search_history` — over shell
equivalents; they are bounded and safe to repeat. `run_shell` is for a short,
non-interactive, ideally read-only query that nothing else covers.

## Spawn subagents through muxloom

When you want to fan work out in parallel, launch a real muxloom session with
`launch_session` (an opencode / codex / claude / pi agent in its own
terminal) — not your harness's built-in subagent or task tool. Built-in
subagents are invisible: they never appear in the dashboard, never post to
the talk board, and die the moment your own process ends. A muxloom session
is a colleague you can watch, message, and hand off to: the person at the
dashboard sees it running and knows when it is done. Give each one a clear
label and a specific brief, then follow it with `wait_for`, `read_screen`,
and `message_agent` exactly like any other session.

Being handed more than one task is the signal to do this. Four tasks worked
one after another take four times as long, hold four times as much in your
context, and give the person watching nothing to look at but whichever one you
happen to be on; four sessions do them at once, each with a row of its own.
So when a message arrives carrying several things — a list, a numbered set of
fixes, \"and also\" — split it before you start:

```
launch_session { kind: \"claude\", label: \"review: relay latency\", initial_prompt: \"...\" }
launch_session { kind: \"claude\", label: \"review: approval gate\", initial_prompt: \"...\" }
talk_post { text: \"Split the review four ways; I am aggregating\", scope: \"task\" }
```

Write each brief so it needs no follow-up question — what to look at, what to
report, and what not to touch — and keep the coordinating, the aggregating,
and the commit for yourself. Doing it all in your own context is the right
choice only when the tasks genuinely depend on each other, or when there is
one of them.

### Say what each one may do

A launch also sets what the new session may do in its own turn, and you cannot
hand out more than you hold:

```
launch_session {
  kind: \"claude\", label: \"review: relay latency\",
  may_message: \"task\",          # parent | task | fleet — default task
  may_launch: [\"claude\"],       # default: your own kind
  may_reach_person: false,      # default false
}
```

The defaults are the right answer most of the time, and they are deliberately
narrow: a helper talks to your team, starts more of what you are, and leaves
the person to you. Widen one when the work actually calls for it — `fleet` for
a session whose findings are genuinely somebody else's business, an empty
`may_launch` for a single job you want done rather than delegated onward. The
grant follows the session through an archive and a resume, so it cannot be
shed by dying and coming back.

If a tool refuses you with \"the agent that started it set that\", that is this,
and the flag is not yours to change — say what you need to whoever started you
and let them carry it, or ask them to start the next session wider.

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

/// Every agent that loads the Agent Skills standard gets the same skill file,
/// so a fleet behaviour learned in one works in all of them. Claude Code and
/// Codex read `SKILL.md` from their own `skills` directories under `home`; Pi
/// does the same from `~/.pi/agent/skills`. OpenCode has no skill directory and
/// is deliberately left out: it gets the higher-priority `instructions` sent by
/// the MCP handshake instead.
///
/// Returns the paths written.
pub fn install_skills(home: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for relative in [CLAUDE, CODEX, PI] {
        if let Some(path) = install_skill_at(home.join(relative))? {
            written.push(path);
        }
    }
    Ok(written)
}

/// Leave the skill under one agent's `skills` directory, unless the file there
/// is not ours to write. Returns the path when it was written.
fn install_skill_at(path: PathBuf) -> Result<Option<PathBuf>> {
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

/// OpenCode keeps its user-scope servers under the `mcp` object in
/// `~/.config/opencode/opencode.json`. That file is hand-written and may hold
/// anything, so the `muxloom` server is inserted into the `mcp` object and
/// everything else is preserved byte-for-byte; a file that does not parse is
/// left untouched.
fn register_with_opencode(path: &Path, entry: &ServerEntry) -> Result<bool> {
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
    let mut command = vec![entry.command.clone()];
    command.extend(entry.args.clone());
    let mut desired = json!({
        "type": "local",
        "command": command,
        "enabled": true,
    });
    if !entry.environment.is_empty() {
        desired["environment"] = json!(entry.environment);
    }
    let servers = object
        .entry("mcp")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("mcp in {} is not an object", path.display()))?;
    if servers.get(SERVER_NAME) == Some(&desired) {
        return Ok(false);
    }
    servers.insert(SERVER_NAME.into(), desired);
    let mut text = serde_json::to_string_pretty(&root).context("failed to encode the config")?;
    text.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_atomically(path, text.as_bytes())?;
    Ok(true)
}

/// Pi has no MCP integration, so the surface is bridged through an extension
/// that speaks the same JSON-RPC-over-stdio protocol muxloomd itself speaks,
/// registering every control tool as a native Pi tool. The extension needs to
/// know where `muxloomd` lives; muxloomd's own binary is the answer, embedded
/// at install time, so a daemon that moves rewrites the extension with it.
///
/// `register_for_this_daemon` passes the daemon path on the entry it already
/// built; here we read it back from the environment the daemon runs with, so
/// the installed extension is only ever pointed at whatever we actually are.
fn install_pi(home: &Path) -> Result<Vec<PathBuf>> {
    install_pi_into(home, &ServerEntry::for_this_machine()?)
}

/// The portion of [`install_pi`] that takes an explicit entry, so tests can
/// point the generated extension at a fake daemon.
fn install_pi_into(home: &Path, entry: &ServerEntry) -> Result<Vec<PathBuf>> {
    let mut args = vec![entry.command.clone()];
    args.extend(entry.args.clone());
    let body = pi_extension_body(&args, &entry.environment);
    let dir = home.join(".pi/agent/extensions/muxloom");
    let index = dir.join("index.ts");
    match fs::read_to_string(&index) {
        Ok(existing) if existing == body => return Ok(Vec::new()),
        _ => {}
    }
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    write_atomically(&index, body.as_bytes())?;
    Ok(vec![index])
}

/// The TypeScript source that bridges muxloom's control surface onto Pi's
/// native tool list. Pi deliberately ships no MCP integration, so this speaks
/// the same JSON-RPC-over-stdio protocol muxloomd itself implements: it spawns
/// `muxloomd mcp`, lists the tools, and registers each one as a first-class Pi
/// tool via `pi.registerTool`. The daemon — not this shim — owns the tool list
/// and validates arguments, so the shim forwards parameters verbatim.
///
/// It is self-contained: only Node built-ins plus `typebox`, which Pi bundles
/// and makes importable from extensions. No npm install for the user.
///
/// `args` is the [`spawn`] argv for the muxloomd mcp process; `env` is the
/// extra environment the daemon entry carries for a non-default state dir.
fn pi_extension_body(args: &[String], env: &BTreeMap<String, String>) -> String {
    let args_json = serde_json::Value::Array(args.iter().map(|a| json!(a)).collect()).to_string();
    let mut env_entries = env
        .iter()
        .map(|(k, v)| format!("    {k:?}: {v:?},"))
        .collect::<Vec<_>>()
        .join("\n");
    if !env_entries.is_empty() {
        env_entries.insert_str(0, "{\n");
        env_entries.push_str("\n  }");
    } else {
        env_entries = "undefined".into();
    }
    // A single \"…\" inside the raw string is fine; we only avoid the sequence
    // \"# that would end the r#…# literal. No tool value here contains it.
    format!(
        r#"// muxloom — managed by muxloomd. Delete this line to keep your own edits.
import type {{ ExtensionAPI }} from "@earendil-works/pi-coding-agent";
import {{ Type }} from "typebox";
import {{ spawn }} from "node:child_process";
import type {{ ChildProcess }} from "node:child_process";
import {{ createInterface }} from "node:readline";
import {{ Readable, Writable }} from "node:stream";

const MUXLOOM_ARGS: string[] = {args_json};
const MUXLOOM_ENV: Record<string, string> | undefined = {env_entries};

// One child process per Pi run, started lazily on first call and torn down on
// shutdown. muxloomd is the authority on the tool list and argument schema;
// this shim only relays JSON-RPC over stdio.
let proc: {{ child: ChildProcess; pending: Map<number, {{resolve: (v: any) => void; reject: (e: Error) => void}}>; nextId: number; lines: import("node:readline").Interface }} | null = null;

function spawnDaemon(): {{ child: ChildProcess; pending: Map<number, {{resolve: (v: any) => void; reject: (e: Error) => void}}>; nextId: number; lines: import("node:readline").Interface }} {{
  const child = spawn(MUXLOOM_ARGS[0]!, MUXLOOM_ARGS.slice(1), {{
    stdio: ["pipe", "pipe", "inherit"],
    env: MUXLOOM_ENV ? {{ ...process.env, ...MUXLOOM_ENV }} : process.env,
  }});
  const state = {{ child, pending: new Map(), nextId: 1, lines: null as any }};
  const reader = createInterface({{ input: child.stdout as unknown as Readable }});
  reader.on("line", (line) => {{
    if (!line.trim()) return;
    let message: any;
    try {{ message = JSON.parse(line); }} catch {{ return; }}
    const id = message?.id;
    if (id == null) return;
    const entry = state.pending.get(id);
    if (!entry) return;
    state.pending.delete(id);
    if (message.error) entry.reject(new Error(String(message.error.message ?? message.error)));
    else entry.resolve(message.result);
  }});
  state.lines = reader;
  reader.on("close", () => {{
    for (const entry of state.pending.values()) entry.reject(new Error("muxloomd mcp closed"));
    state.pending.clear();
  }});
  return state;
}}

async function rpc(state: any, method: string, params: any = {{}}): Promise<any> {{
  const id = state.nextId++;
  const reply = new Promise<any>((resolve, reject) => {{
    state.pending.set(id, {{ resolve, reject }});
  }});
  const line = JSON.stringify({{ jsonrpc: "2.0", id, method, params }});
  (state.child.stdin as Writable).write(line + "\n");
  return reply;
}}

async function getInstance(): Promise<any> {{
  if (proc) return proc;
  proc = spawnDaemon();
  return proc;
}}

export default function (pi: ExtensionAPI) {{
  let registered = false;

  async function registerTools() {{
    if (registered) return;
    registered = true;
    try {{
      const inst = await getInstance();
      await rpc(inst, "initialize", {{
        protocolVersion: "2025-06-18",
        capabilities: {{}},
        clientInfo: {{ name: "muxloom-pi", version: "1" }},
      }});
      const {{ tools }} = await rpc(inst, "tools/list", {{}});
      for (const tool of tools ?? []) {{
        const name: string = tool.name ?? String(tool.name);
        if (!name) continue;
        const description: string = typeof tool.description === "string" ? tool.description : "";
        pi.registerTool({{
          name,
          label: name,
          description,
          parameters: Type.Object({{}}),
          prepareArguments(args) {{ return args ?? {{}}; }},
          async execute(toolCallId, params, _signal, _onUpdate, _ctx) {{
            const inst = await getInstance();
            const result = await rpc(inst, "tools/call", {{
              name,
              arguments: params ?? {{}},
            }});
            const text = Array.isArray(result?.content)
              ? result.content.map((c: any) => c?.text ?? "").join("\n")
              : JSON.stringify(result);
            return {{ content: [{{ type: "text", text }}], details: {{}} }};
          }},
        }});
      }}
    }} catch (error) {{
      // Surfacing the failure keeps the rest of the surface useful even when
      // the local daemon is absent; the tools simply fail with a clear text.
      console.error("[muxloom] could not reach muxloomd mcp:", error);
    }}
  }}

  pi.on("session_start", async () => {{
    await registerTools();
  }});
  pi.on("resources_discover", async () => {{
    await registerTools();
  }});
  pi.on("session_shutdown", () => {{
    if (proc) {{
      proc.child.kill();
      proc = null;
      registered = false;
    }}
  }});
}}
"#
    )
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
    fn every_agent_learns_about_the_daemon_and_stops_being_rewritten_after_that() {
        let home = scratch("fresh");
        let written = register(&home, &entry()).unwrap();
        assert_eq!(written.len(), 3);

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
        let opencode: Value = serde_json::from_str(
            &fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(opencode["mcp"]["muxloom"]["type"], "local");
        assert_eq!(opencode["mcp"]["muxloom"]["command"][0], "/opt/muxloomd");
        assert_eq!(opencode["mcp"]["muxloom"]["command"][1], "mcp");

        // Nothing to say the second time.
        assert!(register(&home, &entry()).unwrap().is_empty());

        // A daemon that moved is followed, not duplicated.
        let moved = ServerEntry {
            command: "/usr/local/bin/muxloomd".into(),
            ..entry()
        };
        assert_eq!(register(&home, &moved).unwrap().len(), 3);
        let text = fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert_eq!(text.matches("[mcp_servers.muxloom]").count(), 1);
        assert!(text.contains("/usr/local/bin/muxloomd"));
        let opencode: Value = serde_json::from_str(
            &fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            opencode["mcp"]["muxloom"]["command"][0],
            "/usr/local/bin/muxloomd"
        );
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
    fn opencode_keeps_the_users_other_servers_with_only_ours_added() {
        let home = scratch("opencode-populated");
        let config = home.join(".config/opencode/opencode.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            r#"{
  "model": "anthropic/claude-sonnet-4-5",
  "mcp": { "other": { "type": "local", "command": ["other-mcp"] } }
}
"#,
        )
        .unwrap();

        register(&home, &entry()).unwrap();

        let root: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(root["model"], "anthropic/claude-sonnet-4-5");
        assert_eq!(root["mcp"]["other"]["command"][0], "other-mcp");
        assert_eq!(root["mcp"]["muxloom"]["command"][0], "/opt/muxloomd");
        assert_eq!(root["mcp"]["muxloom"]["command"][1], "mcp");
        assert_eq!(root["mcp"]["muxloom"]["type"], "local");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn opencode_that_does_not_parse_is_left_exactly_as_it_was() {
        let home = scratch("opencode-broken");
        let config = home.join(".config/opencode/opencode.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{ not json").unwrap();
        assert!(register(&home, &entry()).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "{ not json");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_pi_extension_is_generated_self_contained_and_points_at_the_daemon() {
        let home = scratch("pi");
        let written = install_pi_into(&home, &entry()).unwrap();
        assert_eq!(written.len(), 1);
        let path = home.join(".pi/agent/extensions/muxloom/index.ts");
        assert_eq!(written[0], path);
        let body = fs::read_to_string(&path).unwrap();
        // It registers tools on Pi and spawns the daemon's own binary.
        assert!(body.contains("pi.registerTool"), "{body}");
        assert!(body.contains("\"/opt/muxloomd\""), "{body}");
        assert!(body.contains("\"mcp\""), "{body}");
        assert!(body.contains("tools/list"), "{body}");
        assert!(body.contains("tools/call"), "{body}");
        // A fresh install is idempotent.
        assert!(install_pi_into(&home, &entry()).unwrap().is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_pi_extension_carries_the_state_directory_when_the_daemon_was_given_one() {
        let home = scratch("pi-state");
        let mut entry = entry();
        entry
            .environment
            .insert("MUXLOOMD_STATE_DIR".into(), "/tmp/state".into());
        install_pi_into(&home, &entry).unwrap();
        let body = fs::read_to_string(home.join(".pi/agent/extensions/muxloom/index.ts")).unwrap();
        assert!(body.contains("MUXLOOMD_STATE_DIR"), "{body}");
        assert!(body.contains("/tmp/state"), "{body}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_skill_is_written_to_every_agent_and_refreshed_when_it_goes_stale() {
        let home = scratch("skill");
        let written = install_skills(&home).unwrap();
        assert_eq!(written.len(), 3);
        for relative in [CLAUDE, CODEX, PI] {
            let path = home.join(relative);
            assert!(written.iter().any(|w| w == &path), "missing {relative}");
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.starts_with("---\nname: muxloom\n"), "{text}");
            assert!(text.contains("talk_read"), "{text}");
            assert_eq!(skill_revision(&text), Some(SKILL_REVISION));
        }

        // Current: nothing to do, and no file is touched.
        assert!(install_skills(&home).unwrap().is_empty());
        let path = home.join(CLAUDE);
        let text = fs::read_to_string(&path).unwrap();

        // Stale: ours to replace, in every agent.
        fs::write(&path, text.replace(&format!("r{SKILL_REVISION}"), "r0")).unwrap();
        let written = install_skills(&home).unwrap();
        assert!(written.iter().any(|w| w == &path));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_skill_the_user_wrote_themselves_is_never_overwritten() {
        let home = scratch("skill-mine");
        let mine = home.join(CLAUDE);
        fs::create_dir_all(mine.parent().unwrap()).unwrap();
        fs::write(&mine, "---\nname: muxloom\n---\n\nMy own notes.\n").unwrap();

        // The stamped file is skipped, but the other two agents still get ours.
        let written = install_skills(&home).unwrap();
        assert!(
            written.iter().all(|w| w != &mine),
            "touched the user's file"
        );
        assert_eq!(written.len(), 2);
        assert_eq!(
            fs::read_to_string(&mine).unwrap(),
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

    /// The person is on a phone with no screen to look at, so an agent that
    /// starts working instead of answering looks, from their end, exactly like
    /// one that never heard them. The stamp has to carry it too: a file
    /// already on disk is replaced by revision, so a body that changes under
    /// the old number reaches nobody who has one.
    #[test]
    fn the_skill_says_to_answer_the_person_first_under_a_stamp_that_replaces_the_old_one() {
        let text = skill_document();
        assert!(text.contains("Answer them before you start"), "{text}");
        assert!(text.contains("send_channel_message"), "{text}");
        assert_eq!(skill_revision(&text), Some(SKILL_REVISION));
        assert!(
            !text.contains(&format!("{SKILL_MARKER}8 ")),
            "the body changed under the stamp it shipped with"
        );
    }

    /// Several tasks in one message are several sessions, and the skill has to
    /// say so where an agent reading it is deciding what to do first. Worked
    /// in a row they cost the wall clock of all of them, and the person
    /// watching sees one row saying whichever one is in hand.
    #[test]
    fn the_skill_tells_an_agent_holding_several_tasks_to_hand_them_out() {
        let text = skill_document();
        assert!(
            text.contains("Being handed more than one task is the signal to do this"),
            "{text}"
        );
        assert!(text.contains("launch_session"), "{text}");
        assert_eq!(skill_revision(&text), Some(SKILL_REVISION));
    }

    /// The skill spells the caps out as numbers, because an agent reading
    /// \"keep it short\" writes whatever it thinks short is. Numbers in prose
    /// drift away from the constant that enforces them, and a skill promising
    /// one length while the tool refuses at another is worse than no number at
    /// all — so the two are checked against each other here.
    #[test]
    fn the_skill_quotes_the_caps_the_tool_actually_enforces() {
        let text = skill_document();
        assert!(
            text.contains(&crate::channel::READABLE_LIMIT.to_string()),
            "the skill must name the cap it is teaching: {text}"
        );
        assert!(
            text.contains(&crate::channel::TITLE_LIMIT.to_string()),
            "{text}"
        );
        assert!(text.contains("refused, not\ntrimmed"), "{text}");
        assert_eq!(skill_revision(&text), Some(SKILL_REVISION));
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
