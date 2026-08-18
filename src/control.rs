//! The control surface adapters drive muxloom through.
//!
//! [`ControlSurface`] is the seam between muxloom's capabilities and whatever
//! wants to consume them: `src/mcp.rs` serves it to AI agents over MCP stdio
//! today, and a TCP or serial adapter for a hardware status panel can serve
//! the same trait tomorrow without touching either implementation. Tools are
//! poll-based — session status is computed fresh on every read — so a future
//! push channel belongs here too, as an event callback on the trait, backed by
//! a daemon-side subscription once one exists.
//!
//! Two surfaces implement the trait with the same tool names and shapes:
//! [`DaemonControl`] serves the local `muxloomd` (used by `muxloomd mcp`) and
//! [`ControllerControl`] serves every enabled machine through [`Runtime`]
//! (used by `muxloom mcp`). The daemon surface omits the `machine` parameter
//! and the discovery tools that need a controller's view.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    config::{Config, McpConfig, State, default_state_path},
    model::{AgentKind, FilePreview, FilePreviewKind, LaunchRequest, Target},
    runtime::Runtime,
    ssh_config::{self, MANAGED_INCLUDE, ManagedHosts},
};

/// One tool an adapter can offer on behalf of a surface. `input_schema` is a
/// JSON Schema object in the shape MCP clients expect.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    pub input_schema: Value,
}

/// A capability surface a transport adapter serves. Calls return the text the
/// consumer reads; failures carry the reason and leave the surface usable.
pub trait ControlSurface {
    fn tools(&self) -> Vec<ToolSpec>;
    fn call(&mut self, name: &str, arguments: &Value) -> Result<String>;

    /// How a consumer is meant to use this surface: what muxloom is for, which
    /// tool to reach for first, and what it must not do. Adapters that have a
    /// place for it (MCP's `instructions`) pass it to the agent verbatim.
    fn instructions(&self) -> Option<String> {
        None
    }
}

/// How many rendered rows a screen read returns when the caller does not say.
const DEFAULT_SCREEN_LINES: usize = 200;
/// A cursor jump wider than any terminal is a corrupt row, not indentation.
const SCREEN_COLUMNS_LIMIT: usize = 1_000;
/// Per-session match budget for history searches, matching the daemon's cap.
const SEARCH_MAX_MATCHES: usize = 12;
/// The most bytes of one shell stream a tool answer carries.
const SHELL_OUTPUT_LIMIT: usize = 128 * 1024;

/// Every tool that changes something rather than reporting it. `read_only`
/// denies exactly this set, so a tool added here is denied by that switch from
/// the day it exists.
const WRITE_TOOLS: &[&str] = &[
    "send_input",
    "launch_session",
    "archive_session",
    "delete_session",
    "run_shell",
    "set_machine_enabled",
    "ssh_host",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// Every enabled machine, addressed by a `machine` argument.
    Controller,
    /// The one local daemon. Constructed by the unix-only daemon surface, so
    /// a Windows lib build sees no non-test constructor.
    #[cfg_attr(not(unix), allow(dead_code))]
    Daemon,
}

/// Why the configured policy refuses this tool, if it does.
fn denial(policy: &McpConfig, tool: &str) -> Option<String> {
    if policy.denies(tool) {
        return Some(format!(
            "tool {tool} is disabled by muxloom policy on this machine (mcp.denied_tools)"
        ));
    }
    if policy.read_only && WRITE_TOOLS.contains(&tool) {
        return Some(format!(
            "tool {tool} changes state and muxloom is configured read-only on this machine \
             (mcp.read_only)"
        ));
    }
    None
}

/// The tools a surface offers: its flavor's set, minus what policy denies.
fn allowed_specs(flavor: Flavor, policy: &McpConfig) -> Vec<ToolSpec> {
    let mut tools = specs(flavor);
    tools.retain(|tool| denial(policy, tool.name).is_none());
    tools
}

/// Refuse a denied tool even when an agent calls it by a name it remembers
/// from another machine: hiding it from the list is advice, this is the gate.
fn enforce_policy(policy: &McpConfig, tool: &str) -> Result<()> {
    match denial(policy, tool) {
        Some(reason) => bail!("{reason}"),
        None => Ok(()),
    }
}

/// The guidance an agent gets before its first call. Kept short enough to sit
/// in a system prompt: what muxloom is, what to reach for, what is off limits.
fn instructions(flavor: Flavor, policy: &McpConfig) -> String {
    let reach = match flavor {
        Flavor::Controller => {
            "on this machine and on every other machine the user has enabled (address one with \
             the `machine` argument; see list_machines)"
        }
        Flavor::Daemon => {
            "on this machine (a muxloom controller elsewhere may be watching the same sessions)"
        }
    };
    // Which machines exist, and how to reach them, is only the controller's to
    // change — and changing it reaches past muxloom into the user's own tools.
    let manage = match flavor {
        Flavor::Controller => {
            "- The set of machines and the user's SSH configuration belong to them. \
             set_machine_enabled changes what every agent on this controller can reach, and \
             ssh_host edits a file every ssh command on this machine reads. Run either only \
             when the human asked for that change.\n"
        }
        Flavor::Daemon => "",
    };
    let mut text = format!(
        "muxloom manages long-lived terminal sessions — Codex, Claude Code, and plain shells — \
         {reach}. Sessions outlive this conversation and the muxloom dashboard, and a human may \
         be watching any of them right now.\n\n\
         Work through the sessions rather than around them:\n\
         - To get work done on a machine, talk to the agent session that lives there: send_input \
         (with submit) to say something, then read_screen to read the reply. Treat a session as a \
         colleague you are messaging, not as a subprocess you drive.\n\
         - run_shell is a last resort. Reach for it only for a short, non-interactive, ideally \
         read-only query that no other tool covers. Never start long-running or interactive work \
         with it — that is what launch_session is for — and never use it to do something a \
         session you could talk to would do better.\n\
         - Prefer the narrow tools (list_sessions, read_screen, list_files, preview_file, \
         search_history) over shell equivalents: they are bounded, paged, and safe to repeat.\n\n\
         Boundaries that are not negotiable:\n\
         - Machines the user has not enabled are unreachable. Naming one is an error, not a \
         workaround to route around.\n\
         - send_input is the only supported way to type. Never open a terminal stream just to \
         write bytes — it resizes the session under whoever is attached.\n\
         - Sessions are persistent state. A session you launched is yours to archive or delete \
         when it is done; a session you did not launch is someone else's — do not archive, \
         delete, or reconfigure it unless you were asked to.\n\
         - delete_session destroys a session's history irreversibly. Ask the human first; \
         archive_session keeps the history.\n\
         {manage}\
         - Typing into a session interrupts whoever is using it. Check `working` and \
         `needs_attention` in list_sessions before you type, and keep the interruption short.\n\n\
         When the target is ambiguous — which machine, which session, whether something may be \
         stopped — ask the human instead of guessing."
    );
    let denied: Vec<&str> = specs(flavor)
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| denial(policy, name).is_some())
        .collect();
    if !denied.is_empty() {
        text.push_str(&format!(
            "\n\nThe user has disabled these tools here: {}. They are not available on this \
             machine; do not look for a way around them.",
            denied.join(", ")
        ));
    }
    text
}

