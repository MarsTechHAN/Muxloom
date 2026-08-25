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

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    config::{Config, McpConfig, State, default_state_path},
    daemon_protocol::{DaemonSession, Trigger, TriggerAction},
    model::{AgentKind, FilePreview, FilePreviewKind, LaunchRequest, Target},
    relay::now_ms,
    runtime::Runtime,
    ssh_config::{self, MANAGED_INCLUDE, ManagedHosts},
    talk::{
        MAX_TEXT, TalkAddress, TalkAuthor, TalkDeliver, TalkDraft, TalkFilter, TalkKind,
        TalkMessage, TalkPage, TalkScope, TalkSelector, TalkState, TalkVector, TalkVoice,
        decode_cursor, hostname, paste_bytes,
    },
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
/// How many rendered rows one round of a wait looks at.
const WAIT_SCREEN_LINES: usize = 80;
/// How many of those rows the answer carries back.
const WAIT_TAIL_LINES: usize = 30;
/// How long a wait runs when the caller does not say, and the most it may ask
/// for. The ceiling is under every MCP client's own call timeout: a wait that
/// ends saying "not yet" is answerable, one the transport gives up on is not.
const WAIT_DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const WAIT_MAX_TIMEOUT_SECONDS: u64 = 900;
/// Consecutive failed looks a wait rides out before reporting them. A daemon
/// handing over to a new generation is unreachable for a moment, and a wait
/// that outlives conversations must outlive that too.
const WAIT_ERROR_TOLERANCE: usize = 3;
/// The longest a `talk_read` may sit waiting for someone to say something, and
/// how often it looks while it waits. Same reasoning as the wait ceiling: an
/// answer of "nothing yet, here is your cursor" beats a dropped call.
///
/// Two minutes was too short by a wide margin. An agent asked a question is
/// usually in the middle of something else, and the answer comes when that
/// finishes: on this board most replies take minutes and a fair number take
/// far longer. A cap under the common case turns one wait into a poll loop the
/// asker has to drive, and an asker that stops driving it never hears back at
/// all. Same ceiling as `wait_for`, for the same reason.
const TALK_MAX_WAIT_SECONDS: u64 = WAIT_MAX_TIMEOUT_SECONDS;
const TALK_POLL: Duration = Duration::from_secs(2);
/// How far back a timed-out wait looks for messages of its own still waiting
/// on an answer. Longer than anyone reasonably waits in one call, so the
/// question "is anything of mine outstanding" is answered from the whole
/// exchange rather than from this call's window.
const TALK_OUTSTANDING_MS: u64 = 6 * 60 * 60 * 1000;

