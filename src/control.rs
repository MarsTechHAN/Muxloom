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

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    config::{Config, State, default_state_path},
    model::{AgentKind, FilePreview, FilePreviewKind, LaunchRequest, Target},
    runtime::Runtime,
    ssh_config,
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
}

/// How many rendered rows a screen read returns when the caller does not say.
const DEFAULT_SCREEN_LINES: usize = 200;
/// Per-session match budget for history searches, matching the daemon's cap.
const SEARCH_MAX_MATCHES: usize = 12;
/// The most bytes of one shell stream a tool answer carries.
const SHELL_OUTPUT_LIMIT: usize = 128 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// Every enabled machine, addressed by a `machine` argument.
    Controller,
    /// The one local daemon. Constructed by the unix-only daemon surface, so
    /// a Windows lib build sees no non-test constructor.
    #[cfg_attr(not(unix), allow(dead_code))]
    Daemon,
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
        description: "Type into a session's terminal without disturbing an attached viewer. \
                      `text` is written verbatim, then each named key in `keys` (enter, esc, \
                      tab, backspace, space, delete, up, down, left, right, home, end, \
                      page-up, page-down, or ctrl-a…ctrl-z), then submit=true appends Enter. \
                      Prompts submitted to an agent take effect asynchronously: poll \
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
                      directory. `resume_id` resumes that agent-native conversation; \
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
        description: "Kill a session's process and delete its history and metadata. \
                      Irreversible; archive_session keeps the history instead."
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
        description: "Run a shell script on the machine with `sh -lc` and return its output \
                      and exit code. Runs with the user's full permissions — prefer the \
                      narrower tools when one fits."
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
    format!(
        "{text}\n\n[rows={rows} offset_from_bottom={offset_from_bottom} older_history_above={older}]"
    )
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
}

impl ControllerControl {
    pub fn new(config: Config) -> Result<Self> {
        let state = State::load(&default_state_path())?;
        let runtime = Runtime::new(&config);
        Ok(Self {
            runtime,
            config,
            state,
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
        specs(Flavor::Controller)
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<String> {
        match name {
            "list_machines" => self.list_machines(),
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
        DEFAULT_SCREEN_LINES, Flavor, SEARCH_MAX_MATCHES, agent_kind, build_input, optional_bool,
        optional_str, optional_usize, pretty, preview_text, required_str, screen_page,
        session_json, shell_report, specs,
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
            specs(Flavor::Daemon)
        }

        fn call(&mut self, name: &str, arguments: &Value) -> Result<String> {
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
        for name in ["list_machines", "list_resume_candidates", "search_files"] {
            assert!(controller.iter().any(|tool| tool.name == name));
            assert!(!daemon.iter().any(|tool| tool.name == name));
        }
    }

    #[test]
    fn controller_surface_gates_machines_on_the_enabled_set() {
        let ssh_config =
            std::env::temp_dir().join(format!("muxloom-control-ssh-config-{}", std::process::id()));
        std::fs::write(&ssh_config, "Host gpu\n  HostName 10.0.0.1\n").unwrap();
        let config = Config {
            ssh_config: ssh_config.to_str().unwrap().into(),
            ..Config::default()
        };
        let mut state = State::default();
        state.enabled_hosts.insert("local".into());
        let mut control = ControllerControl {
            runtime: Runtime::new(&config),
            config,
            state,
        };

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
        std::fs::remove_file(&ssh_config).unwrap();
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