fn specs(flavor: Flavor) -> Vec<ToolSpec> {
    let multi = flavor == Flavor::Controller;
    let mut tools = Vec::new();
    if multi {
        tools.push(ToolSpec {
            name: "list_machines",
            description: "List the machines muxloom manages: the local host and enabled SSH \
                          aliases. Other tools address a machine by its id."
                .into(),
            input_schema: schema(false, json!({}), &[]),
        });
        tools.push(ToolSpec {
            name: "set_machine_enabled",
            description: "Let muxloom reach a machine, or stop it: needs explicit human \
                          authorization. Only enabled machines are addressable, so disabling one \
                          cuts it off from every agent and every tool at once, and enabling one \
                          opens it to all of them. The machine must already be \"local\" or an \
                          SSH alias — see ssh_host. A muxloom dashboard that is already running \
                          keeps its own view of this until it restarts."
                .into(),
            input_schema: schema(
                false,
                json!({
                    "machine": { "type": "string", "description": "\"local\" or an SSH alias." },
                    "enabled": { "type": "boolean", "description": "Whether muxloom may reach it." },
                }),
                &["machine", "enabled"],
            ),
        });
        tools.push(ToolSpec {
            name: "ssh_host",
            description: format!(
                "Read or write the SSH aliases this machine can connect to. `list` reports every \
                 alias with the file that defines it and whether muxloom manages it. `set` and \
                 `remove` need explicit human authorization: they edit the user's SSH \
                 configuration, which every ssh command on this machine reads, not just muxloom. \
                 Writes only ever touch {MANAGED_INCLUDE} next to their config plus one Include \
                 line pointing at it, and muxloom refuses to write an alias defined anywhere \
                 else. A new alias still has to be enabled with set_machine_enabled, and \
                 authentication (keys, agent forwarding) is the human's to arrange."
            ),
            input_schema: schema(
                false,
                json!({
                    "action": { "type": "string", "enum": ["list", "set", "remove"] },
                    "host": { "type": "string", "description": "The alias to write or remove. One concrete name, no patterns." },
                    "hostname": { "type": "string", "description": "Address to connect to (HostName)." },
                    "user": { "type": "string", "description": "Login user (User)." },
                    "port": { "type": "integer", "description": "Port (Port), 1-65535." },
                    "identity_file": { "type": "string", "description": "Private key path (IdentityFile)." },
                    "proxy_jump": { "type": "string", "description": "Jump host (ProxyJump)." },
                    "extra": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Further option lines verbatim, e.g. \"ForwardAgent yes\". One keyword and value per entry.",
                    },
                }),
                &["action"],
            ),
        });
    }
    tools.push(ToolSpec {
        name: "list_sessions",
        description: "List managed agent sessions with fresh status: kind (codex/claude/\
                      terminal), working directory, whether the agent is working, whether it \
                      waits for input (needs_attention plus the matched reason), and a recap \
                      line. Archived sessions are included only with include_archived."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "include_archived": { "type": "boolean", "description": "Also list archived sessions." },
            }),
            &[],
        ),
    });
    tools.push(ToolSpec {
        name: "read_screen",
        description: format!(
            "Read a session's terminal as rendered rows: the visible screen plus scrollback. \
             Returns up to `lines` rows (default {DEFAULT_SCREEN_LINES}) ending \
             `offset_from_bottom` rows above the live bottom edge; page older output by \
             raising the offset."
        ),
        input_schema: schema(
            multi,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
                "lines": { "type": "integer", "description": "Rows to return." },
                "offset_from_bottom": { "type": "integer", "description": "Rows above the bottom to end at." },
            }),
            &["session_id"],
        ),
    });
    tools.push(ToolSpec {
        name: "send_input",
        description: "Talk to a session: type into its terminal without disturbing an attached \
                      viewer. This is the main way to get work done on a machine — ask the agent \
                      that lives there. `text` is written verbatim, then each named key in `keys` \
                      (enter, esc, tab, backspace, space, delete, up, down, left, right, home, \
                      end, page-up, page-down, or ctrl-a…ctrl-z), then submit=true appends Enter. \
                      Typing interrupts whoever is using the session, so check `working` and \
                      `needs_attention` first. Prompts take effect asynchronously: poll \
                      list_sessions or read_screen to watch the result."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
                "text": { "type": "string", "description": "Bytes to type verbatim." },
                "keys": { "type": "array", "items": { "type": "string" }, "description": "Named keys to press after the text." },
                "submit": { "type": "boolean", "description": "Press Enter at the end." },
            }),
            &["session_id"],
        ),
    });
    tools.push(ToolSpec {
        name: "launch_session",
        description: "Start a persistent codex, claude, or terminal session in a working \
                      directory. Use this for anything long-running or interactive instead of \
                      run_shell. `resume_id` resumes that agent-native conversation; \
                      `initial_prompt` seeds a fresh agent instead. The session survives \
                      this process: pair every launch with a later archive or delete."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "kind": { "type": "string", "enum": ["codex", "claude", "terminal"] },
                "path": { "type": "string", "description": "Absolute working directory on the machine." },
                "label": { "type": "string", "description": "Display name shown in the dashboard." },
                "resume_id": { "type": "string", "description": "Agent-native session id to resume." },
                "initial_prompt": { "type": "string", "description": "First prompt for a fresh agent." },
            }),
            &["kind", "path"],
        ),
    });
    if multi {
        tools.push(ToolSpec {
            name: "list_resume_candidates",
            description: "List Codex and Claude Code histories recorded in exactly this \
                          working directory, newest first, with recap lines. Feed an id to \
                          launch_session's resume_id."
                .into(),
            input_schema: schema(
                multi,
                json!({
                    "path": { "type": "string", "description": "Absolute working directory on the machine." },
                }),
                &["path"],
            ),
        });
    }
    tools.push(ToolSpec {
        name: "archive_session",
        description: "Retire a live session but keep its history searchable and resumable. \
                      Temporary sessions cannot be archived — delete them instead."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
            }),
            &["session_id"],
        ),
    });
    tools.push(ToolSpec {
        name: "delete_session",
        description: "Destroy a session: needs explicit human authorization. Kills the process \
                      and deletes its history and metadata, irreversibly. Delete only sessions \
                      you launched yourself, or ones the human named; archive_session keeps the \
                      history instead."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
            }),
            &["session_id"],
        ),
    });
    tools.push(ToolSpec {
        name: "search_history",
        description: "Full-text search over session terminal histories, live and archived. \
                      Searches one session when session_id is given, otherwise every session."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "query": { "type": "string" },
                "session_id": { "type": "string", "description": "Limit the search to one session." },
            }),
            &["query"],
        ),
    });
    tools.push(ToolSpec {
        name: "list_directory",
        description: "List the subdirectories of a directory on the machine.".into(),
        input_schema: schema(
            multi,
            json!({
                "path": { "type": "string", "description": "Absolute directory path." },
            }),
            &["path"],
        ),
    });
    tools.push(ToolSpec {
        name: "list_files",
        description: "List a directory's entries with kind, size, and mtime.".into(),
        input_schema: schema(
            multi,
            json!({
                "path": { "type": "string", "description": "Absolute directory path." },
            }),
            &["path"],
        ),
    });
    if multi {
        tools.push(ToolSpec {
            name: "search_files",
            description: "Search filenames recursively below a directory. The pattern \
                          supports * and ** globs."
                .into(),
            input_schema: schema(
                multi,
                json!({
                    "root": { "type": "string", "description": "Absolute directory to search below." },
                    "pattern": { "type": "string" },
                }),
                &["root", "pattern"],
            ),
        });
    }
    tools.push(ToolSpec {
        name: "preview_file",
        description: "Read a text or Markdown file from the machine. Binary and media files \
                      answer with their type and size instead of content."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "path": { "type": "string", "description": "Absolute file path." },
            }),
            &["path"],
        ),
    });
    tools.push(ToolSpec {
        name: "run_shell",
        description: "Last resort: run a shell script on the machine with `sh -lc` and return its \
                      output and exit code. Runs with the user's full permissions and with no \
                      terminal, so it fits only short, non-interactive, ideally read-only \
                      queries no other tool covers. Long-running or interactive work belongs in \
                      launch_session; reading files or history belongs in list_files, \
                      preview_file, and search_history; work another agent could do belongs in a \
                      message to its session."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "script": { "type": "string" },
            }),
            &["script"],
        ),
    });
    tools
}

fn schema(multi_machine: bool, properties: Value, required: &[&str]) -> Value {
    let mut properties = match properties {
        Value::Object(map) => map,
        _ => unreachable!("tool properties are always objects"),
    };
    if multi_machine {
        properties.insert(
            "machine".into(),
            json!({
                "type": "string",
                "description": "Machine id from list_machines. Defaults to \"local\".",
            }),
        );
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

fn required_str<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("missing required argument {key}"))
}