/// Every tool that changes something rather than reporting it. `read_only`
/// denies exactly this set, so a tool added here is denied by that switch from
/// the day it exists.
const WRITE_TOOLS: &[&str] = &[
    "send_input",
    "message_agent",
    "launch_session",
    "archive_session",
    "delete_session",
    "run_shell",
    "set_machine_enabled",
    "ssh_host",
    "trigger",
    "talk_post",
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
            "on this machine, which a muxloom controller may be watching along with others. \
             list_machines shows the others as `remote`: while the controller is attached you can \
             look at them and speak to the agents on them — every tool that reads, plus \
             message_agent and the talk board, takes a `machine` argument and the controller runs \
             it for you. Changing one of those machines is not yours to do; that is what the \
             agents living there are for. Everything else happens here"
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
        Flavor::Daemon => {
            "- A remote machine is only ever reached through the controller, and only to look or \
             to say something. If it is not attached you are told so immediately — that is the \
             whole answer, not a reason to retry. Nothing that changes a machine travels: to have \
             something done over there, ask an agent on that machine with message_agent.\n"
        }
    };
    let mut text = format!(
        "muxloom manages long-lived terminal sessions — Codex, Claude Code, and plain shells — \
         {reach}. Sessions outlive this conversation and the muxloom dashboard, and a human may \
         be watching any of them right now.\n\n\
         Work through the sessions rather than around them:\n\
         - To get work done on a machine, talk to the agent session that lives there: \
         message_agent to say something to another agent, send_input for raw keystrokes and for \
         plain shells, then wait_for or read_screen to see what came of it. Treat a session as a \
         colleague you are messaging, not as a subprocess you drive.\n\
         - run_shell is a last resort. Reach for it only for a short, non-interactive, ideally \
         read-only query that no other tool covers. Never start long-running or interactive work \
         with it — that is what launch_session is for — and never use it to do something a \
         session you could talk to would do better.\n\
         - Prefer the narrow tools (list_sessions, read_screen, list_files, preview_file, \
         search_history) over shell equivalents: they are bounded, paged, and safe to repeat.\n\n\
         Work with the others out in the open:\n\
         - talk_read before you start and after you have been away: the board carries what every \
         other agent and every person at a dashboard is doing, on every machine. talk_post what \
         you are about to change before you change it, and post what you worked out as kind \
         \"note\" so whoever comes next finds it instead of working it out again. When you are \
         running subagents, scope \"task\" keeps that work between you and them instead of in \
         front of everyone on the machine.\n\
         - message_agent is how you ask one agent for something: it lands in that session's \
         prompt in an envelope that names you, and it is read when the turn it is in ends. Its \
         answer comes back as a direct message — wait for it with talk_read {{ scope: \
         \"direct\", wait_seconds: {TALK_MAX_WAIT_SECONDS} }}, and call that again each time it \
         returns nothing rather than asking twice. Minutes is normal. When you are the one asked, \
         answer even if the answer is no: the agent waiting on you cannot act on silence.\n\
         - Nobody here is in charge of anyone else, and nothing you send has to be obeyed. Ask, \
         say why, and leave the other agent to judge it against what it is already doing.\n\n\
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
    tools.push(ToolSpec {
        name: "list_machines",
        description: match flavor {
            Flavor::Controller => {
                "List the machines muxloom manages: the local host and enabled SSH aliases. Other \
                 tools address a machine by its id."
            }
            Flavor::Daemon => {
                "List the machines you can reach: this one, and any a muxloom controller has said \
                 it can carry for you. Those are marked `remote` with the `via` that carries \
                 them — reaching one costs a round trip and only works while that controller is \
                 running. Other tools address a machine by its id."
            }
        }
        .into(),
        input_schema: schema(false, json!({}), &[]),
    });
    if multi {
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
                      line. A terminal has no recap - read_screen is how a shell is read. \
                      Archived sessions are included only with include_archived."
            .into(),
        input_schema: schema_across(
            flavor,
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
        name: "message_agent",
        description: "Say something to another agent: muxloom types your message into that \
                      session's prompt inside an envelope naming you, so the agent there knows it \
                      is hearing from an agent and how to answer. Use this to ask for work, hand \
                      something over, answer a question you were asked, or warn someone off a file \
                      you are changing — and use send_input instead only when you need raw \
                      keystrokes rather than a message. The target must be a codex or claude \
                      session; a terminal has nobody in it to read. Every message is also filed on \
                      the talk board, so read your own with talk_read { scope: \"direct\" } — that \
                      is where replies arrive if the other agent is not sure how to reach you. \
                      A message lands whole even while that agent is mid-turn; it is read when \
                      the turn ends, so one message should say everything you have to say. There \
                      is a rate limit per session and it will tell you. \"auto\" is almost always \
                      right; \"when_idle\" holds it back until the turn ends; \"now\" skips the \
                      checks that keep a message out of a dialog box."
            .into(),
        input_schema: schema_across(
            flavor,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions: who to tell." },
                "text": { "type": "string", "description": "What to say. Say it as you would to a colleague: what you want, and what you already know." },
                "deliver": { "type": "string", "enum": ["auto", "now", "when_idle"], "description": "When to put it in front of them. Default auto." },
                "reply_expected": { "type": "boolean", "description": "Say that you are waiting on an answer, and ask for one even if it is no. Then wait with talk_read { scope: \"direct\", wait_seconds }, calling it again each time it returns nothing: an answer often takes several minutes." },
                "reply_to": { "type": "string", "description": "The message_id this answers. Set it whenever you are replying — it is how the agent waiting on you knows its answer arrived." },
            }),
            &["session_id", "text"],
        ),
    });
    tools.push(ToolSpec {
        name: "wait_for",
        description: format!(
            "Wait for a session to reach a state, instead of calling read_screen in a loop. \
             `until` is one of: idle (not working and not waiting on anything), attention (it is \
             waiting for a human — a prompt, a permission question), output_matches (`pattern` \
             appears on the screen, matched case-insensitively), silence (the screen stops \
             changing for `quiet_seconds`), exit (the session ends). Returns the moment it \
             happens, or with outcome \"timeout\" after `timeout_seconds` (default \
             {WAIT_DEFAULT_TIMEOUT_SECONDS}, max {WAIT_MAX_TIMEOUT_SECONDS}) — a timeout is not a \
             failure, call it again to keep waiting. This is the tool to reach for after \
             send_input; to be told about something you will not be here for, use trigger."
        ),
        input_schema: schema(
            multi,
            json!({
                "session_id": { "type": "string", "description": "Session id from list_sessions." },
                "until": {
                    "type": "string",
                    "enum": ["idle", "attention", "output_matches", "silence", "exit"],
                },
                "pattern": { "type": "string", "description": "Text to wait for; required by until=output_matches." },
                "timeout_seconds": { "type": "integer", "description": "How long to wait before answering \"timeout\"." },
                "poll_ms": { "type": "integer", "description": "How often to look, 200-5000. Default 800." },
                "quiet_seconds": { "type": "integer", "description": "For until=silence: how long the screen must stay unchanged. Default 5." },
            }),
            &["session_id", "until"],
        ),
    });
    tools.push(ToolSpec {
        name: "trigger",
        description: "Leave a standing watch on a session with muxloom, for what you will not be \
                      around to see: when `pattern` appears on that session's screen, muxloom \
                      runs `action_kind` even though this conversation is over. `set` arms one \
                      and returns its id, `list` reports them, `delete` removes one. \
                      action_kind \"send_input\" types `text` back into the session (with Enter \
                      unless submit is false); \"notify\" marks the session as needing attention \
                      with `text` as the reason, which is what list_sessions and the dashboard \
                      show. A trigger fires on the way into a match — text already on the screen \
                      when it is armed does not count — at most once per cooldown_ms, and by \
                      default is removed once it has fired. Triggers outlive the daemon that \
                      took them and are dropped when their session dies. Use wait_for instead \
                      when you are going to sit and wait for it yourself."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "action": { "type": "string", "enum": ["set", "list", "delete"] },
                "session_id": { "type": "string", "description": "Session to watch (set), or to list watches for." },
                "pattern": { "type": "string", "description": "Text to watch the screen for, matched case-insensitively." },
                "action_kind": { "type": "string", "enum": ["send_input", "notify"], "description": "What to do on a match. Default notify." },
                "text": { "type": "string", "description": "Text to type (send_input) or the attention reason to show (notify)." },
                "submit": { "type": "boolean", "description": "For send_input: press Enter after the text. Default true." },
                "once": { "type": "boolean", "description": "Remove the trigger after it fires. Default true." },
                "cooldown_ms": { "type": "integer", "description": "Shortest gap between two firings. Default 5000." },
                "id": { "type": "string", "description": "Trigger id: required by delete, and replaces that trigger on set." },
            }),
            &["action"],
        ),
    });
    tools.push(ToolSpec {
        name: "talk_read",
        description: format!(
            "Read the talk board: the shared, cross-machine log every muxloom agent and every \
             person at a dashboard writes to. Read it before starting work to find out what the \
             others are doing, and after being idle to catch up. By default it shows what is in \
             front of you — this machine's board, this directory's board, what was said to \
             everyone, and messages addressed to you. `include_machines` and `include_paths` \
             widen that to named machines and directories, or to \"all\" to search everywhere. \
             `since_cursor` takes the `cursor` from a previous read and returns only what has \
             happened since, so polling never repeats itself; `wait_seconds` (up to \
             {TALK_MAX_WAIT_SECONDS}) holds the call open until something new is said, which is \
             how you wait to be answered. A wait that ends with nothing is not an answer of no: \
             it comes back with `waiting_on`, listing which of your own messages are still \
             unanswered and what the sessions holding them are doing, and calling it again is \
             usually right. `before` pages into the past. scope \"task\" is narrower than any of \
             that: just you, whoever started you, and the subagents any of you started."
        ),
        input_schema: schema(
            multi,
            json!({
                "scope": { "type": "string", "enum": ["path", "machine", "task", "global", "direct"], "description": "Only one kind of board. Default: all of them." },
                "since_cursor": { "type": "string", "description": "Cursor from an earlier read: return only what has been said since." },
                "wait_seconds": { "type": "integer", "description": "Wait this long for something new before answering. Default 0." },
                "limit": { "type": "integer", "description": "Newest N messages. Default 50." },
                "before": { "type": "integer", "description": "Epoch ms: read backwards from here, for paging into the past." },
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["message", "note", "direct"] } },
                "authors": { "type": "array", "items": { "type": "string" }, "description": "Session ids or labels." },
                "query": { "type": "string", "description": "Only messages containing this text." },
                "include_machines": { "type": "array", "items": { "type": "string" }, "description": "Also read these machines' boards, or [\"all\"] for every machine." },
                "include_paths": { "type": "array", "items": { "type": "string" }, "description": "Also read these directories' boards, or [\"all\"] for every directory." },
                "path": { "type": "string", "description": "Which directory's board counts as yours. Defaults to the session's own." },
            }),
            &[],
        ),
    });
    tools.push(ToolSpec {
        name: "talk_post",
        description: "Say something on the talk board, where every machine and every dashboard \
                      will see it. Use it to tell the others what you are working on, what you \
                      found, and what you are about to change — this is how agents avoid \
                      colliding, and nobody is in charge of anyone else here. `scope` decides who \
                      it is for: \"path\" (default) is the board for one directory on one machine, \
                      the project channel; \"machine\" is everyone on this machine; \"task\" is the \
                      piece of work you are part of — you, whoever started you, and every \
                      subagent any of you started, wherever they run; \"global\" is everyone, \
                      everywhere — keep that one for things that genuinely travel. Use \"task\" \
                      to keep a team of subagents in step without putting half-finished work in \
                      front of everyone else on the machine. kind \"note\" is the same thing meant \
                      to be kept and found later: decisions, gotchas, where a thing lives. \
                      Posting does not interrupt anyone; to put a message in front of one agent, \
                      use message_agent."
            .into(),
        input_schema: schema(
            multi,
            json!({
                "text": { "type": "string", "description": "What to say." },
                "scope": { "type": "string", "enum": ["path", "machine", "task", "global"], "description": "Who it is for. Default path." },
                "path": { "type": "string", "description": "For scope=path: which directory. Defaults to the session's own." },
                "kind": { "type": "string", "enum": ["message", "note"], "description": "\"note\" is meant to be kept and searched later. Default message." },
                "reply_to": { "type": "string", "description": "Message id this answers." },
            }),
            &["text"],
        ),
    });
    tools.push(ToolSpec {
        name: "launch_session",
        description: format!(
            "Start a persistent codex, claude, or terminal session in a working directory. Use \
             this for anything long-running or interactive instead of run_shell. `resume_id` \
             resumes that agent-native conversation; `initial_prompt` seeds a fresh agent \
             instead. The session survives this process: pair every launch with a later archive \
             or delete. A session you start is recorded as yours — it shows in the dashboard \
             indented under you, and it is part of your task on the talk board — so this is how \
             you hand work to a subagent rather than losing it in a list of unrelated \
             sessions.{}",
            match flavor {
                Flavor::Controller => "",
                // The daemon surface starts subagents of the agent calling it,
                // which is why `path` can be left out entirely.
                Flavor::Daemon =>
                    " Your own working directory is the only place you can start one: leave \
                     `path` out for it, or name somewhere inside it. To get work done elsewhere, \
                     or on another machine, ask the agent that lives there with message_agent.",
            }
        ),
        input_schema: schema(
            multi,
            json!({
                "kind": { "type": "string", "enum": ["codex", "claude", "terminal"] },
                "path": {
                    "type": "string",
                    "description": match flavor {
                        Flavor::Controller => "Absolute working directory on the machine.",
                        Flavor::Daemon => "Absolute working directory: your own folder, or one \
                                           inside it. Defaults to your own.",
                    },
                },
                "label": { "type": "string", "description": "Display name shown in the dashboard." },
                "resume_id": { "type": "string", "description": "Agent-native session id to resume." },
                "initial_prompt": { "type": "string", "description": "First prompt for a fresh agent." },
            }),
            match flavor {
                Flavor::Controller => &["kind", "path"][..],
                Flavor::Daemon => &["kind"][..],
            },
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
        name: "search_conversations",
        description: "Search what the agents actually said, on every machine: the backed-up \
                      Codex and Claude Code transcripts, live and archived. This is the \
                      memory of work already done — look here before working something out \
                      again, and to find who dealt with a thing last time. Each hit names the \
                      machine, directory, session and message it came from; feed session_id \
                      and message_index to read_conversation to read around it. Narrow with \
                      machines, paths, kinds, and since/until (epoch ms, on when the \
                      conversation started). The corpus is a backup taken every few minutes, \
                      so the newest turns of a conversation happening right now may not be in \
                      it yet — read_screen sees those."
            .into(),
        input_schema: schema(
            false,
            json!({
                "query": { "type": "string", "description": "Text to look for, matched case-insensitively." },
                "machines": { "type": "array", "items": { "type": "string" }, "description": "Machine ids from list_machines. Default: every enabled machine." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Only conversations held in these directories or below them." },
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["codex", "claude", "terminal"] } },
                "since": { "type": "integer", "description": "Epoch ms: only conversations started at or after this." },
                "until": { "type": "integer", "description": "Epoch ms: only conversations started at or before this." },
                "limit": { "type": "integer", "description": "Most hits to return, 1-100. Default 20." },
            }),
            &["query"],
        ),
    });
    tools.push(ToolSpec {
        name: "read_conversation",
        description: "Read a stretch of one backed-up conversation, addressed by message \
                      index. `around_index` centres the window on a search_conversations hit; \
                      `from_index` reads forward from one; neither starts at the beginning. \
                      A conversation is far too long to read whole, so this returns at most \
                      `limit` messages and `max_chars` characters of text and tells you what \
                      is left: `next_cursor` is where to carry on, has_more_before and \
                      has_more_after say which way there is more. Same backup as \
                      search_conversations, so it may lag a live session by a few minutes."
            .into(),
        input_schema: schema(
            false,
            json!({
                "session_id": { "type": "string", "description": "Session id, as reported by search_conversations." },
                "machine": { "type": "string", "description": "Which machine's session. Default: whichever enabled machine holds it." },
                "around_index": { "type": "integer", "description": "Centre the window on this message index." },
                "from_index": { "type": "integer", "description": "Start the window at this message index. Default 0." },
                "limit": { "type": "integer", "description": "Most messages to return, 1-200. Default 20." },
                "max_chars": { "type": "integer", "description": "Character budget for the whole answer, 500-64000. Default 8000." },
            }),
            &["session_id"],
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

/// The schema of a tool that addresses any machine from either surface. The
/// daemon reaches the others by asking an attached controller to run the call,
/// so the argument is the same and only what it costs differs.
fn schema_across(flavor: Flavor, properties: Value, required: &[&str]) -> Value {
    let mut schema = schema(true, properties, required);
    if flavor == Flavor::Daemon {
        schema["properties"]["machine"] = json!({
            "type": "string",
            "description": "Machine id from list_machines. Defaults to this machine; another one \
                            is reached through the muxloom controller watching this machine, \
                            which has to be running.",
        });
    }
    schema
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

fn optional_u64(arguments: &Value, key: &str, default: u64) -> u64 {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
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

/// The runtime a session is running, as far as its machine will say. `None`
/// when the machine cannot be asked, which is the answer that changes nothing:
/// typing raw is what muxloom has always done.
fn session_kind(sessions: &[DaemonSession], session_id: &str) -> Option<AgentKind> {
    sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.kind.parse().ok())
}

/// The bytes a send_input call types: text, then named keys, then Enter.
///
/// `kind` decides how the text is framed. Codex and Claude Code both decide
/// whether an arriving carriage return submits or just breaks a line by how
/// much came with it: text and Enter written together read as one paste, the
/// Enter is absorbed into it as a newline, and what was typed sits in the prompt
/// unsent while the caller is told it went. Measured on Claude Code v2.1.233,
/// five characters submitted and a hundred did not. Bracketing the text says
/// where the paste ends, so the Enter after it is a keystroke again whatever its
/// length — the same framing a delivered message has always used. Everything
/// else keeps typing raw: a shell has no such heuristic, and a program that
/// never enabled bracketed paste would be handed the brackets as text.
fn build_input(arguments: &Value, kind: Option<AgentKind>) -> Result<Vec<u8>> {
    let text = optional_str(arguments, "text");
    let keys = arguments.get("keys").and_then(Value::as_array);
    let submit = optional_bool(arguments, "submit");
    if text.is_none() && keys.is_none_or(|keys| keys.is_empty()) && !submit {
        bail!("send_input needs text, keys, or submit");
    }
    let pastes = matches!(kind, Some(AgentKind::Codex | AgentKind::Claude));
    let mut bytes = Vec::new();
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        if pastes {
            bytes.extend(paste_bytes(text, false));
        } else {
            bytes.extend_from_slice(text.as_bytes());
        }
    }
    if let Some(keys) = keys {
        for key in keys {
            let name = key.as_str().context("keys must be an array of strings")?;
            bytes.extend(encode_key(name)?);
        }
    }
    if submit {
        bytes.push(b'\r');
    }
    if bytes.is_empty() {
        bail!("send_input needs text, keys, or submit");
    }
    Ok(bytes)
}

/// The trigger a `set` call describes. The daemon stamps the id, the clock,
/// and the counters; everything here comes from the caller.
fn trigger_spec(arguments: &Value) -> Result<Trigger> {
    let pattern = required_str(arguments, "pattern")?;
    if pattern.len() > 256 {
        bail!("pattern must be shorter than 256 characters: watch for one line, not a screen");
    }
    let action = match optional_str(arguments, "action_kind").unwrap_or("notify") {
        "send_input" => TriggerAction::SendInput {
            text: required_str(arguments, "text")?.into(),
            submit: arguments
                .get("submit")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        },
        "notify" => TriggerAction::Notify {
            text: optional_str(arguments, "text")
                .unwrap_or("a muxloom trigger matched this session")
                .into(),
        },
        other => bail!("unknown action_kind {other}: use send_input or notify"),
    };
    Ok(Trigger {
        id: optional_str(arguments, "id").unwrap_or_default().into(),
        session_id: required_str(arguments, "session_id")?.into(),
        pattern: pattern.into(),
        action,
        once: arguments
            .get("once")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        cooldown_ms: optional_u64(arguments, "cooldown_ms", 5_000),
        created_at: 0,
        last_fired_at: None,
        fires: 0,
    })
}

fn trigger_json(machine: &str, trigger: &Trigger) -> Value {
    let (action_kind, text, submit) = match &trigger.action {
        TriggerAction::SendInput { text, submit } => ("send_input", text, Some(*submit)),
        TriggerAction::Notify { text } => ("notify", text, None),
    };
    json!({
        "id": trigger.id,
        "machine": machine,
        "session_id": trigger.session_id,
        "pattern": trigger.pattern,
        "action_kind": action_kind,
        "text": text,
        "submit": submit,
        "once": trigger.once,
        "cooldown_ms": trigger.cooldown_ms,
        "created_at": trigger.created_at,
        "last_fired_at": trigger.last_fired_at,
        "fires": trigger.fires,
    })
}

/// The last rows of a screen, for an answer that reports what it saw without
/// carrying a whole terminal back.
fn screen_tail(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    lines[lines.len().saturating_sub(WAIT_TAIL_LINES)..].join("\n")
}

/// Poll a session until it reaches the state the caller is waiting for.
///
/// Both surfaces share this loop, and both drive it from the adapter side:
/// waiting inside the daemon would need a client that sits on the connection,
/// and a resident client holds generation handover open for as long as an
/// agent cares to wait. `look` reports the session as the daemon sees it —
/// `None` once it is gone — and `read` renders its screen, which is only
/// asked for when the wait is about the screen.
fn wait_loop(
    arguments: &Value,
    machine: &str,
    mut look: impl FnMut() -> Result<Option<DaemonSession>>,
    mut read: impl FnMut() -> Result<String>,
) -> Result<String> {
    let until = required_str(arguments, "until")?.to_string();
    let pattern = optional_str(arguments, "pattern").map(str::to_lowercase);
    if until == "output_matches" && pattern.is_none() {
        bail!("until=output_matches needs a pattern to look for");
    }
    if !["idle", "attention", "output_matches", "silence", "exit"].contains(&until.as_str()) {
        bail!("unknown until {until}: use idle, attention, output_matches, silence, or exit");
    }
    let timeout = Duration::from_secs(
        optional_u64(arguments, "timeout_seconds", WAIT_DEFAULT_TIMEOUT_SECONDS)
            .clamp(1, WAIT_MAX_TIMEOUT_SECONDS),
    );
    let poll = Duration::from_millis(optional_u64(arguments, "poll_ms", 800).clamp(200, 5_000));
    let quiet = Duration::from_secs(optional_u64(arguments, "quiet_seconds", 5).clamp(1, 300));
    let watches_screen = until == "output_matches" || until == "silence";

    let started = Instant::now();
    let mut session = None;
    let mut screen = String::new();
    let mut last_change = started;
    let mut matched = None;
    let mut failures = 0;
    let mut outcome = "timeout";
    loop {
        match look() {
            Ok(Some(observed)) => {
                failures = 0;
                let alive = !observed.dead;
                let idle = !observed.working && !observed.needs_attention;
                let attention = observed.needs_attention;
                session = Some(observed);
                if !alive {
                    outcome = "exit";
                    break;
                }
                if watches_screen {
                    let observed = read()?;
                    if observed != screen {
                        screen = observed;
                        last_change = Instant::now();
                    }
                }
                match until.as_str() {
                    "idle" if idle => {
                        outcome = "idle";
                        break;
                    }
                    "attention" if attention => {
                        outcome = "attention";
                        break;
                    }
                    "output_matches" => {
                        let wanted = pattern.as_deref().unwrap_or_default();
                        if let Some(line) = screen
                            .lines()
                            .find(|line| line.to_lowercase().contains(wanted))
                        {
                            matched = Some(line.trim().to_string());
                            outcome = "matched";
                            break;
                        }
                    }
                    "silence" if last_change.elapsed() >= quiet => {
                        outcome = "silence";
                        break;
                    }
                    _ => {}
                }
            }
            // A session that is no longer listed is over, whichever way the
            // caller was waiting for it to end.
            Ok(None) => {
                outcome = "exit";
                break;
            }
            Err(error) => {
                failures += 1;
                if failures > WAIT_ERROR_TOLERANCE {
                    return Err(error);
                }
            }
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            break;
        }
        thread::sleep(poll.min(timeout - elapsed));
    }

    // What the session was showing when the wait ended is the context the
    // caller needs next, and it is cheap enough to fetch once.
    if !watches_screen {
        screen = read().unwrap_or_default();
    }
    let satisfied = match outcome {
        "timeout" => false,
        "exit" => until == "exit",
        _ => true,
    };
    let note = match outcome {
        "timeout" => Some(
            "the wait ran out, which is not a failure: nothing has happened yet, so call \
             wait_for again to keep waiting",
        ),
        "exit" if until != "exit" => {
            Some("the session ended before what you were waiting for happened")
        }
        _ => None,
    };
    Ok(pretty(&json!({
        "outcome": outcome,
        "satisfied": satisfied,
        "until": until,
        "waited_ms": started.elapsed().as_millis() as u64,
        "matched": matched,
        "session": session.as_ref().map(|session| session_json(machine, session)),
        "screen_tail": screen_tail(&screen),
        "note": note,
    })))
}

/// What muxloom told this process about the session it is running in.
///
/// `launch_session` puts these in a session's environment, so an agent that
/// lives in one never has to be told — or to guess — who and where it is.
/// Outside a muxloom session they are simply absent, and the board says so
/// rather than inventing an identity.
fn session_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The session this tool call is being made from, which is the session a
/// launch it asks for belongs under.
///
/// An agent starting a subagent is handing off part of what it is doing, and
/// the two are one piece of work however many sessions it takes. Nothing about
/// the child says so — its transcript begins with the task, not with who set
/// it — so the only chance to record it is here, where muxloom already knows
/// who is calling. Read from the environment rather than asked for as an
/// argument: an agent naming its own parent could only get it wrong.
fn launching_session() -> Option<String> {
    session_env("MUXLOOM_SESSION_ID")
}

/// Where a launch asked for on the daemon surface may actually run: the
/// caller's own folder, or somewhere inside it.
///
/// This is not a sandbox, and could not be one — the same agent has `run_shell`
/// on this machine and could start whatever it liked by hand. It is a statement
/// of what the surface is for. The daemon flavor is an agent's own machine and
/// its own work: the sessions it starts are its subagents, and they belong
/// beside the work rather than somewhere the user has to go looking for them.
/// An agent asked to do something in another folder has a better move available
/// — ask the agent that lives there — and saying so here is what makes it
/// think of it. Reaching the whole fleet is the moderator's flavor.
///
/// Leaving `path` out is the ordinary case and means the caller's own folder,
/// so starting a subagent takes no argument the agent has to look up.
fn launch_path_within(arguments: &Value, own: &str) -> Result<String> {
    let Some(path) = optional_str(arguments, "path") else {
        return Ok(own.to_string());
    };
    if crate::moderator::within(own, path) {
        return Ok(path.to_string());
    }
    bail!(
        "launch_session starts sessions in your own folder, {own}, and {path} is outside it. \
         Leave path out to start one where you are. For work in another folder or on another \
         machine, ask the agent that lives there with message_agent — or ask a muxloom \
         moderator, whose surface reaches the whole fleet."
    )
}

fn session_voice() -> TalkVoice {
    TalkVoice {
        session_id: session_env("MUXLOOM_SESSION_ID"),
        label: session_env("MUXLOOM_SESSION_LABEL"),
        kind: session_env("MUXLOOM_SESSION_KIND"),
        // Speaking as a person is the dashboard's privilege; anything reaching
        // the board through a tool call is an agent, whoever asked for it.
        human: false,
    }
}

/// The post a `talk_post` call describes. The scope's machine is left empty on
/// purpose: the daemon that mints the message is the one that knows its own
/// origin key, and a caller naming someone else's machine would be filing
/// under a board it does not own.
fn talk_draft(arguments: &Value) -> Result<TalkDraft> {
    let text = required_str(arguments, "text")?;
    if text.len() > MAX_TEXT {
        bail!(
            "text must be shorter than {MAX_TEXT} bytes: post what the others need to know, not \
             the transcript"
        );
    }
    let kind = TalkKind::parse(optional_str(arguments, "kind").unwrap_or("message"))?;
    if kind == TalkKind::Direct {
        bail!(
            "talk_post writes to a board, and a direct message goes to one session: use \
             message_agent instead"
        );
    }
    let scope = match optional_str(arguments, "scope").unwrap_or("path") {
        "global" => TalkScope::Global,
        "machine" => TalkScope::Machine {
            machine: String::new(),
        },
        "path" => TalkScope::Path {
            machine: String::new(),
            path: optional_str(arguments, "path")
                .map(str::to_string)
                .or_else(|| session_env("MUXLOOM_SESSION_PATH"))
                .context(
                    "scope \"path\" needs a path, and this process is not running inside a \
                     muxloom session that could name one",
                )?,
        },
        "task" => TalkScope::Task {
            machine: String::new(),
            root_session: task_root().context(
                "scope \"task\" is the agents working on one piece of work, and this process is \
                 not running inside a muxloom session that could say which one",
            )?,
        },
        other => bail!("unknown scope {other}: use path, machine, task, or global"),
    };
    Ok(TalkDraft {
        scope,
        author: TalkAuthor::default(),
        kind,
        to: None,
        reply_to: optional_str(arguments, "reply_to").map(Into::into),
        text: text.into(),
    })
}

/// The task the calling session belongs to: what the daemon worked out when it
/// launched the session, and failing that the session itself.
///
/// The fallback is what a daemon too old to have said would have meant anyway —
/// a session nobody started is its own task — so an agent talking to one is
/// alone in its task rather than cut off from the scope.
fn task_root() -> Option<String> {
    session_env("MUXLOOM_TASK_ROOT").or_else(|| session_env("MUXLOOM_SESSION_ID"))
}

/// Who a direct message is from.
///
/// A board post can leave this out — the machine that files it is the machine
/// it was said on. A direct message is filed on the *target's* board, so a
/// sender that says nothing about where it is would be recorded as speaking
/// from the machine it reached.
fn direct_author(local: impl FnOnce() -> Result<TalkState>) -> TalkAuthor {
    let mut author = TalkAuthor {
        machine: session_env("MUXLOOM_MACHINE").unwrap_or_default(),
        machine_label: session_env("MUXLOOM_MACHINE_LABEL").unwrap_or_default(),
        voice: session_voice(),
    };
    if author.machine.is_empty() {
        // Not running inside a muxloom session: ask the board here what this
        // machine is called. Failing that, a host name is still a name, and
        // it is not worth refusing to carry a message over.
        let (machine, label) = local()
            .map(|state| (state.origin, state.label))
            .unwrap_or_else(|_| (hostname(), hostname()));
        author.machine = machine;
        author.machine_label = label;
    }
    author
}

/// What a `message_agent` call describes: the message, when it may be typed
/// in, and whether the sender is waiting on an answer.
///
/// The target's machine is left empty for the same reason a post's scope is:
/// the daemon that takes delivery is the one that knows what it is called, and
/// a message addressed to a machine by a name only the sender uses would be
/// filed under a board nobody reads.
fn direct_draft(arguments: &Value, author: TalkAuthor) -> Result<(TalkDraft, TalkDeliver, bool)> {
    let text = required_str(arguments, "text")?;
    if text.len() > MAX_TEXT {
        bail!(
            "text must be shorter than {MAX_TEXT} bytes: say what the other agent has to know and \
             leave the rest where it can read it"
        );
    }
    let draft = TalkDraft {
        // Where it happened, for whoever reads the board later. Who may see it
        // is decided by its two ends, not by this.
        scope: TalkScope::Machine {
            machine: String::new(),
        },
        author,
        kind: TalkKind::Direct,
        to: Some(TalkAddress {
            machine: String::new(),
            session_id: required_str(arguments, "session_id")?.into(),
        }),
        reply_to: optional_str(arguments, "reply_to").map(Into::into),
        text: text.into(),
    };
    Ok((
        draft,
        TalkDeliver::parse(optional_str(arguments, "deliver").unwrap_or("auto"))?,
        arguments
            .get("reply_expected")
            .and_then(Value::as_bool)
            .unwrap_or_default(),
    ))
}

/// What became of a direct message, in the words the sender needs: whether it
/// was typed in, and where to look for an answer.
fn delivery_json(message: &TalkMessage, delivery: &str, reason: Option<String>) -> String {
    pretty(&json!({
        "message_id": message.id,
        "delivery": delivery,
        "reason": reason,
        "note": match delivery {
            "delivered" => "it is in that agent's prompt now, and it reads it when the turn it is \
                            in ends. An answer comes back as a direct message: wait for it with \
                            talk_read { scope: \"direct\", wait_seconds }, and call that again \
                            each time it returns nothing rather than sending this a second time",
            "queued" => "muxloom types it in as soon as that session can take it, and tells you \
                         above why it cannot yet. Do not send it again — wait for the answer with \
                         talk_read { scope: \"direct\", wait_seconds }",
            _ => "the message is on the board, but nothing was typed into that session",
        },
    }))
}

/// What a `talk_read` call asks for. The reader's own identity is filled in
/// here rather than taken from the caller: whose messages count as "mine" is
/// not something an agent should be able to claim.
fn talk_filter(arguments: &Value) -> Result<TalkFilter> {
    let scope = match optional_str(arguments, "scope") {
        None => None,
        Some(scope @ ("global" | "machine" | "path" | "task" | "direct")) => {
            Some(scope.to_string())
        }
        Some(other) => bail!("unknown scope {other}: use path, machine, task, global, or direct"),
    };
    Ok(TalkFilter {
        since: optional_str(arguments, "since_cursor")
            .map(decode_cursor)
            .unwrap_or_default(),
        scope,
        kinds: string_list(arguments, "kinds")?,
        authors: string_list(arguments, "authors")?,
        query: optional_str(arguments, "query").map(Into::into),
        machines: talk_selector(arguments, "include_machines")?,
        paths: talk_selector(arguments, "include_paths")?,
        session_id: session_env("MUXLOOM_SESSION_ID"),
        path: optional_str(arguments, "path")
            .map(Into::into)
            .or_else(|| session_env("MUXLOOM_SESSION_PATH")),
        task: task_root(),
        before: arguments.get("before").and_then(Value::as_u64),
        limit: optional_usize(arguments, "limit", 50),
        // An agent reads the board as the session it runs in. Seeing every
        // direct message on the machine is the dashboard's privilege, and it
        // is not something a caller can ask for.
        owner: false,
    })
}

/// A list of names, however the caller wrote it: an array, or the one string
/// they meant.
fn string_list(arguments: &Value, key: &str) -> Result<Vec<String>> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(one)) => Ok(vec![one.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .with_context(|| format!("{key} must be an array of strings"))
            })
            .collect(),
        Some(_) => bail!("{key} must be an array of strings"),
    }
}

/// How far a read reaches. Saying nothing means "where I am"; `"all"`, alone
/// or among names, means everywhere.
fn talk_selector(arguments: &Value, key: &str) -> Result<TalkSelector> {
    let names = string_list(arguments, key)?;
    Ok(if names.is_empty() {
        TalkSelector::Mine
    } else if names.iter().any(|name| name == "all") {
        TalkSelector::All
    } else {
        TalkSelector::Only { names }
    })
}

fn talk_json(message: &TalkMessage) -> Value {
    let machine = if message.author.machine_label.is_empty() {
        &message.author.machine
    } else {
        &message.author.machine_label
    };
    json!({
        "id": message.id,
        "ts": message.ts,
        "scope": message.scope.name(),
        "scope_machine": message.scope.machine(),
        "scope_path": message.scope.path(),
        "kind": message.kind.name(),
        "from": {
            "name": message.author.voice.name(),
            "machine": machine,
            "session_id": &message.author.voice.session_id,
            "kind": &message.author.voice.kind,
            "human": message.author.voice.human,
        },
        "to": message.to.as_ref().map(|to| json!({
            "machine": to.machine,
            "session_id": to.session_id,
        })),
        "reply_to": &message.reply_to,
        "text": message.text,
    })
}

/// Read the board, and if the caller asked to wait, keep reading until someone
/// says something or the wait runs out.
///
/// Polled from the adapter side for the same reason `wait_loop` is: a client
/// that sits on a daemon connection waiting to be spoken to holds generation
/// handover open for as long as it waits.
fn talk_wait(
    arguments: &Value,
    mut filter: TalkFilter,
    mut read: impl FnMut(&TalkFilter) -> Result<TalkPage>,
    sessions: impl FnOnce() -> Vec<DaemonSession>,
) -> Result<String> {
    let wait =
        Duration::from_secs(optional_u64(arguments, "wait_seconds", 0).min(TALK_MAX_WAIT_SECONDS));
    let started = Instant::now();
    loop {
        let page = read(&filter)?;
        let elapsed = started.elapsed();
        if !page.messages.is_empty() || elapsed >= wait {
            // A wait that ends empty is the one that needs explaining. Say
            // which of the caller's own messages are still unanswered and what
            // the sessions holding them are doing, so the next move is a fact
            // rather than a guess.
            let outstanding = if page.messages.is_empty() && !wait.is_zero() {
                unanswered(&filter, &mut read, sessions)
            } else {
                Vec::new()
            };
            return Ok(pretty(&json!({
                "messages": page.messages.iter().map(talk_json).collect::<Vec<_>>(),
                "cursor": page.cursor,
                "truncated": page.truncated,
                "waited_ms": elapsed.as_millis() as u64,
                "waiting_on": (!outstanding.is_empty()).then_some(&outstanding),
                "note": if page.truncated {
                    Some(
                        "more messages matched than fit: read again with `before` set to the \
                         oldest ts you got to page further back"
                            .to_string(),
                    )
                } else {
                    outstanding_note(&outstanding)
                },
            })));
        }
        // Only what arrives from here on is news, and paging into the past is
        // not what a wait is for.
        filter.since = decode_cursor(&page.cursor);
        filter.before = None;
        thread::sleep(TALK_POLL.min(wait - elapsed));
    }
}