fn optional_str<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn optional_bool(arguments: &Value, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn optional_usize(arguments: &Value, key: &str, default: usize) -> usize {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

/// One SSH alias to write: a single concrete name. Patterns belong to the
/// user's own configuration, and a name carrying whitespace or a comment could
/// rewrite the lines around it once it reaches the file.
fn ssh_alias(arguments: &Value) -> Result<String> {
    let host = required_str(arguments, "host")?.trim();
    if host.len() > 128
        || host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || host.contains(['*', '?', '!', '#', '"', '\'', '='])
    {
        bail!("host must be one concrete alias: no spaces, wildcards, quotes, or comments");
    }
    Ok(host.to_string())
}

/// One SSH option value, held to a single uncommented line for the same
/// reason: everything muxloom writes has to stay inside the block it wrote.
fn ssh_option_value(value: &str, key: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{key} cannot be empty");
    }
    if value.len() > 512 || value.chars().any(char::is_control) || value.contains('#') {
        bail!("{key} must be one line, without control characters or a # comment");
    }
    Ok(value.to_string())
}

fn ssh_value(arguments: &Value, key: &str) -> Result<Option<String>> {
    match optional_str(arguments, key) {
        Some(value) => Ok(Some(ssh_option_value(value, key)?)),
        None => Ok(None),
    }
}

/// The `Keyword value` lines a caller passes through verbatim.
fn ssh_extra_options(arguments: &Value) -> Result<Vec<(String, String)>> {
    let Some(lines) = arguments.get("extra") else {
        return Ok(Vec::new());
    };
    let lines = lines
        .as_array()
        .context("extra must be an array of \"Keyword value\" strings")?;
    let mut options = Vec::new();
    for line in lines {
        let line = line
            .as_str()
            .context("extra must be an array of \"Keyword value\" strings")?
            .trim();
        if line.is_empty() {
            continue;
        }
        let (keyword, value) = line
            .split_once(char::is_whitespace)
            .with_context(|| format!("extra entry {line:?} needs a keyword and a value"))?;
        if !keyword
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
        {
            bail!("extra keyword {keyword:?} is not an SSH option name");
        }
        // Anything that opens a block would take the options after it with
        // it, including the ones muxloom wrote for a different alias.
        if ["host", "match", "include"]
            .iter()
            .any(|reserved| keyword.eq_ignore_ascii_case(reserved))
        {
            bail!("extra may not contain {keyword}: muxloom writes the block structure itself");
        }
        options.push((keyword.to_string(), ssh_option_value(value, "extra")?));
    }
    Ok(options)
}

fn agent_kind(arguments: &Value) -> Result<AgentKind> {
    required_str(arguments, "kind")?
        .parse()
        .map_err(|error: String| anyhow::anyhow!(error))
}

/// The bytes one named key sends to a PTY.
fn encode_key(name: &str) -> Result<Vec<u8>> {
    let bytes: &[u8] = match name {
        "enter" => b"\r",
        "newline" => b"\n",
        "esc" | "escape" => b"\x1b",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "space" => b" ",
        "delete" => b"\x1b[3~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "page-up" => b"\x1b[5~",
        "page-down" => b"\x1b[6~",
        other => {
            if let Some(letter) = other.strip_prefix("ctrl-")
                && let [letter @ b'a'..=b'z'] = letter.as_bytes()
            {
                return Ok(vec![letter - b'a' + 1]);
            }
            bail!("unsupported key {other:?}");
        }
    };
    Ok(bytes.to_vec())
}

/// The bytes a send_input call types: text, then named keys, then Enter.
fn build_input(arguments: &Value) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(text) = optional_str(arguments, "text") {
        bytes.extend_from_slice(text.as_bytes());
    }
    if let Some(keys) = arguments.get("keys").and_then(Value::as_array) {
        for key in keys {
            let name = key.as_str().context("keys must be an array of strings")?;
            bytes.extend(encode_key(name)?);
        }
    }
    if optional_bool(arguments, "submit") {
        bytes.push(b'\r');
    }
    if bytes.is_empty() {
        bail!("send_input needs text, keys, or submit");
    }
    Ok(bytes)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn session_json(machine: &str, session: &crate::daemon_protocol::DaemonSession) -> Value {
    json!({
        "session_id": session.id,
        "machine": machine,
        "kind": session.kind,
        "path": session.path,
        "label": session.label,
        "temporary": session.temporary,
        "created_at": session.created_at,
        "pid": session.pid,
        "dead": session.dead,
        "archived": session.archived,
        "working": session.working,
        "needs_attention": session.needs_attention,
        "attention_reason": session.attention_reason,
        "recap": session.recap,
    })
}

fn screen_page(text: &str, offset_from_bottom: usize, rows: usize, older: bool) -> String {
    let text = plain_screen(text);
    format!(
        "{text}\n\n[rows={rows} offset_from_bottom={offset_from_bottom} older_history_above={older}]"
    )
}

/// Flatten rendered rows into the text a terminal would have shown.
///
/// A row comes back as the bytes a terminal would be sent to paint it, and a
/// reader of this tool wants the screen rather than the paint: colours are
/// noise, and a run of blanks arrives as a jump over them, so dropping the
/// escapes outright would close up the columns an agent aligned its output on.
fn plain_screen(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut plain: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != 0x1b {
            plain.push(byte);
            index += 1;
            continue;
        }
        match bytes.get(index + 1) {
            Some(b'[') => {
                let mut end = index + 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                let Some(final_byte) = bytes.get(end) else {
                    break;
                };
                if *final_byte == b'C' {
                    // Cursor-forward: the blanks the renderer skipped over.
                    let count: usize = std::str::from_utf8(&bytes[index + 2..end])
                        .ok()
                        .and_then(|parameters| parameters.parse().ok())
                        .unwrap_or(1);
                    plain.resize(plain.len() + count.min(SCREEN_COLUMNS_LIMIT), b' ');
                }
                index = end + 1;
            }
            // OSC and the other string sequences run until BEL or ST.
            Some(b']' | b'P' | b'X' | b'^' | b'_') => {
                let mut end = index + 2;
                while end < bytes.len() {
                    if bytes[end] == 0x07 {
                        end += 1;
                        break;
                    }
                    if bytes[end] == 0x1b && bytes.get(end + 1) == Some(&b'\\') {
                        end += 2;
                        break;
                    }
                    end += 1;
                }
                index = end;
            }
            Some(_) => index += 2,
            None => break,
        }
    }
    let plain = String::from_utf8_lossy(&plain);
    let mut lines: Vec<&str> = plain.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn preview_text(preview: &FilePreview) -> String {
    match preview.kind {
        FilePreviewKind::Text | FilePreviewKind::Markdown => format!(
            "[{} {} bytes{}]\n{}",
            preview.mime,
            preview.size,
            if preview.truncated { ", truncated" } else { "" },
            preview.content
        ),
        _ => format!(
            "{} file ({}, {} bytes); content not shown over this tool",
            preview.kind, preview.mime, preview.size
        ),
    }
}

fn clipped(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= limit {
        return text.into_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated {} of {} bytes]",
        &text[..end],
        text.len() - end,
        text.len()
    )
}

fn shell_report(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> String {
    let mut report = format!("exit code {exit_code}");
    if !stdout.is_empty() {
        report.push_str("\n--- stdout ---\n");
        report.push_str(&clipped(stdout, SHELL_OUTPUT_LIMIT));
    }
    if !stderr.is_empty() {
        report.push_str("\n--- stderr ---\n");
        report.push_str(&clipped(stderr, SHELL_OUTPUT_LIMIT));
    }
    report
}

/// Serves every enabled machine through a headless [`Runtime`]: the same
/// config, state, and backend the dashboard uses, without the dashboard.
pub struct ControllerControl {
    runtime: Runtime,
    config: Config,
    state: State,
    state_path: PathBuf,
}

impl ControllerControl {
    pub fn new(config: Config) -> Result<Self> {
        let state_path = default_state_path();
        let state = State::load(&state_path)?;
        let runtime = Runtime::new(&config);
        Ok(Self {
            runtime,
            config,
            state,
            state_path,
        })
    }

    /// The machine an argument set addresses. Only enabled machines are
    /// reachable: a disabled target must never be touched, not even by an
    /// agent that knows its name.
    fn target(&self, arguments: &Value) -> Result<Target> {
        let machine = optional_str(arguments, "machine").unwrap_or(crate::model::LOCAL_TARGET_ID);
        if !self.state.enabled_hosts.contains(machine) {
            bail!("machine {machine} is not enabled in muxloom");
        }
        if machine == crate::model::LOCAL_TARGET_ID {
            Ok(Target::local())
        } else {
            Ok(Target::ssh(machine))
        }
    }

    /// The machines an aggregate call sweeps: the addressed one, or every
    /// enabled machine when none is named.
    fn targets(&self, arguments: &Value) -> Result<Vec<Target>> {
        if optional_str(arguments, "machine").is_some() {
            return Ok(vec![self.target(arguments)?]);
        }
        Ok(self
            .state
            .enabled_hosts
            .iter()
            .map(|host| {
                if host == crate::model::LOCAL_TARGET_ID {
                    Target::local()
                } else {
                    Target::ssh(host)
                }
            })
            .collect())
    }

    fn list_machines(&self) -> Result<String> {
        let mut machines = vec![json!({
            "id": crate::model::LOCAL_TARGET_ID,
            "label": "This machine",
            "enabled": self.state.enabled_hosts.contains(crate::model::LOCAL_TARGET_ID),
            "connected": self.runtime.bridge_pool().is_connected(crate::model::LOCAL_TARGET_ID),
        })];
        let aliases = ssh_config::load_hosts(&self.config.ssh_config_path()).unwrap_or_default();
        for alias in aliases {
            machines.push(json!({
                "id": alias,
                "label": alias,
                "enabled": self.state.enabled_hosts.contains(&alias),
                "connected": self.runtime.bridge_pool().is_connected(&alias),
            }));
        }
        Ok(pretty(&Value::Array(machines)))
    }

    /// Add a machine to the reachable set or take it out of it. The state file
    /// is read again first: the dashboard owns the same file, and an MCP
    /// process that started an hour ago must not write back a stale view.
    fn set_machine_enabled(&mut self, arguments: &Value) -> Result<String> {
        let machine = required_str(arguments, "machine")?.to_string();
        let enabled = arguments
            .get("enabled")
            .and_then(Value::as_bool)
            .context("set_machine_enabled requires enabled: true or false")?;
        if enabled && machine != crate::model::LOCAL_TARGET_ID {
            let aliases = ssh_config::load_hosts(&self.config.ssh_config_path())?;
            if !aliases.iter().any(|alias| alias == &machine) {
                bail!(
                    "{machine} is neither \"local\" nor an SSH alias this machine knows; \
                     add it with ssh_host first"
                );
            }
        }
        let mut state = State::load(&self.state_path)?;
        let changed = if enabled {
            state.enabled_hosts.insert(machine.clone())
        } else {
            state.enabled_hosts.remove(&machine)
        };
        if changed {
            state.save(&self.state_path)?;
        }
        self.state = state;
        Ok(pretty(&json!({
            "machine": machine,
            "enabled": enabled,
            "changed": changed,
            "enabled_machines": self.state.enabled_hosts.iter().collect::<Vec<_>>(),
        })))
    }

    /// Read the SSH aliases this machine knows, or write one into the file
    /// muxloom owns. Hosts the user maintains are read but never rewritten.
    fn ssh_host(&mut self, arguments: &Value) -> Result<String> {
        let ssh_path = self.config.ssh_config_path();
        let managed_path = ssh_config::managed_path(&ssh_path);
        match required_str(arguments, "action")? {
            "list" => {
                let managed_file = ssh_config::normalize(&managed_path);
                let managed = ManagedHosts::load(&managed_path)?;
                let hosts: Vec<Value> = ssh_config::load_host_sources(&ssh_path)?
                    .into_iter()
                    .map(|(alias, sources)| {
                        json!({
                            "host": alias,
                            "enabled": self.state.enabled_hosts.contains(&alias),
                            "managed": sources.iter().any(|source| source == &managed_file),
                            "defined_in": sources
                                .iter()
                                .map(|source| source.display().to_string())
                                .collect::<Vec<_>>(),
                            "options": managed.get(&alias).map(|entry| {
                                entry
                                    .options
                                    .iter()
                                    .map(|(keyword, value)| format!("{keyword} {value}"))
                                    .collect::<Vec<_>>()
                            }),
                        })
                    })
                    .collect();
                Ok(pretty(&json!({
                    "ssh_config": ssh_path.display().to_string(),
                    "managed_file": managed_path.display().to_string(),
                    "hosts": hosts,
                })))
            }
            "set" => {
                let alias = ssh_alias(arguments)?;
                let outside = ssh_config::defined_outside(&ssh_path, &alias)?;
                if !outside.is_empty() {
                    bail!(
                        "{alias} is already defined in {}; muxloom will not shadow a host the \
                         user maintains — choose another alias, or ask them to change that file",
                        outside
                            .iter()
                            .map(|source| source.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                let mut options = Vec::new();
                if let Some(hostname) = ssh_value(arguments, "hostname")? {
                    options.push(("HostName".to_string(), hostname));
                }
                if let Some(user) = ssh_value(arguments, "user")? {
                    options.push(("User".to_string(), user));
                }
                if let Some(port) = arguments.get("port").filter(|port| !port.is_null()) {
                    let port = port
                        .as_u64()
                        .filter(|port| (1..=65_535).contains(port))
                        .context("port must be a number from 1 to 65535")?;
                    options.push(("Port".to_string(), port.to_string()));
                }
                if let Some(identity) = ssh_value(arguments, "identity_file")? {
                    options.push(("IdentityFile".to_string(), identity));
                }
                if let Some(jump) = ssh_value(arguments, "proxy_jump")? {
                    options.push(("ProxyJump".to_string(), jump));
                }
                options.extend(ssh_extra_options(arguments)?);
                if options.is_empty() {
                    bail!(
                        "set needs at least one of hostname, user, port, identity_file, \
                         proxy_jump, or extra"
                    );
                }
                let mut managed = ManagedHosts::load(&managed_path)?;
                let previous = managed.get(&alias).map(|entry| {
                    entry
                        .options
                        .iter()
                        .map(|(keyword, value)| format!("{keyword} {value}"))
                        .collect::<Vec<_>>()
                });
                managed.set(&alias, options.clone());
                ssh_config::write_private(&managed_path, &managed.render())?;
                // Only once the file is there: an Include of nothing is not an
                // Include muxloom can recognise on the next call.
                let included = ssh_config::ensure_include(&ssh_path, &managed_path)?;
                Ok(pretty(&json!({
                    "host": alias,
                    "options": options
                        .iter()
                        .map(|(keyword, value)| format!("{keyword} {value}"))
                        .collect::<Vec<_>>(),
                    "previous": previous,
                    "managed_file": managed_path.display().to_string(),
                    "include_added": included,
                    "enabled": self.state.enabled_hosts.contains(&alias),
                    "note": "muxloom cannot address this machine until set_machine_enabled \
                             turns it on, and connecting still needs working SSH credentials.",
                })))
            }
            "remove" => {
                let alias = ssh_alias(arguments)?;
                let mut managed = ManagedHosts::load(&managed_path)?;
                if !managed.remove(&alias) {
                    let outside = ssh_config::defined_outside(&ssh_path, &alias)?;
                    if outside.is_empty() {
                        bail!("{alias} is not in muxloom's managed SSH file");
                    }
                    bail!(
                        "{alias} is defined in {}; muxloom did not write it and will not remove it",
                        outside
                            .iter()
                            .map(|source| source.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                ssh_config::write_private(&managed_path, &managed.render())?;
                // An alias nobody can resolve is not a machine anyone can
                // reach, so it leaves the enabled set with its definition.
                let mut state = State::load(&self.state_path)?;
                let disabled = state.enabled_hosts.remove(&alias);
                if disabled {
                    state.save(&self.state_path)?;
                }
                self.state = state;
                Ok(pretty(&json!({
                    "host": alias,
                    "removed": true,
                    "disabled": disabled,
                    "managed_file": managed_path.display().to_string(),
                })))
            }
            other => bail!("unknown ssh_host action {other}: use list, set, or remove"),
        }
    }

    fn list_sessions(&self, arguments: &Value) -> Result<String> {
        let include_archived = optional_bool(arguments, "include_archived");
        let mut rendered = Vec::new();
        for target in self.targets(arguments)? {
            match self.runtime.bridge_pool().list_sessions(&target) {
                Ok(sessions) => rendered.extend(
                    sessions
                        .iter()
                        .filter(|session| include_archived || !session.archived)
                        .map(|session| session_json(&target.id, session)),
                ),
                Err(error) => rendered.push(json!({
                    "machine": target.id,
                    "error": format!("{error:#}"),
                })),
            }
        }
        Ok(pretty(&Value::Array(rendered)))
    }

    fn read_screen(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let session_id = required_str(arguments, "session_id")?;
        let lines = optional_usize(arguments, "lines", DEFAULT_SCREEN_LINES);
        let offset = optional_usize(arguments, "offset_from_bottom", 0);
        let page = self
            .runtime
            .capture_page(&target, session_id, offset, lines, 0, 0)?;
        Ok(screen_page(
            &page.text,
            page.offset_from_bottom,
            page.pane_height,
            page.has_older(),
        ))
    }

    fn launch_session(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let kind = agent_kind(arguments)?;
        let request = LaunchRequest {
            target: target.clone(),
            kind,
            path: required_str(arguments, "path")?.into(),
            label: optional_str(arguments, "label").unwrap_or_default().into(),
            temporary: false,
            resume_id: optional_str(arguments, "resume_id").map(Into::into),
            initial_prompt: optional_str(arguments, "initial_prompt").map(Into::into),
        };
        let command = self.config.command_for(&target.id, kind).clone();
        let environment = self.config.environment_for(&target.id)?;
        let session_id = self.runtime.launch(&request, &command, &environment)?;
        Ok(pretty(&json!({
            "session_id": session_id,
            "machine": target.id,
            "kind": kind.as_str(),
            "path": request.path,
        })))
    }

    fn list_resume_candidates(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let path = required_str(arguments, "path")?;
        let mut candidates = Vec::new();
        let mut warnings = Vec::new();
        for kind in [AgentKind::Codex, AgentKind::Claude] {
            match self.runtime.scan_resumes(&target, kind, path) {
                Ok(found) => candidates.extend(found),
                Err(error) => warnings.push(format!("{kind}: {error:#}")),
            }
        }
        if candidates.is_empty() && !warnings.is_empty() {
            bail!("no history could be scanned: {}", warnings.join("; "));
        }
        candidates.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        candidates.truncate(50);
        let rendered: Vec<Value> = candidates
            .iter()
            .map(|candidate| {
                json!({
                    "resume_id": candidate.id,
                    "kind": candidate.kind.as_str(),
                    "updated_at": candidate.updated_at,
                    "summary": candidate.summary(),
                })
            })
            .collect();
        Ok(pretty(&json!({
            "candidates": rendered,
            "warnings": warnings,
        })))
    }

    fn search_history(&self, arguments: &Value) -> Result<String> {
        let query = required_str(arguments, "query")?;
        let mut results = Vec::new();
        for target in self.targets(arguments)? {
            let sessions: Vec<(String, String)> = match optional_str(arguments, "session_id") {
                Some(session_id) => vec![(session_id.into(), String::new())],
                None => match self.runtime.bridge_pool().list_sessions(&target) {
                    Ok(sessions) => sessions
                        .into_iter()
                        .filter(|session| !session.temporary)
                        .map(|session| (session.id, session.label))
                        .collect(),
                    Err(error) => {
                        results.push(json!({
                            "machine": target.id,
                            "error": format!("{error:#}"),
                        }));
                        continue;
                    }
                },
            };
            for (session_id, label) in sessions {
                let matches = match self.runtime.search_history(
                    &target,
                    &session_id,
                    query,
                    SEARCH_MAX_MATCHES,
                ) {
                    Ok(matches) => matches,
                    Err(_) => continue,
                };
                if matches.is_empty() {
                    continue;
                }
                results.push(json!({
                    "machine": target.id,
                    "session_id": session_id,
                    "label": label,
                    "matches": matches
                        .iter()
                        .map(|item| {
                            json!({
                                "line_number": item.line_number,
                                "recap": item.recap,
                                "text": item.text,
                            })
                        })
                        .collect::<Vec<_>>(),
                }));
            }
        }
        Ok(pretty(&Value::Array(results)))
    }

    fn run_shell(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let script = required_str(arguments, "script")?;
        let output = self.runtime.run_shell(&target, script, false)?;
        Ok(shell_report(
            output.status.code().unwrap_or(-1),
            &output.stdout,
            &output.stderr,
        ))
    }
}

impl ControlSurface for ControllerControl {
    fn tools(&self) -> Vec<ToolSpec> {
        allowed_specs(Flavor::Controller, &self.config.mcp)
    }

    fn instructions(&self) -> Option<String> {
        Some(instructions(Flavor::Controller, &self.config.mcp))
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<String> {
        enforce_policy(&self.config.mcp, name)?;
        match name {
            "list_machines" => self.list_machines(),
            "set_machine_enabled" => self.set_machine_enabled(arguments),
            "ssh_host" => self.ssh_host(arguments),
            "list_sessions" => self.list_sessions(arguments),
            "read_screen" => self.read_screen(arguments),
            "send_input" => {
                let target = self.target(arguments)?;
                let session_id = required_str(arguments, "session_id")?;
                let bytes = build_input(arguments)?;
                self.runtime.send_input(&target, session_id, &bytes)?;
                Ok(format!("sent {} bytes to {session_id}", bytes.len()))
            }
            "launch_session" => self.launch_session(arguments),
            "list_resume_candidates" => self.list_resume_candidates(arguments),
            "archive_session" => {
                let target = self.target(arguments)?;
                let session_id = required_str(arguments, "session_id")?;
                self.runtime.archive(&target, session_id)?;
                Ok(format!("archived {session_id}"))
            }
            "delete_session" => {
                let target = self.target(arguments)?;
                let session_id = required_str(arguments, "session_id")?;
                self.runtime.kill(&target, session_id)?;
                Ok(format!("deleted {session_id}"))
            }
            "search_history" => self.search_history(arguments),
            "list_directory" => {
                let target = self.target(arguments)?;
                let path = required_str(arguments, "path")?;
                let listing = self.runtime.list_directory(&target, path)?;
                Ok(pretty(&json!({
                    "path": listing.path,
                    "directories": listing.directories,
                })))
            }
            "list_files" => {
                let target = self.target(arguments)?;
                let path = required_str(arguments, "path")?;
                let listing = self.runtime.list_files(&target, path)?;
                Ok(pretty(&serde_json::to_value(&listing)?))
            }
            "search_files" => {
                let target = self.target(arguments)?;
                let root = required_str(arguments, "root")?;
                let pattern = required_str(arguments, "pattern")?;
                let listing = self.runtime.search_files(&target, root, pattern)?;
                Ok(pretty(&serde_json::to_value(&listing)?))
            }
            "preview_file" => {
                let target = self.target(arguments)?;
                let path = required_str(arguments, "path")?;
                let preview = self.runtime.preview_file(&target, path)?;
                Ok(preview_text(&preview))
            }
            "run_shell" => self.run_shell(arguments),
            other => bail!("unknown tool {other}"),
        }
    }
}

#[cfg(unix)]
pub use daemon_surface::DaemonControl;

#[cfg(unix)]
mod daemon_surface {
    use std::{collections::HashMap, time::Duration};

    use anyhow::{Context, Result, bail};
    use serde_json::{Value, json};

    use super::{
        DEFAULT_SCREEN_LINES, Flavor, SEARCH_MAX_MATCHES, agent_kind, allowed_specs, build_input,
        enforce_policy, instructions, optional_bool, optional_str, optional_usize, pretty,
        preview_text, required_str, screen_page, session_json, shell_report,
    };
    use crate::{
        config::{Config, default_config_path},
        daemon::{DaemonPaths, connect_or_start},
        daemon_protocol::{DaemonRequest, DaemonResponse, DaemonSession, Frame, FrameKind, stream},
        model::LOCAL_TARGET_ID,
        runtime::{launch_arguments, new_daemon_session_id},
    };

    /// How long one daemon request may run. Matches the bridge's own request
    /// timeout: a shell script is the slowest thing a request can carry.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
    /// The most preview bytes a tool answer carries.
    const PREVIEW_LIMIT: usize = 256 * 1024;

    /// Serves the daemon on this machine over its Unix socket. Each call opens
    /// its own connection: a resident client would hold the daemon's client
    /// count up and defer generation handover indefinitely, and a fresh
    /// connection also tolerates the daemon being replaced between calls.
    pub struct DaemonControl {
        paths: DaemonPaths,
        config: Config,
    }

    impl DaemonControl {
        pub fn new() -> Result<Self> {
            Ok(Self {
                paths: DaemonPaths::discover()?,
                config: Config::load(&default_config_path())?,
            })
        }

        /// A surface over an explicit state directory and config, for tests
        /// and for pointing at a non-default daemon.
        pub fn with_paths(paths: DaemonPaths, config: Config) -> Self {
            Self { paths, config }
        }

        /// Send one request and collect its response plus any stream data it
        /// produced, keyed by stream id.
        fn transact(
            &self,
            request: &DaemonRequest,
        ) -> Result<(DaemonResponse, HashMap<u32, Vec<u8>>)> {
            let mut connection = connect_or_start(&self.paths)?;
            connection
                .set_read_timeout(Some(REQUEST_TIMEOUT))
                .context("failed to bound the daemon request")?;
            Frame::json(FrameKind::Request, 0, 1, request)?.write_to(&mut connection)?;
            let mut data: HashMap<u32, Vec<u8>> = HashMap::new();
            loop {
                let frame = Frame::read_from(&mut connection)?
                    .context("muxloomd closed before answering")?;
                match frame.kind {
                    FrameKind::Data if frame.request_id == 1 => {
                        data.entry(frame.stream_id)
                            .or_default()
                            .extend(frame.decoded_payload()?);
                    }
                    FrameKind::Response if frame.request_id == 1 => {
                        let response = frame.decode_json::<DaemonResponse>()?;
                        if let DaemonResponse::Error { message } = response {
                            bail!("{message}");
                        }
                        return Ok((response, data));
                    }
                    _ => {}
                }
            }
        }

        fn sessions(&self) -> Result<Vec<DaemonSession>> {
            match self.transact(&DaemonRequest::ListSessions)?.0 {
                DaemonResponse::Sessions { sessions } => Ok(sessions),
                response => bail!("unexpected session-list response: {response:?}"),
            }
        }

        fn expect_ack(&self, request: &DaemonRequest) -> Result<()> {
            match self.transact(request)?.0 {
                DaemonResponse::Ack => Ok(()),
                response => bail!("unexpected daemon response: {response:?}"),
            }
        }

        fn read_screen(&self, arguments: &Value) -> Result<String> {
            let session_id = required_str(arguments, "session_id")?;
            let lines = optional_usize(arguments, "lines", DEFAULT_SCREEN_LINES);
            let offset = optional_usize(arguments, "offset_from_bottom", 0);
            let (response, data) = self.transact(&DaemonRequest::ReadHistory {
                session_id: session_id.into(),
                offset_from_bottom: offset,
                lines,
                rendered: true,
            })?;
            match response {
                DaemonResponse::HistoryComplete {
                    rows,
                    offset_from_bottom,
                    rendered,
                    reached_start,
                    ..
                } => {
                    let text = String::from_utf8_lossy(
                        data.get(&stream::HISTORY).map_or(&[][..], Vec::as_slice),
                    );
                    let older = rendered && !reached_start && offset_from_bottom >= offset;
                    Ok(screen_page(
                        text.trim_end(),
                        offset_from_bottom,
                        usize::from(rows),
                        older,
                    ))
                }
                response => bail!("unexpected history response: {response:?}"),
            }
        }

        fn launch_session(&self, arguments: &Value) -> Result<String> {
            let kind = agent_kind(arguments)?;
            let path = required_str(arguments, "path")?;
            let command = self.config.command_for(LOCAL_TARGET_ID, kind).clone();
            if command.command.trim().is_empty() && kind != crate::model::AgentKind::Terminal {
                bail!("command for {kind} is empty");
            }
            let args = launch_arguments(
                &command,
                kind,
                false,
                optional_str(arguments, "resume_id"),
                optional_str(arguments, "initial_prompt"),
            );
            let environment = self.config.environment_for(LOCAL_TARGET_ID)?;
            let (session_id, created_at) = new_daemon_session_id(kind, false);
            let response = self
                .transact(&DaemonRequest::Launch {
                    session_id: session_id.clone(),
                    kind: kind.as_str().into(),
                    path: path.into(),
                    label: optional_str(arguments, "label")
                        .unwrap_or_default()
                        .replace(['\t', '\n', '\r'], " "),
                    temporary: false,
                    executable: command.command.clone(),
                    args,
                    environment,
                    created_at,
                    columns: 120,
                    rows: 40,
                })?
                .0;
            match response {
                DaemonResponse::Launched { session } => Ok(pretty(&json!({
                    "session_id": session.id,
                    "machine": LOCAL_TARGET_ID,
                    "kind": session.kind,
                    "path": session.path,
                }))),
                response => bail!("unexpected launch response: {response:?}"),
            }
        }

        fn search_history(&self, arguments: &Value) -> Result<String> {
            let query = required_str(arguments, "query")?;
            let sessions: Vec<(String, String)> = match optional_str(arguments, "session_id") {
                Some(session_id) => vec![(session_id.into(), String::new())],
                None => self
                    .sessions()?
                    .into_iter()
                    .filter(|session| !session.temporary)
                    .map(|session| (session.id, session.label))
                    .collect(),
            };
            let mut results = Vec::new();
            for (session_id, label) in sessions {
                let matches = match self.transact(&DaemonRequest::SearchHistory {
                    session_id: session_id.clone(),
                    query: query.into(),
                    max_matches: SEARCH_MAX_MATCHES,
                }) {
                    Ok((DaemonResponse::HistoryMatches { matches }, _)) => matches,
                    Ok(_) | Err(_) => continue,
                };
                if matches.is_empty() {
                    continue;
                }
                results.push(json!({
                    "machine": LOCAL_TARGET_ID,
                    "session_id": session_id,
                    "label": label,
                    "matches": matches
                        .iter()
                        .map(|item| {
                            json!({
                                "line_number": item.line_number,
                                "recap": item.recap,
                                "text": item.text,
                            })
                        })
                        .collect::<Vec<_>>(),
                }));
            }
            Ok(pretty(&Value::Array(results)))
        }

        fn run_shell(&self, arguments: &Value) -> Result<String> {
            let script = required_str(arguments, "script")?;
            let (response, data) = self.transact(&DaemonRequest::RunShell {
                script: script.into(),
                environment: self.config.environment_for(LOCAL_TARGET_ID)?,
            })?;
            match response {
                DaemonResponse::ShellComplete { exit_code } => Ok(shell_report(
                    exit_code,
                    data.get(&stream::STDOUT).map_or(&[][..], Vec::as_slice),
                    data.get(&stream::STDERR).map_or(&[][..], Vec::as_slice),
                )),
                response => bail!("unexpected shell response: {response:?}"),
            }
        }
    }

    impl super::ControlSurface for DaemonControl {
        fn tools(&self) -> Vec<super::ToolSpec> {
            allowed_specs(Flavor::Daemon, &self.config.mcp)
        }

        fn instructions(&self) -> Option<String> {
            Some(instructions(Flavor::Daemon, &self.config.mcp))
        }

        fn call(&mut self, name: &str, arguments: &Value) -> Result<String> {
            enforce_policy(&self.config.mcp, name)?;
            match name {
                "list_sessions" => {
                    let include_archived = optional_bool(arguments, "include_archived");
                    let sessions = self.sessions()?;
                    let rendered: Vec<Value> = sessions
                        .iter()
                        .filter(|session| include_archived || !session.archived)
                        .map(|session| session_json(LOCAL_TARGET_ID, session))
                        .collect();
                    Ok(pretty(&Value::Array(rendered)))
                }
                "read_screen" => self.read_screen(arguments),
                "send_input" => {
                    let session_id = required_str(arguments, "session_id")?;
                    let bytes = build_input(arguments)?;
                    self.expect_ack(&DaemonRequest::SendInput {
                        session_id: session_id.into(),
                        bytes: bytes.clone(),
                    })?;
                    Ok(format!("sent {} bytes to {session_id}", bytes.len()))
                }
                "launch_session" => self.launch_session(arguments),
                "archive_session" => {
                    let session_id = required_str(arguments, "session_id")?;
                    self.expect_ack(&DaemonRequest::Archive {
                        session_id: session_id.into(),
                    })?;
                    Ok(format!("archived {session_id}"))
                }
                "delete_session" => {
                    let session_id = required_str(arguments, "session_id")?;
                    self.expect_ack(&DaemonRequest::Delete {
                        session_id: session_id.into(),
                    })?;
                    Ok(format!("deleted {session_id}"))
                }
                "search_history" => self.search_history(arguments),
                "list_directory" => {
                    let path = required_str(arguments, "path")?;
                    match self
                        .transact(&DaemonRequest::ListDirectory { path: path.into() })?
                        .0
                    {
                        DaemonResponse::Directory { listing } => Ok(pretty(&json!({
                            "path": listing.path,
                            "directories": listing.directories,
                        }))),
                        response => bail!("unexpected directory response: {response:?}"),
                    }
                }
                "list_files" => {
                    let path = required_str(arguments, "path")?;
                    match self
                        .transact(&DaemonRequest::ListFiles { path: path.into() })?
                        .0
                    {
                        DaemonResponse::Files { listing } => {
                            Ok(pretty(&serde_json::to_value(&listing)?))
                        }
                        response => bail!("unexpected file-list response: {response:?}"),
                    }
                }
                "preview_file" => {
                    let path = required_str(arguments, "path")?;
                    match self
                        .transact(&DaemonRequest::PreviewFile {
                            path: path.into(),
                            limit: PREVIEW_LIMIT,
                        })?
                        .0
                    {
                        DaemonResponse::Preview { preview } => Ok(preview_text(&preview)),
                        response => bail!("unexpected preview response: {response:?}"),
                    }
                }
                "run_shell" => self.run_shell(arguments),
                other => bail!("unknown tool {other}"),
            }
        }
    }
}

#[cfg(not(unix))]
pub struct DaemonControl;

#[cfg(not(unix))]
impl DaemonControl {
    pub fn new() -> Result<Self> {
        bail!("muxloomd is currently supported on Unix targets")
    }
}

#[cfg(not(unix))]
impl ControlSurface for DaemonControl {
    fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&mut self, _name: &str, _arguments: &Value) -> Result<String> {
        bail!("muxloomd is currently supported on Unix targets")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_encode_to_the_bytes_a_terminal_expects() {
        assert_eq!(encode_key("enter").unwrap(), b"\r");
        assert_eq!(encode_key("up").unwrap(), b"\x1b[A");
        assert_eq!(encode_key("ctrl-c").unwrap(), vec![3]);
        assert_eq!(encode_key("ctrl-z").unwrap(), vec![26]);
        assert!(encode_key("ctrl-1").is_err());
        assert!(encode_key("meta-x").is_err());
    }

    #[test]
    fn input_is_text_then_keys_then_submit() {
        let bytes = build_input(&json!({
            "text": "ls",
            "keys": ["tab", "ctrl-a"],
            "submit": true,
        }))
        .unwrap();
        assert_eq!(bytes, b"ls\t\x01\r");
        assert!(build_input(&json!({})).is_err());
        assert_eq!(build_input(&json!({"submit": true})).unwrap(), b"\r");
    }

    #[test]
    fn a_screen_reads_back_as_text_with_its_columns_intact() {
        // A row as the renderer writes it: colour, a jump over blanks that
        // stands in for indentation, and a title nobody reading wants.
        let page = screen_page(
            "\x1b[1;32m❯ 1. Yes\x1b[m\n\x1b[m\x1b]0;claude\x07 2.\x1b[3CNo   \n\n",
            0,
            40,
            false,
        );
        let (text, _) = page.split_once("\n\n[rows=").unwrap();
        assert_eq!(text, "❯ 1. Yes\n 2.   No");
    }

    #[test]
    fn daemon_and_controller_surfaces_share_tool_shapes() {
        let daemon: Vec<_> = specs(Flavor::Daemon);
        let controller: Vec<_> = specs(Flavor::Controller);
        for tool in &daemon {
            let twin = controller
                .iter()
                .find(|candidate| candidate.name == tool.name)
                .unwrap_or_else(|| panic!("{} missing from controller surface", tool.name));
            // The controller adds machine addressing; everything else matches.
            let mut twin_schema = twin.input_schema.clone();
            twin_schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("machine");
            assert_eq!(twin_schema, tool.input_schema, "{} diverged", tool.name);
        }
        for name in [
            "list_machines",
            "list_resume_candidates",
            "search_files",
            // The machine set and the SSH config live on the controller: a
            // daemon has neither to offer.
            "set_machine_enabled",
            "ssh_host",
        ] {
            assert!(controller.iter().any(|tool| tool.name == name));
            assert!(!daemon.iter().any(|tool| tool.name == name));
        }
    }

    #[test]
    fn denied_tools_leave_the_list_and_are_refused_by_name() {
        let mut policy = McpConfig {
            denied_tools: vec!["run_shell".into()],
            read_only: false,
        };
        let offered: Vec<&str> = allowed_specs(Flavor::Controller, &policy)
            .iter()
            .map(|tool| tool.name)
            .collect();
        assert!(!offered.contains(&"run_shell"));
        assert!(offered.contains(&"read_screen"));
        // Hiding the tool is advice; the gate is the call itself, because an
        // agent can remember a name from a machine where it is allowed.
        let error = enforce_policy(&policy, "run_shell")
            .unwrap_err()
            .to_string();
        assert!(error.contains("denied_tools"), "{error}");
        assert!(enforce_policy(&policy, "read_screen").is_ok());
        assert!(instructions(Flavor::Controller, &policy).contains("disabled these tools"));

        // Read-only denies every write tool and leaves the observers alone.
        policy = McpConfig {
            denied_tools: Vec::new(),
            read_only: true,
        };
        for tool in WRITE_TOOLS {
            assert!(
                enforce_policy(&policy, tool).is_err(),
                "{tool} must be denied read-only"
            );
        }
        let offered: Vec<&str> = allowed_specs(Flavor::Daemon, &policy)
            .iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            offered,
            [
                "list_sessions",
                "read_screen",
                "search_history",
                "list_directory",
                "list_files",
                "preview_file"
            ]
        );
    }

    #[test]
    fn instructions_point_at_sessions_and_fence_the_shell() {
        let text = instructions(Flavor::Controller, &McpConfig::default());
        assert!(text.contains("run_shell is a last resort"));
        assert!(text.contains("send_input"));
        assert!(text.contains("not enabled") || text.contains("has not enabled"));
        assert!(!text.contains("disabled these tools"));
        // The daemon surface has no machine argument to talk about.
        let daemon = instructions(Flavor::Daemon, &McpConfig::default());
        assert!(!daemon.contains("`machine` argument"));
    }

    /// A controller surface over a throwaway home: its own SSH config and its
    /// own state file, so a test can write both.
    fn controller_over_temp(name: &str) -> (ControllerControl, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "muxloom-control-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(root.join("ssh")).unwrap();
        let config = Config {
            ssh_config: root.join("ssh/config").to_str().unwrap().into(),
            ..Config::default()
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let control = ControllerControl {
            runtime: Runtime::new(&config),
            config,
            state,
            state_path: root.join("state.json"),
        };
        (control, root)
    }

    #[test]
    fn controller_surface_gates_machines_on_the_enabled_set() {
        let (mut control, root) = controller_over_temp("gate");
        let ssh_config = control.config.ssh_config_path();
        std::fs::write(&ssh_config, "Host gpu\n  HostName 10.0.0.1\n").unwrap();

        assert_eq!(control.target(&json!({})).unwrap().id, "local");
        // A machine the user has not enabled must be unreachable even by name.
        let error = control
            .target(&json!({ "machine": "gpu" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not enabled"), "{error}");

        let machines: Value = serde_json::from_str(&control.list_machines().unwrap()).unwrap();
        let machine = |id: &str| {
            machines
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == id)
                .unwrap_or_else(|| panic!("{id} missing from list_machines"))
                .clone()
        };
        assert_eq!(machine("local")["enabled"], true);
        assert_eq!(machine("gpu")["enabled"], false);

        control.state.enabled_hosts.insert("gpu".into());
        assert_eq!(
            control.target(&json!({ "machine": "gpu" })).unwrap().id,
            "gpu"
        );
        // An aggregate call without a machine sweeps every enabled machine.
        assert_eq!(control.targets(&json!({})).unwrap().len(), 2);
        assert_eq!(
            control.targets(&json!({ "machine": "gpu" })).unwrap().len(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ssh_writes_stay_inside_the_file_muxloom_owns() {
        let (mut control, root) = controller_over_temp("ssh");
        let ssh_path = control.config.ssh_config_path();
        std::fs::write(&ssh_path, "Host mine\n  HostName mine.example\n").unwrap();

        // An alias this machine cannot resolve is not a machine: enabling it
        // would leave a name in the state file that never connects.
        let error = control
            .call(
                "set_machine_enabled",
                &json!({ "machine": "gpu", "enabled": true }),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("ssh_host"), "{error}");

        let written = control
            .call(
                "ssh_host",
                &json!({
                    "action": "set",
                    "host": "gpu",
                    "hostname": "10.0.0.5",
                    "user": "ada",
                    "port": 2222,
                    "extra": ["ForwardAgent yes"],
                }),
            )
            .unwrap();
        let written: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(written["include_added"], true);
        let managed_path = ssh_config::managed_path(&ssh_path);
        let managed = std::fs::read_to_string(&managed_path).unwrap();
        assert!(
            managed.contains(
                "Host gpu\n    HostName 10.0.0.5\n    User ada\n    Port 2222\n    \
                 ForwardAgent yes\n"
            ),
            "{managed}"
        );
        // The user's own file keeps every line it had, and gains one Include.
        let config_text = std::fs::read_to_string(&ssh_path).unwrap();
        assert!(config_text.starts_with("# Added by muxloom"));
        assert!(config_text.contains("Host mine\n  HostName mine.example\n"));

        // Writing the alias does not make it reachable; enabling it does.
        assert!(control.target(&json!({ "machine": "gpu" })).is_err());
        control
            .call(
                "set_machine_enabled",
                &json!({ "machine": "gpu", "enabled": true }),
            )
            .unwrap();
        assert_eq!(
            control.target(&json!({ "machine": "gpu" })).unwrap().id,
            "gpu"
        );
        assert!(
            State::load(&control.state_path)
                .unwrap()
                .enabled_hosts
                .contains("gpu")
        );

        // A host the user wrote is readable, and theirs alone to change.
        let error = control
            .call(
                "ssh_host",
                &json!({ "action": "set", "host": "mine", "hostname": "stolen" }),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("will not shadow"), "{error}");
        let error = control
            .call("ssh_host", &json!({ "action": "remove", "host": "mine" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("will not remove"), "{error}");

        // Arguments that would break out of the block they are written into.
        for arguments in [
            json!({ "action": "set", "host": "gpu", "hostname": "a\nHost evil" }),
            json!({ "action": "set", "host": "e vil", "hostname": "a" }),
            json!({ "action": "set", "host": "*", "hostname": "a" }),
            json!({ "action": "set", "host": "gpu", "extra": ["Host evil"] }),
            json!({ "action": "set", "host": "gpu", "port": 0 }),
            json!({ "action": "set", "host": "gpu" }),
        ] {
            assert!(
                control.call("ssh_host", &arguments).is_err(),
                "{arguments} must be refused"
            );
        }
        assert_eq!(std::fs::read_to_string(&managed_path).unwrap(), managed);

        let listed: Value = serde_json::from_str(
            &control
                .call("ssh_host", &json!({ "action": "list" }))
                .unwrap(),
        )
        .unwrap();
        let host = |name: &str| {
            listed["hosts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|host| host["host"] == name)
                .unwrap_or_else(|| panic!("{name} missing from ssh_host list"))
                .clone()
        };
        assert_eq!(host("gpu")["managed"], true);
        assert_eq!(host("gpu")["enabled"], true);
        assert_eq!(host("mine")["managed"], false);
        assert!(host("mine")["options"].is_null());

        // Removing the definition takes the machine out of reach with it.
        control
            .call("ssh_host", &json!({ "action": "remove", "host": "gpu" }))
            .unwrap();
        assert!(control.target(&json!({ "machine": "gpu" })).is_err());
        assert!(
            !State::load(&control.state_path)
                .unwrap()
                .enabled_hosts
                .contains("gpu")
        );
        assert!(
            !std::fs::read_to_string(&managed_path)
                .unwrap()
                .contains("Host gpu")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    mod daemon_round_trip {
        use std::{
            path::PathBuf,
            thread,
            time::{Duration, Instant, SystemTime, UNIX_EPOCH},
        };

        use super::super::{ControlSurface, DaemonControl};
        use crate::{config::Config, daemon::DaemonPaths};
        use serde_json::{Value, json};

        /// One serve() loop on a temporary state directory, reached the same
        /// way a real `muxloomd mcp` reaches the real daemon.
        fn surface(name: &str) -> DaemonControl {
            // A short, fixed prefix: the state dir carries daemon and keeper
            // sockets, whose paths must stay under the ~104-byte sockaddr_un
            // limit that macOS's deep per-user temp dir nearly exhausts.
            let root = PathBuf::from("/tmp").join(format!(
                "mxl-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            ));
            let paths = DaemonPaths::under(root);
            let serve_paths = paths.clone();
            thread::spawn(move || {
                if let Err(error) = crate::daemon::serve_with_in_process_keepers(&serve_paths) {
                    eprintln!("test daemon exited: {error:#}");
                }
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while std::os::unix::net::UnixStream::connect(&paths.socket).is_err() {
                assert!(Instant::now() < deadline, "daemon socket never came up");
                thread::sleep(Duration::from_millis(20));
            }
            DaemonControl::with_paths(paths, Config::default())
        }

        fn call(surface: &mut DaemonControl, name: &str, arguments: Value) -> String {
            surface
                .call(name, &arguments)
                .unwrap_or_else(|error| panic!("{name} failed: {error:#}"))
        }

        #[test]
        fn a_session_lives_types_reads_and_dies_through_the_surface() {
            let mut surface = surface("rt");
            let workdir = std::env::temp_dir();
            let launched = call(
                &mut surface,
                "launch_session",
                json!({ "kind": "terminal", "path": workdir.to_str().unwrap() }),
            );
            let launched: Value = serde_json::from_str(&launched).unwrap();
            let session_id = launched["session_id"].as_str().unwrap().to_string();
            assert!(session_id.starts_with("muxloomd-terminal-"));

            let listed = call(&mut surface, "list_sessions", json!({}));
            assert!(listed.contains(&session_id));

            call(
                &mut surface,
                "send_input",
                json!({
                    "session_id": session_id,
                    "text": "echo muxloom-control-probe",
                    "submit": true,
                }),
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut screen = String::new();
            while Instant::now() < deadline {
                screen = call(
                    &mut surface,
                    "read_screen",
                    json!({ "session_id": session_id, "lines": 50 }),
                );
                // The echoed command and its output both carry the marker; the
                // shell writing the output is what proves the input arrived.
                if screen.matches("muxloom-control-probe").count() >= 2 {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            assert!(
                screen.matches("muxloom-control-probe").count() >= 2,
                "typed command must run: {screen}"
            );

            let found = call(
                &mut surface,
                "search_history",
                json!({ "query": "muxloom-control-probe" }),
            );
            assert!(found.contains(&session_id));

            let report = call(
                &mut surface,
                "run_shell",
                json!({ "script": "printf control-shell; exit 3" }),
            );
            assert!(report.contains("exit code 3") && report.contains("control-shell"));

            call(
                &mut surface,
                "delete_session",
                json!({ "session_id": session_id }),
            );
            let listed = call(
                &mut surface,
                "list_sessions",
                json!({ "include_archived": true }),
            );
            assert!(!listed.contains(&session_id));
        }
    }
}