/// The caller's own direct messages that nobody has answered, newest first,
/// each with what the session holding it is doing right now.
///
/// "Answered" is the plain reading: a direct message back from that session
/// after the one that was sent. Nothing here depends on `reply_to` being set,
/// because most replies do not set it — the tools ask for it so a sender can
/// match an answer to a question, not so muxloom can. It is honoured when it
/// is there, which is what lets a bounce close the question it bounced: the
/// daemon saying "this never arrived" is not that session talking, and by the
/// author rule alone it would leave the sender waiting on it forever.
fn unanswered(
    filter: &TalkFilter,
    read: &mut impl FnMut(&TalkFilter) -> Result<TalkPage>,
    sessions: impl FnOnce() -> Vec<DaemonSession>,
) -> Vec<Value> {
    let Some(me) = filter.session_id.clone() else {
        return Vec::new();
    };
    let mut directs = TalkFilter {
        since: TalkVector::default(),
        scope: Some("direct".into()),
        kinds: vec!["direct".into()],
        authors: Vec::new(),
        query: None,
        before: None,
        limit: 200,
        ..filter.clone()
    };
    directs.machines = TalkSelector::All;
    let Ok(page) = read(&directs) else {
        return Vec::new();
    };
    let recent = now_ms().saturating_sub(TALK_OUTSTANDING_MS);
    let mine = |message: &TalkMessage| message.author.voice.session_id.as_deref() == Some(&me);
    // The last time each session said anything to me at all. Newer than what I
    // sent it means the exchange moved on, whatever it was about.
    let mut answered: BTreeMap<&str, u64> = BTreeMap::new();
    // The particular messages something came back about, whoever it came from.
    let mut closed: BTreeSet<&str> = BTreeSet::new();
    for message in &page.messages {
        if !message.to.as_ref().is_some_and(|to| to.session_id == me) {
            continue;
        }
        if let Some(from) = message.author.voice.session_id.as_deref() {
            let seen = answered.entry(from).or_default();
            *seen = (*seen).max(message.ts);
        }
        if let Some(replied) = message.reply_to.as_deref() {
            closed.insert(replied);
        }
    }
    let mut waiting: Vec<&TalkMessage> = page
        .messages
        .iter()
        .filter(|message| mine(message) && message.ts >= recent)
        .filter(|message| !closed.contains(message.id.as_str()))
        .filter(|message| {
            message.to.as_ref().is_some_and(|to| {
                answered.get(to.session_id.as_str()).copied().unwrap_or(0) < message.ts
            })
        })
        .collect();
    // One entry per session, the last thing said to it: a follow-up does not
    // become a second thing to wait on. Then oldest first, so whoever reads
    // this sees the exchange that has been stalled longest at the top.
    waiting.sort_by_key(|message| std::cmp::Reverse(message.ts));
    let mut seen = BTreeSet::new();
    waiting.retain(|message| {
        message
            .to
            .as_ref()
            .is_some_and(|to| seen.insert(to.session_id.clone()))
    });
    waiting.sort_by_key(|message| message.ts);
    if waiting.is_empty() {
        return Vec::new();
    }
    let sessions = sessions();
    let now = now_ms();
    waiting
        .into_iter()
        .map(|message| {
            let to = message.to.as_ref().expect("filtered to addressed messages");
            let session = sessions.iter().find(|session| session.id == to.session_id);
            json!({
                "message_id": message.id,
                "to": { "machine": to.machine, "session_id": to.session_id },
                "sent_seconds_ago": now.saturating_sub(message.ts) / 1000,
                "text": message.text,
                "state": session.map(|session| json!({
                    "label": session.label,
                    "working": session.working,
                    "needs_attention": session.needs_attention,
                    "attention_reason": session.attention_reason,
                })),
                "reading": reading(session, now.saturating_sub(message.ts)),
            })
        })
        .collect()
}

/// What the state of the session holding an unanswered message means for the
/// agent waiting on it.
fn reading(session: Option<&DaemonSession>, waited_ms: u64) -> &'static str {
    let Some(session) = session else {
        return "that session is no longer on this machine; no answer is coming, so act without \
                one or find another agent";
    };
    if session.dead || session.archived {
        return "that session has ended; no answer is coming, so act without one or find another \
                agent";
    }
    if session.needs_attention {
        return "it is stopped on a question only a person can answer, and will not get to your \
                message until somebody does; tell the human if you can reach one";
    }
    if session.working {
        return "it is mid-turn — it has your message and answers when the turn ends; wait again";
    }
    if waited_ms < 2 * 60 * 1000 {
        return "it is between turns and has only just been asked; wait again";
    }
    "it has been idle since, so it has probably read your message and decided it needs no answer, \
     or forgot to send one; ask once more or carry on without it"
}

/// The one line that turns the list above into a next move.
fn outstanding_note(outstanding: &[Value]) -> Option<String> {
    let first = outstanding.first()?;
    Some(format!(
        "nothing new was said, and {} message{} of yours {} still unanswered — see `waiting_on`. \
         An answer commonly takes several minutes, so calling this again with wait_seconds up to \
         {TALK_MAX_WAIT_SECONDS} is usually the right move; sending the same thing twice is not. \
         For the one you have waited longest on: {}",
        outstanding.len(),
        if outstanding.len() == 1 { "" } else { "s" },
        if outstanding.len() == 1 { "is" } else { "are" },
        first["reading"].as_str().unwrap_or_default(),
    ))
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
        // What the runtime called the conversation, when it keeps a transcript
        // and has named one. The label is what a human typed; this is what the
        // session turned out to be about.
        "title": session.title,
        "temporary": session.temporary,
        "created_at": session.created_at,
        "pid": session.pid,
        "dead": session.dead,
        "archived": session.archived,
        "working": session.working,
        "needs_attention": session.needs_attention,
        "attention_reason": session.attention_reason,
        "recap": session.recap,
        // The agent that started this one; null when a person did.
        "parent": session.parent,
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

/// One page of a conversation under a character budget: the messages as JSON,
/// where to carry on if the budget ran out mid-page, and whether anything came
/// back clipped. The budget covers the whole page rather than each message,
/// because a page that fills the reader's context is not a page.
#[cfg(feature = "controller")]
fn conversation_page(
    window: &[(usize, crate::backup::ExtractedMessage)],
    max_chars: usize,
) -> (Vec<Value>, Option<usize>, bool) {
    let mut budget = max_chars;
    let mut clipped_any = false;
    let mut messages = Vec::new();
    for (position, message) in window {
        if budget == 0 {
            return (messages, Some(*position), clipped_any);
        }
        let first = messages.is_empty();
        let text: String = message.text.chars().take(budget).collect();
        let kept = text.chars().count();
        let clipped = kept < message.text.chars().count();
        clipped_any |= clipped;
        budget -= kept;
        messages.push(json!({
            "index": position,
            "role": message.role,
            "ts": message.ts,
            "text": text,
            "truncated": clipped,
        }));
        if clipped {
            // Resume at the message that got cut, so a fresh budget can read it
            // whole — unless it alone overran the budget, in which case reading
            // it again would return the same half and page forever.
            let resume = if first { position + 1 } else { *position };
            return (messages, Some(resume), clipped_any);
        }
    }
    (messages, None, clipped_any)
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
        let runtime = Runtime::new(&config);
        Self::with_runtime(config, runtime)
    }

    /// The same surface over a runtime that already exists. The dashboard's
    /// runtime holds the connections to every machine; a surface that ran
    /// errands over its own would dial all of them a second time.
    pub fn with_runtime(config: Config, runtime: Runtime) -> Result<Self> {
        let state_path = default_state_path();
        let state = State::load(&state_path)?;
        Ok(Self {
            runtime,
            config,
            state,
            state_path,
        })
    }

    /// The name a machine argument goes by here. This machine is `local` to
    /// the controller, but every daemon already calls its own machine that, so
    /// the fleet was told this one's hostname instead (see `relay::run_pump`).
    /// A relayed call naming the hostname means here, not a machine that is
    /// missing. An ssh alias of the same spelling wins: it is the more
    /// deliberate answer, and it points at this host anyway.
    fn spelled_here<'a>(&self, machine: &'a str) -> &'a str {
        if !self.state.enabled_hosts.contains(machine)
            && machine.eq_ignore_ascii_case(&crate::talk::hostname())
        {
            return crate::model::LOCAL_TARGET_ID;
        }
        machine
    }

    /// The machine an argument set addresses. Only enabled machines are
    /// reachable: a disabled target must never be touched, not even by an
    /// agent that knows its name.
    fn target(&self, arguments: &Value) -> Result<Target> {
        let machine = optional_str(arguments, "machine").unwrap_or(crate::model::LOCAL_TARGET_ID);
        let machine = self.spelled_here(machine);
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

    fn wait_for(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let session_id = required_str(arguments, "session_id")?.to_string();
        let pool = self.runtime.bridge_pool();
        wait_loop(
            arguments,
            &target.id,
            || {
                Ok(pool
                    .list_sessions(&target)?
                    .into_iter()
                    .find(|session| session.id == session_id))
            },
            || {
                let page =
                    self.runtime
                        .capture_page(&target, &session_id, 0, WAIT_SCREEN_LINES, 0, 0)?;
                Ok(plain_screen(&page.text))
            },
        )
    }

    fn trigger(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let pool = self.runtime.bridge_pool();
        match required_str(arguments, "action")? {
            "set" => {
                let stored = pool.set_trigger(&target, trigger_spec(arguments)?)?;
                Ok(pretty(&trigger_json(&target.id, &stored)))
            }
            "list" => {
                let triggers = pool.list_triggers(
                    &target,
                    optional_str(arguments, "session_id").map(Into::into),
                )?;
                Ok(pretty(&Value::Array(
                    triggers
                        .iter()
                        .map(|trigger| trigger_json(&target.id, trigger))
                        .collect(),
                )))
            }
            "delete" => {
                let id = required_str(arguments, "id")?;
                pool.delete_trigger(&target, id.into())?;
                Ok(format!("deleted trigger {id}"))
            }
            other => bail!("unknown trigger action {other}: use set, list, or delete"),
        }
    }

    fn talk_post(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let message = self
            .runtime
            .bridge_pool()
            .talk_post(&target, talk_draft(arguments)?)?;
        Ok(pretty(&talk_json(&message)))
    }

    fn talk_read(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let pool = self.runtime.bridge_pool();
        talk_wait(
            arguments,
            talk_filter(arguments)?,
            |filter| pool.talk_read(&target, filter.clone()),
            || pool.list_sessions(&target).unwrap_or_default(),
        )
    }

    fn message_agent(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let pool = self.runtime.bridge_pool();
        let author = direct_author(|| pool.talk_status(&Target::local(), None));
        let (draft, deliver, reply_expected) = direct_draft(arguments, author)?;
        let (message, delivery, reason) =
            pool.talk_deliver(&target, draft, deliver, reply_expected)?;
        Ok(delivery_json(&message, &delivery, reason))
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
            parent: launching_session(),
        };
        let command = self.config.command_for(&target.id, kind).clone();
        let environment = self.config.environment_for(&target.id)?;
        let session_id = self.runtime.launch(&request, &command, &environment)?;
        Ok(pretty(&json!({
            "session_id": session_id,
            "machine": target.id,
            "kind": kind.as_str(),
            "path": request.path,
            "parent": request.parent,
        })))
    }

    fn list_resume_candidates(&self, arguments: &Value) -> Result<String> {
        let target = self.target(arguments)?;
        let path = required_str(arguments, "path")?;
        let mut candidates = Vec::new();
        let mut warnings = Vec::new();
        for kind in AgentKind::agents().filter(|kind| kind.has_native_history()) {
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

    /// The backup partitions an agent here may look in: one per enabled
    /// machine. A machine the user switched off is unreachable, and what was
    /// said on it is not readable either.
    #[cfg(feature = "controller")]
    fn searchable_machines(
        &self,
        index: &crate::backup::BackupIndex,
        asked: &[String],
    ) -> Result<Vec<String>> {
        if asked.is_empty() {
            return Ok(self
                .state
                .enabled_hosts
                .iter()
                .map(|host| index.machine_key_for_alias(host))
                .collect());
        }
        asked
            .iter()
            .map(|machine| {
                let machine = self.spelled_here(machine);
                if !self.state.enabled_hosts.contains(machine) {
                    bail!(
                        "machine {machine} is not enabled in muxloom; ask the user to enable it \
                         rather than working around it"
                    );
                }
                Ok(index.machine_key_for_alias(machine))
            })
            .collect()
    }

    #[cfg(feature = "controller")]
    fn search_conversations(&self, arguments: &Value) -> Result<String> {
        use crate::backup::{BackupStore, SearchFilter, search_where};

        let query = required_str(arguments, "query")?;
        let limit = optional_u64(arguments, "limit", 20).clamp(1, 100) as usize;
        let store = BackupStore::new(BackupStore::default_root());
        let index = store.load_index()?;
        let machines = self.searchable_machines(&index, &string_list(arguments, "machines")?)?;
        let filter = SearchFilter {
            machines: machines.clone(),
            paths: string_list(arguments, "paths")?,
            kinds: string_list(arguments, "kinds")?,
            since: optional_u64(arguments, "since", 0),
            until: optional_u64(arguments, "until", 0),
        };
        let hits = search_where(&store, query, limit, &filter)?;
        let rendered: Vec<Value> = hits
            .iter()
            .map(|hit| {
                json!({
                    "machine": hit.target_id,
                    "session_id": hit.session_id,
                    "kind": hit.kind,
                    "path": hit.cwd,
                    "title": hit.title,
                    "started_at": hit.created_at,
                    "role": hit.role,
                    "ts": hit.ts,
                    // A title match belongs to no message, so there is no index
                    // to read around — open the conversation from the start.
                    "message_index": (hit.message_index != usize::MAX).then_some(hit.message_index),
                    "snippet": hit.snippet,
                    "matches": hit.score,
                })
            })
            .collect();
        Ok(pretty(&json!({
            "query": query,
            "machines_searched": machines,
            "hits": rendered,
            "note": "read around a hit with read_conversation { session_id, machine, \
                     around_index }. This is a backup taken every few minutes, so a conversation \
                     happening right now may be missing its last turns.",
        })))
    }

    #[cfg(not(feature = "controller"))]
    fn search_conversations(&self, _arguments: &Value) -> Result<String> {
        bail!("this muxloom build keeps no conversation backup to search")
    }

    #[cfg(feature = "controller")]
    fn read_conversation(&self, arguments: &Value) -> Result<String> {
        use crate::backup::{BackupStore, read_messages};

        let session_id = required_str(arguments, "session_id")?;
        let limit = optional_u64(arguments, "limit", 20).clamp(1, 200) as usize;
        let max_chars = optional_u64(arguments, "max_chars", 8_000).clamp(500, 64_000) as usize;
        let store = BackupStore::new(BackupStore::default_root());
        let index = store.load_index()?;
        let asked: Vec<String> = optional_str(arguments, "machine")
            .map(|machine| vec![machine.to_string()])
            .unwrap_or_default();
        let machines = self.searchable_machines(&index, &asked)?;
        let record = index
            .records
            .iter()
            .find(|record| record.session_id == session_id && machines.contains(&record.target_id))
            .with_context(|| {
                format!("no backed-up conversation {session_id} on any enabled machine")
            })?;

        let from = match optional_u64(arguments, "around_index", u64::MAX) {
            u64::MAX => optional_u64(arguments, "from_index", 0) as usize,
            around => (around as usize).saturating_sub(limit / 2),
        };
        let (window, total) = read_messages(&store, &record.target_id, session_id, from, limit)?;

        let (messages, next, clipped_any) = conversation_page(&window, max_chars);
        let after = window
            .last()
            .map(|(position, _)| position + 1)
            .unwrap_or(from);
        let next_cursor = next
            .filter(|resume| *resume < total)
            .or((after < total).then_some(after));
        Ok(pretty(&json!({
            "machine": record.target_id,
            "session_id": record.session_id,
            "kind": record.kind,
            "path": record.cwd,
            "title": record.title,
            "total_messages": total,
            "from_index": from,
            "messages": messages,
            "has_more_before": from > 0,
            "has_more_after": next_cursor.is_some(),
            "next_cursor": next_cursor,
            "truncated": clipped_any,
        })))
    }

    #[cfg(not(feature = "controller"))]
    fn read_conversation(&self, _arguments: &Value) -> Result<String> {
        bail!("this muxloom build keeps no conversation backup to read")
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
            "wait_for" => self.wait_for(arguments),
            "trigger" => self.trigger(arguments),
            "talk_read" => self.talk_read(arguments),
            "talk_post" => self.talk_post(arguments),
            "message_agent" => self.message_agent(arguments),
            "send_input" => {
                let target = self.target(arguments)?;
                let session_id = required_str(arguments, "session_id")?;
                let kind = self
                    .runtime
                    .bridge_pool()
                    .list_sessions(&target)
                    .ok()
                    .and_then(|sessions| session_kind(&sessions, session_id));
                let bytes = build_input(arguments, kind)?;
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
            "search_conversations" => self.search_conversations(arguments),
            "read_conversation" => self.read_conversation(arguments),
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
    use std::{
        collections::HashMap,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use serde_json::{Value, json};

    use super::{
        DEFAULT_SCREEN_LINES, Flavor, SEARCH_MAX_MATCHES, WAIT_SCREEN_LINES, agent_kind,
        allowed_specs, build_input, delivery_json, direct_author, direct_draft, enforce_policy,
        instructions, launch_path_within, launching_session, optional_bool, optional_str,
        optional_usize, plain_screen, pretty, preview_text, required_str, screen_page, session_env,
        session_json, session_kind, shell_report, talk_draft, talk_filter, talk_json, talk_wait,
        trigger_json, trigger_spec, wait_loop,
    };
    use crate::{
        config::{Config, default_config_path},
        daemon::{DaemonPaths, connect_or_start},
        daemon_protocol::{
            DaemonRequest, DaemonResponse, DaemonSession, Frame, FrameKind, Trigger, stream,
        },
        model::LOCAL_TARGET_ID,
        runtime::{launch_arguments, new_daemon_session_id},
    };

    /// How long one daemon request may run. Matches the bridge's own request
    /// timeout: a shell script is the slowest thing a request can carry.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
    /// The most preview bytes a tool answer carries.
    const PREVIEW_LIMIT: usize = 256 * 1024;
    /// How long a call the controller runs for us may take, and how often we
    /// look for its answer. A controller comes round for work every couple of
    /// seconds and the call itself takes as long as it takes; a minute covers
    /// a search across a slow link, and failing after it is better than being
    /// dropped by the client mid-call.
    const RELAY_WAIT: Duration = Duration::from_secs(60);
    const RELAY_POLL: Duration = Duration::from_millis(250);

    /// The folder the caller works in. muxloom put it in the session's
    /// environment when it started it; a process driving the surface by hand
    /// has no such session, and there its own working directory is the same
    /// statement made a different way.
    fn own_folder() -> Option<String> {
        session_env("MUXLOOM_SESSION_PATH").or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
    }

    /// Serves the daemon on this machine over its Unix socket. Each call opens
    /// its own connection: a resident client would hold the daemon's client
    /// count up and defer generation handover indefinitely, and a fresh
    /// connection also tolerates the daemon being replaced between calls.
    pub struct DaemonControl {
        paths: DaemonPaths,
        config: Config,
        /// The folder the caller works in, which is as far as a launch on this
        /// surface reaches. Settled once, when the surface is built: it comes
        /// from the session's environment, and that does not change under a
        /// running agent.
        own_folder: Option<String>,
    }

    impl DaemonControl {
        pub fn new() -> Result<Self> {
            Ok(Self {
                paths: DaemonPaths::discover()?,
                config: Config::load(&default_config_path())?,
                own_folder: own_folder(),
            })
        }

        /// A surface over an explicit state directory and config, for tests
        /// and for pointing at a non-default daemon.
        pub fn with_paths(paths: DaemonPaths, config: Config, own_folder: Option<String>) -> Self {
            Self {
                paths,
                config,
                own_folder,
            }
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

        /// One page of a session's screen as the daemon renders it: the rows
        /// themselves, where they end, the pane height, and whether there is
        /// older history above them.
        fn screen_rows(
            &self,
            session_id: &str,
            offset: usize,
            lines: usize,
        ) -> Result<(String, usize, usize, bool)> {
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
                    Ok((
                        text.trim_end().to_string(),
                        offset_from_bottom,
                        usize::from(rows),
                        rendered && !reached_start && offset_from_bottom >= offset,
                    ))
                }
                response => bail!("unexpected history response: {response:?}"),
            }
        }

        fn read_screen(&self, arguments: &Value) -> Result<String> {
            let session_id = required_str(arguments, "session_id")?;
            let lines = optional_usize(arguments, "lines", DEFAULT_SCREEN_LINES);
            let offset = optional_usize(arguments, "offset_from_bottom", 0);
            let (text, offset_from_bottom, rows, older) =
                self.screen_rows(session_id, offset, lines)?;
            Ok(screen_page(&text, offset_from_bottom, rows, older))
        }

        fn wait_for(&self, arguments: &Value) -> Result<String> {
            let session_id = required_str(arguments, "session_id")?.to_string();
            wait_loop(
                arguments,
                LOCAL_TARGET_ID,
                || {
                    Ok(self
                        .sessions()?
                        .into_iter()
                        .find(|session| session.id == session_id))
                },
                || {
                    let (text, ..) = self.screen_rows(&session_id, 0, WAIT_SCREEN_LINES)?;
                    Ok(plain_screen(&text))
                },
            )
        }

        fn trigger(&self, arguments: &Value) -> Result<String> {
            match required_str(arguments, "action")? {
                "set" => {
                    let stored = match self
                        .transact(&DaemonRequest::SetTrigger {
                            trigger: trigger_spec(arguments)?,
                        })?
                        .0
                    {
                        DaemonResponse::Triggers { triggers } => triggers
                            .into_iter()
                            .next()
                            .context("muxloomd stored the trigger but did not report it back")?,
                        response => bail!("unexpected trigger response: {response:?}"),
                    };
                    Ok(pretty(&trigger_json(LOCAL_TARGET_ID, &stored)))
                }
                "list" => {
                    let triggers: Vec<Trigger> = match self
                        .transact(&DaemonRequest::ListTriggers {
                            session_id: optional_str(arguments, "session_id").map(Into::into),
                        })?
                        .0
                    {
                        DaemonResponse::Triggers { triggers } => triggers,
                        response => bail!("unexpected trigger response: {response:?}"),
                    };
                    Ok(pretty(&Value::Array(
                        triggers
                            .iter()
                            .map(|trigger| trigger_json(LOCAL_TARGET_ID, trigger))
                            .collect(),
                    )))
                }
                "delete" => {
                    let id = required_str(arguments, "id")?;
                    self.expect_ack(&DaemonRequest::DeleteTrigger { id: id.into() })?;
                    Ok(format!("deleted trigger {id}"))
                }
                other => bail!("unknown trigger action {other}: use set, list, or delete"),
            }
        }

        fn talk_post(&self, arguments: &Value) -> Result<String> {
            let draft = talk_draft(arguments)?;
            match self.transact(&DaemonRequest::TalkPost { draft })?.0 {
                DaemonResponse::Talk { page } => {
                    let message = page
                        .messages
                        .into_iter()
                        .next()
                        .context("muxloomd took the post but did not report it back")?;
                    Ok(pretty(&talk_json(&message)))
                }
                response => bail!("unexpected talk response: {response:?}"),
            }
        }

        fn talk_read(&self, arguments: &Value) -> Result<String> {
            talk_wait(
                arguments,
                talk_filter(arguments)?,
                |filter| match self
                    .transact(&DaemonRequest::TalkRead {
                        filter: filter.clone(),
                    })?
                    .0
                {
                    DaemonResponse::Talk { page } => Ok(page),
                    response => bail!("unexpected talk response: {response:?}"),
                },
                || self.sessions().unwrap_or_default(),
            )
        }

        /// Whether these arguments name a machine other than this one, in
        /// which case the call is the controller's to make. `local` is what
        /// this surface calls the machine it runs on, so it never travels.
        /// Past that the board knows both names this machine goes by: the key
        /// it mints messages under and the label the controller calls it. Not
        /// knowing them is not fatal — the errand comes back to this daemon
        /// and is answered here, one hop later than it needed to be.
        fn elsewhere(&self, arguments: &Value) -> Option<String> {
            let machine = optional_str(arguments, "machine")?;
            if machine == LOCAL_TARGET_ID {
                return None;
            }
            let here = match self.transact(&DaemonRequest::TalkStatus { label: None }) {
                Ok((DaemonResponse::TalkBoard { state }, _)) => state,
                _ => return Some(machine.into()),
            };
            (machine != here.origin && machine != here.label).then(|| machine.into())
        }

        /// Have the controller run one call and wait for the answer. Every
        /// look is its own short connection, the same as any other call here:
        /// a wait that held one open would be the resident client the daemon
        /// must not have.
        fn relay(&self, tool: &str, arguments: &Value) -> Result<String> {
            let id = match self
                .transact(&DaemonRequest::RelaySubmit {
                    tool: tool.into(),
                    arguments: arguments.to_string(),
                })?
                .0
            {
                DaemonResponse::RelayTicket { id } => id,
                response => bail!("unexpected relay response: {response:?}"),
            };
            let deadline = Instant::now() + RELAY_WAIT;
            loop {
                std::thread::sleep(RELAY_POLL);
                let answer = match self
                    .transact(&DaemonRequest::RelayResult { id: id.clone() })?
                    .0
                {
                    DaemonResponse::Relayed { answer } => answer,
                    response => bail!("unexpected relay response: {response:?}"),
                };
                if answer.done {
                    if answer.ok {
                        return Ok(answer.output);
                    }
                    bail!("{}", answer.output);
                }
                if Instant::now() >= deadline {
                    bail!(
                        "the muxloom controller has not answered in {} seconds. It may be busy or \
                         the machine may be unreachable from it; try again, or ask an agent on \
                         that machine instead.",
                        RELAY_WAIT.as_secs()
                    );
                }
            }
        }

        /// Which machines this agent can reach, and which way.
        ///
        /// A daemon has no idea another machine exists until a controller comes
        /// round and names one, so this is a record of the last round rather
        /// than a search. The machines that are not this one are marked as what
        /// they are: reachable only because something else is carrying, and
        /// only while it still is. An agent that reads `remote` knows the calls
        /// it makes there cost a round trip through a controller, and that the
        /// short list of tools it can make is not the machine being poor.
        ///
        /// A controller too old to say anything about the fleet leaves nothing
        /// to answer from, and then the question goes to it, exactly as it
        /// always did.
        fn list_machines(&self) -> Result<String> {
            let (peers, attached) = match self.transact(&DaemonRequest::RelayPeers)?.0 {
                DaemonResponse::RelayReach { peers, attached } => (peers, attached),
                response => bail!("unexpected relay response: {response:?}"),
            };
            if peers.is_empty() {
                return self.relay("list_machines", &json!({}));
            }
            let own = peers.iter().find(|peer| peer.own);
            let mut machines = vec![json!({
                "id": LOCAL_TARGET_ID,
                "label": own.map_or("This machine", |peer| peer.label.as_str()),
                "fleet_id": own.map(|peer| peer.id.clone()),
                "remote": false,
                "connected": true,
            })];
            for peer in peers.iter().filter(|peer| !peer.own) {
                machines.push(json!({
                    "id": peer.id,
                    "label": peer.label,
                    "remote": true,
                    "via": if peer.via.is_empty() { "the muxloom controller" } else { &peer.via },
                    "connected": attached,
                }));
            }
            Ok(pretty(&Value::Array(machines)))
        }

        fn message_agent(&self, arguments: &Value) -> Result<String> {
            // The board here is the one the message will be filed on, so an
            // author with no machine on it would be right anyway; asking keeps
            // the record the same shape as one that crossed a machine.
            let author = direct_author(|| {
                match self.transact(&DaemonRequest::TalkStatus { label: None })?.0 {
                    DaemonResponse::TalkBoard { state } => Ok(state),
                    response => bail!("unexpected talk response: {response:?}"),
                }
            });
            let (draft, deliver, reply_expected) = direct_draft(arguments, author)?;
            match self
                .transact(&DaemonRequest::TalkDeliver {
                    draft,
                    deliver,
                    reply_expected,
                })?
                .0
            {
                DaemonResponse::TalkDelivery {
                    message,
                    delivery,
                    reason,
                } => Ok(delivery_json(&message, &delivery, reason)),
                response => bail!("unexpected talk response: {response:?}"),
            }
        }

        fn launch_session(&self, arguments: &Value) -> Result<String> {
            let kind = agent_kind(arguments)?;
            let own = self.own_folder.as_deref().context(
                "launch_session starts a session where you are, and muxloom cannot tell which \
                 folder that is",
            )?;
            let path = launch_path_within(arguments, own)?;
            let path = path.as_str();
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
                    parent: launching_session(),
                })?
                .0;
            match response {
                DaemonResponse::Launched { session } => Ok(pretty(&json!({
                    "session_id": session.id,
                    "machine": LOCAL_TARGET_ID,
                    "kind": session.kind,
                    "path": session.path,
                    "parent": session.parent,
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
            // What another machine holds is the controller's to answer. The
            // two that are only ever about other machines go straight out;
            // the rest go out only when they name one.
            if matches!(name, "search_conversations" | "read_conversation")
                || (crate::relay::relayed(name) && self.elsewhere(arguments).is_some())
            {
                return self.relay(name, arguments);
            }
            match name {
                "list_machines" => self.list_machines(),
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
                "wait_for" => self.wait_for(arguments),
                "trigger" => self.trigger(arguments),
                "talk_read" => self.talk_read(arguments),
                "talk_post" => self.talk_post(arguments),
                "message_agent" => self.message_agent(arguments),
                "send_input" => {
                    let session_id = required_str(arguments, "session_id")?;
                    let kind = self
                        .sessions()
                        .ok()
                        .and_then(|sessions| session_kind(&sessions, session_id));
                    let bytes = build_input(arguments, kind)?;
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

    /// A board holding one exchange, so a wait can be run against it.
    fn direct(seq: u64, from: &str, to: &str, ts: u64, text: &str) -> TalkMessage {
        TalkMessage {
            id: format!("here:{seq}"),
            origin: "here".into(),
            seq,
            ts,
            scope: TalkScope::Machine {
                machine: "here".into(),
            },
            author: TalkAuthor {
                machine: "here".into(),
                machine_label: "tiger".into(),
                voice: TalkVoice {
                    session_id: Some(from.into()),
                    label: Some(from.into()),
                    kind: Some("claude".into()),
                    human: false,
                },
            },
            kind: TalkKind::Direct,
            to: Some(TalkAddress {
                machine: "here".into(),
                session_id: to.into(),
            }),
            reply_to: None,
            text: text.into(),
        }
    }

    /// What the daemon puts on the board when a message never arrived: a
    /// direct from muxloom itself, naming the message it is about.
    fn bounced(seq: u64, to: &str, about: &str, ts: u64) -> TalkMessage {
        let mut message = direct(
            seq,
            "-",
            to,
            ts,
            "[muxloom] Your message never reached it: the session ended.",
        );
        message.author.voice = TalkVoice {
            session_id: None,
            label: Some("muxloom".into()),
            kind: None,
            human: false,
        };
        message.reply_to = Some(about.into());
        message
    }

    fn probe_session(id: &str, working: bool, attention: bool) -> DaemonSession {
        DaemonSession {
            id: id.into(),
            kind: "claude".into(),
            path: "/tmp".into(),
            label: id.into(),
            temporary: false,
            created_at: 0,
            pid: Some(1),
            dead: false,
            archived: false,
            recap: None,
            title: None,
            thread: None,
            working,
            needs_attention: attention,
            attention_reason: attention.then(|| "waiting on a person".into()),
            composer: None,
            parent: None,
        }
    }

    #[test]
    fn a_wait_that_ends_empty_says_which_of_its_own_messages_are_still_unanswered() {
        let now = now_ms();
        let board = vec![
            // Answered: they said something back afterwards.
            direct(1, "me", "settled", now - 600_000, "did you land that?"),
            direct(2, "settled", "me", now - 500_000, "yes, an hour ago"),
            // Outstanding, and the session is mid-turn.
            direct(
                3,
                "me",
                "thinking",
                now - 400_000,
                "can you take the lexer?",
            ),
            // Outstanding, asked twice; only the later one is worth reporting.
            direct(4, "me", "gone-quiet", now - 300_000, "any thoughts?"),
            direct(5, "me", "gone-quiet", now - 200_000, "still after those"),
            // Somebody else's exchange is none of this session's business.
            direct(6, "other", "thinking", now - 100_000, "and from me"),
            // Bounced: nobody read it, and the daemon said so. It is not that
            // session talking, so only the id it names closes the question.
            direct(
                7,
                "me",
                "vanished",
                now - 150_000,
                "still on for the merge?",
            ),
            bounced(8, "me", "here:7", now - 140_000),
        ];
        let filter = TalkFilter {
            session_id: Some("me".into()),
            ..TalkFilter::default()
        };
        let answer: Value = serde_json::from_str(
            &talk_wait(
                &json!({ "scope": "direct", "wait_seconds": 1 }),
                filter,
                |_| {
                    Ok(TalkPage {
                        messages: Vec::new(),
                        cursor: String::new(),
                        truncated: false,
                    })
                },
                Vec::new,
            )
            .unwrap(),
        )
        .unwrap();
        // Nothing on the board to read means nothing to report, and no advice
        // invented out of an empty exchange.
        assert_eq!(answer["waiting_on"], Value::Null);
        assert_eq!(answer["note"], Value::Null);

        let filter = TalkFilter {
            session_id: Some("me".into()),
            ..TalkFilter::default()
        };
        let answer: Value = serde_json::from_str(
            &talk_wait(
                &json!({ "scope": "direct", "wait_seconds": 1 }),
                filter,
                |filter| {
                    // Nothing new is said while the wait runs. The look for
                    // outstanding messages reaches every machine, and that is
                    // the read the whole board answers.
                    let sweep = matches!(filter.machines, TalkSelector::All);
                    Ok(TalkPage {
                        messages: if sweep { board.clone() } else { Vec::new() },
                        cursor: String::new(),
                        truncated: false,
                    })
                },
                || {
                    vec![
                        probe_session("thinking", true, false),
                        probe_session("gone-quiet", false, false),
                    ]
                },
            )
            .unwrap(),
        )
        .unwrap();
        let waiting = answer["waiting_on"].as_array().unwrap();
        // The bounced one is not among them: it was answered, by the only
        // answer it was ever going to get.
        assert_eq!(waiting.len(), 2, "{answer:#}");
        // Oldest first, and the follow-up stands in for the pair.
        assert_eq!(waiting[0]["message_id"], "here:3");
        assert_eq!(waiting[1]["message_id"], "here:5");
        assert!(
            waiting[0]["reading"].as_str().unwrap().contains("mid-turn"),
            "{answer:#}"
        );
        assert!(
            waiting[1]["reading"].as_str().unwrap().contains("idle"),
            "{answer:#}"
        );
        let note = answer["note"].as_str().unwrap();
        assert!(note.contains("2 messages"), "{note}");
        assert!(note.contains("mid-turn"), "{note}");
    }

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
        let bytes = build_input(
            &json!({
                "text": "ls",
                "keys": ["tab", "ctrl-a"],
                "submit": true,
            }),
            None,
        )
        .unwrap();
        assert_eq!(bytes, b"ls\t\x01\r");
        assert!(build_input(&json!({}), None).is_err());
        assert_eq!(build_input(&json!({"submit": true}), None).unwrap(), b"\r");
    }

    #[test]
    fn text_for_a_cli_is_bracketed_so_its_enter_still_submits() {
        // Long enough that Claude Code would read an unbracketed trailing return
        // as part of the paste and leave it all sitting in the prompt.
        let text = "a".repeat(200);
        let bytes = build_input(
            &json!({ "text": text, "submit": true }),
            Some(AgentKind::Claude),
        )
        .unwrap();
        let mut want = b"\x1b[200~".to_vec();
        want.extend(text.as_bytes());
        want.extend_from_slice(b"\x1b[201~\r");
        assert_eq!(bytes, want);

        // A shell never asked for brackets, so it keeps getting plain typing.
        assert_eq!(
            build_input(
                &json!({"text": "ls", "submit": true}),
                Some(AgentKind::Terminal)
            )
            .unwrap(),
            b"ls\r"
        );
    }

    #[cfg(feature = "controller")]
    #[test]
    fn a_conversation_page_stops_at_its_budget_and_says_where_to_carry_on() {
        let said = |text: &str| crate::backup::ExtractedMessage {
            role: "user".into(),
            text: text.into(),
            ts: "2026-08-18T14:02:11Z".into(),
        };
        let window = vec![(4, said("aaaa")), (5, said("bbbbbb")), (6, said("c"))];

        // Everything fits: the whole window comes back, nowhere left to go.
        let (page, next, clipped) = conversation_page(&window, 100);
        assert_eq!(page.len(), 3);
        assert_eq!(page[0]["index"], 4);
        assert_eq!(page[2]["text"], "c");
        assert_eq!(next, None);
        assert!(!clipped);

        // The budget runs out inside the second message: it comes back cut and
        // flagged, and the cursor points at it so the next page reads it whole.
        let (page, next, clipped) = conversation_page(&window, 6);
        assert_eq!(page.len(), 2);
        assert_eq!(page[1]["text"], "bb");
        assert_eq!(page[1]["truncated"], true);
        assert!(clipped);
        assert_eq!(next, Some(5));

        // One message alone over the budget can't be the cursor, or paging
        // forward would hand back the same half forever.
        let (page, next, _) = conversation_page(&window[1..], 2);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["text"], "bb");
        assert_eq!(next, Some(6));

        // Nothing to read pages nowhere.
        assert_eq!(conversation_page(&[], 100), (Vec::new(), None, false));
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
            // How far a launch reaches is the other difference, and the one
            // this tool exists to state: the controller names any folder on
            // any machine, and the daemon starts subagents where the caller
            // already is, so there the folder is optional and bounded.
            if tool.name == "launch_session" {
                assert_eq!(tool.input_schema["required"], json!(["kind"]));
                assert_eq!(twin.input_schema["required"], json!(["kind", "path"]));
                continue;
            }
            // Machine addressing is the difference between the surfaces: the
            // controller reaches every machine, and the daemon only the few
            // the controller will run errands for. Everything else matches.
            let mut twin_schema = twin.input_schema.clone();
            let mut tool_schema = tool.input_schema.clone();
            twin_schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("machine");
            if tool_schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("machine")
                .is_some()
            {
                assert!(
                    crate::relay::relayed(tool.name),
                    "{} takes a machine the controller will not relay",
                    tool.name
                );
            }
            assert_eq!(twin_schema, tool_schema, "{} diverged", tool.name);
        }
        for name in [
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
    fn a_daemon_surface_launch_stays_in_the_folder_it_was_called_from() {
        let own = "/home/agent/project";
        // The ordinary call: no folder named, so the caller's own.
        assert_eq!(
            launch_path_within(&json!({ "kind": "claude" }), own).unwrap(),
            own
        );
        for inside in [
            own,
            "/home/agent/project/crates/core",
            "/home/agent/project/",
        ] {
            assert_eq!(
                launch_path_within(&json!({ "path": inside }), own).unwrap(),
                inside,
                "{inside} is inside {own}"
            );
        }
        // A prefix in text is not a folder in the tree, and the parent, a
        // sibling, and the root are all somebody else's work.
        for outside in [
            "/home/agent",
            "/home/agent/project-notes",
            "/home/agent/other",
            "/",
            "/etc",
        ] {
            let error = launch_path_within(&json!({ "path": outside }), own)
                .expect_err("{outside} is outside {own}")
                .to_string();
            // Refusing is half of it: the answer has to say where the agent
            // actually is, what it asked for, and what to do instead, or the
            // next thing it tries is a guess.
            assert!(error.contains(own), "{error}");
            assert!(error.contains(outside), "{error}");
            assert!(error.contains("message_agent"), "{error}");
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
                "list_machines",
                "list_sessions",
                "read_screen",
                // Waiting only watches; arming a trigger acts, so `trigger`
                // is not here. Reading the board is watching too — saying
                // something on it is not.
                "wait_for",
                "talk_read",
                "search_history",
                // Reading someone else's conversation back is watching as
                // well, even when the controller has to fetch it.
                "search_conversations",
                "read_conversation",
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
        // The daemon surface reaches other machines only through the
        // controller, and only for the errands it will run. The list of those
        // is too long to recite now, so the instructions state the rule
        // instead: look and speak, but do not change.
        let daemon = instructions(Flavor::Daemon, &McpConfig::default());
        assert!(daemon.contains("`machine` argument"));
        assert!(daemon.contains("remote"));
        assert!(daemon.contains("every tool that reads"));
        assert!(daemon.contains("message_agent"));
        assert!(daemon.contains("not yours to do"));
        assert!(daemon.contains("not a reason to retry"));
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
    fn a_call_coming_back_from_the_fleet_calls_this_machine_by_its_hostname() {
        let (control, root) = controller_over_temp("hostname");
        let host = crate::talk::hostname();

        // The name every daemon was handed for this machine, because `local`
        // out there means the machine the agent is already sitting on.
        assert_eq!(
            control
                .target(&json!({ "machine": host.clone() }))
                .unwrap()
                .id,
            "local"
        );
        // A name nobody handed out is still a machine that is not enabled.
        assert!(
            control
                .target(&json!({ "machine": format!("{host}-elsewhere") }))
                .is_err()
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
        use crate::{
            config::Config,
            daemon::DaemonPaths,
            daemon_protocol::{DaemonRequest, DaemonResponse, Frame, FrameKind},
        };
        use serde_json::{Value, json};

        /// One serve() loop on a temporary state directory, reached the same
        /// way a real `muxloomd mcp` reaches the real daemon.
        fn surface(name: &str) -> DaemonControl {
            surface_with(name, Config::default())
        }

        /// The same, with the commands the adapter would launch agents by —
        /// tests that need a session the daemon reads as an agent's point
        /// `claude` at a shell.
        fn surface_with(name: &str, config: Config) -> DaemonControl {
            surface_and_paths(name, config).0
        }

        /// The same again, keeping the state directory: a test that plays the
        /// controller as well as the agent needs its own way in.
        fn surface_and_paths(name: &str, config: Config) -> (DaemonControl, DaemonPaths) {
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
            // The temp dir stands in for the caller's own folder: it is where
            // these tests start their sessions, and a launch on this surface
            // reaches no further than that.
            let own = std::env::temp_dir().to_string_lossy().into_owned();
            (
                DaemonControl::with_paths(paths.clone(), config, Some(own)),
                paths,
            )
        }

        /// One request to the daemon as something other than the surface under
        /// test — the controller, in the tests that need one.
        fn ask(paths: &DaemonPaths, request: &DaemonRequest) -> DaemonResponse {
            let mut connection = crate::daemon::connect_or_start(paths).unwrap();
            Frame::json(FrameKind::Request, 0, 1, request)
                .unwrap()
                .write_to(&mut connection)
                .unwrap();
            loop {
                let frame = Frame::read_from(&mut connection).unwrap().unwrap();
                if frame.kind == FrameKind::Response && frame.request_id == 1 {
                    return frame.decode_json::<DaemonResponse>().unwrap();
                }
            }
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

        #[test]
        fn a_trigger_fires_once_and_wait_for_sees_what_it_did() {
            let mut surface = surface("trg");
            let launched: Value = serde_json::from_str(&call(
                &mut surface,
                "launch_session",
                json!({
                    "kind": "terminal",
                    "path": std::env::temp_dir().to_str().unwrap(),
                }),
            ))
            .unwrap();
            let session_id = launched["session_id"].as_str().unwrap().to_string();

            let armed: Value = serde_json::from_str(&call(
                &mut surface,
                "trigger",
                json!({
                    "action": "set",
                    "session_id": session_id,
                    "pattern": "muxloom-trigger-probe",
                    "action_kind": "notify",
                    "text": "the probe printed",
                }),
            ))
            .unwrap();
            let trigger_id = armed["id"].as_str().unwrap().to_string();
            assert_eq!(armed["fires"], 0);
            assert!(
                call(&mut surface, "trigger", json!({ "action": "list" })).contains(&trigger_id)
            );

            // Printing the marker is what the trigger is watching for, and
            // waiting for it is what an agent that stays does instead.
            call(
                &mut surface,
                "send_input",
                json!({
                    "session_id": session_id,
                    // Split so that typing the command does not itself put the
                    // pattern on screen: the trigger fires on arrival, and the
                    // echo of the command line would be an arrival.
                    "text": "printf 'muxloom-%s-probe\\n' trigger",
                    "submit": true,
                }),
            );
            let waited: Value = serde_json::from_str(&call(
                &mut surface,
                "wait_for",
                json!({
                    "session_id": session_id,
                    "until": "output_matches",
                    "pattern": "muxloom-trigger-probe",
                    "timeout_seconds": 10,
                    "poll_ms": 200,
                }),
            ))
            .unwrap();
            assert_eq!(waited["outcome"], "matched", "{waited:#}");
            assert_eq!(waited["satisfied"], true);
            assert!(
                waited["matched"]
                    .as_str()
                    .unwrap()
                    .contains("muxloom-trigger-probe")
            );

            // The trigger sees the same screen, so by now it has fired: its
            // notice is what list_sessions reports as the reason.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut listed = String::new();
            while Instant::now() < deadline {
                listed = call(&mut surface, "list_sessions", json!({}));
                if listed.contains("the probe printed") {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            assert!(
                listed.contains("the probe printed"),
                "a fired trigger must show up as attention: {listed}"
            );
            // once defaults to true, so it is gone rather than armed again.
            assert!(
                !call(&mut surface, "trigger", json!({ "action": "list" })).contains(&trigger_id)
            );

            // Typing is the human being there, which retires the notice.
            call(
                &mut surface,
                "send_input",
                json!({ "session_id": session_id, "keys": ["enter"] }),
            );
            assert!(!call(&mut surface, "list_sessions", json!({})).contains("the probe printed"));

            let waited: Value = serde_json::from_str(&call(
                &mut surface,
                "wait_for",
                json!({
                    "session_id": session_id,
                    "until": "attention",
                    "timeout_seconds": 1,
                    "poll_ms": 200,
                }),
            ))
            .unwrap();
            assert_eq!(waited["outcome"], "timeout", "{waited:#}");
            assert_eq!(waited["satisfied"], false);

            call(
                &mut surface,
                "delete_session",
                json!({ "session_id": session_id }),
            );
        }

        #[test]
        fn a_post_lands_on_the_board_the_reader_is_standing_on() {
            let mut surface = surface("talk");
            let here = "/tmp/muxloom-talk-here";
            let elsewhere = "/tmp/muxloom-talk-elsewhere";

            let posted: Value = serde_json::from_str(&call(
                &mut surface,
                "talk_post",
                json!({ "text": "the kettle is on", "path": here }),
            ))
            .unwrap();
            assert_eq!(posted["scope"], "path");
            assert_eq!(posted["scope_path"], here);
            assert_eq!(posted["kind"], "message");
            // A poster names the board; the daemon that mints the message is
            // the one that knows which machine it is.
            assert!(
                posted["scope_machine"]
                    .as_str()
                    .is_some_and(|machine| !machine.is_empty()),
                "{posted:#}"
            );

            call(
                &mut surface,
                "talk_post",
                json!({
                    "text": "the flour is in the second drawer",
                    "scope": "global",
                    "kind": "note",
                }),
            );

            let texts = |page: &Value| -> Vec<String> {
                page["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|message| message["text"].as_str().unwrap().to_string())
                    .collect()
            };
            let read = |surface: &mut DaemonControl, arguments: Value| -> Value {
                serde_json::from_str(&call(surface, "talk_read", arguments)).unwrap()
            };

            let page = read(&mut surface, json!({ "path": here }));
            assert_eq!(
                texts(&page),
                ["the kettle is on", "the flour is in the second drawer"],
                "{page:#}"
            );

            // Standing somewhere else, that project's board is not yours —
            // what was said to everyone still is.
            let page_elsewhere = read(&mut surface, json!({ "path": elsewhere }));
            assert_eq!(
                texts(&page_elsewhere),
                ["the flour is in the second drawer"]
            );
            // Unless you ask for it by name, which is how a board is looked
            // into from outside.
            let widened = read(
                &mut surface,
                json!({ "path": elsewhere, "include_paths": [here] }),
            );
            assert_eq!(texts(&widened).len(), 2, "{widened:#}");

            let global_only = read(&mut surface, json!({ "path": here, "scope": "global" }));
            assert_eq!(texts(&global_only), ["the flour is in the second drawer"]);

            // A cursor is how an agent polls without reading the board twice,
            // and waiting on one that has nothing new answers rather than
            // hanging on.
            let cursor = page["cursor"].as_str().unwrap().to_string();
            let waited = read(
                &mut surface,
                json!({ "path": here, "since_cursor": cursor, "wait_seconds": 1 }),
            );
            assert!(texts(&waited).is_empty(), "{waited:#}");
            assert!(waited["waited_ms"].as_u64().unwrap() >= 900, "{waited:#}");

            call(
                &mut surface,
                "talk_post",
                json!({ "text": "the kettle boiled", "path": here }),
            );
            let after = read(
                &mut surface,
                json!({ "path": here, "since_cursor": cursor }),
            );
            assert_eq!(texts(&after), ["the kettle boiled"]);

            // A direct message goes to a session, not to a board.
            let refused = surface
                .call("talk_post", &json!({ "text": "psst", "kind": "direct" }))
                .unwrap_err()
                .to_string();
            assert!(refused.contains("message_agent"), "{refused}");
        }

        #[test]
        fn an_errand_reaches_a_watching_controller_and_fails_at_once_without_one() {
            let (mut surface, paths) = surface_and_paths("relay", Config::default());

            // Nothing is watching this machine. The agent is told so on the
            // call it made, rather than left holding a minute-long wait.
            let started = Instant::now();
            let error = surface
                .call("list_machines", &json!({}))
                .unwrap_err()
                .to_string();
            assert!(error.contains("attached muxloom controller"), "{error}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "waited {:?} to be told nobody is there",
                started.elapsed()
            );

            // A controller turns up and asks for work, which is the only way
            // this daemon learns it is there.
            let controller = paths.clone();
            let (ready, attached) = std::sync::mpsc::channel();
            let round = thread::spawn(move || {
                let first = ask(
                    &controller,
                    &DaemonRequest::RelayPoll {
                        peers: Vec::new(),
                        via: String::new(),
                    },
                );
                assert!(
                    matches!(&first, DaemonResponse::RelayWork { jobs, .. } if jobs.is_empty()),
                    "{first:?}"
                );
                ready.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let DaemonResponse::RelayWork { jobs, .. } = ask(
                        &controller,
                        &DaemonRequest::RelayPoll {
                            peers: Vec::new(),
                            via: String::new(),
                        },
                    ) && let Some(job) = jobs.into_iter().next()
                    {
                        assert_eq!(job.tool, "search_conversations");
                        let arguments: Value = serde_json::from_str(&job.arguments).unwrap();
                        assert_eq!(arguments["query"], "the envelope");
                        ask(
                            &controller,
                            &DaemonRequest::RelayComplete {
                                id: job.id,
                                ok: true,
                                output: "[]".into(),
                            },
                        );
                        return;
                    }
                    assert!(Instant::now() < deadline, "the errand never came round");
                    thread::sleep(Duration::from_millis(50));
                }
            });
            attached.recv().unwrap();

            // The corpus lives on the controller, so the answer does too: what
            // comes back is whatever it said, verbatim.
            let answer = call(
                &mut surface,
                "search_conversations",
                json!({ "query": "the envelope" }),
            );
            assert_eq!(answer, "[]");
            round.join().unwrap();
        }

        #[test]
        fn the_fleet_an_agent_sees_is_the_one_a_controller_came_round_and_named() {
            let (mut surface, paths) = surface_and_paths("reach", Config::default());

            // A controller comes round and says where it can reach, which is
            // the only way this daemon ever learns another machine exists.
            let poll = DaemonRequest::RelayPoll {
                peers: vec![
                    crate::relay::RelayPeer {
                        id: "seed".into(),
                        label: "seed".into(),
                        own: true,
                        ..Default::default()
                    },
                    crate::relay::RelayPeer {
                        id: "laptop".into(),
                        label: "laptop".into(),
                        ..Default::default()
                    },
                ],
                via: "laptop".into(),
            };
            assert!(matches!(
                ask(&paths, &poll),
                DaemonResponse::RelayWork { .. }
            ));

            let listed: Value =
                serde_json::from_str(&call(&mut surface, "list_machines", json!({}))).unwrap();
            let machines = listed.as_array().unwrap();
            assert_eq!(machines.len(), 2);

            // This machine answers as itself, under the id every tool here
            // takes, and carries the name the fleet knows it by as well.
            assert_eq!(machines[0]["id"], "local");
            assert_eq!(machines[0]["remote"], false);
            assert_eq!(machines[0]["fleet_id"], "seed");
            assert_eq!(machines[0]["label"], "seed");

            // Everywhere else is marked for what it is: borrowed reach, named
            // with what is doing the carrying, and only while it is there. The
            // controller's own machine is not `local` out here — that word is
            // taken, by the machine the agent asking is sitting on.
            assert_eq!(machines[1]["id"], "laptop");
            assert_eq!(machines[1]["remote"], true);
            assert_eq!(machines[1]["via"], "laptop");
            assert_eq!(machines[1]["connected"], true);
        }

        #[test]
        fn a_message_waits_for_a_working_agent_and_the_board_keeps_the_receipt() {
            // A stand-in for Claude Code, because a bare shell no longer is
            // one: what decides whether a message goes in is the prompt box on
            // screen, so the stand-in draws a prompt box. It echoes each line
            // it is handed, and starts out carrying the marker muxloom reads as
            // working, which is how the test can tell the daemon's poll loop
            // has actually looked at the screen and not just that the screen
            // has something on it.
            let script = std::env::temp_dir().join(format!(
                "mxl-fake-claude-{}-{}.sh",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            ));
            std::fs::write(
                &script,
                "rule='────────────────────────────────────────'\n\
                 hint='  esc to interrupt'\n\
                 draw() { printf '%s\\n❯ \\n%s\\n%s\\n' \"$rule\" \"$rule\" \"$hint\"; }\n\
                 draw\n\
                 while IFS= read -r line; do\n\
                 \x20 printf '%s\\n' \"$line\"\n\
                 \x20 case $line in\n\
                 \x20   *settled*) hint='  ⏵⏵ accepts edits on' ;;\n\
                 \x20 esac\n\
                 \x20 draw\n\
                 done\n",
            )
            .unwrap();
            let mut config = Config::default();
            config.agents.claude.command = "sh".into();
            config.agents.claude.args = vec![script.to_str().unwrap().to_string()];
            let mut surface = surface_with("msg", config);
            let workdir = std::env::temp_dir().to_str().unwrap().to_string();

            let launch = |surface: &mut DaemonControl, kind: &str| -> String {
                let launched: Value = serde_json::from_str(&call(
                    surface,
                    "launch_session",
                    json!({ "kind": kind, "path": workdir }),
                ))
                .unwrap();
                launched["session_id"].as_str().unwrap().to_string()
            };
            let until = |surface: &mut DaemonControl, session: &str, marker: &str| -> String {
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let screen = call(
                        surface,
                        "read_screen",
                        json!({ "session_id": session, "lines": 60 }),
                    );
                    if screen.contains(marker) {
                        return screen;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "no {marker:?} on screen: {screen}"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
            };
            // What decides a delivery is the snapshot the daemon's poll loop
            // writes, which lands a moment after the screen it was read from.
            // Waiting on the daemon's own account of the session is the only
            // way to know that pass has happened; waiting on the screen races
            // it. The stand-in starts out working, so the flag going true and
            // then false is a poll pass observed from the outside.
            let reads = |surface: &mut DaemonControl, session: &str, working: bool| {
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let listed: Value =
                        serde_json::from_str(&call(surface, "list_sessions", json!({}))).unwrap();
                    let seen = listed
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|entry| entry["session_id"] == session)
                        .map(|entry| entry["working"] == Value::Bool(true));
                    if seen == Some(working) {
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{session} never read as working={working}: {listed:#}"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
            };

            // Nobody is reading a shell, so nothing is typed into one.
            let shell = launch(&mut surface, "terminal");
            let refused = surface
                .call(
                    "message_agent",
                    &json!({ "session_id": shell, "text": "are you there" }),
                )
                .unwrap_err()
                .to_string();
            assert!(refused.contains("send_input"), "{refused}");

            // A session that is not working gets it straight away.
            let idle = launch(&mut surface, "claude");
            reads(&mut surface, &idle, true);
            call(
                &mut surface,
                "send_input",
                json!({ "session_id": idle, "text": "settled", "submit": true }),
            );
            reads(&mut surface, &idle, false);
            let sent: Value = serde_json::from_str(&call(
                &mut surface,
                "message_agent",
                json!({
                    "session_id": idle,
                    "text": "the parser is yours, I am on the lexer",
                    "reply_expected": true,
                }),
            ))
            .unwrap();
            assert_eq!(sent["delivery"], "delivered", "{sent:#}");
            until(&mut surface, &idle, "Message from");

            // A working session is left alone by a when_idle message, even
            // though its prompt box is empty and an ordinary one would go in.
            let busy = launch(&mut surface, "claude");
            reads(&mut surface, &busy, true);
            let queued: Value = serde_json::from_str(&call(
                &mut surface,
                "message_agent",
                json!({
                    "session_id": busy,
                    "text": "when you surface, the lexer needs a second pair of eyes",
                    "deliver": "when_idle",
                }),
            ))
            .unwrap();
            assert_eq!(queued["delivery"], "queued", "{queued:#}");

            // Sending again immediately is how one agent drowns another.
            let too_soon = surface
                .call(
                    "message_agent",
                    &json!({ "session_id": busy, "text": "and also" }),
                )
                .unwrap_err()
                .to_string();
            assert!(too_soon.contains("wait"), "{too_soon}");

            // The marker going off the screen is the session going quiet, and
            // the queue notices within a second.
            call(
                &mut surface,
                "send_input",
                json!({ "session_id": busy, "text": "settled", "submit": true }),
            );
            until(&mut surface, &busy, "Message from");

            // Both messages are on the board, so what the agents said to each
            // other can be read by anyone who was not in the room.
            let board: Value = serde_json::from_str(&call(
                &mut surface,
                "talk_read",
                json!({ "scope": "direct" }),
            ))
            .unwrap();
            let texts: Vec<&str> = board["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["text"].as_str().unwrap())
                .collect();
            assert_eq!(
                texts,
                [
                    "the parser is yours, I am on the lexer",
                    "when you surface, the lexer needs a second pair of eyes",
                ],
                "{board:#}"
            );
            assert_eq!(board["messages"][0]["kind"], "direct");
            assert_eq!(board["messages"][0]["to"]["session_id"], idle);

            for session in [shell, idle, busy] {
                call(
                    &mut surface,
                    "delete_session",
                    json!({ "session_id": session }),
                );
            }
            let _ = std::fs::remove_file(&script);
        }
    }
}
